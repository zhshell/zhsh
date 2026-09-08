//! 通过匿名管道把 Bash 状态和用户命令帧传入固定文件描述符。
//!
//! Bash 的 argv 只保留固定启动器；会话投影和用户命令仅存在于管道中，避免被
//! `ps`、`/proc/<pid>/cmdline` 或审计命令直接观察。

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::thread::{self, JoinHandle};

pub(super) const SCRIPT_FD: RawFd = 3;
/// 固定启动器从 FD 3 读取两个 NUL 结尾帧，先恢复会话状态，再执行用户命令。
///
/// 两次 `eval` 刻意分离：状态帧的行数不会污染用户命令的 Bash 诊断；执行用户命令
/// 前关闭 FD 3 并清除传输变量，避免向命令暴露 zhsh 的传输实现。
pub(super) const LAUNCHER: &str = r#"IFS= read -r -d '' __zhsh_transport_state <&3 || exit 125; eval "unset __zhsh_transport_state; $__zhsh_transport_state" || exit $?; IFS= read -r -d '' __zhsh_transport_command <&3 || exit 125; exec 3<&-; eval "unset __zhsh_transport_command; $__zhsh_transport_command""#;

/// 父进程持有的帧管道两端及待发送内容。
pub(super) struct ScriptPipe {
    reader: OwnedFd,
    writer: File,
    payload: Vec<u8>,
}

impl ScriptPipe {
    /// 创建带 `CLOEXEC` 的匿名管道，并把两个 NUL 结尾帧发送给待启动命令。
    pub(super) fn attach(
        command: &mut Command,
        state: String,
        user_command: String,
    ) -> io::Result<Self> {
        if state.contains('\0') || user_command.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Bash 传输帧不能包含 NUL",
            ));
        }

        let mut descriptors = [-1; 2];
        // SAFETY: `descriptors` 指向两个有效的 `c_int`；成功后两个 fd 立即交给 RAII
        // 所有者。`O_CLOEXEC` 防止多线程进程中的其他并发 spawn 泄漏管道。
        if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe2 成功后两个 fd 均为独占且有效，分别只构造一个所有者。
        let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let writer = unsafe { File::from_raw_fd(descriptors[1]) };
        let reader_fd = reader.as_raw_fd();

        // SAFETY: pre_exec 闭包只调用异步信号安全的 dup2/fcntl/close。reader 由
        // `ScriptPipe` 持有到 spawn 完成，因此闭包执行时 reader_fd 仍然有效。
        unsafe {
            command.pre_exec(move || {
                if reader_fd == SCRIPT_FD {
                    if libc::fcntl(SCRIPT_FD, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                } else {
                    if libc::dup2(reader_fd, SCRIPT_FD) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    libc::close(reader_fd);
                }
                Ok(())
            });
        }

        let mut payload = Vec::with_capacity(state.len() + user_command.len() + 2);
        payload.extend_from_slice(state.as_bytes());
        payload.push(0);
        payload.extend_from_slice(user_command.as_bytes());
        payload.push(0);

        Ok(Self {
            reader,
            writer,
            payload,
        })
    }

    /// 子进程启动后关闭父进程读端，并异步写入完整帧序列。
    pub(super) fn start_writer(self) -> ScriptWriter {
        drop(self.reader);
        let mut writer = self.writer;
        let payload = self.payload;
        ScriptWriter(thread::spawn(move || writer.write_all(&payload)))
    }
}

/// 帧写线程；等待它可确保 Bash 没有收到截断输入。
pub(super) struct ScriptWriter(JoinHandle<io::Result<()>>);

impl ScriptWriter {
    pub(super) fn finish(self) -> io::Result<()> {
        self.0
            .join()
            .map_err(|_| io::Error::other("Bash 传输帧写线程异常终止"))?
    }
}
