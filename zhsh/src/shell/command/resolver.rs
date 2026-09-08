//! PATH 可执行文件解析。
//!
//! 首词补全和 `type` 内建命令共享本模块，避免分别采用“路径存在”“目录条目”或
//! “具有执行权限”等不一致规则。REPL 顶层输入路由不查询本模块。

use super::super::SessionState;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_SHEBANG_BYTES: usize = 4096;
const MAX_INTERPRETER_DEPTH: usize = 4;

/// Agent 对实际执行目标控制边界的宿主判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExecutableBinding {
    SystemTrusted,
    UserBound,
    Untrusted,
    Dynamic,
}

/// 执行文件在命令计划准备时记录的 Unix 身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    pub(crate) mtime_nsec: i64,
    pub(crate) ctime: i64,
    pub(crate) ctime_nsec: i64,
}

/// 脚本 shebang 产生的附加执行目标身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterpreterIdentity {
    pub(crate) resolved_path: PathBuf,
    pub(crate) canonical_path: PathBuf,
    pub(crate) resolved_identity: FileIdentity,
    pub(crate) identity: FileIdentity,
    pub(crate) path_components: Vec<PathComponentIdentity>,
}

/// 从解析入口到规范目标的每个路径组件身份，包括中间符号链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathComponentIdentity {
    pub(crate) path: PathBuf,
    pub(crate) identity: FileIdentity,
}

/// Agent 对一个外部程序名的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentResolvedExecutable {
    pub(crate) resolved_path: PathBuf,
    pub(crate) canonical_path: PathBuf,
    pub(crate) resolved_identity: FileIdentity,
    pub(crate) identity: FileIdentity,
    pub(crate) path_index: Option<usize>,
    pub(crate) binding: ExecutableBinding,
    pub(crate) binding_reason: Option<String>,
    pub(crate) path_components: Vec<PathComponentIdentity>,
    pub(crate) interpreters: Vec<InterpreterIdentity>,
}

/// 判断路径是否为当前平台可执行的普通文件。
///
/// Unix 平台要求至少一个执行权限位；其他平台只要求普通文件。
pub(crate) fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 按会话 cwd 和 PATH 返回名称对应的全部不重复可执行文件。
///
/// # Arguments
///
/// - `session`：提供 cwd 和 PATH 的当前会话。
/// - `name`：命令名或包含路径分隔符的显式路径。
pub(crate) fn executable_paths(session: &SessionState, name: &str) -> Vec<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        let path = if path.is_absolute() {
            path
        } else {
            session.cwd.join(path)
        };
        return is_executable(&path).then_some(path).into_iter().collect();
    }

    let mut paths = Vec::new();
    for directory in session
        .env
        .get("PATH")
        .map(String::as_str)
        .unwrap_or("")
        .split(':')
    {
        let directory = if directory.is_empty() {
            session.cwd.clone()
        } else {
            let configured = PathBuf::from(directory);
            if configured.is_absolute() {
                configured
            } else {
                session.cwd.join(configured)
            }
        };
        let candidate = directory.join(name);
        if is_executable(&candidate) && !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
    paths
}

/// 为 Agent 解析唯一的实际外部目标，并记录用于执行前复核的身份。
pub(crate) fn resolve_agent_executable(
    session: &SessionState,
    name: &str,
) -> Option<AgentResolvedExecutable> {
    resolve_agent_executable_from(
        &session.cwd,
        session.env.get("PATH").map(String::as_str),
        name,
        &session.env,
        0,
    )
}

