//! 用户私有插件文件共用的受限文件系统操作。
//!
//! 本模块只处理普通文件、固定用户目录、权限和原子换入，不理解 Safety 或 Codec 格式。

use super::{AppError, AppResult};
use ring::rand::{SecureRandom, SystemRandom};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// 从一个已打开普通文件取得的稳定字节快照。
#[derive(Debug)]
pub(crate) struct FileSnapshot {
    pub(crate) basename: String,
    pub(crate) bytes: Vec<u8>,
}

/// 目标已存在且内容不同时的处理方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PersistPolicy {
    IdenticalOnly,
    Replace,
}

/// 私有文件提交结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PersistOutcome {
    Created,
    Replaced,
    Identical,
}

/// 已经落盘并复核的目标。
#[derive(Debug)]
pub(crate) struct PersistReceipt {
    pub(crate) outcome: PersistOutcome,
    pub(crate) path: PathBuf,
}

/// 读取一个不跟随符号链接、大小受限的普通文件。
pub(crate) fn read_file_snapshot(path: &Path, limit: usize) -> AppResult<FileSnapshot> {
    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::input("插件源文件名必须是非空 UTF-8"))?
        .to_owned();
    let safe_path = terminal_safe_path(path);
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查插件源文件 {safe_path}: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::input("插件源必须是非符号链接普通文件"));
    }
    if metadata.len() > limit as u64 {
        return Err(AppError::input(format!("插件源文件超过 {} 字节", limit)));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| AppError::io(format!("无法打开插件源文件 {safe_path}: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| AppError::io(format!("无法复核插件源文件 {safe_path}: {error}")))?;
    if !opened.is_file() || opened.len() > limit as u64 {
        return Err(AppError::input("插件源文件在打开期间发生变化"));
    }

    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::io(format!("无法读取插件源文件 {safe_path}: {error}")))?;
    if bytes.len() > limit {
        return Err(AppError::input("插件源文件读取超过资源上限"));
    }
    Ok(FileSnapshot { basename, bytes })
}

/// 从已验证的绝对 HOME 向下创建并维护一条当前用户私有目录链。
pub(crate) fn ensure_private_tree(home: &Path, components: &[&str]) -> AppResult<PathBuf> {
    if !home.is_absolute() {
        return Err(AppError::input("用户 HOME 不是绝对路径"));
    }
    verify_home(home)?;
    let mut current = home.to_path_buf();
    for component in components {
        if !valid_component(component) {
            return Err(AppError::internal("私有目录组件不合法"));
        }
        let parent = current.clone();
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(_) => secure_directory(&current)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&current)?;
                secure_directory(&current)?;
                sync_directory(&parent)?;
            }
            Err(error) => {
                return Err(AppError::io(format!(
                    "无法检查私有目录 {}: {error}",
                    terminal_safe_path(&current)
                )))
            }
        }
    }
    Ok(current)
}

