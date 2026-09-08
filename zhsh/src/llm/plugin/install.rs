//! 已签名 Codec 的检查、首次信任、安装、卸载与逐字节导出。

use super::super::CodecRuntime;
use super::{
    container, format_from_filename, load_trusted_keys, read_regular, verify_bytes,
    verify_directory, verify_payload, HeaderPartSpec, LockMode, ManagementLock, PluginCatalog,
    PluginRoot, PluginScope, PluginSource, MAX_ARTIFACT_BYTES, OFFICIAL_PUBLIC_KEY,
};
use crate::common::{
    ensure_private_tree, persist_private_file, read_file_snapshot, rollback_created_private_file,
    AppError, AppResult, CancellationToken, PersistOutcome, PersistPolicy, PersistReceipt,
};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static EXPORT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodecTrustState {
    Official,
    Trusted,
    Untrusted,
}

/// 对外部源文件一次读取所得的固定、已验签事实。
#[derive(Debug)]
pub(crate) struct CodecInspection {
    pub(crate) format: String,
    pub(crate) publisher: String,
    pub(crate) codec_sha256: String,
    pub(crate) key_fingerprint: String,
    pub(crate) trust: CodecTrustState,
    pub(crate) supports_json_schema: bool,
    pub(crate) query_secret_warning: bool,
    bytes: Arc<[u8]>,
    public_key: [u8; 32],
    plugin: super::VerifiedPlugin,
}

impl CodecInspection {
    pub(crate) fn is_currently_trusted(&self, runtime: &CodecRuntime) -> bool {
        runtime.is_key_trusted(&self.public_key)
    }
}

/// Codec 安装完成后的磁盘和当前进程 generation 事实。
#[derive(Debug)]
pub(crate) struct UserCodecInstallReport {
    pub(crate) format: String,
    pub(crate) artifact: PersistReceipt,
    pub(crate) source: PluginSource,
    pub(crate) official: bool,
    pub(crate) trusted_key: Option<PersistReceipt>,
    pub(crate) key_fingerprint: String,
    pub(crate) generation: u64,
    pub(crate) disk_revision: u64,
}

/// Codec 卸载完成后的磁盘和当前进程 generation 事实。
#[derive(Debug)]
pub(crate) struct UserCodecUninstallReport {
    pub(crate) format: String,
    pub(crate) artifact_path: PathBuf,
    pub(crate) key_fingerprint: String,
    pub(crate) official: bool,
    pub(crate) generation: u64,
    pub(crate) disk_revision: u64,
}

#[derive(Debug)]
pub(crate) struct CodecExportReport {
    pub(crate) format: String,
    pub(crate) destination: PathBuf,
    pub(crate) identical: bool,
}

/// 完整检查一个外部 `.zhcodec`；外部 basename 不参与身份判定。
pub(crate) fn inspect_codec(runtime: &CodecRuntime, source: &Path) -> AppResult<CodecInspection> {
    let artifact = read_file_snapshot(source, MAX_ARTIFACT_BYTES)?;
    if !artifact.basename.ends_with(".zhcodec") {
        return Err(AppError::input("Codec 文件必须以 .zhcodec 结尾"));
    }
    let envelope = container::verify_signature(&artifact.bytes)?;
    let public_key = envelope.public_key;
    let key_fingerprint = envelope.key_fingerprint.clone();
    let plugin = verify_payload(envelope, &artifact.bytes, PluginScope::User)?;
    let format = plugin.package.format();
    let trust = if plugin.official {
        CodecTrustState::Official
    } else if runtime.is_key_trusted(&public_key) {
        CodecTrustState::Trusted
    } else {
        CodecTrustState::Untrusted
    };
    let query_secret_warning = plugin
        .package
        .codec
        .encode
        .iter()
        .flat_map(|encode| &encode.query)
        .flat_map(|query| &query.parts)
        .any(|part| matches!(part, HeaderPartSpec::SecretSlot { .. }));

    Ok(CodecInspection {
        format,
        publisher: plugin.package.publisher.clone(),
        codec_sha256: container::hex_sha256(&plugin.sha256),
        key_fingerprint,
        trust,
        supports_json_schema: plugin.supports_json_schema(),
        query_secret_warning,
        bytes: artifact.bytes.into(),
        public_key,
        plugin,
    })
}

