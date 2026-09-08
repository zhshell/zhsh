//! Agent 授信等级及其可选的 `~/.zhshrc` 持久化。

#[cfg(unix)]
use std::fs::Permissions;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MANAGED_BEGIN: &str = "# >>> zhsh trust (managed by `zh trust -w`) >>>";
const MANAGED_END: &str = "# <<< zhsh trust <<<";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 当前会话对 LLM 生成命令采用的本地授信等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTrust {
    Balanced,
    Confirm,
    Trusted,
}

impl AgentTrust {
    pub(crate) const ENVIRONMENT_KEY: &'static str = "ZHSH_AGENT_TRUST";
    pub(crate) const VALUES: [&'static str; 3] = ["balanced", "confirm", "trusted"];

    /// 解析用户显式指定的等级；忽略 ASCII 大小写和首尾空白。
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "balanced" => Some(Self::Balanced),
            "confirm" => Some(Self::Confirm),
            "trusted" => Some(Self::Trusted),
            _ => None,
        }
    }

    /// 从会话环境取得有效等级；缺失、空值或非法值都使用 `balanced`。
    pub(crate) fn from_environment(value: Option<&str>) -> Self {
        value.and_then(Self::parse).unwrap_or(Self::Balanced)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Confirm => "confirm",
            Self::Trusted => "trusted",
        }
    }
}

/// 将等级写入 `~/.zhshrc` 中由 zhsh 独占维护的尾部块。
pub(super) fn persist(home: &Path, trust: AgentTrust) -> Result<(), String> {
    if !home.is_absolute() {
        return Err("HOME 为空或不是绝对路径".into());
    }
    let configured_path = home.join(".zhshrc");
    let target = resolve_target(&configured_path)?;
    let existing = match std::fs::read_to_string(&target) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("无法读取 {}: {error}", configured_path.display())),
    };
    let updated = update_managed_block(&existing, trust)?;
    atomic_write(&target, updated.as_bytes())
        .map_err(|error| format!("无法写入 {}: {error}", configured_path.display()))
}

fn resolve_target(configured_path: &Path) -> Result<PathBuf, String> {
    match std::fs::symlink_metadata(configured_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(configured_path)
            .map_err(|error| format!("无法解析 {}: {error}", configured_path.display())),
        Ok(metadata) if metadata.is_file() => Ok(configured_path.to_path_buf()),
        Ok(_) => Err(format!("{} 不是普通文件", configured_path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(configured_path.to_path_buf())
        }
        Err(error) => Err(format!("无法检查 {}: {error}", configured_path.display())),
    }
}

fn update_managed_block(content: &str, trust: AgentTrust) -> Result<String, String> {
    let mut output = String::with_capacity(content.len() + 128);
    let mut inside = false;
    let mut found = false;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if text == MANAGED_BEGIN {
            if inside || found {
                return Err("~/.zhshrc 包含重复的 zhsh trust 管理块".into());
            }
            inside = true;
            found = true;
            continue;
        }
        if text == MANAGED_END {
            if !inside {
                return Err("~/.zhshrc 包含不完整的 zhsh trust 管理块".into());
            }
            inside = false;
            continue;
        }
        if !inside {
            output.push_str(line);
        }
    }
    if inside {
        return Err("~/.zhshrc 包含不完整的 zhsh trust 管理块".into());
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if !output.is_empty() && !output.ends_with("\n\n") {
        output.push('\n');
    }
    output.push_str(MANAGED_BEGIN);
    output.push('\n');
    output.push_str("export ZHSH_AGENT_TRUST=");
    output.push_str(trust.as_str());
    output.push('\n');
    output.push_str(MANAGED_END);
    output.push('\n');
    Ok(output)
}

fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("~/.zhshrc 没有父目录"))?;
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    for _ in 0..100 {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".zhshrc.tmp-{}-{sequence}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(content)?;
            file.sync_all()?;
            drop(file);
            if let Some(permissions) = permissions {
                std::fs::set_permissions(&temporary, permissions)?;
            } else {
                set_private_permissions(&temporary)?;
            }
            std::fs::rename(&temporary, path)?;
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        return result;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "无法创建 ~/.zhshrc 原子写入临时文件",
    ))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_defaults_to_balanced() {
        for value in [None, Some(""), Some("   "), Some("unknown")] {
            assert_eq!(AgentTrust::from_environment(value), AgentTrust::Balanced);
        }
        assert_eq!(
            AgentTrust::from_environment(Some(" CONFIRM ")),
            AgentTrust::Confirm
        );
    }

    #[test]
    fn managed_block_preserves_user_content_and_moves_to_the_end() {
        let original = format!(
            "export EDITOR=vim\n\n{MANAGED_BEGIN}\nexport ZHSH_AGENT_TRUST=confirm\n{MANAGED_END}\nalias ll='ls -l'\n"
        );
        let updated = update_managed_block(&original, AgentTrust::Trusted).unwrap();

        assert!(updated.starts_with("export EDITOR=vim\n\nalias ll='ls -l'\n"));
        assert!(updated.ends_with(&format!(
            "{MANAGED_BEGIN}\nexport ZHSH_AGENT_TRUST=trusted\n{MANAGED_END}\n"
        )));
        assert_eq!(updated.matches(MANAGED_BEGIN).count(), 1);
    }

    #[test]
    fn malformed_managed_block_fails_without_guessing() {
        assert!(update_managed_block(MANAGED_BEGIN, AgentTrust::Balanced).is_err());
        assert!(update_managed_block(MANAGED_END, AgentTrust::Balanced).is_err());
    }
}
