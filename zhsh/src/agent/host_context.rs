//! 发送给 LLM 的最小宿主上下文。
//!
//! 本模块只投影固定白名单字段。完整环境、PATH、Shell 状态、LLM 配置和 Safety
//! 规则都不属于该结构，也不能通过通用 map 扩展进来。

use crate::shell::Shell;
use serde::Serialize;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::OnceLock;

const OS_RELEASE_MAX_BYTES: usize = 16 * 1024;
const HOST_FIELD_MAX_BYTES: usize = 128;
const TOKEN_MAX_BYTES: usize = 64;
const CWD_MAX_BYTES: usize = 4096;

/// REPL 生命周期内复用的进程级宿主事实缓存。
pub(super) struct HostContextProvider {
    facts: OnceLock<HostFacts>,
    collector: fn() -> HostFacts,
}

impl HostContextProvider {
    /// 构造本身不读取文件，也不执行系统调用。
    pub(super) const fn new() -> Self {
        Self {
            facts: OnceLock::new(),
            collector: collect_host_facts,
        }
    }

    /// 在第一次 Agent 任务时采集一次稳定事实，并为当前任务投影 cwd 和 Locale。
    pub(super) fn context_for(&self, shell: &Shell) -> LlmHostContext {
        let facts = self.facts.get_or_init(self.collector);
        LlmHostContext::from_facts_and_session(facts, shell)
    }

    #[cfg(test)]
    fn with_collector(collector: fn() -> HostFacts) -> Self {
        Self {
            facts: OnceLock::new(),
            collector,
        }
    }

    #[cfg(test)]
    pub(super) fn fixture() -> Self {
        let provider = Self::with_collector(fixture_host_facts);
        let _ = provider.facts.set(fixture_host_facts());
        provider
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HostFacts {
    os: OsFacts,
    kernel: KernelFacts,
    process_arch: &'static str,
    privilege: &'static str,
    memory_total_bytes: Option<u64>,
    zhsh_version: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OsFacts {
    id: Option<String>,
    version_id: Option<String>,
    id_like: Vec<String>,
    codename: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct KernelFacts {
    release: Option<String>,
    machine: Option<String>,
}

/// 唯一允许序列化到 Provider 消息中的宿主状态视图。
#[derive(Debug, Serialize)]
pub(super) struct LlmHostContext {
    schema_version: u8,
    scope: &'static str,
    os: LlmOsContext,
    kernel: LlmKernelContext,
    process: LlmProcessContext,
    memory: LlmMemoryContext,
    zhsh_version: &'static str,
    cwd: Option<String>,
}

#[derive(Debug, Serialize)]
struct LlmOsContext {
    id: Option<String>,
    version_id: Option<String>,
    id_like: Vec<String>,
    codename: Option<String>,
}

#[derive(Debug, Serialize)]
struct LlmKernelContext {
    release: Option<String>,
    machine: Option<String>,
}

#[derive(Debug, Serialize)]
struct LlmProcessContext {
    arch: &'static str,
    privilege: &'static str,
    locale: String,
}

#[derive(Debug, Serialize)]
struct LlmMemoryContext {
    total_bytes: Option<u64>,
    scope: &'static str,
}

impl LlmHostContext {
    fn from_facts_and_session(facts: &HostFacts, shell: &Shell) -> Self {
        Self {
            schema_version: 1,
            scope: "process_cached",
            os: LlmOsContext {
                id: facts.os.id.clone(),
                version_id: facts.os.version_id.clone(),
                id_like: facts.os.id_like.clone(),
                codename: facts.os.codename.clone(),
            },
            kernel: LlmKernelContext {
                release: facts.kernel.release.clone(),
                machine: facts.kernel.machine.clone(),
            },
            process: LlmProcessContext {
                arch: facts.process_arch,
                privilege: facts.privilege,
                locale: effective_locale(shell),
            },
            memory: LlmMemoryContext {
                total_bytes: facts.memory_total_bytes,
                scope: "kernel_visible",
            },
            zhsh_version: facts.zhsh_version,
            cwd: project_cwd(shell),
        }
    }

    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).expect("LLM host context contains only serializable fields")
    }
}

fn collect_host_facts() -> HostFacts {
    HostFacts {
        os: collect_os_release(),
        kernel: collect_uname(),
        process_arch: std::env::consts::ARCH,
        privilege: collect_privilege(),
        memory_total_bytes: collect_total_memory(),
        zhsh_version: env!("CARGO_PKG_VERSION"),
    }
}

fn collect_os_release() -> OsFacts {
    collect_os_release_from(
        Path::new("/etc/os-release"),
        Path::new("/usr/lib/os-release"),
    )
}

fn collect_os_release_from(primary: &Path, fallback: &Path) -> OsFacts {
    let content = match read_limited(primary) {
        Ok(content) => content,
        Err(ReadLimitedError::NotFound) => match read_limited(fallback) {
            Ok(content) => content,
            Err(ReadLimitedError::NotFound | ReadLimitedError::Invalid) => {
                return OsFacts::default();
            }
        },
        Err(ReadLimitedError::Invalid) => return OsFacts::default(),
    };
    parse_os_release(&content)
}

enum ReadLimitedError {
    NotFound,
    Invalid,
}

fn read_limited(path: &Path) -> Result<String, ReadLimitedError> {
    let file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ReadLimitedError::NotFound
        } else {
            ReadLimitedError::Invalid
        }
    })?;
    let mut bytes = Vec::with_capacity(OS_RELEASE_MAX_BYTES.min(4096));
    file.take((OS_RELEASE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadLimitedError::Invalid)?;
    if bytes.len() > OS_RELEASE_MAX_BYTES {
        return Err(ReadLimitedError::Invalid);
    }
    String::from_utf8(bytes).map_err(|_| ReadLimitedError::Invalid)
}