/// 在跨进程事务内安装检查所得的固定字节，并原子发布新 generation。
pub(crate) fn install_user_codec(
    runtime: &CodecRuntime,
    user_home: &Path,
    inspection: CodecInspection,
    authorize_publisher: bool,
    cancellation: &CancellationToken,
) -> AppResult<UserCodecInstallReport> {
    if !user_home.is_absolute() {
        return Err(AppError::input("用户 HOME 不是绝对路径"));
    }
    if runtime.user_home.as_deref() != Some(user_home) {
        return Err(AppError::internal("CodecRuntime 与安装 HOME 不一致"));
    }
    if let Some(existing) =
        preflight_system_codec(runtime, &inspection.format, &inspection.plugin.sha256)?
    {
        return Ok(existing_system_install_report(
            runtime, inspection, existing,
        ));
    }
    let _serial = runtime
        .publish_serial
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock = ManagementLock::acquire(user_home, LockMode::Exclusive, true, cancellation)?
        .ok_or_else(|| AppError::internal("未取得 Codec 管理锁"))?;
    let revision = lock
        .read_revision()?
        .checked_add(1)
        .ok_or_else(|| AppError::protocol("Codec revision 数值溢出"))?;

    let llm_root = lock.root();
    let (installed_keys, _) = installed_user_keys(llm_root)?;
    let already_trusted = inspection.public_key == *OFFICIAL_PUBLIC_KEY
        || installed_keys
            .iter()
            .any(|candidate| candidate == &inspection.public_key);
    if !already_trusted && !authorize_publisher {
        return Err(AppError::input(format!(
            "Codec 发布密钥 {} 未受信任；安装已取消",
            inspection.key_fingerprint
        )));
    }
    if let Some(existing) =
        preflight_conflicts(user_home, &inspection.format, &inspection.plugin.sha256)?
    {
        return Ok(existing_system_install_report(
            runtime, inspection, existing,
        ));
    }

    let expected_name = format!("{}.zhcodec", inspection.format);
    let artifact = persist_private_file(
        llm_root,
        &expected_name,
        &inspection.bytes,
        PersistPolicy::IdenticalOnly,
    )?;
    let trusted_key = if already_trusted {
        None
    } else {
        let trusted_keys_root =
            ensure_private_tree(user_home, &[".zhsh", "plugins", "llm", "trusted-keys"])?;
        match persist_private_file(
            &trusted_keys_root,
            &format!("{}.pub", inspection.key_fingerprint),
            &inspection.public_key,
            PersistPolicy::IdenticalOnly,
        ) {
            Ok(receipt) => Some(receipt),
            Err(error) => {
                let rollback = rollback_created_private_file(&artifact, &inspection.bytes);
                return Err(with_rollback(error, rollback, &artifact.path));
            }
        }
    };

    let loaded = PluginCatalog::load_runtime(Some(user_home));
    let selected = loaded.catalog.resolve(&inspection.format).ok();
    if selected.as_ref().map(|selected| selected.sha256) != Some(inspection.plugin.sha256) {
        rollback_install(&artifact, trusted_key.as_ref(), &inspection)?;
        return Err(AppError::protocol(format!(
            "Codec 安装后未能成为唯一可用候选；{}",
            first_relevant_issue(&loaded, &inspection.format)
                .unwrap_or_else(|| "请检查 Codec 目录诊断".into())
        )));
    }
    if let Err(error) = runtime.validate_report(&loaded) {
        rollback_install(&artifact, trusted_key.as_ref(), &inspection)?;
        return Err(error);
    }
    let rechecked = PluginCatalog::load_runtime(Some(user_home));
    if !loaded.equivalent_to(&rechecked) {
        rollback_install(&artifact, trusted_key.as_ref(), &inspection)?;
        return Err(AppError::protocol("Codec 受管目录在提交前发生变化；请重试"));
    }
    if let Err(error) = lock.write_revision(revision) {
        rollback_install(&artifact, trusted_key.as_ref(), &inspection)?;
        return Err(error);
    }
    let generation = runtime.publish_report(rechecked, revision)?;

    Ok(UserCodecInstallReport {
        format: inspection.format,
        artifact,
        source: PluginSource::UserInstalled,
        official: inspection.plugin.official,
        trusted_key,
        key_fingerprint: inspection.key_fingerprint,
        generation,
        disk_revision: revision,
    })
}

