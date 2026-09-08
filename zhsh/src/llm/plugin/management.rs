//! 同一用户 HOME 下 Codec 管理操作的跨进程锁与磁盘 revision。

use crate::common::{
    ensure_private_tree, persist_private_file, AppError, AppResult, CancellationToken,
    PersistPolicy,
};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = ".management.lock";
const REVISION_FILE: &str = ".revision";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(25);
const MAX_REVISION_BYTES: usize = 32;

#[derive(Clone, Copy)]
pub(crate) enum LockMode {
    Shared,
    Exclusive,
}

/// 锁由打开的文件描述符持有；固定锁文件本身不会在 Drop 时删除。
pub(crate) struct ManagementLock {
    _file: File,
    root: PathBuf,
}

impl ManagementLock {
    pub(crate) fn acquire(
        user_home: &Path,
        mode: LockMode,
        create_root: bool,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<Self>> {
        let root = user_home.join(".zhsh/plugins/llm");
        if create_root {
            let prepared = ensure_private_tree(user_home, &[".zhsh", "plugins", "llm"])?;
            if prepared != root {
                return Err(AppError::internal("Codec 管理目录与固定 HOME 不一致"));
            }
        } else if !root.exists() {
            return Ok(None);
        }

        let path = root.join(LOCK_FILE);
        let file = open_lock_file(&path, create_root)?;
        let Some(file) = file else {
            return Ok(None);
        };
        verify_lock_file(&file)?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let operation = match mode {
                LockMode::Shared => libc::LOCK_SH,
                LockMode::Exclusive => libc::LOCK_EX,
            } | libc::LOCK_NB;
            let deadline = Instant::now() + LOCK_TIMEOUT;
            loop {
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled());
                }
                if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::EWOULDBLOCK) => {
                        if Instant::now() >= deadline {
                            return Err(AppError::io("另一个 zhsh 正在管理 Codec；等待锁超时"));
                        }
                        std::thread::sleep(LOCK_RETRY);
                    }
                    Some(libc::EINTR) => continue,
                    _ => {
                        return Err(AppError::io(format!(
                            "当前文件系统不支持可靠的 Codec 管理锁: {error}"
                        )))
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            return Err(AppError::io("当前平台不支持 Codec 跨进程管理锁"));
        }

        Ok(Some(Self { _file: file, root }))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn read_revision(&self) -> AppResult<u64> {
        read_revision_from_root(&self.root)
    }

    pub(crate) fn write_revision(&self, revision: u64) -> AppResult<()> {
        let bytes = format!("{revision}\n");
        persist_private_file(
            &self.root,
            REVISION_FILE,
            bytes.as_bytes(),
            PersistPolicy::Replace,
        )?;
        Ok(())
    }
}

pub(crate) fn read_revision(user_home: Option<&Path>) -> AppResult<u64> {
    let Some(home) = user_home else {
        return Ok(0);
    };
    if !home.is_absolute() {
        return Err(AppError::input("用户 HOME 不是绝对路径"));
    }
    read_revision_from_root(&home.join(".zhsh/plugins/llm"))
}

fn read_revision_from_root(root: &Path) -> AppResult<u64> {
    let path = root.join(REVISION_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(AppError::io(format!("无法检查 Codec revision: {error}"))),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::protocol(
            "Codec revision 必须是非符号链接普通文件",
        ));
    }
    verify_owned_private_metadata(&metadata, "Codec revision")?;
    if metadata.len() > MAX_REVISION_BYTES as u64 {
        return Err(AppError::protocol("Codec revision 内容超限"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| AppError::io(format!("无法读取 Codec revision: {error}")))?;
    verify_lock_file(&file)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_REVISION_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::io(format!("无法读取 Codec revision: {error}")))?;
    parse_revision(&bytes)
}

fn parse_revision(bytes: &[u8]) -> AppResult<u64> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AppError::protocol("Codec revision 不是 UTF-8 十进制数"))?;
    let value = text
        .strip_suffix('\n')
        .ok_or_else(|| AppError::protocol("Codec revision 缺少结尾换行"))?;
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AppError::protocol("Codec revision 不是规范十进制 u64"));
    }
    value
        .parse::<u64>()
        .map_err(|_| AppError::protocol("Codec revision 数值溢出"))
}

fn open_lock_file(path: &Path, create: bool) -> AppResult<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    match options.open(path) {
        Ok(file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| AppError::io(format!("无法收紧 Codec 管理锁权限: {error}")))?;
            }
            Ok(Some(file))
        }
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::io(format!("无法打开 Codec 管理锁: {error}"))),
    }
}

fn verify_lock_file(file: &File) -> AppResult<()> {
    let metadata = file
        .metadata()
        .map_err(|error| AppError::io(format!("无法复核 Codec 管理文件: {error}")))?;
    if !metadata.is_file() {
        return Err(AppError::protocol("Codec 管理文件必须是普通文件"));
    }
    verify_owned_private_metadata(&metadata, "Codec 管理文件")
}

fn verify_owned_private_metadata(metadata: &std::fs::Metadata, label: &str) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o600 {
            return Err(AppError::protocol(format!(
                "{label} 必须由当前用户所有且权限为 0600"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_parser_accepts_only_canonical_u64_with_newline() {
        assert_eq!(parse_revision(b"0\n").unwrap(), 0);
        assert_eq!(parse_revision(b"42\n").unwrap(), 42);
        for invalid in [b"".as_slice(), b"01\n", b"+1\n", b"1", b"1\n2\n"] {
            assert!(parse_revision(invalid).is_err());
        }
    }
}
