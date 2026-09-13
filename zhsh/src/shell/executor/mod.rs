//! 外部 Bash 命令执行器。
//!
//! 交互执行和 Agent 捕获执行共享同一套命令构造逻辑；`source` 所需的 Bash 状态快照
//! 协议由 [`source_loader`] 专门处理。

use super::{BoundCommand, SessionState};
use crate::common::{AppError, AppResult, CancellationToken};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

mod captured;
mod compound;
#[cfg(unix)]
mod foreground;
mod interactive;
pub(super) mod native;
mod script_pipe;
pub(crate) mod source_loader;

/// Agent 单个 run 计划允许产生的 stdout 与 stderr 总字节数。
pub(crate) const AGENT_COMMAND_OUTPUT_LIMIT: usize = 1024 * 1024;
/// Agent 单条命令最多反馈给模型的捕获文本字节数。
pub(crate) const AGENT_COMMAND_FEEDBACK_LIMIT: usize = 64 * 1024;
/// 一次自然语言任务最多累计反馈给模型的命令输出字节数。
pub(crate) const AGENT_TASK_FEEDBACK_LIMIT: usize = 256 * 1024;

pub(super) fn requires_foreground_terminal(input: &str) -> bool {
    interactive::requires_terminal(input)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentStdioMode {
    Capture,
    ForegroundCapture,
    Inherit,
}

pub(super) fn agent_stdio_mode(input: &str) -> AgentStdioMode {
    match interactive::terminal_mode(input) {
        interactive::TerminalMode::None => AgentStdioMode::Capture,
        interactive::TerminalMode::Captured => AgentStdioMode::ForegroundCapture,
        interactive::TerminalMode::Opaque => AgentStdioMode::Inherit,
    }
}

const COMMAND_NOT_FOUND_MARKER: &[u8] = b"\x1eZHSH_COMMAND_NOT_FOUND_HANDLE\x1e";
const STARTUP_ALIASES_MARKER: &[u8] = b"\x1eZHSH_STARTUP_ALIASES_V1\x1e";
const PRINT_BASH_STARTUP_STATE: &str = r#"printf '\036ZHSH_STARTUP_ALIASES_V1\036\000'; for __zhsh_startup_name in "${!BASH_ALIASES[@]}"; do printf '%s\000%s\000' "$__zhsh_startup_name" "${BASH_ALIASES[$__zhsh_startup_name]}"; done; unset __zhsh_startup_name; printf '\036ZHSH_COMMAND_NOT_FOUND_HANDLE\036'; declare -f command_not_found_handle || :"#;
const BASH_STARTUP_OUTPUT_LIMIT: u64 = 1024 * 1024;

#[derive(Default)]
struct BashStartupSnapshot {
    aliases: HashMap<String, String>,
    command_not_found_handle: Option<String>,
}

/// Agent 外部命令的结束原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandTermination {
    /// 子进程自行退出或被用户取消信号中断。
    Exited,
    /// stdout 与 stderr 总量超过硬上限，进程组被终止。
    OutputLimit,
    /// Bash leader 退出后发现原进程组中仍有后代，监督器已清理。
    BackgroundTerminated,
    /// 在有界 deadline 内无法证明进程组和输出管道均已清理。
    SupervisionFailed,
    /// Agent 前台交互命令被终端暂停；宿主已继续并终止该进程组以收回终端。
    StoppedTerminated,
    /// Agent 为进入手动澄清而中断并回收了捕获命令。
    Interrupted,
}

/// zhsh 对本次命令输出通道的实际观测完整性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputEvidence {
    /// 命令终止前的输出通道已完整排空；内容可以为空。
    Complete,
    /// 输出通道已观测，但用于 Agent 的有界副本省略了部分内容。
    Truncated,
    /// 命令未正常完成，只保留终止前已经观测到的部分输出。
    Partial,
    /// 完整终端会话直接透传，zhsh 没有取得可作为文本证据的输出。
    Unavailable,
    /// 输出读取或实时展示发生技术错误，不能把现有内容视为完整证据。
    CaptureFailed,
}

impl OutputEvidence {
    pub(crate) fn supports_observation(self) -> bool {
        matches!(self, Self::Complete | Self::Truncated)
    }

    pub(super) fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::CaptureFailed, _) | (_, Self::CaptureFailed) => Self::CaptureFailed,
            (Self::Unavailable, Self::Unavailable) => Self::Unavailable,
            (Self::Unavailable, _) | (_, Self::Unavailable) => Self::Partial,
            (Self::Partial, _) | (_, Self::Partial) => Self::Partial,
            (Self::Truncated, _) | (_, Self::Truncated) => Self::Truncated,
            _ => Self::Complete,
        }
    }
}