fn parse_os_release(content: &str) -> OsFacts {
    let mut values: [Option<String>; 4] = std::array::from_fn(|_| None);
    let mut seen = [false; 4];
    let mut duplicated = [false; 4];

    for line in content.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let Some(index) = os_release_key_index(key) else {
            continue;
        };
        if seen[index] {
            duplicated[index] = true;
            values[index] = None;
            continue;
        }
        seen[index] = true;
        values[index] = parse_os_release_value(raw_value);
    }

    for (index, is_duplicated) in duplicated.into_iter().enumerate() {
        if is_duplicated {
            values[index] = None;
        }
    }

    OsFacts {
        id: values[0].as_deref().and_then(normalize_id_token),
        version_id: values[1].as_deref().and_then(normalize_version_id),
        id_like: values[2]
            .as_deref()
            .map(normalize_id_like)
            .unwrap_or_default(),
        codename: values[3].as_deref().and_then(normalize_id_token),
    }
}

fn os_release_key_index(key: &str) -> Option<usize> {
    match key {
        "ID" => Some(0),
        "VERSION_ID" => Some(1),
        "ID_LIKE" => Some(2),
        "VERSION_CODENAME" => Some(3),
        _ => None,
    }
}

fn parse_os_release_value(raw: &str) -> Option<String> {
    if raw.contains(['\0', '\n', '\r', '$', '`']) {
        return None;
    }
    if let Some(inner) = raw
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        if inner.contains('\'') {
            return None;
        }
        return Some(inner.to_string());
    }
    if let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        let mut decoded = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(character) = chars.next() {
            if character != '\\' {
                if character == '"' {
                    return None;
                }
                decoded.push(character);
                continue;
            }
            match chars.next()? {
                '\\' => decoded.push('\\'),
                '"' => decoded.push('"'),
                _ => return None,
            }
        }
        return Some(decoded);
    }
    if raw.contains(['\'', '"', '\\']) || raw.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    Some(raw.to_string())
}

fn normalize_id_token(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > TOKEN_MAX_BYTES || !value.is_ascii() {
        return None;
    }
    let normalized = value.to_ascii_lowercase();
    normalized
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        .then_some(normalized)
}

fn normalize_version_id(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > TOKEN_MAX_BYTES || !value.is_ascii() {
        return None;
    }
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        .then(|| value.to_string())
}

fn normalize_id_like(value: &str) -> Vec<String> {
    let parts: Vec<_> = value.split_ascii_whitespace().collect();
    if parts.is_empty() || parts.len() > 8 {
        return Vec::new();
    }
    let mut normalized = Vec::with_capacity(parts.len());
    for part in parts {
        let Some(token) = normalize_id_token(part) else {
            return Vec::new();
        };
        if !normalized.contains(&token) {
            normalized.push(token);
        }
    }
    normalized
}

