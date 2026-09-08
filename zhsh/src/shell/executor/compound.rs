//! 已绑定组合命令的直接进程执行。
//!
//! 所有 argv、目标路径和 FD 连接都来自不可变计划；本模块不构造命令字符串，也不启动 Bash。

use super::super::{
    BoundCommand, BoundRedirection, BoundTarget, OutputMode as BoundOutputMode, StandardStream,
};
use super::{
    captured, interactive, AgentStdioMode, CapturedExecution, SessionState,
    AGENT_COMMAND_OUTPUT_LIMIT,
};
use crate::common::{AppError, AppResult, CancellationToken};
use std::fs::{File, OpenOptions};
use std::process::{Child, Command, Stdio};

pub(super) fn run_pipeline(
    session: &SessionState,
    commands: &[BoundCommand],
    hard_limit: usize,
    mode: AgentStdioMode,
    cancellation: &CancellationToken,
) -> AppResult<Option<CapturedExecution>> {
    if commands.is_empty() {
        return Err(AppError::internal("bound pipeline cannot be empty"));
    }
    #[cfg(not(unix))]
    {
        let _ = (session, commands, hard_limit, mode, cancellation);
        return Err(AppError::input("当前平台不支持 Agent 静态 pipeline"));
    }
    #[cfg(unix)]
    run_pipeline_unix(session, commands, hard_limit, mode, cancellation)
}

#[cfg(unix)]
fn run_pipeline_unix(
    session: &SessionState,
    commands: &[BoundCommand],
    hard_limit: usize,
    mode: AgentStdioMode,
    cancellation: &CancellationToken,
) -> AppResult<Option<CapturedExecution>> {
    let (output_reader, output_writer) = if mode == AgentStdioMode::Inherit {
        (None, None)
    } else {
        let (reader, writer) = pipe_pair()
            .map_err(|error| AppError::io(format!("无法创建组合命令输出管道: {error}")))?;
        (Some(reader), Some(writer))
    };
    let mut links = Vec::with_capacity(commands.len().saturating_sub(1));
    for _ in 1..commands.len() {
        links.push(
            pipe_pair().map_err(|error| AppError::io(format!("无法创建 pipeline: {error}")))?,
        );
    }

    let mut prepared = Vec::with_capacity(commands.len());
    for (index, bound) in commands.iter().enumerate() {
        let BoundTarget::External { path, .. } = &bound.target else {
            return Err(AppError::internal(
                "query builtin must not appear inside a bound pipeline",
            ));
        };
        let stdin = if index == 0 {
            if mode == AgentStdioMode::Capture {
                FdSource::Null
            } else {
                FdSource::Inherited(libc::STDIN_FILENO)
            }
        } else {
            FdSource::File(
                links[index - 1]
                    .0
                    .try_clone()
                    .map_err(|error| AppError::io(format!("无法复制 pipeline 输入: {error}")))?,
            )
        };
        let stdout = if index + 1 == commands.len() {
            match &output_writer {
                Some(output_writer) => {
                    FdSource::File(output_writer.try_clone().map_err(|error| {
                        AppError::io(format!("无法复制组合命令输出管道: {error}"))
                    })?)
                }
                None => FdSource::Inherited(libc::STDOUT_FILENO),
            }
        } else {
            FdSource::File(
                links[index]
                    .1
                    .try_clone()
                    .map_err(|error| AppError::io(format!("无法复制 pipeline 输出: {error}")))?,
            )
        };
        let stderr = match &output_writer {
            Some(output_writer) => FdSource::File(
                output_writer
                    .try_clone()
                    .map_err(|error| AppError::io(format!("无法复制组合命令错误管道: {error}")))?,
            ),
            None => FdSource::Inherited(libc::STDERR_FILENO),
        };
        let mut descriptors = [stdin, stdout, stderr];
        apply_redirections(&mut descriptors, &bound.redirections)?;

        let mut command = Command::new(path);
        command.args(&bound.arguments);
        command.current_dir(&session.cwd);
        command.env_clear();
        command.envs(&session.env);
        command.stdin(descriptors[0].stdio()?);
        command.stdout(descriptors[1].stdio()?);
        command.stderr(descriptors[2].stdio()?);
        prepared.push(command);
    }
    drop(links);
    drop(output_writer);

    let mut children: Vec<Child> = Vec::with_capacity(prepared.len());
    let mut process_group = None;
    for mut command in prepared {
        match cancellation.spawn_in_process_group(&mut command, process_group) {
            Ok(Some(child)) => {
                process_group.get_or_insert_with(|| child.id());
                children.push(child);
            }
            Ok(None) => {
                abort_pipeline(&mut children, process_group, cancellation);
                return Ok(None);
            }
            Err(error) => {
                abort_pipeline(&mut children, process_group, cancellation);
                return Err(AppError::io(format!("无法启动 Agent 组合命令: {error}")));
            }
        }
    }
    let process_group =
        process_group.ok_or_else(|| AppError::internal("pipeline has no leader"))?;
    match mode {
        AgentStdioMode::Capture => captured::wait_group(
            children,
            process_group,
            output_reader.expect("capture mode creates an output pipe"),
            cancellation,
            hard_limit.min(AGENT_COMMAND_OUTPUT_LIMIT),
        ),
        AgentStdioMode::ForegroundCapture => {
            let terminal = match interactive::ForegroundTerminal::give_to(process_group) {
                Ok(terminal) => terminal,
                Err(error) => {
                    abort_pipeline(&mut children, Some(process_group), cancellation);
                    return Err(AppError::io(format!("无法交出前台终端: {error}")));
                }
            };
            let result = captured::wait_group_foreground(
                children,
                process_group,
                Some(output_reader.expect("foreground capture creates an output pipe")),
                cancellation,
            );
            drop(terminal);
            result
        }
        AgentStdioMode::Inherit => {
            let terminal = match interactive::ForegroundTerminal::give_to(process_group) {
                Ok(terminal) => terminal,
                Err(error) => {
                    abort_pipeline(&mut children, Some(process_group), cancellation);
                    return Err(AppError::io(format!("无法交出前台终端: {error}")));
                }
            };
            let result =
                captured::wait_group_foreground(children, process_group, None, cancellation);
            drop(terminal);
            result
        }
    }
    .map(Some)
}

