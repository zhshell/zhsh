//! 固定目录发现、签名验证和 FORMAT 精确解析。

mod codec_spec;
mod container;
mod install;
mod management;
mod package;

use super::{JsonSchemaMode, JsonSchemaResolution};
use crate::common::{AppError, AppResult};
use container::{VerifiedEnvelope, MAX_ARTIFACT_BYTES};
#[cfg(test)]
pub(crate) use package::CodecPackage as TestCodecPackage;
use package::{CodecPackage, MAX_PACKAGE_BYTES};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) use codec_spec::{
    ErrorKindSpec, ErrorRule, Expr, FinishCondition, FinishOutcome, FinishSpec, HeaderPartSpec,
    InputField, MessageField, MissingFinish, RuleMode, TextSpec,
};
pub(crate) use install::{
    export_user_codec, inspect_codec, install_user_codec, uninstall_user_codec, CodecTrustState,
};
pub(crate) use management::{read_revision, LockMode, ManagementLock};
pub(crate) use package::{
    valid_header_name, valid_id, valid_relative_path, HttpPolicy, SecretSpec, MAX_REQUEST_BODY,
};

// 官方私钥不进入仓库；二进制只固化 32 字节发布公钥作为信任锚。
pub(super) const OFFICIAL_PUBLIC_KEY: &[u8; 32] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/official-codec.pub"
));

struct LockedOfficialCodec {
    format: &'static str,
    artifact_len: usize,
    sha256: [u8; 32],
}

include!(concat!(env!("OUT_DIR"), "/official_codecs.rs"));

const MAX_CODEC_FILES_PER_ROOT: usize = 512;
const MAX_TRUSTED_KEYS_PER_ROOT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PluginScope {
    System,
    User,
}

impl PluginScope {
    fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PluginLoadIssueKind {
    RootUnreadable,
    RootUnsafe,
    RootLimitExceeded,
    KeyInvalid,
    ArtifactInvalid,
    FilenameMismatch,
    DuplicateFormat,
    DuplicateOfficialCopy,
    ScopeConflict,
}

impl PluginLoadIssueKind {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::RootUnreadable => "root_unreadable",
            Self::RootUnsafe => "root_unsafe",
            Self::RootLimitExceeded => "root_limit_exceeded",
            Self::KeyInvalid => "key_invalid",
            Self::ArtifactInvalid => "artifact_invalid",
            Self::FilenameMismatch => "filename_mismatch",
            Self::DuplicateFormat => "duplicate_format",
            Self::DuplicateOfficialCopy => "duplicate_official_copy",
            Self::ScopeConflict => "scope_conflict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginLoadIssue {
    pub(crate) scope: PluginScope,
    pub(crate) path: PathBuf,
    pub(crate) kind: PluginLoadIssueKind,
    pub(crate) message: String,
}

impl std::fmt::Display for PluginLoadIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "[codec.{}.{}] {}: {}",
            self.scope.label(),
            self.kind.code(),
            self.path.display(),
            self.message
        )
    }
}