#[cfg(unix)]
fn collect_uname() -> KernelFacts {
    let mut value = std::mem::MaybeUninit::<libc::utsname>::zeroed();
    // SAFETY: `value` points to writable storage for libc::uname. A zero return initializes it.
    if unsafe { libc::uname(value.as_mut_ptr()) } != 0 {
        return KernelFacts::default();
    }
    // SAFETY: successful libc::uname initialized the complete utsname structure.
    let value = unsafe { value.assume_init() };
    KernelFacts {
        release: normalize_uts_field(&value.release),
        machine: normalize_uts_field(&value.machine),
    }
}

#[cfg(not(unix))]
fn collect_uname() -> KernelFacts {
    KernelFacts::default()
}

#[cfg(unix)]
fn normalize_uts_field(field: &[libc::c_char]) -> Option<String> {
    let end = field.iter().position(|character| *character == 0)?;
    if end == 0 || end > HOST_FIELD_MAX_BYTES {
        return None;
    }
    let bytes: Vec<u8> = field[..end]
        .iter()
        .map(|character| *character as u8)
        .collect();
    if !bytes.iter().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+' | b'~')
    }) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(target_os = "linux")]
fn collect_total_memory() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::sysinfo>::zeroed();
    // SAFETY: `info` points to writable storage for libc::sysinfo. A zero return initializes it.
    if unsafe { libc::sysinfo(info.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: successful libc::sysinfo initialized the complete sysinfo structure.
    let info = unsafe { info.assume_init() };
    total_memory_bytes(info.totalram as u128, info.mem_unit as u128)
}

#[cfg(not(target_os = "linux"))]
fn collect_total_memory() -> Option<u64> {
    None
}

fn total_memory_bytes(totalram: u128, mem_unit: u128) -> Option<u64> {
    if mem_unit == 0 {
        return None;
    }
    let bytes = totalram.checked_mul(mem_unit)?;
    u64::try_from(bytes).ok()
}

#[cfg(unix)]
fn collect_privilege() -> &'static str {
    // SAFETY: geteuid has no preconditions and returns the effective process UID.
    if unsafe { libc::geteuid() } == 0 {
        "root"
    } else {
        "user"
    }
}

#[cfg(not(unix))]
fn collect_privilege() -> &'static str {
    "unknown"
}

fn project_cwd(shell: &Shell) -> Option<String> {
    let cwd = shell.cwd.to_str()?;
    (cwd.len() <= CWD_MAX_BYTES).then(|| cwd.to_string())
}

fn effective_locale(shell: &Shell) -> String {
    for name in ["LC_ALL", "LANG"] {
        if let Some(value) = shell.env.get(name).filter(|value| !value.is_empty()) {
            return normalize_locale(value).unwrap_or_else(|| "C".to_string());
        }
    }
    "C".to_string()
}

fn normalize_locale(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > TOKEN_MAX_BYTES || !value.is_ascii() {
        return None;
    }
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@'))
        .then(|| value.to_string())
}