/// 有界捕获后返回给 Shell 和 Agent 的命令结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedExecution {
    /// 有界的 UTF-8 宽松解码输出，可能包含截断标记。
    pub(crate) output: String,
    /// 从两个管道实际读取的原始字节总数。
    pub(crate) total_output_bytes: usize,
    /// Bash 的退出码；信号终止按 `128 + signal` 映射。
    pub(crate) exit_code: i32,
    /// 正常退出、手动中断、输出超限、残留后代清理或监督失败。
    pub(crate) termination: CommandTermination,
    /// 输出通道是否真正被观测，以及进入 Agent 的内容是否完整。
    pub(crate) output_evidence: OutputEvidence,
}

#[derive(Clone, Copy)]
enum OutputMode {
    Inherit,
    Capture,
    ForegroundCapture,
}

/// 使用固定 `bash -c` 启动器和 FD 3 分帧管道实现的外部命令执行器。
#[derive(Default)]
pub(crate) struct BashExecutor {
    #[cfg(unix)]
    foreground: foreground::ForegroundSupervisor,
}

struct PreparedBashCommand {
    command: Command,
    script_pipe: script_pipe::ScriptPipe,
}

impl PreparedBashCommand {
    fn into_parts(self) -> (Command, script_pipe::ScriptPipe) {
        (self.command, self.script_pipe)
    }
}

/// Shell 编排层使用的最小外部命令端口。
pub(crate) trait CommandExecutor {
    /// 继承当前终端执行命令并返回退出状态。
    ///
    /// # Arguments
    ///
    /// - `session`：用于 cwd、环境和 Bash 状态投影的只读会话。
    /// - `input`：交给 Bash 的命令文本。
    ///
    /// # Errors
    ///
    /// 无法启动或等待 Bash 时返回 I/O 错误。
    fn run_interactive(&mut self, session: &SessionState, input: &str) -> AppResult<i32>;

    /// 继承终端执行命令，并用给定字节流替代命令的标准输入。
    fn run_interactive_with_input(
        &mut self,
        session: &SessionState,
        input: &str,
        stdin: &[u8],
    ) -> AppResult<i32>;

    /// 执行 Agent 命令；普通命令有界捕获，需要终端的命令切换到行式采集或不透明透传。
    ///
    /// # Arguments
    ///
    /// - `session`：用于 cwd、环境和 Bash 状态投影的只读会话。
    /// - `input`：交给 Bash 的命令文本。
    /// - `cancellation`：控制启动竞态和活动进程组中断的任务令牌。
    ///
    /// # Returns
    ///
    /// 返回执行结果；`None` 表示命令在启动前已取消。捕获模式的 stdout 和 stderr 按
    /// 读取到达顺序合并，不保证保留两个内核管道的严格写入时序；结果中的
    /// [`OutputEvidence`] 独立说明输出完整、截断、部分、不可取得或捕获失败。
    ///
    /// # Errors
    ///
    /// 无法创建或等待 Bash 子进程时返回 I/O 错误。
    #[cfg(test)]
    fn run_agent(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>>;
}

impl BashExecutor {
    fn external_command(
        &self,
        session: &SessionState,
        path: &Path,
        arguments: &[impl AsRef<OsStr>],
        mode: OutputMode,
    ) -> Command {
        let mut command = Command::new(path);
        command.args(arguments);
        command.current_dir(&session.cwd);
        command.env_clear();
        command.envs(&session.env);
        match mode {
            OutputMode::Inherit => {
                command.stdin(Stdio::inherit());
                command.stdout(Stdio::inherit());
                command.stderr(Stdio::inherit());
            }
            OutputMode::Capture => {
                command.stdin(Stdio::null());
                command.stdout(Stdio::piped());
                command.stderr(Stdio::piped());
            }
            OutputMode::ForegroundCapture => {
                command.stdin(Stdio::inherit());
                command.stdout(Stdio::piped());
                command.stderr(Stdio::piped());
            }
        }
        command
    }

    fn command(
        &self,
        session: &SessionState,
        input: &str,
        mode: OutputMode,
    ) -> AppResult<PreparedBashCommand> {
        self.command_with_alias_policy(session, input, mode, true)
    }