fn resolve_agent_executable_from(
    cwd: &Path,
    path_value: Option<&str>,
    name: &str,
    environment: &std::collections::HashMap<String, String>,
    depth: usize,
) -> Option<AgentResolvedExecutable> {
    let (resolved_path, path_index, stable_path_entry) =
        first_executable_path(cwd, path_value, name)?;
    let resolved_path = if resolved_path.is_absolute() {
        resolved_path
    } else {
        cwd.join(resolved_path)
    };
    let canonical_path = fs::canonicalize(&resolved_path).ok()?;
    // 当前 SessionState 只能保存 UTF-8 参数；不可无损表示的目标不应被转换成另一
    // 个路径，而是退回动态/未解析计划并强制确认。
    canonical_path.to_str()?;
    let metadata = fs::metadata(&canonical_path).ok()?;
    let resolved_metadata = fs::symlink_metadata(&resolved_path).ok()?;
    let identity = file_identity(&metadata);
    let resolved_identity = file_identity(&resolved_metadata);
    let path_components =
        snapshot_path_components([resolved_path.as_path(), canonical_path.as_path()])?;
    let (mut binding, mut binding_reason) =
        classify_agent_target(&resolved_path, &canonical_path, &metadata);
    if !stable_path_entry {
        binding = ExecutableBinding::Untrusted;
        binding_reason = Some("PATH 包含空项或相对目录".into());
    }
    if environment
        .get("LD_PRELOAD")
        .is_some_and(|value| !value.is_empty())
        || environment
            .get("LD_AUDIT")
            .is_some_and(|value| !value.is_empty())
    {
        binding = ExecutableBinding::Untrusted;
        binding_reason = Some("环境包含动态加载器注入变量".into());
    }

    let mut interpreters = Vec::new();
    match interpreter_targets(&canonical_path) {
        Ok(targets) if depth < MAX_INTERPRETER_DEPTH => {
            for target in targets {
                let Some(interpreter) =
                    resolve_agent_executable_from(cwd, path_value, &target, environment, depth + 1)
                else {
                    binding = ExecutableBinding::Untrusted;
                    binding_reason = Some(format!("无法绑定脚本解释器 {target}"));
                    break;
                };
                if interpreter.binding > binding {
                    binding = interpreter.binding;
                    binding_reason = interpreter
                        .binding_reason
                        .clone()
                        .or_else(|| Some(format!("脚本解释器 {target} 的绑定等级更严格")));
                }
                interpreters.push(InterpreterIdentity {
                    resolved_path: interpreter.resolved_path,
                    canonical_path: interpreter.canonical_path,
                    resolved_identity: interpreter.resolved_identity,
                    identity: interpreter.identity,
                    path_components: interpreter.path_components,
                });
                interpreters.extend(interpreter.interpreters);
            }
        }
        Ok(targets) if !targets.is_empty() => {
            binding = ExecutableBinding::Untrusted;
            binding_reason = Some("脚本解释器链超过静态绑定深度".into());
        }
        Ok(_) => {}
        Err(reason) => {
            binding = ExecutableBinding::Untrusted;
            binding_reason = Some(reason);
        }
    }
    Some(AgentResolvedExecutable {
        resolved_path,
        canonical_path,
        resolved_identity,
        identity,
        path_index,
        binding,
        binding_reason,
        path_components,
        interpreters,
    })
}

fn first_executable_path(
    cwd: &Path,
    path_value: Option<&str>,
    name: &str,
) -> Option<(PathBuf, Option<usize>, bool)> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        return is_executable(&path).then_some((path, None, true));
    }
    for (index, directory) in path_value.unwrap_or("").split(':').enumerate() {
        let configured = PathBuf::from(directory);
        let stable = !directory.is_empty() && configured.is_absolute();
        let directory = if directory.is_empty() {
            cwd.to_path_buf()
        } else if configured.is_absolute() {
            configured
        } else {
            cwd.join(configured)
        };
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Some((candidate, Some(index), stable));
        }
    }
    None
}