fn existing_system_install_report(
    runtime: &CodecRuntime,
    inspection: CodecInspection,
    existing: super::LoadedPlugin,
) -> UserCodecInstallReport {
    let source = existing.source();
    UserCodecInstallReport {
        format: inspection.format,
        artifact: PersistReceipt {
            outcome: PersistOutcome::Identical,
            path: existing.path,
        },
        source,
        official: inspection.plugin.official,
        trusted_key: None,
        key_fingerprint: inspection.key_fingerprint,
        generation: runtime.generation_number(),
        disk_revision: runtime.disk_revision(),
    }
}

fn preflight_system_codec(
    runtime: &CodecRuntime,
    format: &str,
    sha256: &[u8; 32],
) -> AppResult<Option<super::LoadedPlugin>> {
    let Ok(existing) = runtime.snapshot().catalog.entry(format) else {
        return Ok(None);
    };
    if existing.scope != PluginScope::System {
        return Ok(None);
    }
    if existing.plugin.sha256 != *sha256 {
        return Err(AppError::input(format!(
            "Codec FORMAT {format} 已由不同内容的系统制品占用"
        )));
    }
    Ok(Some(existing))
}

/// 在跨进程事务内删除一个精确 user-scope Codec，并原子发布新 generation。
pub(crate) fn uninstall_user_codec(
    runtime: &CodecRuntime,
    user_home: &Path,
    format: &str,
    cancellation: &CancellationToken,
) -> AppResult<UserCodecUninstallReport> {
    super::super::validate_format(format).map_err(AppError::input)?;
    if !user_home.is_absolute() {
        return Err(AppError::input("用户 HOME 不是绝对路径"));
    }
    if runtime.user_home.as_deref() != Some(user_home) {
        return Err(AppError::internal("CodecRuntime 与卸载 HOME 不一致"));
    }

    let _serial = runtime
        .publish_serial
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock = ManagementLock::acquire(user_home, LockMode::Exclusive, false, cancellation)?
        .ok_or_else(|| uninstall_missing_error(user_home, format))?;
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled());
    }
    let revision = lock
        .read_revision()?
        .checked_add(1)
        .ok_or_else(|| AppError::protocol("Codec revision 数值溢出"))?;
    let llm_root = lock.root();
    let expected_name = format!("{format}.zhcodec");
    let artifact_path = llm_root.join(&expected_name);

    match fs::symlink_metadata(&artifact_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(uninstall_missing_error(user_home, format));
        }
        Err(error) => {
            return Err(AppError::io(format!(
                "无法检查待卸载 Codec {}: {error}",
                crate::common::terminal_safe_path(&artifact_path)
            )))
        }
    }

    let bytes = read_regular(&artifact_path, MAX_ARTIFACT_BYTES, current_uid(), true)?;
    let envelope = container::verify_signature(&bytes)?;
    let plugin = verify_payload(envelope, &bytes, PluginScope::User)?;
    if plugin.package.format() != format {
        return Err(AppError::protocol(
            "待卸载 Codec 的签名身份与目标 FORMAT 不一致",
        ));
    }

    fs::remove_file(&artifact_path).map_err(|error| {
        AppError::io(format!(
            "无法删除 Codec {}: {error}",
            crate::common::terminal_safe_path(&artifact_path)
        ))
    })?;
    if let Err(error) = sync_managed_directory(llm_root) {
        return Err(restore_removed_codec(
            llm_root,
            &expected_name,
            &bytes,
            error,
        ));
    }

    let candidate = PluginCatalog::load_runtime(Some(user_home));
    if let Err(error) = runtime.validate_report(&candidate) {
        return Err(restore_removed_codec(
            llm_root,
            &expected_name,
            &bytes,
            error,
        ));
    }
    let rechecked = PluginCatalog::load_runtime(Some(user_home));
    if !candidate.equivalent_to(&rechecked) {
        return Err(restore_removed_codec(
            llm_root,
            &expected_name,
            &bytes,
            AppError::protocol("Codec 受管目录在卸载提交前发生变化；请重试"),
        ));
    }
    if let Err(error) = lock.write_revision(revision) {
        return Err(restore_removed_codec(
            llm_root,
            &expected_name,
            &bytes,
            error,
        ));
    }
    let generation = match runtime.publish_report(rechecked, revision) {
        Ok(generation) => generation,
        Err(error) => {
            return Err(restore_removed_codec(
                llm_root,
                &expected_name,
                &bytes,
                error,
            ))
        }
    };

    Ok(UserCodecUninstallReport {
        format: format.to_owned(),
        artifact_path,
        key_fingerprint: plugin.key_fingerprint,
        official: plugin.official,
        generation,
        disk_revision: revision,
    })
}