    fn command_with_alias_policy(
        &self,
        session: &SessionState,
        input: &str,
        mode: OutputMode,
        expand_alias: bool,
    ) -> AppResult<PreparedBashCommand> {
        let expanded = if expand_alias {
            session.expand_alias(input)
        } else {
            input.to_owned()
        };
        let state = session.prepare_bash_state();
        let mut command = Command::new("bash");
        command.args(["-c", script_pipe::LAUNCHER]);
        command.current_dir(&session.cwd);
        command.env_clear();
        command.envs(&session.env);
        command.env_remove("__zhsh_transport_state");
        command.env_remove("__zhsh_transport_command");
        match mode {
            OutputMode::Inherit => {
                command.stdin(Stdio::inherit());
                command.stdout(Stdio::inherit());
                command.stderr(Stdio::inherit());
            }
            OutputMode::Capture => {
                command.stdin(Stdio::null());
                command.stdout(Stdio::piped());
                command.stderr(Stdio::piped());
            }
            OutputMode::ForegroundCapture => {
                command.stdin(Stdio::inherit());
                command.stdout(Stdio::piped());
                command.stderr(Stdio::piped());
            }
        }
        let script_pipe =
            script_pipe::ScriptPipe::attach(&mut command, state, expanded).map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidInput {
                    AppError::input(error.to_string())
                } else {
                    AppError::io(format!("无法创建 Bash 传输管道: {error}"))
                }
            })?;
        Ok(PreparedBashCommand {
            command,
            script_pipe,
        })
    }

    fn execute(&mut self, session: &SessionState, input: &str) -> AppResult<i32> {
        let (mut command, script_pipe) = self
            .command(session, input, OutputMode::Inherit)?
            .into_parts();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| AppError::io(format!("无法启动 Bash: {error}")))?;
        let writer = script_pipe.start_writer();
        if let Err(error) = writer.finish() {
            abort_user_command(&mut child);
            return Err(AppError::io(format!("无法向 Bash 发送脚本: {error}")));
        }
        #[cfg(unix)]
        {
            self.foreground
                .wait(child, input.to_owned())
                .map_err(|error| AppError::io(format!("等待 Bash 失败: {error}")))
        }
        #[cfg(not(unix))]
        {
            let status = child
                .wait()
                .map_err(|error| AppError::io(format!("等待 Bash 失败: {error}")))?;
            Ok(status.code().unwrap_or(1))
        }
    }

    fn execute_with_input(
        &mut self,
        session: &SessionState,
        input: &str,
        stdin: &[u8],
    ) -> AppResult<i32> {
        let (mut command, script_pipe) = self
            .command(session, input, OutputMode::Inherit)?
            .into_parts();
        command.stdin(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| AppError::io(format!("无法启动 Bash: {error}")))?;
        let Some(mut child_stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::internal("Bash stdin 管道未创建"));
        };
        let script_writer = script_pipe.start_writer();
        let stdin = stdin.to_vec();
        let stdin_writer = thread::spawn(move || child_stdin.write_all(&stdin));

        let script_result = script_writer
            .finish()
            .map_err(|error| AppError::io(format!("无法向 Bash 发送脚本: {error}")));
        let stdin_result = stdin_writer
            .join()
            .map_err(|_| AppError::internal("Bash stdin 写线程异常终止"))?;

        if let Err(error) = script_result {
            abort_user_command(&mut child);
            return Err(error);
        }
        if let Err(error) = stdin_result {
            if error.kind() != io::ErrorKind::BrokenPipe {
                abort_user_command(&mut child);
                return Err(AppError::io(format!("无法写入 Bash stdin: {error}")));
            }
        }
        #[cfg(unix)]
        {
            self.foreground
                .wait(child, input.to_owned())
                .map_err(|error| AppError::io(format!("等待 Bash 失败: {error}")))
        }
        #[cfg(not(unix))]
        {
            let status = child
                .wait()
                .map_err(|error| AppError::io(format!("等待 Bash 失败: {error}")))?;
            Ok(status.code().unwrap_or(1))
        }
    }

    #[cfg(test)]
    fn execute_captured(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let (mut command, script_pipe) = self
            .command(session, input, OutputMode::Capture)?
            .into_parts();
        let child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => return Err(AppError::io(format!("无法启动命令: {error}"))),
        };
        let writer = script_pipe.start_writer();
        let result = captured::wait(child, cancellation, AGENT_COMMAND_OUTPUT_LIMIT);
        let write_result = writer.finish();
        let execution = result?;
        if execution.termination == CommandTermination::Exited && !cancellation.is_cancelled() {
            write_result.map_err(|error| AppError::io(format!("无法向 Bash 发送脚本: {error}")))?;
        }
        Ok(Some(execution))
    }

    /// 执行已经由 Agent 命令计划冻结的 Bash 文本，不再次展开首词 alias。
    pub(super) fn run_agent_prepared(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        #[cfg(unix)]
        match interactive::terminal_mode(input) {
            interactive::TerminalMode::Captured => {
                return self.execute_agent_foreground_captured_prepared(
                    session,
                    input,
                    cancellation,
                );
            }
            interactive::TerminalMode::Opaque => {
                return self.execute_agent_interactive_prepared(session, input, cancellation);
            }
            interactive::TerminalMode::None => {}
        }
        self.execute_captured_prepared(session, input, cancellation)
    }

    /// 直接启动已经绑定到绝对路径的 Agent 外部目标，不再经过 Bash 名称解析。
    pub(super) fn run_agent_external(
        &self,
        session: &SessionState,
        path: &Path,
        arguments: &[impl AsRef<OsStr>],
        original: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        #[cfg(unix)]
        match interactive::terminal_mode(original) {
            interactive::TerminalMode::Captured => {
                return self.execute_external_foreground_captured(
                    session,
                    path,
                    arguments,
                    cancellation,
                );
            }
            interactive::TerminalMode::Opaque => {
                return self.execute_external_interactive(session, path, arguments, cancellation);
            }
            interactive::TerminalMode::None => {}
        }
        let mut command = self.external_command(session, path, arguments, OutputMode::Capture);
        let child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => return Err(AppError::io(format!("无法启动 Agent 外部命令: {error}"))),
        };
        captured::wait(child, cancellation, AGENT_COMMAND_OUTPUT_LIMIT).map(Some)
    }

    /// 直接执行已绑定的外部 pipeline；参数和重定向均不再经过 Bash 解释。
    pub(super) fn run_agent_bound_pipeline(
        &self,
        session: &SessionState,
        commands: &[BoundCommand],
        hard_limit: usize,
        mode: AgentStdioMode,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        compound::run_pipeline(session, commands, hard_limit, mode, cancellation)
    }

    #[cfg(unix)]
    fn execute_external_foreground_captured(
        &self,
        session: &SessionState,
        path: &Path,
        arguments: &[impl AsRef<OsStr>],
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let mut command =
            self.external_command(session, path, arguments, OutputMode::ForegroundCapture);
        let mut child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(AppError::io(format!(
                    "无法启动 Agent 前台采集命令: {error}"
                )))
            }
        };
        let process_group = child.id();
        let terminal = match interactive::ForegroundTerminal::give_to(process_group) {
            Ok(terminal) => terminal,
            Err(error) => {
                abort_foreground_spawn(&mut child, process_group, cancellation);
                return Err(AppError::io(format!("无法交出前台终端: {error}")));
            }
        };
        let execution = captured::wait_foreground_captured(child, cancellation)?;
        drop(terminal);
        Ok(Some(execution))
    }

    #[cfg(unix)]
    fn execute_external_interactive(
        &self,
        session: &SessionState,
        path: &Path,
        arguments: &[impl AsRef<OsStr>],
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let mut command = self.external_command(session, path, arguments, OutputMode::Inherit);
        let mut child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(AppError::io(format!(
                    "无法启动 Agent 交互外部命令: {error}"
                )))
            }
        };
        let process_group = child.id();
        let terminal = match interactive::ForegroundTerminal::give_to(process_group) {
            Ok(terminal) => terminal,
            Err(error) => {
                // SAFETY: cancellation.spawn 已建立独立进程组；负 PID 只作用于该组。
                unsafe {
                    libc::kill(-(process_group as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                cancellation.finish(process_group);
                return Err(AppError::io(format!("无法交出前台终端: {error}")));
            }
        };
        let execution = captured::wait_interactive(child, cancellation);
        drop(terminal);
        Ok(Some(execution))
    }

    fn execute_captured_prepared(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let (mut command, script_pipe) = self
            .command_with_alias_policy(session, input, OutputMode::Capture, false)?
            .into_parts();
        let child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => return Err(AppError::io(format!("无法启动命令: {error}"))),
        };
        let writer = script_pipe.start_writer();
        let result = captured::wait(child, cancellation, AGENT_COMMAND_OUTPUT_LIMIT);
        let write_result = writer.finish();
        let execution = result?;
        if execution.termination == CommandTermination::Exited && !cancellation.is_cancelled() {
            write_result.map_err(|error| AppError::io(format!("无法向 Bash 发送脚本: {error}")))?;
        }
        Ok(Some(execution))
    }

    #[cfg(unix)]
    fn execute_agent_foreground_captured_prepared(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let (mut command, script_pipe) = self
            .command_with_alias_policy(session, input, OutputMode::ForegroundCapture, false)?
            .into_parts();
        let mut child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => return Err(AppError::io(format!("无法启动前台采集命令: {error}"))),
        };
        let writer = script_pipe.start_writer();
        let process_group = child.id();
        let terminal = match interactive::ForegroundTerminal::give_to(process_group) {
            Ok(terminal) => terminal,
            Err(error) => {
                abort_foreground_spawn(&mut child, process_group, cancellation);
                let _ = writer.finish();
                return Err(AppError::io(format!("无法交出前台终端: {error}")));
            }
        };
        let execution = captured::wait_foreground_captured(child, cancellation);
        drop(terminal);
        let write_result = writer.finish();
        let execution = execution?;
        if execution.termination == CommandTermination::Exited && !cancellation.is_cancelled() {
            write_result.map_err(|error| AppError::io(format!("无法向 Bash 发送脚本: {error}")))?;
        }
        Ok(Some(execution))
    }

    #[cfg(unix)]
    fn execute_agent_interactive_prepared(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let (mut command, script_pipe) = self
            .command_with_alias_policy(session, input, OutputMode::Inherit, false)?
            .into_parts();
        let mut child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => return Err(AppError::io(format!("无法启动交互命令: {error}"))),
        };
        let writer = script_pipe.start_writer();
        let process_group = child.id();
        let terminal = match interactive::ForegroundTerminal::give_to(process_group) {
            Ok(terminal) => terminal,
            Err(error) => {
                // SAFETY: spawn 已把子进程 PID 建成独立进程组；负 PID 只作用于该组。
                unsafe {
                    libc::kill(-(process_group as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                let _ = writer.finish();
                cancellation.finish(process_group);
                return Err(AppError::io(format!("无法交出前台终端: {error}")));
            }
        };
        let execution = captured::wait_interactive(child, cancellation);
        drop(terminal);
        let write_result = writer.finish();
        if execution.termination == CommandTermination::Exited && !cancellation.is_cancelled() {
            write_result.map_err(|error| AppError::io(format!("无法向 Bash 发送脚本: {error}")))?;
        }
        Ok(Some(execution))
    }

    #[cfg(all(test, unix))]
    fn execute_agent_interactive(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let (mut command, script_pipe) = self
            .command(session, input, OutputMode::Inherit)?
            .into_parts();
        let mut child = match cancellation.spawn(&mut command) {
            Ok(Some(child)) => child,
            Ok(None) => return Ok(None),
            Err(error) => return Err(AppError::io(format!("无法启动交互命令: {error}"))),
        };
        let writer = script_pipe.start_writer();
        let process_group = child.id();
        let terminal = match interactive::ForegroundTerminal::give_to(process_group) {
            Ok(terminal) => terminal,
            Err(error) => {
                // SAFETY: spawn 已把子进程 PID 建成独立进程组；负 PID 只作用于该组。
                unsafe {
                    libc::kill(-(process_group as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                let _ = writer.finish();
                cancellation.finish(process_group);
                return Err(AppError::io(format!("无法交出前台终端: {error}")));
            }
        };

        let execution = captured::wait_interactive(child, cancellation);
        drop(terminal);
        let write_result = writer.finish();
        if execution.termination == CommandTermination::Exited && !cancellation.is_cancelled() {
            write_result.map_err(|error| AppError::io(format!("无法向 Bash 发送脚本: {error}")))?;
        }

        Ok(Some(execution))
    }

    #[cfg(test)]
    fn execute_agent(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        #[cfg(unix)]
        if interactive::requires_terminal(input) {
            return self.execute_agent_interactive(session, input, cancellation);
        }
        self.execute_captured(session, input, cancellation)
    }

    /// 从非登录交互 Bash 导入启动时可见的别名和真实命令未找到处理函数。
    ///
    /// # Arguments
    ///
    /// - `session`：提供 cwd/环境并接收成功解析状态的会话。
    ///
    /// 只导入 Bash 启动文件实际定义的 `command_not_found_handle`；不存在时不安装 zhsh
    /// 自定义 fallback。启动失败或输出无法解析时静默跳过，不阻止 REPL 可用。
    pub(crate) fn import_bash_startup_state(&self, session: &mut SessionState) {
        let mut command = Command::new("bash");
        command
            .args(["-ic", PRINT_BASH_STARTUP_STATE])
            .env_clear()
            .envs(&session.env)
            .current_dir(&session.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let mut bytes = Vec::new();
        if stdout
            .take(BASH_STARTUP_OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > BASH_STARTUP_OUTPUT_LIMIT
        {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        let Ok(status) = child.wait() else {
            return;
        };
        if !status.success() {
            return;
        }
        let Some(snapshot) = parse_bash_startup_snapshot(&bytes) else {
            return;
        };
        session.aliases.extend(snapshot.aliases);
        if let Some(definition) = snapshot.command_not_found_handle {
            session
                .functions
                .insert("command_not_found_handle".into(), definition);
        }
    }
}

fn abort_user_command(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let process_group = child.id();
        foreground::terminate_process_group(child, process_group);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
fn abort_foreground_spawn(
    child: &mut std::process::Child,
    process_group: u32,
    cancellation: &CancellationToken,
) {
    // SAFETY: cancellation.spawn 已把 child PID 建成独立 PGID；负 PID 只作用于该组。
    unsafe {
        libc::kill(-(process_group as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    cancellation.finish(process_group);
}

fn extract_command_not_found_handle(output: &[u8]) -> Option<String> {
    let marker_start = output
        .windows(COMMAND_NOT_FOUND_MARKER.len())
        .rposition(|window| window == COMMAND_NOT_FOUND_MARKER)?;
    let definition = &output[marker_start + COMMAND_NOT_FOUND_MARKER.len()..];
    let definition = std::str::from_utf8(definition).ok()?.trim().to_string();
    definition
        .starts_with("command_not_found_handle ()")
        .then_some(definition)
}

fn parse_bash_startup_snapshot(output: &[u8]) -> Option<BashStartupSnapshot> {
    let aliases_start = output
        .windows(STARTUP_ALIASES_MARKER.len())
        .rposition(|window| window == STARTUP_ALIASES_MARKER)?
        + STARTUP_ALIASES_MARKER.len();
    let handler_offset = output[aliases_start..]
        .windows(COMMAND_NOT_FOUND_MARKER.len())
        .position(|window| window == COMMAND_NOT_FOUND_MARKER)?;
    let aliases_end = aliases_start + handler_offset;
    let fields: Vec<_> = output[aliases_start..aliases_end]
        .split(|byte| *byte == 0)
        .collect();
    if fields.len() < 2
        || !fields.first().is_some_and(|field| field.is_empty())
        || !fields.last().is_some_and(|field| field.is_empty())
    {
        return None;
    }
    let fields = &fields[1..fields.len() - 1];
    let (pairs, remainder) = fields.as_chunks::<2>();
    if !remainder.is_empty() || pairs.len() > 4096 {
        return None;
    }
    let mut aliases = HashMap::new();
    for pair in pairs {
        if pair[0].len() > 256 || pair[1].len() > 64 * 1024 {
            continue;
        }
        let (Ok(name), Ok(value)) = (std::str::from_utf8(pair[0]), std::str::from_utf8(pair[1]))
        else {
            continue;
        };
        if !name.is_empty() {
            aliases.insert(name.to_string(), value.to_string());
        }
    }
    Some(BashStartupSnapshot {
        aliases,
        command_not_found_handle: extract_command_not_found_handle(output),
    })
}

impl CommandExecutor for BashExecutor {
    fn run_interactive(&mut self, session: &SessionState, input: &str) -> AppResult<i32> {
        self.execute(session, input)
    }

    fn run_interactive_with_input(
        &mut self,
        session: &SessionState,
        input: &str,
        stdin: &[u8],
    ) -> AppResult<i32> {
        self.execute_with_input(session, input, stdin)
    }

    #[cfg(test)]
    fn run_agent(
        &self,
        session: &SessionState,
        input: &str,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        self.execute_agent(session, input, cancellation)
    }
}

impl super::builtin::fg::ForegroundCommandControl for BashExecutor {
    fn resume_stopped_command(&mut self) -> AppResult<Option<i32>> {
        #[cfg(unix)]
        {
            self.foreground
                .resume()
                .map_err(|error| AppError::io(format!("无法恢复前台命令: {error}")))
        }
        #[cfg(not(unix))]
        {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeExecutor;

    impl CommandExecutor for FakeExecutor {
        fn run_interactive(&mut self, _: &SessionState, _: &str) -> AppResult<i32> {
            Ok(23)
        }

        fn run_interactive_with_input(
            &mut self,
            _: &SessionState,
            _: &str,
            _: &[u8],
        ) -> AppResult<i32> {
            Ok(25)
        }

        fn run_agent(
            &self,
            _: &SessionState,
            _: &str,
            _: &CancellationToken,
        ) -> AppResult<Option<CapturedExecution>> {
            Ok(Some(CapturedExecution {
                output: "fake".into(),
                total_output_bytes: 4,
                exit_code: 24,
                termination: CommandTermination::Exited,
                output_evidence: OutputEvidence::Complete,
            }))
        }
    }

    #[test]
    fn command_port_can_be_tested_without_bash() {
        let executor: &mut dyn CommandExecutor = &mut FakeExecutor;
        let session = SessionState::test();
        assert_eq!(executor.run_interactive(&session, "ignored").unwrap(), 23);
        assert_eq!(
            executor
                .run_interactive_with_input(&session, "ignored", b"input")
                .unwrap(),
            25
        );
        assert_eq!(
            executor
                .run_agent(&session, "ignored", &CancellationToken::default())
                .unwrap(),
            Some(CapturedExecution {
                output: "fake".into(),
                total_output_bytes: 4,
                exit_code: 24,
                termination: CommandTermination::Exited,
                output_evidence: OutputEvidence::Complete,
            })
        );
    }

    #[test]
    fn command_not_found_definition_is_extracted_after_startup_noise() {
        let output = b"startup output\n\x1eZHSH_COMMAND_NOT_FOUND_HANDLE\x1e\
command_not_found_handle () \n{ \n    return 127\n}\n";

        assert_eq!(
            extract_command_not_found_handle(output).as_deref(),
            Some("command_not_found_handle () \n{ \n    return 127\n}")
        );
        assert_eq!(
            extract_command_not_found_handle(COMMAND_NOT_FOUND_MARKER),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_import_uses_only_the_handler_defined_by_interactive_bash() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!("zhsh-bash-startup-{suffix}"));
        std::fs::create_dir_all(&home).unwrap();
        let counter = home.join("startup-count");
        std::fs::write(
            home.join(".bashrc"),
            format!(
                "printf x >> '{}'\n\
                 alias zhsh_startup_alias='printf \"imported value\"'\n\
                 command_not_found_handle () {{ printf 'from-bash:%s\\n' \"$1\"; return 69; }}\n",
                counter.display()
            ),
        )
        .unwrap();

        let executor = BashExecutor::default();
        let mut session = SessionState::test();
        session
            .env
            .insert("HOME".into(), home.display().to_string());
        executor.import_bash_startup_state(&mut session);

        assert_eq!(
            session
                .aliases
                .get("zhsh_startup_alias")
                .map(String::as_str),
            Some("printf \"imported value\"")
        );
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "x");
        assert!(session
            .functions
            .get("command_not_found_handle")
            .is_some_and(|definition| definition.contains("from-bash:%s")));

        std::fs::write(
            home.join(".bashrc"),
            "unset -f command_not_found_handle 2>/dev/null || :\n",
        )
        .unwrap();
        let mut without_handler = SessionState::test();
        without_handler
            .env
            .insert("HOME".into(), home.display().to_string());
        executor.import_bash_startup_state(&mut without_handler);
        assert!(!without_handler
            .functions
            .contains_key("command_not_found_handle"));

        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn bash_diagnostic_uses_user_source_name_and_line_number() {
        let executor = BashExecutor::default();
        let mut session = SessionState::test();
        session.env.remove("BASH_ENV");
        session.variables.insert(
            "ZHSH_DIAGNOSTIC_PREAMBLE".into(),
            "declare -- ZHSH_DIAGNOSTIC_PREAMBLE='present'".into(),
        );

        let result = executor
            .run_agent(
                &session,
                ":\n:\nzhsh_definitely_missing_command",
                &CancellationToken::default(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.exit_code, 127);
        assert!(result.output.contains("bash: line 3:"), "{}", result.output);
        assert!(
            result
                .output
                .contains("zhsh_definitely_missing_command: command not found"),
            "{}",
            result.output
        );
        assert!(!result.output.contains("/dev/fd/3"), "{}", result.output);
    }

    #[cfg(unix)]
    #[test]
    fn bash_children_observe_the_same_default_prompt_variables_as_the_repl() {
        let executor = BashExecutor::default();
        let session = SessionState::test();
        let result = executor
            .run_agent(
                &session,
                "printf '<%s>' \"$PS1\"",
                &CancellationToken::default(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, format!("<{}>", SessionState::DEFAULT_PS1));
    }

    #[cfg(unix)]
    #[test]
    fn user_command_cannot_observe_transport_fd_or_variables() {
        let executor = BashExecutor::default();
        let mut session = SessionState::test();
        session.env.remove("BASH_ENV");
        session
            .env
            .insert("__zhsh_transport_state".into(), "from-env".into());
        session
            .env
            .insert("__zhsh_transport_command".into(), "from-env".into());

        let result = executor
            .run_agent(
                &session,
                "printf '%s|%s|' \"${__zhsh_transport_state-unset}\" \"${__zhsh_transport_command-unset}\"; if [[ -e /dev/fd/3 ]]; then printf open; else printf closed; fi",
                &CancellationToken::default(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, "unset|unset|closed");
    }

    #[cfg(unix)]
    #[test]
    fn projected_command_not_found_handler_preserves_bash_behavior() {
        let executor = BashExecutor::default();
        let mut session = SessionState::test();
        session.env.remove("BASH_ENV");
        session.functions.insert(
            "command_not_found_handle".into(),
            "command_not_found_handle () { printf 'handled:%s\\n' \"$1\"; return 71; }".into(),
        );

        let result = executor
            .run_agent(
                &session,
                "zhsh_handler_probe",
                &CancellationToken::default(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.exit_code, 71);
        assert_eq!(result.output, "handled:zhsh_handler_probe\n");
    }

    #[cfg(unix)]
    #[test]
    fn nul_framing_preserves_trailing_newline_and_exit_status() {
        let executor = BashExecutor::default();
        let mut session = SessionState::test();
        session.env.remove("BASH_ENV");

        let continued = executor
            .run_agent(&session, "printf foo\\\n", &CancellationToken::default())
            .unwrap()
            .unwrap();
        let exited = executor
            .run_agent(&session, "exit 23", &CancellationToken::default())
            .unwrap()
            .unwrap();

        assert_eq!(continued.exit_code, 0);
        assert_eq!(continued.output, "foo");
        assert_eq!(exited.exit_code, 23);
    }

    #[test]
    fn nul_in_user_command_is_rejected_before_spawn() {
        let executor = BashExecutor::default();
        let error = executor
            .command(
                &SessionState::test(),
                "printf before\0printf after",
                OutputMode::Capture,
            )
            .err()
            .expect("NUL should be rejected");

        assert_eq!(error.kind(), crate::common::ErrorKind::Input);
        assert!(error.to_string().contains("不能包含 NUL"));
    }

    #[cfg(unix)]
    #[test]
    fn agent_capture_terminates_output_flood_without_limiting_interactive_path() {
        let executor = BashExecutor::default();
        let session = SessionState::test();
        let result = executor
            .run_agent(
                &session,
                "while :; do printf 1234567890; done",
                &CancellationToken::default(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.termination, CommandTermination::OutputLimit);
        assert!(result.total_output_bytes > AGENT_COMMAND_OUTPUT_LIMIT);
        assert!(result.output.len() <= AGENT_COMMAND_FEEDBACK_LIMIT);
        assert!(result.output.contains("输出已截断"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bash_argv_contains_only_the_fixed_fd_launcher() {
        let executor = BashExecutor::default();
        let mut session = SessionState::test();
        session.variables.insert(
            "ZHSH_ARGV_SECRET".into(),
            "declare -- ZHSH_ARGV_SECRET='zhsh-variable-sentinel'".into(),
        );
        session.aliases.insert(
            "zhsh_argv_alias".into(),
            "printf zhsh-alias-sentinel".into(),
        );
        session.functions.insert(
            "zhsh_argv_function".into(),
            "zhsh_argv_function () { printf zhsh-function-sentinel; }".into(),
        );

        let result = executor
            .run_agent(
                &session,
                "ps -ww -o args= -p $$ | cat # zhsh-user-command-sentinel",
                &CancellationToken::default(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.termination, CommandTermination::Exited);
        assert_eq!(result.exit_code, 0);
        assert!(
            result.output.contains(script_pipe::LAUNCHER),
            "{}",
            result.output
        );
        for hidden in [
            "zhsh-variable-sentinel",
            "zhsh-alias-sentinel",
            "zhsh-function-sentinel",
            "zhsh-user-command-sentinel",
        ] {
            assert!(!result.output.contains(hidden), "{}", result.output);
        }
    }

    #[cfg(unix)]
    #[test]
    fn fd_script_streams_state_larger_than_pipe_capacity() {
        let executor = BashExecutor::default();
        let mut session = SessionState::test();
        let value = "x".repeat(256 * 1024);
        session.variables.insert(
            "ZHSH_LARGE_PIPE_VALUE".into(),
            format!("declare -- ZHSH_LARGE_PIPE_VALUE='{value}'"),
        );

        let result = executor
            .run_agent(
                &session,
                "printf '%s' \"${#ZHSH_LARGE_PIPE_VALUE}\"",
                &CancellationToken::default(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, (256 * 1024).to_string());
    }
}