#[cfg(test)]
fn fixture_host_facts() -> HostFacts {
    HostFacts {
        os: OsFacts {
            id: Some("fixture-linux".into()),
            version_id: Some("1".into()),
            id_like: vec!["linux".into()],
            codename: Some("stable".into()),
        },
        kernel: KernelFacts {
            release: Some("1.0-fixture".into()),
            machine: Some("fixture64".into()),
        },
        process_arch: "fixture64",
        privilege: "user",
        memory_total_bytes: Some(8 * 1024 * 1024 * 1024),
        zhsh_version: env!("CARGO_PKG_VERSION"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parses_representative_os_release_fields_without_expansion() {
        let ubuntu = parse_os_release(
            "ID=ubuntu\nVERSION_ID=\"24.04\"\nID_LIKE=\"Debian debian\"\nVERSION_CODENAME=noble\nPRETTY_NAME=ignored\n",
        );
        assert_eq!(ubuntu.id.as_deref(), Some("ubuntu"));
        assert_eq!(ubuntu.version_id.as_deref(), Some("24.04"));
        assert_eq!(ubuntu.id_like, ["debian"]);
        assert_eq!(ubuntu.codename.as_deref(), Some("noble"));

        let fedora = parse_os_release(
            "ID=fedora\nVERSION_ID=42\nID_LIKE=\"RHEL centos\"\nVERSION_CODENAME=adams\n",
        );
        assert_eq!(fedora.id.as_deref(), Some("fedora"));
        assert_eq!(fedora.id_like, ["rhel", "centos"]);

        let invalid = parse_os_release(
            "ID=first\nID=second\nVERSION_ID=\"$(id)\"\nID_LIKE=\"linux bad/value\"\nVERSION_CODENAME=\"unterminated\n",
        );
        assert_eq!(invalid, OsFacts::default());
    }

    #[test]
    fn os_release_falls_back_only_when_primary_is_missing_and_enforces_limit() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-host-context-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let primary = root.join("primary");
        let fallback = root.join("fallback");
        fs::write(&fallback, "ID=fedora\n").unwrap();
        assert_eq!(
            collect_os_release_from(&primary, &fallback).id.as_deref(),
            Some("fedora")
        );

        fs::create_dir(&primary).unwrap();
        assert_eq!(
            collect_os_release_from(&primary, &fallback),
            OsFacts::default()
        );
        fs::remove_dir(&primary).unwrap();
        fs::write(&primary, vec![b'x'; OS_RELEASE_MAX_BYTES + 1]).unwrap();
        assert_eq!(
            collect_os_release_from(&primary, &fallback),
            OsFacts::default()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn memory_math_rejects_zero_units_and_overflow() {
        assert_eq!(total_memory_bytes(1024, 4096), Some(4 * 1024 * 1024));
        assert_eq!(total_memory_bytes(1024, 0), None);
        assert_eq!(total_memory_bytes(u64::MAX as u128, 2), None);
    }

    static COLLECTIONS: AtomicUsize = AtomicUsize::new(0);

    fn counting_facts() -> HostFacts {
        COLLECTIONS.fetch_add(1, Ordering::SeqCst);
        fixture_host_facts()
    }

    #[test]
    fn caches_host_facts_but_projects_current_session_fields() {
        COLLECTIONS.store(0, Ordering::SeqCst);
        let provider = HostContextProvider::with_collector(counting_facts);
        let mut shell = Shell::new();
        shell.cwd = Path::new("/first").to_path_buf();
        shell.env.insert("LANG".into(), "C.UTF-8".into());
        let first = provider.context_for(&shell).to_json();
        shell.cwd = Path::new("/second").to_path_buf();
        shell.env.insert("LC_ALL".into(), "zh_CN.UTF-8".into());
        let second = provider.context_for(&shell).to_json();

        assert_eq!(COLLECTIONS.load(Ordering::SeqCst), 1);
        assert!(first.contains("/first"));
        assert!(first.contains("C.UTF-8"));
        assert!(second.contains("/second"));
        assert!(second.contains("zh_CN.UTF-8"));
    }

    #[test]
    fn serialized_context_excludes_private_shell_state() {
        let provider = HostContextProvider::fixture();
        let mut shell = Shell::new();
        shell.llm = Some(crate::llm::LlmConfig {
            name: "private-config-marker".into(),
            url: "https://private-provider-marker.example".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: "private-access-token-marker".into(),
            models: crate::llm::ModelTiers {
                flash: "private-flash-model-marker".into(),
                standard: "private-standard-model-marker".into(),
                max: "private-max-model-marker".into(),
            },
            tier: crate::llm::ModelTier::Flash,
        });
        shell.cwd = Path::new("/allowed-cwd-marker").to_path_buf();
        shell
            .env
            .insert("PATH".into(), "/private-path-marker".into());
        shell
            .env
            .insert("HOME".into(), "/private-home-marker".into());
        shell
            .aliases
            .insert("private-alias-marker".into(), "ls".into());
        shell
            .functions
            .insert("private-function-marker".into(), "() { :; }".into());
        shell.variables.insert(
            "PRIVATE_VAR".into(),
            "declare -- PRIVATE_VAR=private-variable-marker".into(),
        );
        shell
            .prompt_variables
            .insert("PS1".into(), "private-prompt-marker".into());

        let json = provider.context_for(&shell).to_json();
        assert!(json.contains("/allowed-cwd-marker"));
        for private in [
            "private-path-marker",
            "private-home-marker",
            "private-alias-marker",
            "private-function-marker",
            "private-variable-marker",
            "private-prompt-marker",
            "private-config-marker",
            "private-provider-marker",
            "private-access-token-marker",
            "private-flash-model-marker",
        ] {
            assert!(!json.contains(private), "private marker leaked: {private}");
        }
    }
}