#[cfg(unix)]
fn abort_pipeline(
    children: &mut [Child],
    process_group: Option<u32>,
    cancellation: &CancellationToken,
) {
    if let Some(process_group) = process_group {
        // SAFETY: process_group 是本次 pipeline leader 的 PID/PGID。
        unsafe {
            libc::kill(-(process_group as i32), libc::SIGKILL);
        }
        for child in children {
            let _ = child.wait();
        }
        cancellation.finish(process_group);
    }
}

enum FdSource {
    Null,
    File(File),
    Inherited(libc::c_int),
}

impl FdSource {
    fn duplicate(&self) -> AppResult<Self> {
        match self {
            Self::Null => Ok(Self::Null),
            Self::File(file) => file
                .try_clone()
                .map(Self::File)
                .map_err(|error| AppError::io(format!("无法复制重定向 FD: {error}"))),
            Self::Inherited(fd) => duplicate_fd(*fd).map(Self::File),
        }
    }

    fn stdio(&self) -> AppResult<Stdio> {
        match self {
            Self::Null => Ok(Stdio::null()),
            Self::File(file) => file
                .try_clone()
                .map(Stdio::from)
                .map_err(|error| AppError::io(format!("无法配置组合命令 FD: {error}"))),
            Self::Inherited(fd) => duplicate_fd(*fd).map(Stdio::from),
        }
    }
}

#[cfg(unix)]
fn duplicate_fd(fd: libc::c_int) -> AppResult<File> {
    use std::os::fd::FromRawFd;

    // SAFETY: fd 是调用者当前继承的标准流；成功后的新 fd 立即交给 File 独占。
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        Err(AppError::io(format!(
            "无法复制标准流 FD {fd}: {}",
            std::io::Error::last_os_error()
        )))
    } else {
        // SAFETY: fcntl 成功返回尚未被 Rust 管理的新描述符。
        Ok(unsafe { File::from_raw_fd(duplicated) })
    }
}

fn apply_redirections(
    descriptors: &mut [FdSource; 3],
    redirections: &[BoundRedirection],
) -> AppResult<()> {
    for redirection in redirections {
        match redirection {
            BoundRedirection::InputFile { fd, path } => {
                let file = File::open(&path.absolute).map_err(|error| {
                    AppError::io(format!(
                        "无法打开输入文件 {}: {error}",
                        path.absolute.display()
                    ))
                })?;
                descriptors[index(*fd)?] = FdSource::File(file);
            }
            BoundRedirection::OutputFile { fd, path, mode } => {
                let mut options = OpenOptions::new();
                options.create(true).write(true);
                match mode {
                    BoundOutputMode::Overwrite => {
                        options.truncate(true);
                    }
                    BoundOutputMode::Append => {
                        options.append(true);
                    }
                }
                let file = options.open(&path.absolute).map_err(|error| {
                    AppError::io(format!(
                        "无法打开输出文件 {}: {error}",
                        path.absolute.display()
                    ))
                })?;
                descriptors[index(*fd)?] = FdSource::File(file);
            }
            BoundRedirection::Duplicate { from, to } => {
                descriptors[index(*from)?] = descriptors[index(*to)?].duplicate()?;
            }
            BoundRedirection::Close { fd } | BoundRedirection::Null { fd } => {
                descriptors[index(*fd)?] = FdSource::Null;
            }
            BoundRedirection::StandardStream { fd, target } => {
                let target = match target {
                    StandardStream::Stdin => 0,
                    StandardStream::Stdout => 1,
                    StandardStream::Stderr => 2,
                };
                descriptors[index(*fd)?] = descriptors[target].duplicate()?;
            }
        }
    }
    Ok(())
}

fn index(fd: u32) -> AppResult<usize> {
    usize::try_from(fd)
        .ok()
        .filter(|fd| *fd <= 2)
        .ok_or_else(|| AppError::input("静态组合命令只支持 FD 0、1、2"))
}

#[cfg(unix)]
fn pipe_pair() -> std::io::Result<(File, File)> {
    use std::os::fd::FromRawFd;

    let mut descriptors = [0; 2];
    // SAFETY: descriptors 指向两个有效的 int 槽；成功后所有权立即交给 File。
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 成功返回的两个 FD 均唯一有效。
    Ok(unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        )
    })
}