fn uninstall_missing_error(user_home: &Path, format: &str) -> AppError {
    let report = PluginCatalog::load_runtime(Some(user_home));
    if report
        .catalog
        .entry(format)
        .is_ok_and(|entry| entry.scope == PluginScope::System)
    {
        AppError::input(format!(
            "Codec {format} 由系统软件包管理；请使用 apt、dnf 或对应包管理器卸载"
        ))
    } else {
        AppError::input(format!("用户目录未安装 Codec {format}"))
    }
}

fn restore_removed_codec(root: &Path, basename: &str, bytes: &[u8], cause: AppError) -> AppError {
    match persist_private_file(root, basename, bytes, PersistPolicy::IdenticalOnly) {
        Ok(_) => cause,
        Err(rollback_error) => AppError::io(format!(
            "{cause}；且无法恢复已删除的 Codec：{rollback_error}"
        )),
    }
}

fn sync_managed_directory(path: &Path) -> AppResult<()> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| AppError::io(format!("无法同步 Codec 管理目录: {error}")))
}

/// 从当前 generation 导出一个已经验证的 user Codec，绝不导出官方制品。
pub(crate) fn export_user_codec(
    runtime: &CodecRuntime,
    user_home: &Path,
    format: &str,
    output_dir: &Path,
    require_owned_output: bool,
    cancellation: &CancellationToken,
) -> AppResult<CodecExportReport> {
    let snapshot = runtime.snapshot();
    let entry = snapshot.catalog.entry(format)?;
    if entry.scope != PluginScope::User || entry.plugin.official {
        return Err(AppError::input("官方 Codec 不支持导出"));
    }

    let bytes = {
        let _serial = runtime
            .publish_serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lock = ManagementLock::acquire(user_home, LockMode::Shared, false, cancellation)?
            .ok_or_else(|| {
                AppError::protocol("Codec 管理锁尚未建立；请先执行 `zh codec reload`")
            })?;
        if lock.read_revision()? != snapshot.disk_revision {
            return Err(AppError::protocol(
                "当前 Codec generation 已过期；请先执行 `zh codec reload`",
            ));
        }
        let bytes = read_regular(&entry.path, MAX_ARTIFACT_BYTES, current_uid(), true)?;
        let verified = verify_bytes(&bytes, PluginScope::User, &snapshot.trusted_user_keys)?;
        if verified.package.format() != format || verified.sha256 != entry.plugin.sha256 {
            return Err(AppError::protocol("受管 Codec 在导出前发生变化"));
        }
        bytes
    };

    let destination = output_dir.join(format!("{format}.zhcodec"));
    let identical = write_export(output_dir, &destination, &bytes, require_owned_output)?;
    Ok(CodecExportReport {
        format: format.to_owned(),
        destination,
        identical,
    })
}

fn installed_user_keys(root: &Path) -> AppResult<(Vec<[u8; 32]>, Vec<String>)> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()))
        }
        Err(error) => return Err(AppError::io(format!("无法检查用户 Codec 目录: {error}"))),
        Ok(_) => {}
    }
    verify_directory(root, current_uid(), true)?;
    let plugin_root = PluginRoot {
        path: root,
        expected_owner: current_uid(),
        scope: PluginScope::User,
        strict_permissions: true,
    };
    let (keys, issues) = load_trusted_keys(plugin_root).map_err(|failure| {
        AppError::protocol(format!("{}: {}", failure.path.display(), failure.message))
    })?;
    Ok((
        keys,
        issues.into_iter().map(|issue| issue.to_string()).collect(),
    ))
}

/// 返回完全相同且已由 system scope 提供的制品；调用方应将其作为无写入的幂等成功。
fn preflight_conflicts(
    user_home: &Path,
    format: &str,
    sha256: &[u8; 32],
) -> AppResult<Option<super::LoadedPlugin>> {
    let destination = user_home
        .join(".zhsh/plugins/llm")
        .join(format!("{format}.zhcodec"));
    let report = PluginCatalog::load_runtime(Some(user_home));
    preflight_conflicts_in_report(&report, &destination, format, sha256)
}