impl PluginLoadIssue {
    /// 只有系统根本身不可用或随包锁定 FORMAT 受损才阻止整个 runtime generation。
    /// 独立系统包中的单个坏制品保持局部隔离，不能拖垮仍然有效的随包基线。
    pub(crate) fn blocks_runtime(&self) -> bool {
        if self.scope != PluginScope::System {
            return false;
        }
        if matches!(
            self.kind,
            PluginLoadIssueKind::RootUnreadable
                | PluginLoadIssueKind::RootUnsafe
                | PluginLoadIssueKind::RootLimitExceeded
        ) {
            return true;
        }
        format_from_filename(&self.path)
            .is_some_and(|format| expected_official_codec(&format).is_some())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PluginLoadReport {
    pub(crate) catalog: PluginCatalog,
    pub(crate) issues: Vec<PluginLoadIssue>,
    pub(crate) trusted_user_keys: Vec<[u8; 32]>,
}

impl PluginLoadReport {
    pub(crate) fn issue_messages(&self) -> Vec<String> {
        self.issues.iter().map(ToString::to_string).collect()
    }

    /// 比较两次完整扫描所得的可执行事实，用于提交前发现未遵守管理锁的并发写入。
    pub(crate) fn equivalent_to(&self, other: &Self) -> bool {
        self.catalog.summaries() == other.catalog.summaries()
            && self.issues == other.issues
            && self.trusted_user_keys == other.trusted_user_keys
    }

    // 生产构建保留该入口给外部发布门禁；交互启动只允许使用隔离式加载。
    #[allow(dead_code)]
    pub(crate) fn into_strict(self) -> AppResult<PluginCatalog> {
        let Some(first) = self.issues.first() else {
            return Ok(self.catalog);
        };
        Err(AppError::protocol(format!(
            "LLM Codec 严格加载发现 {} 个问题；首项 {}",
            self.issues.len(),
            first
        )))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VerifiedPlugin {
    pub(crate) package: Arc<CodecPackage>,
    pub(crate) sha256: [u8; 32],
    pub(crate) key_fingerprint: String,
    pub(crate) official: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginSummary {
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) sha256: [u8; 32],
    pub(crate) key_fingerprint: String,
    pub(crate) official: bool,
    pub(crate) source: PluginSource,
    pub(crate) supports_json_schema: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginSource {
    Bundled,
    SystemPackage,
    UserInstalled,
}

impl PluginSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::SystemPackage => "system-package",
            Self::UserInstalled => "user-installed",
        }
    }
}

impl PluginSummary {
    pub(crate) fn label(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }

    pub(crate) fn resolve_json_schema(
        &self,
        requested: JsonSchemaMode,
    ) -> AppResult<JsonSchemaResolution> {
        match (requested, self.supports_json_schema, self.official) {
            (JsonSchemaMode::Off, _, _) => Ok(JsonSchemaResolution::Off),
            (JsonSchemaMode::On, true, _) => Ok(JsonSchemaResolution::On),
            (JsonSchemaMode::On, false, false) => Ok(JsonSchemaResolution::Downgraded),
            (JsonSchemaMode::On, false, true) => Err(AppError::protocol(format!(
                "官方 Codec {} 缺少 JSON Schema 编码 Profile",
                self.label()
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PluginCatalog {
    plugins: Arc<BTreeMap<String, LoadedPlugin>>,
}

impl PluginCatalog {
    /// 单元测试加载仓库内制品的便捷入口；生产只经由 `CodecRuntime` 加载。
    #[cfg(test)]
    pub(crate) fn load(home: impl AsRef<Path>) -> AppResult<Self> {
        let home = home.as_ref();
        let home = (!home.as_os_str().is_empty()).then_some(home);
        Ok(Self::load_runtime(home).catalog)
    }

    /// 为交互运行加载所有可验证 Codec，并把局部故障留在报告中。
    pub(crate) fn load_runtime(home: Option<&Path>) -> PluginLoadReport {
        #[cfg(not(test))]
        let system_root = system_plugin_root();
        #[cfg(not(test))]
        let system = Some(PluginRoot {
            path: &system_root,
            expected_owner: system_uid(),
            scope: PluginScope::System,
            strict_permissions: true,
        });

        // 单元测试用仓库内签名制品替代主机 `/usr/lib`，并保留原有空 HOME fixture 语义。
        #[cfg(test)]
        let bundled_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/llm-codecs");
        #[cfg(test)]
        let system = home.is_none().then_some(PluginRoot {
            path: &bundled_root,
            expected_owner: current_uid(),
            scope: PluginScope::System,
            strict_permissions: false,
        });

        let user_root = home.map(|home| home.join(".zhsh/plugins/llm"));
        let user = user_root.as_deref().map(|path| PluginRoot {
            path,
            expected_owner: current_uid(),
            scope: PluginScope::User,
            strict_permissions: true,
        });
        load_from_roots(system, user)
    }

    /// 为构建、Golden 和发行门禁加载 Codec；任何诊断都使检查失败。
    // CI/release-check 在测试目标中调用；运行时不能误用严格模式拖垮 Shell。
    #[allow(dead_code)]
    pub(crate) fn load_strict(home: Option<&Path>) -> AppResult<Self> {
        Self::load_runtime(home).into_strict()
    }

    pub(crate) fn summaries(&self) -> Vec<PluginSummary> {
        self.plugins
            .values()
            .map(|loaded| PluginSummary {
                id: loaded.plugin.package.id.clone(),
                version: loaded.plugin.package.version.to_string(),
                sha256: loaded.plugin.sha256,
                key_fingerprint: loaded.plugin.key_fingerprint.clone(),
                official: loaded.plugin.official,
                source: loaded.source(),
                supports_json_schema: loaded.plugin.package.codec.encode.supports_json_schema(),
            })
            .collect()
    }

    pub(crate) fn resolve(&self, format: &str) -> AppResult<VerifiedPlugin> {
        self.plugins
            .get(format)
            .map(|loaded| loaded.plugin.clone())
            .ok_or_else(|| AppError::input(format!("未找到已验证的 LLM Codec {format}")))
    }

    pub(crate) fn entry(&self, format: &str) -> AppResult<LoadedPlugin> {
        self.plugins
            .get(format)
            .cloned()
            .ok_or_else(|| AppError::input(format!("未找到已验证的 LLM Codec {format}")))
    }
}

impl VerifiedPlugin {
    pub(crate) fn supports_json_schema(&self) -> bool {
        self.package.codec.encode.supports_json_schema()
    }

    pub(crate) fn resolve_json_schema(
        &self,
        requested: JsonSchemaMode,
    ) -> AppResult<JsonSchemaResolution> {
        match (requested, self.supports_json_schema(), self.official) {
            (JsonSchemaMode::Off, _, _) => Ok(JsonSchemaResolution::Off),
            (JsonSchemaMode::On, true, _) => Ok(JsonSchemaResolution::On),
            (JsonSchemaMode::On, false, false) => Ok(JsonSchemaResolution::Downgraded),
            (JsonSchemaMode::On, false, true) => Err(AppError::protocol(format!(
                "官方 Codec {} 缺少 JSON Schema 编码 Profile",
                self.package.format()
            ))),
        }
    }
}

#[cfg(not(test))]
fn system_plugin_root() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("ZHSH_TEST_SYSTEM_CODEC_DIR") {
        return PathBuf::from(path);
    }
    PathBuf::from("/usr/lib/zhsh/plugins/llm")
}

fn verify_bytes(
    artifact: &[u8],
    scope: PluginScope,
    trusted_keys: &[[u8; 32]],
) -> AppResult<VerifiedPlugin> {
    let envelope = container::verify(artifact, |key, _| {
        key == OFFICIAL_PUBLIC_KEY || trusted_keys.iter().any(|trusted| trusted == key)
    })?;
    verify_payload(envelope, artifact, scope)
}

fn verify_payload(
    envelope: VerifiedEnvelope<'_>,
    artifact: &[u8],
    scope: PluginScope,
) -> AppResult<VerifiedPlugin> {
    if envelope.payload.len() > MAX_PACKAGE_BYTES {
        return Err(AppError::protocol("LLM Codec payload 超过 384 KiB"));
    }
    let package = CodecPackage::parse(envelope.payload)?;
    let official = envelope.public_key == *OFFICIAL_PUBLIC_KEY;
    let format = package.format();
    if official && !package.codec.encode.supports_json_schema() {
        return Err(AppError::protocol(format!(
            "官方 Codec {format} 缺少 JSON Schema 编码 Profile"
        )));
    }
    if let Some(expected) = expected_official_codec(&format) {
        if !official
            || artifact.len() != expected.artifact_len
            || envelope.artifact_sha256 != expected.sha256
        {
            return Err(AppError::protocol(
                "随包 Codec FORMAT 的制品内容与当前 zhsh 发布清单不一致",
            ));
        }
    }
    match scope {
        PluginScope::System if !official => {
            return Err(AppError::protocol("系统目录只允许官方签名的 Codec"));
        }
        PluginScope::User if matches!(package.id.as_str(), "openai" | "anthropic") && !official => {
            return Err(AppError::protocol(
                "非官方发布密钥不得使用官方保留 Codec ID",
            ));
        }
        PluginScope::System | PluginScope::User => {}
    }
    Ok(VerifiedPlugin {
        package: Arc::new(package),
        sha256: envelope.artifact_sha256,
        key_fingerprint: envelope.key_fingerprint,
        official,
    })
}

fn expected_official_codec(format: &str) -> Option<&'static LockedOfficialCodec> {
    OFFICIAL_CODEC_LOCKS
        .iter()
        .find(|locked| locked.format == format)
}

#[derive(Debug, Clone, Copy)]
struct PluginRoot<'a> {
    path: &'a Path,
    expected_owner: Option<u32>,
    scope: PluginScope,
    strict_permissions: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedPlugin {
    pub(crate) plugin: VerifiedPlugin,
    pub(crate) path: PathBuf,
    pub(crate) scope: PluginScope,
}

impl LoadedPlugin {
    fn source(&self) -> PluginSource {
        match self.scope {
            PluginScope::User => PluginSource::UserInstalled,
            PluginScope::System
                if expected_official_codec(&self.plugin.package.format()).is_some() =>
            {
                PluginSource::Bundled
            }
            PluginScope::System => PluginSource::SystemPackage,
        }
    }
}

#[derive(Debug, Default)]
struct RootDiscovery {
    candidates: Vec<LoadedPlugin>,
    claimed_formats: BTreeSet<String>,
    issues: Vec<PluginLoadIssue>,
    trusted_keys: Vec<[u8; 32]>,
}

#[derive(Debug)]
struct RootFailure {
    path: PathBuf,
    kind: PluginLoadIssueKind,
    message: &'static str,
}

fn load_from_roots(
    system: Option<PluginRoot<'_>>,
    user: Option<PluginRoot<'_>>,
) -> PluginLoadReport {
    let system = system.map(discover_root).unwrap_or_default();
    let user = user.map(discover_root).unwrap_or_default();
    let trusted_user_keys = user.trusted_keys.clone();
    let system_claims = system.claimed_formats.clone();
    let mut issues = system.issues;
    issues.extend(user.issues);

    let mut selected = BTreeMap::<String, LoadedPlugin>::new();
    let mut conflicts = BTreeSet::new();
    for candidate in system.candidates {
        merge_same_scope(&mut selected, &mut conflicts, &mut issues, candidate);
    }
    for candidate in user.candidates {
        let format = candidate.plugin.package.format();
        if conflicts.contains(&format) {
            push_issue(
                &mut issues,
                candidate.scope,
                &candidate.path,
                PluginLoadIssueKind::ScopeConflict,
                "Codec FORMAT 已存在身份冲突",
            );
            continue;
        }
        if let Some(system_plugin) = selected.get(&format) {
            if system_plugin.scope == PluginScope::System
                && system_plugin.plugin.official
                && candidate.plugin.official
                && system_plugin.plugin.sha256 == candidate.plugin.sha256
            {
                push_issue(
                    &mut issues,
                    candidate.scope,
                    &candidate.path,
                    PluginLoadIssueKind::DuplicateOfficialCopy,
                    "用户目录中的相同官方 Codec 副本已忽略",
                );
                continue;
            }
            push_issue(
                &mut issues,
                candidate.scope,
                &candidate.path,
                PluginLoadIssueKind::ScopeConflict,
                "system/user Codec 身份冲突，已拒绝歧义选择",
            );
            selected.remove(&format);
            conflicts.insert(format);
            continue;
        }
        // 一个损坏但名称有效的系统制品仍占有该身份，不能静默回退到用户目录。
        if system_claims.contains(&format) {
            push_issue(
                &mut issues,
                candidate.scope,
                &candidate.path,
                PluginLoadIssueKind::ScopeConflict,
                "系统 Codec 身份不可用，已拒绝用户目录回退",
            );
            conflicts.insert(format);
            continue;
        }
        merge_same_scope(&mut selected, &mut conflicts, &mut issues, candidate);
    }

    issues.sort_by(|left, right| {
        left.scope
            .cmp(&right.scope)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.message.cmp(&right.message))
    });
    PluginLoadReport {
        catalog: PluginCatalog {
            plugins: Arc::new(selected),
        },
        issues,
        trusted_user_keys,
    }
}

fn merge_same_scope(
    selected: &mut BTreeMap<String, LoadedPlugin>,
    conflicts: &mut BTreeSet<String>,
    issues: &mut Vec<PluginLoadIssue>,
    candidate: LoadedPlugin,
) {
    let format = candidate.plugin.package.format();
    if conflicts.contains(&format) {
        push_issue(
            issues,
            candidate.scope,
            &candidate.path,
            PluginLoadIssueKind::DuplicateFormat,
            "Codec FORMAT 已存在身份冲突",
        );
        return;
    }
    if let Some(previous) = selected.get(&format) {
        let identical = previous.plugin.sha256 == candidate.plugin.sha256;
        push_issue(
            issues,
            candidate.scope,
            &candidate.path,
            PluginLoadIssueKind::DuplicateFormat,
            if identical {
                "重复 Codec FORMAT 已忽略"
            } else {
                "FORMAT 对应多个 Codec，已拒绝歧义选择"
            },
        );
        if !identical {
            selected.remove(&format);
            conflicts.insert(format);
        }
        return;
    }
    selected.insert(format, candidate);
}

fn discover_root(root: PluginRoot<'_>) -> RootDiscovery {
    let mut result = RootDiscovery::default();
    match std::fs::symlink_metadata(root.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return result,
        Err(_) => {
            push_issue(
                &mut result.issues,
                root.scope,
                root.path,
                PluginLoadIssueKind::RootUnreadable,
                "无法读取 Codec 根目录",
            );
            return result;
        }
        Ok(_) => {}
    }
    if verify_directory(root.path, root.expected_owner, root.strict_permissions).is_err() {
        push_issue(
            &mut result.issues,
            root.scope,
            root.path,
            PluginLoadIssueKind::RootUnsafe,
            "Codec 根目录类型、所有者或权限不安全",
        );
        return result;
    }
    let entries = match sorted_entries(root.path) {
        Ok(entries) => entries,
        Err(()) => {
            push_issue(
                &mut result.issues,
                root.scope,
                root.path,
                PluginLoadIssueKind::RootUnreadable,
                "无法完整枚举 Codec 根目录",
            );
            return result;
        }
    };
    let artifacts: Vec<_> = entries
        .into_iter()
        .filter(|path| path.extension() == Some(OsStr::new("zhcodec")))
        .collect();
    if artifacts.len() > MAX_CODEC_FILES_PER_ROOT {
        push_issue(
            &mut result.issues,
            root.scope,
            root.path,
            PluginLoadIssueKind::RootLimitExceeded,
            "Codec 根目录中的制品数量超过 512 个，已禁用整根",
        );
        return result;
    }

    let trusted_keys = match load_trusted_keys(root) {
        Ok((keys, key_issues)) => {
            result.issues.extend(key_issues);
            keys
        }
        Err(failure) => {
            push_issue(
                &mut result.issues,
                root.scope,
                &failure.path,
                failure.kind,
                failure.message,
            );
            return result;
        }
    };
    result.trusted_keys = trusted_keys.clone();

    for path in artifacts {
        if root.scope == PluginScope::System {
            if let Some(format) = format_from_filename(&path) {
                result.claimed_formats.insert(format);
            }
        }
        let artifact = match read_regular(
            &path,
            MAX_ARTIFACT_BYTES,
            root.expected_owner,
            root.strict_permissions,
        ) {
            Ok(artifact) => artifact,
            Err(_) => {
                push_issue(
                    &mut result.issues,
                    root.scope,
                    &path,
                    PluginLoadIssueKind::ArtifactInvalid,
                    "Codec 制品类型、大小、所有者或权限无效",
                );
                continue;
            }
        };
        let plugin = match verify_bytes(&artifact, root.scope, &trusted_keys) {
            Ok(plugin) => plugin,
            Err(_) => {
                push_issue(
                    &mut result.issues,
                    root.scope,
                    &path,
                    PluginLoadIssueKind::ArtifactInvalid,
                    "Codec 制品签名、schema 或发布身份验证失败",
                );
                continue;
            }
        };
        let format = plugin.package.format();
        if root.scope == PluginScope::System {
            result.claimed_formats.insert(format.clone());
        }
        let expected_name = format!("{format}.zhcodec");
        if path.file_name() != Some(OsStr::new(&expected_name)) {
            push_issue(
                &mut result.issues,
                root.scope,
                &path,
                PluginLoadIssueKind::FilenameMismatch,
                "Codec 文件名与签名包身份不一致",
            );
            continue;
        }
        result.candidates.push(LoadedPlugin {
            plugin,
            path,
            scope: root.scope,
        });
    }
    result
}

fn load_trusted_keys(
    root: PluginRoot<'_>,
) -> Result<(Vec<[u8; 32]>, Vec<PluginLoadIssue>), RootFailure> {
    let directory = root.path.join("trusted-keys");
    match std::fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()))
        }
        Err(_) => {
            return Err(RootFailure {
                path: directory,
                kind: PluginLoadIssueKind::RootUnreadable,
                message: "无法读取 Codec 信任目录",
            })
        }
        Ok(_) => {}
    }
    if verify_directory(&directory, root.expected_owner, root.strict_permissions).is_err() {
        return Err(RootFailure {
            path: directory,
            kind: PluginLoadIssueKind::RootUnsafe,
            message: "Codec 信任目录类型、所有者或权限不安全",
        });
    }
    let entries = sorted_entries(&directory).map_err(|()| RootFailure {
        path: directory.clone(),
        kind: PluginLoadIssueKind::RootUnreadable,
        message: "无法完整枚举 Codec 信任目录",
    })?;
    let key_paths: Vec<_> = entries
        .into_iter()
        .filter(|path| path.extension() == Some(OsStr::new("pub")))
        .collect();
    if key_paths.len() > MAX_TRUSTED_KEYS_PER_ROOT {
        return Err(RootFailure {
            path: directory,
            kind: PluginLoadIssueKind::RootLimitExceeded,
            message: "Codec 信任公钥数量超过 64 个，已禁用整根",
        });
    }

    let mut keys = Vec::new();
    let mut issues = Vec::new();
    for path in key_paths {
        let key = read_regular(&path, 32, root.expected_owner, root.strict_permissions)
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        match key {
            Some(key)
                if path.file_name()
                    == Some(OsStr::new(&format!(
                        "{}.pub",
                        container::key_fingerprint(&key)
                    ))) =>
            {
                keys.push(key)
            }
            None => push_issue(
                &mut issues,
                root.scope,
                &path,
                PluginLoadIssueKind::KeyInvalid,
                "Codec 信任公钥类型、大小、所有者或权限无效",
            ),
            Some(_) => push_issue(
                &mut issues,
                root.scope,
                &path,
                PluginLoadIssueKind::KeyInvalid,
                "Codec 信任公钥文件名不是其完整 SHA-256 指纹",
            ),
        }
    }
    Ok((keys, issues))
}

fn sorted_entries(path: &Path) -> Result<Vec<PathBuf>, ()> {
    let entries = std::fs::read_dir(path).map_err(|_| ())?;
    let mut paths = Vec::new();
    for entry in entries {
        paths.push(entry.map_err(|_| ())?.path());
    }
    paths.sort();
    Ok(paths)
}

fn format_from_filename(path: &Path) -> Option<String> {
    let format = path.file_name()?.to_str()?.strip_suffix(".zhcodec")?;
    let (id, version) = format.rsplit_once('@')?;
    if !valid_id(id) {
        return None;
    }
    let parsed = semver::Version::parse(version).ok()?;
    (parsed.to_string() == version).then(|| format.to_string())
}

fn push_issue(
    issues: &mut Vec<PluginLoadIssue>,
    scope: PluginScope,
    path: &Path,
    kind: PluginLoadIssueKind,
    message: impl Into<String>,
) {
    issues.push(PluginLoadIssue {
        scope,
        path: terminal_safe_path(path),
        kind,
        message: message.into(),
    });
}

fn terminal_safe_path(path: &Path) -> PathBuf {
    use std::fmt::Write as _;

    let mut safe = String::new();
    for character in path.to_string_lossy().chars() {
        match character {
            '\u{1b}' => safe.push_str("\\x1b"),
            character if character.is_control() => {
                let _ = write!(safe, "\\u{{{:x}}}", character as u32);
            }
            _ => safe.push(character),
        }
    }
    PathBuf::from(safe)
}

fn verify_directory(
    path: &Path,
    expected_owner: Option<u32>,
    strict_permissions: bool,
) -> AppResult<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查 {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::protocol(format!(
            "插件路径 {} 必须是普通目录且不能是符号链接",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if strict_permissions && metadata.mode() & 0o022 != 0 {
            return Err(AppError::protocol(format!(
                "插件目录 {} 不能由 group/other 写入",
                path.display()
            )));
        }
        if strict_permissions && expected_owner.is_some_and(|owner| metadata.uid() != owner) {
            return Err(AppError::protocol(format!(
                "插件目录 {} 所有者不匹配",
                path.display()
            )));
        }
    }
    Ok(())
}

fn read_regular(
    path: &Path,
    limit: usize,
    expected_owner: Option<u32>,
    strict_permissions: bool,
) -> AppResult<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查 {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(AppError::protocol(format!(
            "插件文件 {} 不是允许大小的普通文件",
            path.display()
        )));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| AppError::io(format!("无法打开 {}: {error}", path.display())))?;
    let opened = file
        .metadata()
        .map_err(|error| AppError::io(format!("无法检查 {}: {error}", path.display())))?;
    if !opened.is_file() || opened.len() > limit as u64 {
        return Err(AppError::protocol("插件文件在打开期间发生变化"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if strict_permissions
            && (opened.mode() & 0o022 != 0
                || expected_owner.is_some_and(|owner| opened.uid() != owner))
        {
            return Err(AppError::protocol(format!(
                "插件文件 {} 所有者或写权限不安全",
                path.display()
            )));
        }
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::io(format!("无法读取 {}: {error}", path.display())))?;
    if bytes.len() > limit {
        return Err(AppError::protocol("插件文件读取超过资源上限"));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    Some(unsafe { libc::geteuid() })
}

#[cfg(all(unix, not(test)))]
fn system_uid() -> Option<u32> {
    Some(0)
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

#[cfg(all(not(unix), not(test)))]
fn system_uid() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn signed_user_codec() -> (Vec<u8>, [u8; 32]) {
        let official = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        ));
        let envelope = container::verify(official, |_, _| true).unwrap();
        let mut payload: serde_json::Value = serde_json::from_slice(envelope.payload).unwrap();
        payload["id"] = "internal-format".into();
        payload["version"] = "1.0.0".into();
        let payload = serde_json::to_vec(&payload).unwrap();
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
        let public_key: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
        let mut artifact = Vec::new();
        artifact.extend_from_slice(container::MAGIC);
        artifact.extend_from_slice(&container::CONTAINER_VERSION.to_be_bytes());
        artifact.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        artifact.extend_from_slice(&public_key);
        artifact.extend_from_slice(&payload);
        artifact.extend_from_slice(key.sign(&artifact).as_ref());
        (artifact, public_key)
    }

    #[test]
    fn every_repository_package_passes_signature_and_schema_verification() {
        assert_eq!(
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/official-codec.pub"
            )),
            OFFICIAL_PUBLIC_KEY
        );
        let catalog = PluginCatalog::load_strict(None).unwrap();
        assert_eq!(catalog.summaries().len(), 2);
        assert!(catalog.resolve("openai@0.3.0").is_ok());
        assert!(catalog.resolve("anthropic@0.3.0").is_ok());
        assert!(catalog
            .summaries()
            .iter()
            .all(|summary| summary.supports_json_schema));
    }