pub(crate) fn resolution_matches(
    cwd: &Path,
    path_value: Option<&OsStr>,
    name: &str,
    expected: &AgentResolvedExecutable,
    environment: &std::collections::HashMap<String, String>,
) -> bool {
    let path_value = match path_value {
        Some(value) => match value.to_str() {
            Some(value) => Some(value),
            None => return false,
        },
        None => None,
    };
    resolve_agent_executable_from(cwd, path_value, name, environment, 0).is_some_and(|current| {
        current.resolved_path == expected.resolved_path
            && current.canonical_path == expected.canonical_path
            && current.resolved_identity == expected.resolved_identity
            && current.identity == expected.identity
            && current.path_index == expected.path_index
            && current.binding == expected.binding
            && current.path_components == expected.path_components
            && current.interpreters == expected.interpreters
    })
}

/// 判断计划中的外部目标是否仍是准备时验证的同一文件。
pub(crate) fn identity_matches(path: &Path, expected: FileIdentity) -> bool {
    fs::metadata(path)
        .ok()
        .map(|metadata| file_identity(&metadata) == expected)
        .unwrap_or(false)
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode(),
        size: metadata.size(),
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
fn file_identity(_: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: 0,
        ino: 0,
        uid: 0,
        gid: 0,
        mode: 0,
        size: 0,
        mtime: 0,
        mtime_nsec: 0,
        ctime: 0,
        ctime_nsec: 0,
    }
}

#[cfg(unix)]
fn classify_agent_target(
    resolved_path: &Path,
    canonical_path: &Path,
    metadata: &fs::Metadata,
) -> (ExecutableBinding, Option<String>) {
    use std::os::unix::fs::MetadataExt;

    let effective_uid = unsafe { libc::geteuid() };
    // root 运行时权限和所有权推断不再能提供有意义的保护边界。
    if effective_uid == 0 {
        return (
            ExecutableBinding::Untrusted,
            Some("zhsh 以 root 身份运行".into()),
        );
    }
    // 容器、只读系统镜像和经过 UID 映射的宿主不一定把可信系统根显示为 UID 0。
    // 以 `/` 的所有者作为当前挂载命名空间的系统锚点，同时继续接受传统 root。
    let Ok(root_metadata) = fs::metadata("/") else {
        return (
            ExecutableBinding::Untrusted,
            Some("无法验证系统信任根".into()),
        );
    };
    let system_owner = root_metadata.uid();
    if system_owner == effective_uid {
        return (
            ExecutableBinding::Untrusted,
            Some("系统信任根由当前用户控制".into()),
        );
    }
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return (
            ExecutableBinding::Untrusted,
            Some("目标不是可执行普通文件".into()),
        );
    }
    if metadata.mode() & 0o6000 != 0 {
        return (
            ExecutableBinding::Untrusted,
            Some("目标文件带有 setuid/setgid 位".into()),
        );
    }
    if has_linux_file_capabilities(canonical_path) {
        return (
            ExecutableBinding::Untrusted,
            Some("目标文件带有 Linux file capabilities".into()),
        );
    }

    if system_target_is_trusted(
        resolved_path,
        canonical_path,
        metadata,
        effective_uid,
        system_owner,
    ) {
        return (ExecutableBinding::SystemTrusted, None);
    }

    match user_target_is_bound(
        resolved_path,
        canonical_path,
        metadata,
        effective_uid,
        system_owner,
    ) {
        Ok(()) => (
            ExecutableBinding::UserBound,
            Some("执行目标由当前用户管理并已静态绑定".into()),
        ),
        Err(reason) => (ExecutableBinding::Untrusted, Some(reason)),
    }
}

#[cfg(unix)]
fn system_target_is_trusted(
    resolved_path: &Path,
    canonical_path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
    system_owner: u32,
) -> bool {
    use std::os::unix::fs::MetadataExt;

    if (!matches!(metadata.uid(), 0) && metadata.uid() != system_owner)
        || metadata.mode() & 0o022 != 0
        || is_effectively_writable(canonical_path)
    {
        return false;
    }

    for path in [resolved_path, canonical_path] {
        let Some(components) = absolute_prefixes(path) else {
            return false;
        };
        for component in components {
            let Ok(component_metadata) = fs::symlink_metadata(&component) else {
                return false;
            };
            let permissions_apply = !component_metadata.file_type().is_symlink();
            if (!matches!(component_metadata.uid(), 0) && component_metadata.uid() != system_owner)
                || (permissions_apply && component_metadata.mode() & 0o0022 != 0)
                || (permissions_apply && is_effectively_writable(&component))
            {
                return false;
            }
        }
    }
    effective_uid != 0
}