fn preflight_conflicts_in_report(
    report: &super::PluginLoadReport,
    destination: &Path,
    format: &str,
    sha256: &[u8; 32],
) -> AppResult<Option<super::LoadedPlugin>> {
    if let Ok(existing) = report.catalog.entry(format) {
        if existing.plugin.sha256 != *sha256 {
            return Err(AppError::input(format!(
                "Codec FORMAT {format} 已由不同内容的制品占用"
            )));
        }
        if existing.scope == PluginScope::System {
            return Ok(Some(existing));
        }
    }
    for issue in &report.issues {
        if format_from_filename(&issue.path).as_deref() == Some(format) && issue.path != destination
        {
            return Err(AppError::input(format!(
                "Codec FORMAT {format} 存在冲突：{}",
                issue.message
            )));
        }
    }
    Ok(None)
}

fn rollback_install(
    artifact: &PersistReceipt,
    trusted_key: Option<&PersistReceipt>,
    inspection: &CodecInspection,
) -> AppResult<()> {
    let key_result = trusted_key
        .map(|receipt| rollback_created_private_file(receipt, &inspection.public_key))
        .transpose();
    let artifact_result = rollback_created_private_file(artifact, &inspection.bytes);
    match (key_result, artifact_result) {
        (Ok(_), Ok(_)) => Ok(()),
        (Err(key_error), Ok(_)) => Err(AppError::io(format!(
            "无法撤销 Publisher Trust Anchor {}：{key_error}",
            crate::common::terminal_safe_path(
                trusted_key
                    .expect("key rollback error requires a trust receipt")
                    .path
                    .as_path()
            )
        ))),
        (Ok(_), Err(artifact_error)) => Err(AppError::io(format!(
            "无法撤销 Codec {}：{artifact_error}",
            crate::common::terminal_safe_path(&artifact.path)
        ))),
        (Err(key_error), Err(artifact_error)) => Err(AppError::io(format!(
            "无法撤销 Publisher Trust Anchor {}：{key_error}；无法撤销 Codec {}：{artifact_error}",
            crate::common::terminal_safe_path(
                trusted_key
                    .expect("key rollback error requires a trust receipt")
                    .path
                    .as_path()
            ),
            crate::common::terminal_safe_path(&artifact.path)
        ))),
    }
}

fn first_relevant_issue(report: &super::PluginLoadReport, format: &str) -> Option<String> {
    report
        .issues
        .iter()
        .find(|issue| format_from_filename(&issue.path).as_deref() == Some(format))
        .map(|issue| issue.message.clone())
}

fn with_rollback(error: AppError, rollback: AppResult<bool>, artifact_path: &Path) -> AppError {
    match rollback {
        Ok(_) => error,
        Err(rollback_error) => AppError::io(format!(
            "{error}；且无法撤销 {}：{rollback_error}",
            crate::common::terminal_safe_path(artifact_path)
        )),
    }
}