/// 把已经验证的字节以 `0600` 原子提交到私有目录。
pub(crate) fn persist_private_file(
    directory: &Path,
    basename: &str,
    bytes: &[u8],
    policy: PersistPolicy,
) -> AppResult<PersistReceipt> {
    if !valid_component(basename) {
        return Err(AppError::input("插件目标文件名不合法"));
    }
    secure_directory(directory)?;
    let destination = directory.join(basename);
    if let Some(identical) = inspect_existing(&destination, bytes)? {
        if identical {
            secure_existing_file(&destination)?;
            sync_directory(directory)?;
            return Ok(PersistReceipt {
                outcome: PersistOutcome::Identical,
                path: destination,
            });
        }
        if policy == PersistPolicy::IdenticalOnly {
            return Err(AppError::input(format!(
                "目标已存在且内容不同：{}",
                terminal_safe_path(&destination)
            )));
        }
    }

    let mut staging = StagingFile::create(directory)?;
    staging.write_all(bytes)?;
    let existed = fs::symlink_metadata(&destination).is_ok();
    if existed {
        // 重新复核，避免在首次检查后把符号链接或错误所有者当作可替换目标。
        match inspect_existing(&destination, bytes)? {
            Some(true) => {
                secure_existing_file(&destination)?;
                return Ok(PersistReceipt {
                    outcome: PersistOutcome::Identical,
                    path: destination,
                });
            }
            Some(false) if policy == PersistPolicy::Replace => {}
            Some(false) => return Err(AppError::input("目标已被并发修改")),
            None => return Err(AppError::input("目标在提交期间消失，请重试")),
        }
        fs::rename(&staging.path, &destination).map_err(|error| {
            AppError::io(format!(
                "无法原子替换 {}: {error}",
                terminal_safe_path(&destination)
            ))
        })?;
        staging.committed = true;
        secure_existing_file(&destination)?;
        sync_directory(directory)?;
        return Ok(PersistReceipt {
            outcome: PersistOutcome::Replaced,
            path: destination,
        });
    }

    match fs::hard_link(&staging.path, &destination) {
        Ok(()) => {
            fs::remove_file(&staging.path)
                .map_err(|error| AppError::io(format!("无法清理安装临时文件: {error}")))?;
            staging.committed = true;
            secure_existing_file(&destination)?;
            sync_directory(directory)?;
            Ok(PersistReceipt {
                outcome: PersistOutcome::Created,
                path: destination,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match inspect_existing(&destination, bytes)? {
                Some(true) => {
                    secure_existing_file(&destination)?;
                    Ok(PersistReceipt {
                        outcome: PersistOutcome::Identical,
                        path: destination,
                    })
                }
                _ => Err(AppError::input("目标已被其他进程创建且内容不同")),
            }
        }
        Err(error) => Err(AppError::io(format!(
            "无法提交插件文件 {}: {error}",
            terminal_safe_path(&destination)
        ))),
    }
}

/// 在多文件提交后续步骤失败时，仅撤销本次新建且内容仍完全一致的文件。
pub(crate) fn rollback_created_private_file(
    receipt: &PersistReceipt,
    expected: &[u8],
) -> AppResult<bool> {
    if receipt.outcome != PersistOutcome::Created {
        return Ok(false);
    }
    if inspect_existing(&receipt.path, expected)? != Some(true) {
        return Err(AppError::io(format!(
            "无法安全撤销已经变化的文件：{}",
            terminal_safe_path(&receipt.path)
        )));
    }
    fs::remove_file(&receipt.path).map_err(|error| {
        AppError::io(format!(
            "无法撤销文件 {}: {error}",
            terminal_safe_path(&receipt.path)
        ))
    })?;
    if let Some(parent) = receipt.path.parent() {
        sync_directory(parent)?;
    }
    Ok(true)
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\0'])
        && !value.chars().any(is_terminal_format_control)
}

fn inspect_existing(path: &Path, expected: &[u8]) -> AppResult<Option<bool>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::io(format!(
                "无法检查目标 {}: {error}",
                terminal_safe_path(path)
            )))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::input("插件目标必须是非符号链接普通文件"));
    }
    verify_current_owner(&metadata, "插件目标")?;
    if metadata.len() != expected.len() as u64 {
        return Ok(Some(false));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .map_err(|error| AppError::io(format!("无法读取现有插件目标: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| AppError::io(format!("无法复核现有插件目标: {error}")))?;
    if !opened.is_file() || opened.len() != expected.len() as u64 {
        return Err(AppError::input("插件目标在检查期间发生变化"));
    }
    verify_current_owner(&opened, "插件目标")?;
    let mut actual = Vec::with_capacity(expected.len());
    file.read_to_end(&mut actual)
        .map_err(|error| AppError::io(format!("无法读取现有插件目标: {error}")))?;
    Ok(Some(actual == expected))
}

struct StagingFile {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl StagingFile {
    fn create(directory: &Path) -> AppResult<Self> {
        for _ in 0..16 {
            let mut random = [0u8; 16];
            SystemRandom::new()
                .fill(&mut random)
                .map_err(|_| AppError::internal("无法生成安装临时文件名"))?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let path = directory.join(format!(".zhsh-install-{suffix}.tmp"));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            }
            match options.open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        committed: false,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(AppError::io(format!("无法创建安装临时文件: {error}"))),
            }
        }
        Err(AppError::io("无法分配唯一安装临时文件"))
    }

    fn write_all(&mut self, bytes: &[u8]) -> AppResult<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| AppError::internal("安装临时文件已经关闭"))?;
        file.write_all(bytes)
            .map_err(|error| AppError::io(format!("无法写入安装临时文件: {error}")))?;
        set_file_mode(file, 0o600)?;
        file.sync_all()
            .map_err(|error| AppError::io(format!("无法同步安装临时文件: {error}")))?;
        let metadata = file
            .metadata()
            .map_err(|error| AppError::io(format!("无法复核安装临时文件: {error}")))?;
        if !metadata.is_file() || metadata.len() != bytes.len() as u64 {
            return Err(AppError::io("安装临时文件复核失败"));
        }
        verify_current_owner(&metadata, "安装临时文件")?;
        self.file.take();
        Ok(())
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn verify_home(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查用户 HOME: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::input("用户 HOME 必须是非符号链接目录"));
    }
    verify_current_owner(&metadata, "用户 HOME")
}

fn create_private_directory(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(AppError::io(format!(
                "无法创建私有目录 {}: {error}",
                terminal_safe_path(path)
            ))),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(AppError::input("当前平台不支持安全插件安装"))
    }
}