#[cfg(unix)]
fn user_target_is_bound(
    resolved_path: &Path,
    canonical_path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
    system_owner: u32,
) -> Result<(), String> {
    let primary_gid = primary_group(effective_uid);
    validate_user_controlled_component(
        canonical_path,
        metadata,
        effective_uid,
        system_owner,
        primary_gid,
    )?;
    for path in [resolved_path, canonical_path] {
        let components = absolute_prefixes(path)
            .ok_or_else(|| format!("目标路径不是绝对路径 {}", path.display()))?;
        for component in components {
            let component_metadata = fs::symlink_metadata(&component)
                .map_err(|_| format!("无法验证目标路径组件 {}", component.display()))?;
            validate_user_controlled_component(
                &component,
                &component_metadata,
                effective_uid,
                system_owner,
                primary_gid,
            )?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_user_controlled_component(
    path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
    system_owner: u32,
    primary_gid: Option<u32>,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    if !matches!(metadata.uid(), 0)
        && metadata.uid() != system_owner
        && metadata.uid() != effective_uid
    {
        return Err(format!("目标路径包含其他用户拥有的组件 {}", path.display()));
    }
    let permissions_apply = !metadata.file_type().is_symlink();
    if permissions_apply && metadata.mode() & 0o002 != 0 {
        return Err(format!(
            "目标路径包含 world-writable 组件 {}",
            path.display()
        ));
    }
    if permissions_apply
        && metadata.mode() & 0o020 != 0
        && (metadata.uid() != effective_uid || primary_gid != Some(metadata.gid()))
    {
        return Err(format!("目标路径包含非用户主组可写组件 {}", path.display()));
    }
    if permissions_apply && has_linux_posix_acl(path) {
        return Err(format!("目标路径包含无法静态解释的 ACL {}", path.display()));
    }
    Ok(())
}

fn absolute_prefixes(path: &Path) -> Option<Vec<PathBuf>> {
    if !path.is_absolute() {
        return None;
    }
    let mut prefixes = Vec::new();
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        prefixes.push(current.clone());
    }
    Some(prefixes)
}

fn snapshot_path_components<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> Option<Vec<PathComponentIdentity>> {
    let mut seen = HashSet::new();
    let mut identities = Vec::new();
    for path in paths {
        for component in absolute_prefixes(path)? {
            if !seen.insert(component.clone()) {
                continue;
            }
            let metadata = fs::symlink_metadata(&component).ok()?;
            let mut identity = file_identity(&metadata);
            if metadata.is_dir() {
                // 目录内容的增删会改变 size/mtime/ctime，但不会改变本路径的目标绑定；
                // 目录替换、所有权或权限变化仍由 dev/ino/uid/gid/mode 捕获。
                identity.size = 0;
                identity.mtime = 0;
                identity.mtime_nsec = 0;
                identity.ctime = 0;
                identity.ctime_nsec = 0;
            }
            identities.push(PathComponentIdentity {
                path: component,
                identity,
            });
        }
    }
    Some(identities)
}

#[cfg(unix)]
fn primary_group(effective_uid: u32) -> Option<u32> {
    let effective_gid = unsafe { libc::getegid() };
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() < 4 {
            return None;
        }
        let uid = fields[2].parse::<u32>().ok()?;
        let gid = fields[3].parse::<u32>().ok()?;
        if uid == effective_uid {
            return (gid == effective_gid).then_some(gid);
        }
    }
    None
}

fn interpreter_targets(path: &Path) -> Result<Vec<String>, String> {
    let mut file = fs::File::open(path).map_err(|_| "无法检查脚本解释器".to_owned())?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_SHEBANG_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法检查脚本解释器".to_owned())?;
    if !bytes.starts_with(b"#!") {
        return Ok(Vec::new());
    }
    let line = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .ok_or_else(|| "脚本 shebang 无效".to_owned())?;
    let line = std::str::from_utf8(&line[2..]).map_err(|_| "脚本 shebang 不是 UTF-8".to_owned())?;
    let words: Vec<_> = line.split_ascii_whitespace().collect();
    let Some(interpreter) = words.first() else {
        return Err("脚本 shebang 缺少解释器".into());
    };
    if *interpreter == "/usr/bin/env" {
        if words.len() != 2 || words[1].starts_with('-') || words[1].contains('/') {
            return Err("脚本使用了不受支持的 /usr/bin/env shebang".into());
        }
        return Ok(vec![(*interpreter).into(), words[1].into()]);
    }
    if !Path::new(interpreter).is_absolute() || words.len() > 2 {
        return Err("脚本 shebang 无法静态绑定".into());
    }
    Ok(vec![(*interpreter).into()])
}

#[cfg(unix)]
fn is_effectively_writable(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: path 是以 NUL 结尾、调用期间有效的 C 字符串；access 不保留指针。
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(target_os = "linux")]
fn has_linux_file_capabilities(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: path 指向有效 NUL 结尾字符串；空缓冲区与长度 0 只查询扩展属性大小。
    unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        ) > 0
    }
}