    #[test]
    fn user_codec_without_schema_profile_preserves_on_but_downgrades() {
        let official = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        ));
        let envelope = container::verify(official, |_, _| true).unwrap();
        let mut payload: serde_json::Value = serde_json::from_slice(envelope.payload).unwrap();
        payload["id"] = "com.example.codec".into();
        payload["version"] = "1.0.0".into();
        payload["codec"]["encode"]
            .as_object_mut()
            .unwrap()
            .remove("json_schema");
        let package = CodecPackage::parse(&serde_json::to_vec(&payload).unwrap()).unwrap();
        let plugin = VerifiedPlugin {
            package: Arc::new(package),
            sha256: [0; 32],
            key_fingerprint: "0".repeat(64),
            official: false,
        };
        let summary = PluginSummary {
            id: "com.example.codec".into(),
            version: "1.0.0".into(),
            sha256: [0; 32],
            key_fingerprint: "0".repeat(64),
            official: false,
            source: PluginSource::UserInstalled,
            supports_json_schema: false,
        };

        assert_eq!(
            summary.resolve_json_schema(JsonSchemaMode::On).unwrap(),
            JsonSchemaResolution::Downgraded
        );
        assert_eq!(
            plugin.resolve_json_schema(JsonSchemaMode::On).unwrap(),
            JsonSchemaResolution::Downgraded
        );
    }

    #[test]
    fn format_names_reserve_short_ids_for_official_codecs() {
        assert!(valid_id("openai"));
        assert!(valid_id("anthropic"));
        assert!(valid_id("deepseek.junglelk.github.io"));
        assert!(valid_id("vendor"));
        assert!(valid_id("Internal_Format"));
        assert!(!valid_id("bad/name"));
    }

    #[test]
    fn user_codecs_receive_only_generic_loadability_validation() {
        let (valid, key) = signed_user_codec();
        assert!(verify_bytes(&valid, PluginScope::User, &[key]).is_ok());
        assert!(verify_bytes(&valid, PluginScope::System, &[key]).is_err());
    }

    #[test]
    fn unbundled_official_format_is_not_rejected_by_the_bundled_lock() {
        let artifact = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        ));
        let original = container::verify_signature(artifact).unwrap();
        let mut payload: serde_json::Value = serde_json::from_slice(original.payload).unwrap();
        payload["version"] = "0.3.1".into();
        let payload = serde_json::to_vec(&payload).unwrap();
        let envelope = VerifiedEnvelope {
            payload: &payload,
            public_key: original.public_key,
            key_fingerprint: original.key_fingerprint,
            artifact_sha256: [1; 32],
        };
        let synthetic_artifact = vec![0; artifact.len() + 1];

        let plugin = verify_payload(envelope, &synthetic_artifact, PluginScope::System).unwrap();
        assert_eq!(plugin.package.format(), "openai@0.3.1");
        assert!(plugin.official);
    }
}

#[cfg(test)]
mod isolation_tests;