fn secure_directory(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查私有目录: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::input(format!(
            "私有路径不是普通目录：{}",
            terminal_safe_path(path)
        )));
    }
    verify_current_owner(&metadata, "私有目录")?;
    set_path_mode(path, 0o700)?;
    let checked = fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法复核私有目录: {error}")))?;
    verify_mode(&checked, 0o700, "私有目录")
}

fn secure_existing_file(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查插件目标: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::input("插件目标必须是非符号链接普通文件"));
    }
    verify_current_owner(&metadata, "插件目标")?;
    set_path_mode(path, 0o600)?;
    let checked = fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法复核插件目标: {error}")))?;
    verify_mode(&checked, 0o600, "插件目标")
}

#[cfg(unix)]
fn verify_current_owner(metadata: &fs::Metadata, label: &str) -> AppResult<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(AppError::input(format!("{label}所有者不是当前用户")));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_current_owner(_: &fs::Metadata, _: &str) -> AppResult<()> {
    Err(AppError::input("当前平台不支持安全插件安装"))
}

#[cfg(unix)]
fn set_path_mode(path: &Path, mode: u32) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| AppError::io(format!("无法维护私有权限: {error}")))
}

#[cfg(not(unix))]
fn set_path_mode(_: &Path, _: u32) -> AppResult<()> {
    Err(AppError::input("当前平台不支持安全插件安装"))
}

#[cfg(unix)]
fn set_file_mode(file: &File, mode: u32) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|error| AppError::io(format!("无法维护临时文件权限: {error}")))
}

#[cfg(not(unix))]
fn set_file_mode(_: &File, _: u32) -> AppResult<()> {
    Err(AppError::input("当前平台不支持安全插件安装"))
}

#[cfg(unix)]
fn verify_mode(metadata: &fs::Metadata, expected: u32, label: &str) -> AppResult<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.mode() & 0o777 != expected {
        return Err(AppError::input(format!("{label}权限必须是 {expected:04o}")));
    }
    verify_current_owner(metadata, label)
}

#[cfg(not(unix))]
fn verify_mode(_: &fs::Metadata, _: u32, _: &str) -> AppResult<()> {
    Err(AppError::input("当前平台不支持安全插件安装"))
}

fn sync_directory(path: &Path) -> AppResult<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| AppError::io(format!("无法同步插件目录: {error}")))
}

/// 把路径转换为不会注入终端控制序列的可展示字符串。
pub(crate) fn terminal_safe_path(path: &Path) -> String {
    let mut output = String::new();
    for character in path.to_string_lossy().chars() {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if is_terminal_format_control(character) => {
                output.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => output.push(character),
        }
    }
    output
}

fn is_terminal_format_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character as u32,
            0x061c | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2069 | 0xfeff
        )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zhsh-private-file-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn creates_private_tree_and_atomically_persists() {
        let home = fixture();
        let directory = ensure_private_tree(&home, &[".zhsh", "plugins", "safety"]).unwrap();
        let created = persist_private_file(
            &directory,
            "test.zhse.json",
            b"one",
            PersistPolicy::IdenticalOnly,
        )
        .unwrap();
        assert_eq!(created.outcome, PersistOutcome::Created);
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&created.path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&created.path).unwrap().uid(), unsafe {
            libc::geteuid()
        });

        let identical = persist_private_file(
            &directory,
            "test.zhse.json",
            b"one",
            PersistPolicy::IdenticalOnly,
        )
        .unwrap();
        assert_eq!(identical.outcome, PersistOutcome::Identical);
        assert!(persist_private_file(
            &directory,
            "test.zhse.json",
            b"two",
            PersistPolicy::IdenticalOnly
        )
        .is_err());
        let replaced =
            persist_private_file(&directory, "test.zhse.json", b"two", PersistPolicy::Replace)
                .unwrap();
        assert_eq!(replaced.outcome, PersistOutcome::Replaced);
        assert_eq!(fs::read(replaced.path).unwrap(), b"two");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn rejects_source_and_destination_symlinks() {
        use std::os::unix::fs::symlink;

        let home = fixture();
        let source = home.join("source");
        fs::write(&source, "value").unwrap();
        let source_link = home.join("source-link");
        symlink(&source, &source_link).unwrap();
        assert!(read_file_snapshot(&source_link, 1024).is_err());

        let directory = ensure_private_tree(&home, &[".zhsh", "plugins", "safety"]).unwrap();
        symlink(&source, directory.join("target.zhse.json")).unwrap();
        assert!(persist_private_file(
            &directory,
            "target.zhse.json",
            b"value",
            PersistPolicy::Replace
        )
        .is_err());
        fs::remove_dir_all(home).unwrap();
    }
}