#[cfg(target_os = "linux")]
fn has_linux_posix_acl(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: path 指向有效 NUL 结尾字符串；空缓冲区只查询扩展属性大小。
    unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        ) > 0
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn has_linux_file_capabilities(_: &Path) -> bool {
    false
}

#[cfg(all(unix, not(target_os = "linux")))]
fn has_linux_posix_acl(_: &Path) -> bool {
    false
}

#[cfg(not(unix))]
fn classify_agent_target(
    _: &Path,
    _: &Path,
    _: &fs::Metadata,
) -> (ExecutableBinding, Option<String>) {
    (
        ExecutableBinding::Untrusted,
        Some("当前平台没有实现可信执行文件身份验证".into()),
    )
}

/// 收集当前会话 PATH 中所有可执行普通文件的 UTF-8 文件名。
///
/// 返回值稳定排序且去重，供补全和其他命令发现逻辑共享。无法读取的 PATH 目录和非
/// UTF-8 文件名会被忽略。
pub(crate) fn executable_names(path_value: &str, cwd: &Path) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for directory in path_value.split(':') {
        let directory = if directory.is_empty() {
            cwd
        } else {
            Path::new(directory)
        };
        let directory = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            cwd.join(directory)
        };
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if is_executable(&path) && seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn path_resolution_rejects_directories_and_non_executable_files() {
        let root =
            std::env::temp_dir().join(format!("zhsh-command-resolver-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("plain"), "plain").unwrap();
        std::fs::write(root.join("runnable"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            root.join("runnable"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::create_dir(root.join("folder")).unwrap();

        let mut session = SessionState::test();
        session
            .env
            .insert("PATH".into(), root.to_string_lossy().into_owned());

        assert!(executable_paths(&session, "plain").is_empty());
        assert!(executable_paths(&session, "folder").is_empty());
        assert_eq!(executable_paths(&session, "runnable").len(), 1);
        assert_eq!(
            executable_names(session.env.get("PATH").unwrap(), &session.cwd),
            vec!["runnable"]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn immutable_system_binary_uses_the_mounted_root_owner_as_trust_anchor() {
        if unsafe { libc::geteuid() } == 0 || !Path::new("/usr/bin/ls").exists() {
            return;
        }
        let mut session = SessionState::test();
        session.env.insert("PATH".into(), "/usr/bin:/bin".into());

        let target = resolve_agent_executable(&session, "ls").unwrap();

        assert_eq!(
            target.binding,
            ExecutableBinding::SystemTrusted,
            "{:?}",
            target.binding_reason
        );
    }
}