fn write_export(
    directory: &Path,
    destination: &Path,
    bytes: &[u8],
    require_owned_output: bool,
) -> AppResult<bool> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| AppError::io(format!("无法检查导出目录: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::input("导出位置必须是非符号链接目录"));
    }
    #[cfg(unix)]
    if require_owned_output {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(AppError::input("显式导出目录必须属于当前用户"));
        }
    }
    #[cfg(not(unix))]
    let _ = require_owned_output;
    if destination.extension() != Some(OsStr::new("zhcodec")) {
        return Err(AppError::internal("Codec 导出文件名无效"));
    }
    match read_file_snapshot(destination, MAX_ARTIFACT_BYTES) {
        Ok(existing) if existing.bytes == bytes => return Ok(true),
        Ok(_) => return Err(AppError::input("导出目标已存在且内容不同")),
        Err(_) if destination.exists() => {
            return Err(AppError::input("导出目标不是可复用的普通文件"))
        }
        Err(_) => {}
    }

    let basename = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::input("导出目标文件名不是 UTF-8"))?;
    let staging = directory.join(format!(
        ".{basename}.tmp-{}-{}",
        std::process::id(),
        EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&staging)
        .map_err(|error| AppError::io(format!("无法创建 Codec 导出临时文件: {error}")))?;
    let result: AppResult<()> = (|| {
        file.write_all(bytes)
            .map_err(|error| AppError::io(format!("无法写入 Codec 导出文件: {error}")))?;
        file.sync_all()
            .map_err(|error| AppError::io(format!("无法同步 Codec 导出文件: {error}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o644))
                .map_err(|error| AppError::io(format!("无法设置 Codec 导出权限: {error}")))?;
        }
        fs::hard_link(&staging, destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::input("导出目标已被并发创建")
            } else {
                AppError::io(format!("无法提交 Codec 导出文件: {error}"))
            }
        })?;
        Ok(())
    })();
    drop(file);
    let _ = fs::remove_file(&staging);
    result?;
    if let Ok(directory_file) = OpenOptions::new().read(true).open(directory) {
        let _ = directory_file.sync_all();
    }
    Ok(false)
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    Some(unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn signed_user_codec() -> Vec<u8> {
        let official = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        ));
        let envelope = container::verify_signature(official).unwrap();
        let mut payload: serde_json::Value = serde_json::from_slice(envelope.payload).unwrap();
        payload["id"] = "com.example.provider".into();
        payload["version"] = "0.1.0".into();
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
        artifact
    }

    #[test]
    fn user_codec_installs_exports_and_uninstalls_without_revoking_publisher() {
        let home = std::env::temp_dir().join(format!(
            "zhsh-codec-lifecycle-{}-{}",
            std::process::id(),
            EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&home).unwrap();
        let source = home.join("download.zhcodec");
        let bytes = signed_user_codec();
        fs::write(&source, &bytes).unwrap();
        let runtime = CodecRuntime::load(Some(&home));
        let inspected = inspect_codec(&runtime, &source).unwrap();
        assert_eq!(inspected.format, "com.example.provider@0.1.0");
        assert_eq!(inspected.trust, CodecTrustState::Untrusted);

        let installed = install_user_codec(
            &runtime,
            &home,
            inspected,
            true,
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(installed.generation, 2);
        assert_eq!(installed.disk_revision, 1);
        assert!(home
            .join(".zhsh/plugins/llm/com.example.provider@0.1.0.zhcodec")
            .is_file());
        assert!(runtime.resolve("com.example.provider@0.1.0").is_ok());
        let trusted_key = installed.trusted_key.unwrap().path;
        assert!(trusted_key.is_file());

        let output = home.join("dist");
        fs::create_dir(&output).unwrap();
        let exported = export_user_codec(
            &runtime,
            &home,
            "com.example.provider@0.1.0",
            &output,
            true,
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(fs::read(exported.destination).unwrap(), bytes);

        let removed = uninstall_user_codec(
            &runtime,
            &home,
            "com.example.provider@0.1.0",
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(removed.generation, 3);
        assert_eq!(removed.disk_revision, 2);
        assert!(!removed.artifact_path.exists());
        assert!(trusted_key.is_file());
        assert!(runtime.resolve("com.example.provider@0.1.0").is_err());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn official_codec_can_be_installed_and_removed_from_user_scope() {
        let home = std::env::temp_dir().join(format!(
            "zhsh-official-codec-lifecycle-{}-{}",
            std::process::id(),
            EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&home).unwrap();
        let source = home.join("official-download.zhcodec");
        fs::write(
            &source,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/llm-codecs/openai@0.3.0.zhcodec"
            )),
        )
        .unwrap();
        let runtime = CodecRuntime::load(Some(&home));
        let inspected = inspect_codec(&runtime, &source).unwrap();
        assert_eq!(inspected.trust, CodecTrustState::Official);

        let installed = install_user_codec(
            &runtime,
            &home,
            inspected,
            false,
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(installed.trusted_key.is_none());
        let summary = runtime
            .summaries()
            .into_iter()
            .find(|summary| summary.label() == "openai@0.3.0")
            .unwrap();
        assert!(summary.official);
        assert_eq!(summary.source, super::super::PluginSource::UserInstalled);

        let removed = uninstall_user_codec(
            &runtime,
            &home,
            "openai@0.3.0",
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(removed.official);
        assert!(!removed.artifact_path.exists());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn identical_bundled_codec_is_an_idempotent_runtime_match() {
        let artifact = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/anthropic@0.3.0.zhcodec"
        ));
        let runtime = CodecRuntime::load(None);
        let sha256 = container::verify_signature(artifact)
            .unwrap()
            .artifact_sha256;

        let existing = preflight_system_codec(&runtime, "anthropic@0.3.0", &sha256)
            .unwrap()
            .unwrap();

        assert_eq!(existing.scope, PluginScope::System);
        assert_eq!(existing.source(), PluginSource::Bundled);
        assert!(preflight_system_codec(&runtime, "anthropic@0.3.0", &[0; 32]).is_err());
    }
}
