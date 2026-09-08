//! 用户直接命令的粗粒度前台进程监督。
//!
//! 每一整行 Bash 输入拥有独立进程组；本模块只保存最近一次暂停的整行命令。它不解析
//! Bash 内部 pipeline/background 作业，也不提供完整 job table。

use super::interactive::ForegroundTerminal;
use std::io;
use std::process::Child;
use std::time::{Duration, Instant};

const TERMINATION_GRACE: Duration = Duration::from_millis(500);
const KILL_GRACE: Duration = Duration::from_millis(500);

pub(super) enum ChildState {
    Running,
    Exited { code: i32, signal: Option<i32> },
    Stopped(i32),
}

struct StoppedCommand {
    child: Child,
    process_group: u32,
    command: String,
    terminal_settings: Option<libc::termios>,
}

#[derive(Default)]
pub(super) struct ForegroundSupervisor {
    stopped: Option<StoppedCommand>,
}

impl ForegroundSupervisor {
    /// 等待新启动的整行命令；暂停时保存到单一恢复槽位。
    pub(super) fn wait(&mut self, child: Child, command: String) -> io::Result<i32> {
        let process_group = child.id();
        self.wait_command(StoppedCommand {
            child,
            process_group,
            command,
            terminal_settings: None,
        })
    }

    /// 恢复最近暂停的整行命令。
    pub(super) fn resume(&mut self) -> io::Result<Option<i32>> {
        let Some(command) = self.stopped.take() else {
            return Ok(None);
        };
        self.wait_command(command).map(Some)
    }

    fn wait_command(&mut self, mut command: StoppedCommand) -> io::Result<i32> {
        if let ChildState::Exited { code, .. } = poll_child(&command.child)? {
            return Ok(code);
        }

        let terminal = match ForegroundTerminal::give_to_with_settings(
            command.process_group,
            command.terminal_settings.as_ref(),
        ) {
            Ok(terminal) => Some(terminal),
            Err(error) if terminal_control_is_unavailable(&error) => None,
            Err(error) => {
                terminate_process_group(&mut command.child, command.process_group);
                return Err(error);
            }
        };
        if terminal.is_none() {
            signal_group(command.process_group, libc::SIGCONT);
        }

        let state = wait_child(&command.child);
        if matches!(state, Ok(ChildState::Stopped(_))) {
            command.terminal_settings = terminal
                .as_ref()
                .and_then(|terminal| terminal.current_settings().ok());
        }
        drop(terminal);

        match state? {
            ChildState::Exited { code, .. } => Ok(code),
            ChildState::Stopped(signal) => {
                if self.stopped.is_some() {
                    terminate_process_group(&mut command.child, command.process_group);
                    eprintln!(
                        "zhsh: 已有一条暂停命令；新暂停命令已终止（完整 job control 尚未实现）"
                    );
                    return Ok(125);
                }
                eprintln!("[暂停] {}（信号 {signal}；运行 fg 恢复）", command.command);
                self.stopped = Some(command);
                Ok(128 + signal)
            }
            ChildState::Running => unreachable!("blocking wait cannot return running"),
        }
    }
}

impl Drop for ForegroundSupervisor {
    fn drop(&mut self) {
        if let Some(mut command) = self.stopped.take() {
            terminate_process_group(&mut command.child, command.process_group);
        }
    }
}

/// 非阻塞观察直属 leader 的退出或停止状态。
pub(super) fn poll_child(child: &Child) -> io::Result<ChildState> {
    waitpid(child.id(), libc::WNOHANG | libc::WUNTRACED)
}

/// 继续并有界终止完整原进程组，同时回收直属 leader。
pub(super) fn terminate_process_group(child: &mut Child, process_group: u32) -> bool {
    signal_group(process_group, libc::SIGCONT);
    signal_group(process_group, libc::SIGTERM);
    if wait_for_exit(child, process_group, TERMINATION_GRACE) {
        return true;
    }
    signal_group(process_group, libc::SIGKILL);
    wait_for_exit(child, process_group, KILL_GRACE)
}

pub(super) fn wait_child(child: &Child) -> io::Result<ChildState> {
    loop {
        match waitpid(child.id(), libc::WUNTRACED) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn waitpid(pid: u32, options: libc::c_int) -> io::Result<ChildState> {
    let mut status = 0;
    // SAFETY: status 指向有效整数；pid 是当前进程创建并持有的直属子进程。
    let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, options) };
    if result == 0 {
        return Ok(ChildState::Running);
    }
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if libc::WIFSTOPPED(status) {
        return Ok(ChildState::Stopped(libc::WSTOPSIG(status)));
    }
    if libc::WIFEXITED(status) {
        return Ok(ChildState::Exited {
            code: libc::WEXITSTATUS(status),
            signal: None,
        });
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        return Ok(ChildState::Exited {
            code: 128 + signal,
            signal: Some(signal),
        });
    }
    Ok(ChildState::Running)
}

fn wait_for_exit(child: &Child, process_group: u32, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        match poll_child(child) {
            Ok(ChildState::Exited { .. }) if !process_group_exists(process_group) => return true,
            Ok(ChildState::Stopped(_)) => signal_group(process_group, libc::SIGCONT),
            Ok(ChildState::Running | ChildState::Exited { .. }) | Err(_) => {}
        }
        if !process_group_exists(process_group) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn signal_group(process_group: u32, signal: libc::c_int) {
    // SAFETY: 负 PID 只选择本监督器创建的进程组。
    unsafe {
        libc::kill(-(process_group as libc::pid_t), signal);
    }
}

fn process_group_exists(process_group: u32) -> bool {
    // SAFETY: signal 0 只探测指定进程组，不投递信号。
    if unsafe { libc::kill(-(process_group as libc::pid_t), 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

pub(super) fn terminal_control_is_unavailable(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported || error.raw_os_error() == Some(libc::ENOTTY)
}
