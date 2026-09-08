//! Agent 外部命令的有界管道捕获与进程监督。
//!
//! Unix 实现直接以非阻塞管道和 `poll` 排空 stdout/stderr。命令 leader 退出
//! 并不代表命令已结束：监督器还会检查原进程组、有界地执行 TERM 到 KILL
//! 升级，并在后代逃离进程组但仍持有管道时按 deadline 失败关闭读端。

use super::{CapturedExecution, CommandTermination, OutputEvidence, AGENT_COMMAND_FEEDBACK_LIMIT};
use crate::common::{AppError, AppResult, CancellationToken};
use std::fs::File;
use std::io::Read;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

const READ_CHUNK: usize = 8 * 1024;
const READ_BUDGET_PER_PIPE: usize = 128 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const TERMINATION_GRACE: Duration = Duration::from_millis(500);
const KILL_GRACE: Duration = Duration::from_millis(500);
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);
const TAIL_LIMIT: usize = 16 * 1024;

pub(super) fn wait(
    child: Child,
    cancellation: &CancellationToken,
    hard_limit: usize,
) -> AppResult<CapturedExecution> {
    #[cfg(unix)]
    {
        wait_unix(child, cancellation, hard_limit)
    }
    #[cfg(not(unix))]
    {
        wait_portable(child, cancellation, hard_limit)
    }
}

/// 等待共享同一进程组和捕获管道的静态 pipeline。
#[cfg(unix)]
pub(super) fn wait_group(
    mut children: Vec<Child>,
    process_group: u32,
    reader: File,
    cancellation: &CancellationToken,
    hard_limit: usize,
) -> AppResult<CapturedExecution> {
    use std::os::fd::AsRawFd;

    set_nonblocking(reader.as_raw_fd()).map_err(|error| {
        AppError::io(format!(
            "cannot configure nonblocking pipeline output: {error}"
        ))
    })?;
    let mut pipe = Some(reader);
    let mut output = BoundedOutput::new(AGENT_COMMAND_FEEDBACK_LIMIT, TAIL_LIMIT);
    let mut statuses: Vec<Option<ExitStatus>> = (0..children.len()).map(|_| None).collect();
    let mut stop = None;
    let mut all_exited_at = None;
    let mut group_gone_at = None;
    let mut supervision_failed = false;

    loop {
        if drain_pipe(&mut pipe, &mut output).is_err() {
            supervision_failed = true;
        }
        if cancellation.is_cancelled() {
            request_stop(
                &mut stop,
                StopReason::Cancelled,
                process_group,
                libc::SIGINT,
            );
        } else if output.total > hard_limit {
            request_stop(
                &mut stop,
                StopReason::OutputLimit,
                process_group,
                libc::SIGTERM,
            );
        }
        for (child, status) in children.iter_mut().zip(&mut statuses) {
            if status.is_none() {
                match child.try_wait() {
                    Ok(result) => *status = result,
                    Err(_) => supervision_failed = true,
                }
            }
        }
        let all_exited = statuses.iter().all(Option::is_some);
        if all_exited {
            all_exited_at.get_or_insert_with(Instant::now);
        }
        let group_alive = process_group_exists(process_group);
        if all_exited && group_alive && stop.is_none() {
            request_stop(
                &mut stop,
                StopReason::Background,
                process_group,
                libc::SIGTERM,
            );
        }
        if !group_alive {
            group_gone_at.get_or_insert_with(Instant::now);
        } else {
            group_gone_at = None;
        }
        if let Some(active_stop) = stop.as_mut() {
            if active_stop.kill_sent_at.is_none()
                && active_stop.started.elapsed() >= TERMINATION_GRACE
                && group_alive
            {
                signal_group(process_group, libc::SIGKILL);
                active_stop.kill_sent_at = Some(Instant::now());
            }
            if active_stop
                .kill_sent_at
                .is_some_and(|sent| sent.elapsed() >= KILL_GRACE && group_alive)
            {
                supervision_failed = true;
            }
        }
        if all_exited && !group_alive && pipe.is_none() {
            break;
        }
        if group_gone_at.is_some_and(|gone| pipe.is_some() && gone.elapsed() >= PIPE_DRAIN_GRACE) {
            supervision_failed = true;
        }
        if all_exited_at.is_some_and(|exited| {
            stop.is_some() && exited.elapsed() >= TERMINATION_GRACE + KILL_GRACE + PIPE_DRAIN_GRACE
        }) {
            supervision_failed = true;
        }
        if supervision_failed {
            signal_group(process_group, libc::SIGKILL);
            drop(pipe.take());
            break;
        }
        poll_file(pipe.as_ref(), POLL_INTERVAL).unwrap_or_else(|error| {
            if error.kind() != std::io::ErrorKind::Interrupted {
                supervision_failed = true;
            }
        });
    }
    for (child, status) in children.iter_mut().zip(&mut statuses) {
        if status.is_none() {
            *status = child.try_wait().ok().flatten();
        }
    }
    cancellation.finish(process_group);
    let cancelled = cancellation.is_cancelled();
    let termination = if supervision_failed {
        CommandTermination::SupervisionFailed
    } else if cancellation.is_interrupted() {
        CommandTermination::Interrupted
    } else {
        match stop.map(|state| state.reason) {
            Some(StopReason::OutputLimit) => CommandTermination::OutputLimit,
            Some(StopReason::Background) => CommandTermination::BackgroundTerminated,
            Some(StopReason::Cancelled) | None => CommandTermination::Exited,
        }
    };
    let exit_code = if cancelled {
        130
    } else if supervision_failed {
        125
    } else {
        statuses
            .last()
            .and_then(Option::as_ref)
            .map(|status| exit_code(*status))
            .unwrap_or(125)
    };
    Ok(CapturedExecution {
        output: output.render(),
        total_output_bytes: output.total,
        exit_code,
        termination,
        output_evidence: output_evidence(&output, termination, supervision_failed),
    })
}

/// 等待已经取得真实前台终端的静态组合命令。
///
/// `reader=Some` 表示行式实时采集；`None` 表示完整终端透传。两种形态共用停止检测、
/// 取消和残留进程组清理，因此组合命令不会因进入终端模式而失去监督。
#[cfg(unix)]
pub(super) fn wait_group_foreground(
    mut children: Vec<Child>,
    process_group: u32,
    reader: Option<File>,
    cancellation: &CancellationToken,
) -> AppResult<CapturedExecution> {
    use std::os::fd::AsRawFd;

    if let Some(reader) = &reader {
        set_nonblocking(reader.as_raw_fd()).map_err(|error| {
            AppError::io(format!(
                "cannot configure foreground pipeline output: {error}"
            ))
        })?;
    }
    let captured = reader.is_some();
    let mut pipe = reader;
    let mut output = BoundedOutput::new(AGENT_COMMAND_FEEDBACK_LIMIT, TAIL_LIMIT);
    let mut statuses: Vec<Option<i32>> = (0..children.len()).map(|_| None).collect();
    let mut stop = None;
    let mut stopped_signal = None;
    let mut group_gone_at = None;
    let mut supervision_failed = false;
    let mut capture_failed = false;

    loop {
        if captured {
            match drain_pipe_to(&mut pipe, &mut output, Some(libc::STDOUT_FILENO)) {
                Ok(presentation_failed) => capture_failed |= presentation_failed,
                Err(_) => capture_failed = true,
            }
        }
        if cancellation.is_cancelled() {
            request_stop(
                &mut stop,
                StopReason::Cancelled,
                process_group,
                libc::SIGTERM,
            );
        }
        for (child, status) in children.iter_mut().zip(&mut statuses) {
            if status.is_some() {
                continue;
            }
            match super::foreground::poll_child(child) {
                Ok(super::foreground::ChildState::Exited { code, signal }) => {
                    *status = Some(code);
                    if signal == Some(libc::SIGINT) {
                        cancellation.cancel();
                    }
                }
                Ok(super::foreground::ChildState::Stopped(signal)) => {
                    stopped_signal.get_or_insert(signal);
                    signal_group(process_group, libc::SIGCONT);
                    request_stop(
                        &mut stop,
                        StopReason::Background,
                        process_group,
                        libc::SIGTERM,
                    );
                }
                Ok(super::foreground::ChildState::Running) => {}
                Err(_) => supervision_failed = true,
            }
        }

        let all_exited = statuses.iter().all(Option::is_some);
        let group_alive = process_group_exists(process_group);
        if all_exited && group_alive && stop.is_none() {
            request_stop(
                &mut stop,
                StopReason::Background,
                process_group,
                libc::SIGTERM,
            );
        }
        if !group_alive {
            group_gone_at.get_or_insert_with(Instant::now);
        } else {
            group_gone_at = None;
        }
        if let Some(active_stop) = stop.as_mut() {
            if active_stop.kill_sent_at.is_none()
                && active_stop.started.elapsed() >= TERMINATION_GRACE
                && group_alive
            {
                signal_group(process_group, libc::SIGKILL);
                active_stop.kill_sent_at = Some(Instant::now());
            }
            if active_stop
                .kill_sent_at
                .is_some_and(|sent| sent.elapsed() >= KILL_GRACE && group_alive)
            {
                supervision_failed = true;
            }
        }

        if all_exited && !group_alive && pipe.is_none() {
            break;
        }
        if group_gone_at.is_some_and(|gone| pipe.is_some() && gone.elapsed() >= PIPE_DRAIN_GRACE) {
            capture_failed = true;
            pipe = None;
        }
        if supervision_failed {
            signal_group(process_group, libc::SIGKILL);
            drop(pipe.take());
            break;
        }
        if let Some(pipe) = &pipe {
            poll_file(Some(pipe), POLL_INTERVAL).unwrap_or_else(|error| {
                if error.kind() != std::io::ErrorKind::Interrupted {
                    capture_failed = true;
                }
            });
        } else {
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    for (child, status) in children.iter().zip(&mut statuses) {
        if status.is_none() {
            if let Ok(super::foreground::ChildState::Exited { code, .. }) =
                super::foreground::poll_child(child)
            {
                *status = Some(code);
            }
        }
    }
    cancellation.finish(process_group);
    let cancelled = cancellation.is_cancelled();
    let termination = if supervision_failed {
        CommandTermination::SupervisionFailed
    } else if cancellation.is_interrupted() {
        CommandTermination::Interrupted
    } else if stopped_signal.is_some() {
        CommandTermination::StoppedTerminated
    } else if stop
        .as_ref()
        .is_some_and(|state| state.reason == StopReason::Background)
        && !cancelled
    {
        CommandTermination::BackgroundTerminated
    } else {
        CommandTermination::Exited
    };
    let exit_code = if cancelled {
        130
    } else if supervision_failed {
        125
    } else if let Some(signal) = stopped_signal {
        128 + signal
    } else {
        statuses.last().and_then(|status| *status).unwrap_or(125)
    };
    Ok(CapturedExecution {
        output: if captured {
            sanitize_terminal_transcript(&output.render())
        } else {
            String::new()
        },
        total_output_bytes: output.total,
        exit_code,
        termination,
        output_evidence: if captured {
            output_evidence(&output, termination, supervision_failed || capture_failed)
        } else {
            OutputEvidence::Unavailable
        },
    })
}

#[cfg(unix)]
fn poll_file(file: Option<&File>, timeout: Duration) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let Some(file) = file else {
        std::thread::sleep(timeout);
        return Ok(());
    };
    let mut descriptor = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    // SAFETY: descriptor 是一个有效的 pollfd，调用期间保持可写。
    let result = unsafe {
        libc::poll(
            &mut descriptor,
            1,
            timeout.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Wait for an Agent command that owns the real foreground terminal, then prove that its
/// original process group is empty before unregistering cancellation.
///
/// Interactive output is inherited rather than captured, so this path has no pipe-drain phase or
/// output limit. It still applies the same bounded residual-process policy as captured execution:
/// after the leader exits, remaining group members receive SIGTERM, a finite grace period, and
/// then SIGKILL. Cancellation remains registered for the whole cleanup interval and always maps
/// the result to status 130.
#[cfg(unix)]
pub(super) fn wait_interactive(
    mut child: Child,
    cancellation: &CancellationToken,
) -> CapturedExecution {
    let process_group = child.id();
    let mut leader_exit_code = None;
    let mut leader_signal = None;
    let mut stopped_signal = None;
    let mut supervision_failed = false;
    let mut cancellation_started = None;
    let mut cancellation_kill_sent = None;

    // Interactive commands have no ordinary runtime deadline. Once cancellation is observed,
    // however, waiting must become bounded even if the foreground program ignores SIGINT.
    loop {
        match super::foreground::poll_child(&child) {
            Ok(super::foreground::ChildState::Exited { code, signal }) => {
                leader_exit_code = Some(code);
                leader_signal = signal;
                break;
            }
            Ok(super::foreground::ChildState::Stopped(signal)) => {
                stopped_signal = Some(signal);
                if !super::foreground::terminate_process_group(&mut child, process_group) {
                    supervision_failed = true;
                }
                break;
            }
            Ok(super::foreground::ChildState::Running) => {}
            Err(_) => {
                supervision_failed = true;
                break;
            }
        }

        if cancellation.is_cancelled() {
            let started = cancellation_started.get_or_insert_with(|| {
                signal_group(process_group, libc::SIGTERM);
                Instant::now()
            });
            if cancellation_kill_sent.is_none() && started.elapsed() >= TERMINATION_GRACE {
                signal_group(process_group, libc::SIGKILL);
                cancellation_kill_sent = Some(Instant::now());
            }
            if cancellation_kill_sent.is_some_and(|sent| sent.elapsed() >= KILL_GRACE) {
                supervision_failed = true;
                break;
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let interrupted = leader_signal.is_some_and(|signal| signal == libc::SIGINT);
    if interrupted {
        // With a real foreground terminal, the kernel delivers Ctrl-C directly to the child
        // group. Reflect that exit in the task token before residual cleanup and unregistration.
        cancellation.cancel();
    }

    let had_residual_group = process_group_exists(process_group);
    if stopped_signal.is_none() && had_residual_group {
        signal_group(process_group, libc::SIGTERM);
        if !wait_for_group_exit(process_group, TERMINATION_GRACE) {
            signal_group(process_group, libc::SIGKILL);
            if !wait_for_group_exit(process_group, KILL_GRACE) {
                supervision_failed = true;
            }
        }
    }

    // A failed leader wait can leave the leader reapable after the bounded group cleanup. Never
    // introduce an unbounded fallback wait on the failure path.
    cancellation.finish(process_group);

    let cancelled = cancellation.is_cancelled();
    let termination = if supervision_failed {
        CommandTermination::SupervisionFailed
    } else if cancellation.is_interrupted() {
        CommandTermination::Interrupted
    } else if stopped_signal.is_some() {
        CommandTermination::StoppedTerminated
    } else if had_residual_group && !cancelled {
        CommandTermination::BackgroundTerminated
    } else {
        CommandTermination::Exited
    };
    let exit_code = if cancelled {
        130
    } else if supervision_failed {
        125
    } else if let Some(signal) = stopped_signal {
        128 + signal
    } else {
        leader_exit_code.unwrap_or(125)
    };

    CapturedExecution {
        output: String::new(),
        total_output_bytes: 0,
        exit_code,
        termination,
        output_evidence: OutputEvidence::Unavailable,
    }
}

/// 等待拥有真实前台终端的行式命令，原样实时显示 stdout/stderr，同时只保存有界证据副本。
///
/// 与普通捕获不同，本路径绝不因证据缓冲区达到上限而终止命令。stdin 和 `/dev/tty` 仍直接
/// 指向当前控制终端，因此 sudo/ssh 的密码输入不会经过本采集器。
#[cfg(unix)]
pub(super) fn wait_foreground_captured(
    mut child: Child,
    cancellation: &CancellationToken,
) -> AppResult<CapturedExecution> {
    use std::os::fd::AsRawFd;

    let process_group = child.id();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        abort_supervision(&mut child, process_group, cancellation);
        return Err(AppError::internal(
            "Agent foreground command stdout and stderr must be piped",
        ));
    };
    if let Err(error) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        abort_supervision(&mut child, process_group, cancellation);
        return Err(AppError::io(format!(
            "cannot configure foreground command pipes: {error}"
        )));
    }

    let mut pipes = PipeSet::new(stdout, stderr);
    let mut output = BoundedOutput::new(AGENT_COMMAND_FEEDBACK_LIMIT, TAIL_LIMIT);
    let mut leader_exit_code = None;
    let mut leader_signal = None;
    let mut stopped_signal = None;
    let mut supervision_failed = false;
    let mut capture_failed = false;
    let mut cancellation_started = None;
    let mut cancellation_kill_sent = None;

    loop {
        match pipes.drain(&mut output, true) {
            Ok(presentation_failed) => capture_failed |= presentation_failed,
            Err(_) => capture_failed = true,
        }
        match super::foreground::poll_child(&child) {
            Ok(super::foreground::ChildState::Exited { code, signal }) => {
                leader_exit_code = Some(code);
                leader_signal = signal;
                break;
            }
            Ok(super::foreground::ChildState::Stopped(signal)) => {
                stopped_signal = Some(signal);
                if !super::foreground::terminate_process_group(&mut child, process_group) {
                    supervision_failed = true;
                }
                break;
            }
            Ok(super::foreground::ChildState::Running) => {}
            Err(_) => {
                supervision_failed = true;
                break;
            }
        }

        if cancellation.is_cancelled() {
            let started = cancellation_started.get_or_insert_with(|| {
                signal_group(process_group, libc::SIGTERM);
                Instant::now()
            });
            if cancellation_kill_sent.is_none() && started.elapsed() >= TERMINATION_GRACE {
                signal_group(process_group, libc::SIGKILL);
                cancellation_kill_sent = Some(Instant::now());
            }
            if cancellation_kill_sent.is_some_and(|sent| sent.elapsed() >= KILL_GRACE) {
                supervision_failed = true;
                break;
            }
        }
        if let Err(error) = pipes.poll(POLL_INTERVAL) {
            if error.kind() != std::io::ErrorKind::Interrupted {
                capture_failed = true;
            }
        }
    }

    if leader_signal == Some(libc::SIGINT) {
        cancellation.cancel();
    }

    let had_residual_group = process_group_exists(process_group);
    if stopped_signal.is_none() && had_residual_group {
        signal_group(process_group, libc::SIGTERM);
        if !wait_for_group_exit_while_draining(
            process_group,
            TERMINATION_GRACE,
            &mut pipes,
            &mut output,
            &mut capture_failed,
        ) {
            signal_group(process_group, libc::SIGKILL);
            if !wait_for_group_exit_while_draining(
                process_group,
                KILL_GRACE,
                &mut pipes,
                &mut output,
                &mut capture_failed,
            ) {
                supervision_failed = true;
            }
        }
    }

    let drain_deadline = Instant::now() + PIPE_DRAIN_GRACE;
    while pipes.is_open() && Instant::now() < drain_deadline {
        match pipes.drain(&mut output, true) {
            Ok(presentation_failed) => capture_failed |= presentation_failed,
            Err(_) => capture_failed = true,
        }
        if pipes.is_open() {
            let _ = pipes.poll(POLL_INTERVAL);
        }
    }
    if pipes.is_open() {
        capture_failed = true;
        pipes.close();
    }

    cancellation.finish(process_group);
    let cancelled = cancellation.is_cancelled();
    let termination = if supervision_failed {
        CommandTermination::SupervisionFailed
    } else if cancellation.is_interrupted() {
        CommandTermination::Interrupted
    } else if stopped_signal.is_some() {
        CommandTermination::StoppedTerminated
    } else if had_residual_group && !cancelled {
        CommandTermination::BackgroundTerminated
    } else {
        CommandTermination::Exited
    };
    let exit_code = if cancelled {
        130
    } else if supervision_failed {
        125
    } else if let Some(signal) = stopped_signal {
        128 + signal
    } else {
        leader_exit_code.unwrap_or(125)
    };
    Ok(CapturedExecution {
        output: sanitize_terminal_transcript(&output.render()),
        total_output_bytes: output.total,
        exit_code,
        termination,
        output_evidence: output_evidence(
            &output,
            termination,
            supervision_failed || capture_failed,
        ),
    })
}

#[cfg(unix)]
fn wait_for_group_exit_while_draining(
    process_group: u32,
    grace: Duration,
    pipes: &mut PipeSet,
    output: &mut BoundedOutput,
    capture_failed: &mut bool,
) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        match pipes.drain(output, true) {
            Ok(presentation_failed) => *capture_failed |= presentation_failed,
            Err(_) => *capture_failed = true,
        }
        if !process_group_exists(process_group) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = pipes.poll(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn wait_unix(
    child: Child,
    cancellation: &CancellationToken,
    hard_limit: usize,
) -> AppResult<CapturedExecution> {
    wait_unix_with_options(child, cancellation, Some(hard_limit), false)
}

#[cfg(unix)]
fn wait_unix_with_options(
    mut child: Child,
    cancellation: &CancellationToken,
    hard_limit: Option<usize>,
    live: bool,
) -> AppResult<CapturedExecution> {
    use std::os::fd::AsRawFd;

    let process_group = child.id();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        abort_supervision(&mut child, process_group, cancellation);
        return Err(AppError::internal(
            "Agent command stdout and stderr must be piped",
        ));
    };
    if let Err(error) =
        set_nonblocking(stdout.as_raw_fd()).and_then(|()| set_nonblocking(stderr.as_raw_fd()))
    {
        abort_supervision(&mut child, process_group, cancellation);
        return Err(AppError::io(format!(
            "cannot configure nonblocking command pipes: {error}"
        )));
    }

    let mut pipes = PipeSet::new(stdout, stderr);
    let mut output = BoundedOutput::new(AGENT_COMMAND_FEEDBACK_LIMIT, TAIL_LIMIT);
    let mut leader_status = None;
    let mut leader_exited_at = None;
    let mut group_gone_at = None;
    let mut stop = None;
    let mut supervision_failed = false;
    let mut capture_failed = false;

    loop {
        match pipes.drain(&mut output, live) {
            Ok(presentation_failed) => capture_failed |= presentation_failed,
            Err(_) if live => capture_failed = true,
            Err(_) => supervision_failed = true,
        }

        if cancellation.is_cancelled() {
            request_stop(
                &mut stop,
                StopReason::Cancelled,
                process_group,
                libc::SIGINT,
            );
        } else if hard_limit.is_some_and(|limit| output.total > limit) {
            request_stop(
                &mut stop,
                StopReason::OutputLimit,
                process_group,
                libc::SIGTERM,
            );
        }

        if leader_status.is_none() {
            match child.try_wait() {
                Ok(status) => {
                    leader_status = status;
                    if leader_status.is_some() {
                        leader_exited_at = Some(Instant::now());
                    }
                }
                Err(_) => supervision_failed = true,
            }
        }

        let group_alive = process_group_exists(process_group);
        if leader_status.is_some() && group_alive && stop.is_none() {
            request_stop(
                &mut stop,
                StopReason::Background,
                process_group,
                libc::SIGTERM,
            );
        }

        if leader_status.is_some() && !group_alive {
            group_gone_at.get_or_insert_with(Instant::now);
        } else {
            group_gone_at = None;
        }

        if let Some(active_stop) = stop.as_mut() {
            if active_stop.kill_sent_at.is_none()
                && active_stop.started.elapsed() >= TERMINATION_GRACE
                && group_alive
            {
                // Leader 可能已退出，所以这里不能再以 try_wait 状态作为强杀条件。
                signal_group(process_group, libc::SIGKILL);
                active_stop.kill_sent_at = Some(Instant::now());
            }
            if active_stop
                .kill_sent_at
                .is_some_and(|sent| sent.elapsed() >= KILL_GRACE && group_alive)
            {
                supervision_failed = true;
            }
        }

        let pipes_drained = !pipes.is_open();
        let fully_reaped = leader_status.is_some() && !group_alive;
        if fully_reaped && pipes_drained {
            break;
        }

        // 原 PGID 已消失但管道仍不 EOF，通常意味着后代已 setsid 逃离。
        // 无 cgroup/subreaper 时无法可靠定位它，只能有界关闭本端并报告失败。
        if group_gone_at.is_some_and(|gone| pipes.is_open() && gone.elapsed() >= PIPE_DRAIN_GRACE) {
            supervision_failed = true;
        }

        // 理论上 leader 退出却一直无法观察到 PGID 状态的情况也必须有界。
        if leader_exited_at.is_some_and(|exited| {
            stop.is_some() && exited.elapsed() >= TERMINATION_GRACE + KILL_GRACE + PIPE_DRAIN_GRACE
        }) {
            supervision_failed = true;
        }

        if supervision_failed {
            signal_group(process_group, libc::SIGKILL);
            pipes.close();
            break;
        }

        if let Err(error) = pipes.poll(POLL_INTERVAL) {
            if error.kind() != std::io::ErrorKind::Interrupted {
                supervision_failed = true;
            }
        }
    }

    // 失败路径也做一次非阻塞 reap；不再为不可中断的子进程无界等待。
    if leader_status.is_none() {
        leader_status = child.try_wait().ok().flatten();
    }
    cancellation.finish(process_group);

    let cancelled = cancellation.is_cancelled();
    let termination = if supervision_failed {
        CommandTermination::SupervisionFailed
    } else {
        match stop.map(|state| state.reason) {
            Some(StopReason::OutputLimit) => CommandTermination::OutputLimit,
            Some(StopReason::Background) => CommandTermination::BackgroundTerminated,
            Some(StopReason::Cancelled) | None => CommandTermination::Exited,
        }
    };
    let exit_code = if cancelled {
        130
    } else if supervision_failed {
        125
    } else {
        leader_status.map(exit_code).unwrap_or(125)
    };

    Ok(CapturedExecution {
        output: if live {
            sanitize_terminal_transcript(&output.render())
        } else {
            output.render()
        },
        total_output_bytes: output.total,
        exit_code,
        termination,
        output_evidence: output_evidence(
            &output,
            termination,
            supervision_failed || capture_failed,
        ),
    })
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    Background,
    OutputLimit,
    Cancelled,
}

#[cfg(unix)]
struct StopState {
    reason: StopReason,
    started: Instant,
    kill_sent_at: Option<Instant>,
}

#[cfg(unix)]
fn request_stop(stop: &mut Option<StopState>, reason: StopReason, process_group: u32, signal: i32) {
    let priority = |reason| match reason {
        StopReason::Background => 0,
        StopReason::OutputLimit => 1,
        StopReason::Cancelled => 2,
    };
    match stop {
        Some(active) if priority(reason) > priority(active.reason) => {
            active.reason = reason;
            signal_group(process_group, signal);
        }
        Some(_) => {}
        None => {
            signal_group(process_group, signal);
            *stop = Some(StopState {
                reason,
                started: Instant::now(),
                kill_sent_at: None,
            });
        }
    }
}

#[cfg(unix)]
struct PipeSet {
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
}

#[cfg(unix)]
impl PipeSet {
    fn new(stdout: std::process::ChildStdout, stderr: std::process::ChildStderr) -> Self {
        Self {
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }

    fn is_open(&self) -> bool {
        self.stdout.is_some() || self.stderr.is_some()
    }

    fn close(&mut self) {
        self.stdout = None;
        self.stderr = None;
    }

    fn drain(&mut self, output: &mut BoundedOutput, live: bool) -> std::io::Result<bool> {
        let stdout_failed = drain_pipe_to(
            &mut self.stdout,
            output,
            live.then_some(libc::STDOUT_FILENO),
        )?;
        let stderr_failed = drain_pipe_to(
            &mut self.stderr,
            output,
            live.then_some(libc::STDERR_FILENO),
        )?;
        Ok(stdout_failed || stderr_failed)
    }

    fn poll(&self, timeout: Duration) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;

        let mut descriptors = Vec::with_capacity(2);
        if let Some(stdout) = &self.stdout {
            descriptors.push(libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if let Some(stderr) = &self.stderr {
            descriptors.push(libc::pollfd {
                fd: stderr.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if descriptors.is_empty() {
            std::thread::sleep(timeout);
            return Ok(());
        }
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: descriptors 指向有效、可写的 pollfd 数组，长度与 nfds 一致；
        // poll 只在调用期间修改 revents，不保留指针。
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn drain_pipe<R: Read>(pipe: &mut Option<R>, output: &mut BoundedOutput) -> std::io::Result<()> {
    drain_pipe_to(pipe, output, None).map(|_| ())
}

#[cfg(unix)]
fn drain_pipe_to<R: Read>(
    pipe: &mut Option<R>,
    output: &mut BoundedOutput,
    terminal_fd: Option<libc::c_int>,
) -> std::io::Result<bool> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(false);
    };
    let mut buffer = [0_u8; READ_CHUNK];
    let mut drained = 0;
    let mut close = false;
    let mut presentation_failed = false;
    let result = loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                close = true;
                break Ok(());
            }
            Ok(size) => {
                output.push(&buffer[..size]);
                if terminal_fd.is_some_and(|fd| write_terminal(fd, &buffer[..size]).is_err()) {
                    presentation_failed = true;
                }
                drained += size;
                if drained >= READ_BUDGET_PER_PIPE {
                    break Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                close = true;
                break Err(error);
            }
        }
    };
    if close {
        *pipe = None;
    }
    result.map(|()| presentation_failed)
}

#[cfg(unix)]
fn write_terminal(fd: libc::c_int, mut bytes: &[u8]) -> std::io::Result<()> {
    let mut blocked = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigset_t 由 libc 初始化；write 只读取传入切片；信号掩码在返回前恢复。
    unsafe {
        libc::sigemptyset(blocked.as_mut_ptr());
        libc::sigaddset(blocked.as_mut_ptr(), libc::SIGTTOU);
        let blocked = blocked.assume_init();
        let block_error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, previous.as_mut_ptr());
        if block_error != 0 {
            return Err(std::io::Error::from_raw_os_error(block_error));
        }
        let previous = previous.assume_init();
        let result = (|| {
            while !bytes.is_empty() {
                let written = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                if written > 0 {
                    bytes = &bytes[written as usize..];
                    continue;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            Ok(())
        })();
        let restore_error =
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if restore_error != 0 {
            Err(std::io::Error::from_raw_os_error(restore_error))
        } else {
            result
        }
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl 只读取/修改由 ChildStdout/ChildStderr 持有的有效 fd 标志。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd 仍由调用者持有，F_SETFL 只增加 O_NONBLOCK。
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn process_group_exists(process_group: u32) -> bool {
    // SAFETY: signal 0 不发送信号，只检查以负 PID 表示的进程组是否可见。
    if unsafe { libc::kill(-(process_group as i32), 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn wait_for_group_exit(process_group: u32, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if !process_group_exists(process_group) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn abort_supervision(child: &mut Child, process_group: u32, cancellation: &CancellationToken) {
    signal_group(process_group, libc::SIGKILL);
    let deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < deadline {
        let leader_exited = child.try_wait().ok().flatten().is_some();
        if leader_exited && !process_group_exists(process_group) {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    cancellation.finish(process_group);
}

#[cfg(not(unix))]
fn wait_portable(
    mut child: Child,
    cancellation: &CancellationToken,
    hard_limit: usize,
) -> AppResult<CapturedExecution> {
    use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
    use std::thread;

    const PIPE_QUEUE_DEPTH: usize = 16;

    let process_group = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("Agent command stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::internal("Agent command stderr was not piped"))?;
    let (sender, receiver) = mpsc::sync_channel(PIPE_QUEUE_DEPTH);
    let spawn_reader = |mut reader: Box<dyn Read + Send>, sender: SyncSender<Vec<u8>>| {
        thread::spawn(move || {
            let mut buffer = [0_u8; READ_CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(size) if sender.send(buffer[..size].to_vec()).is_err() => return,
                    Ok(_) => {}
                }
            }
        })
    };
    let stdout_reader = spawn_reader(Box::new(stdout), sender.clone());
    let stderr_reader = spawn_reader(Box::new(stderr), sender);
    let mut output = BoundedOutput::new(AGENT_COMMAND_FEEDBACK_LIMIT, TAIL_LIMIT);
    let mut channel_open = true;
    let mut status = None;
    let mut output_limited = false;

    while status.is_none() || channel_open {
        if channel_open {
            match receiver.recv_timeout(POLL_INTERVAL) {
                Ok(chunk) => output.push(&chunk),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => channel_open = false,
            }
        }
        if output.total > hard_limit && !output_limited {
            output_limited = true;
            let _ = child.kill();
        }
        if cancellation.is_cancelled() {
            let _ = child.kill();
        }
        if status.is_none() {
            status = child
                .try_wait()
                .map_err(|error| AppError::io(format!("等待命令失败: {error}")))?;
        }
    }
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    cancellation.finish(process_group);
    let status = status.ok_or_else(|| AppError::internal("Agent command lost its exit status"))?;
    Ok(CapturedExecution {
        output: output.render(),
        total_output_bytes: output.total,
        exit_code: if cancellation.is_cancelled() {
            130
        } else {
            exit_code(status)
        },
        termination: if output_limited {
            CommandTermination::OutputLimit
        } else {
            CommandTermination::Exited
        },
        output_evidence: if output_limited || output.total > AGENT_COMMAND_FEEDBACK_LIMIT {
            OutputEvidence::Truncated
        } else {
            OutputEvidence::Complete
        },
    })
}

#[cfg(unix)]
fn signal_group(process_group: u32, signal: i32) {
    // SAFETY: Agent 子进程在启动时被放入以其 PID 命名的独立进程组。负 PID 按
    // POSIX 语义向该进程组发送信号；调用不解引用指针，失败只返回错误码。
    unsafe {
        libc::kill(-(process_group as i32), signal);
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| 128 + signal).unwrap_or(1)
    }
    #[cfg(not(unix))]
    1
}

struct BoundedOutput {
    head: Vec<u8>,
    tail: Vec<u8>,
    head_limit: usize,
    tail_limit: usize,
    total: usize,
}

impl BoundedOutput {
    fn new(feedback_limit: usize, tail_limit: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            head_limit: feedback_limit.saturating_sub(tail_limit),
            tail_limit,
            total: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head_needed = self.head_limit.saturating_sub(self.head.len());
        let split = head_needed.min(bytes.len());
        self.head.extend_from_slice(&bytes[..split]);
        self.tail.extend_from_slice(&bytes[split..]);
        if self.tail.len() > self.tail_limit {
            let excess = self.tail.len() - self.tail_limit;
            self.tail.drain(..excess);
        }
    }

    fn render(&self) -> String {
        let mut bytes = self.head.clone();
        if self.total > self.head.len() + self.tail.len() {
            let marker = format!("\n[zhsh: 命令输出已截断；共产生 {} 字节]\n", self.total);
            let allowed_head = AGENT_COMMAND_FEEDBACK_LIMIT
                .saturating_sub(self.tail.len())
                .saturating_sub(marker.len());
            bytes.truncate(allowed_head);
            bytes.extend_from_slice(marker.as_bytes());
        }
        bytes.extend_from_slice(&self.tail);
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        truncate_utf8(&mut text, AGENT_COMMAND_FEEDBACK_LIMIT);
        text
    }
}

fn output_evidence(
    output: &BoundedOutput,
    termination: CommandTermination,
    capture_failed: bool,
) -> OutputEvidence {
    if capture_failed {
        OutputEvidence::CaptureFailed
    } else if output.total > AGENT_COMMAND_FEEDBACK_LIMIT {
        OutputEvidence::Truncated
    } else if termination == CommandTermination::Exited {
        OutputEvidence::Complete
    } else {
        OutputEvidence::Partial
    }
}

/// 把实时终端副本转换为不会移动光标或伪造宿主事件的稳定文本。
fn sanitize_terminal_transcript(input: &str) -> String {
    let mut plain = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            match characters.peek().copied() {
                Some('[') => {
                    characters.next();
                    for next in characters.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    characters.next();
                    let mut escaped = false;
                    for next in characters.by_ref() {
                        if next == '\u{7}' || (escaped && next == '\\') {
                            break;
                        }
                        escaped = next == '\u{1b}';
                    }
                }
                Some(_) => {
                    characters.next();
                }
                None => {}
            }
            continue;
        }
        match character {
            '\r' if characters.peek() == Some(&'\n') => {}
            '\r' => plain.push('\n'),
            '\u{8}' => {
                if !plain.ends_with('\n') {
                    plain.pop();
                }
            }
            '\n' | '\t' => plain.push(character),
            value if !value.is_control() => plain.push(value),
            _ => {}
        }
    }
    plain
}

fn truncate_utf8(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::process::{Command, Stdio};

    #[cfg(unix)]
    fn supervise(script: &str, cancellation: &CancellationToken) -> CapturedExecution {
        let mut command = Command::new("bash");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cancellation
            .spawn(&mut command)
            .unwrap()
            .expect("test command should start");
        wait(child, cancellation, 1024 * 1024).unwrap()
    }

    #[cfg(unix)]
    fn supervise_interactive(
        script: &str,
        cancellation: &CancellationToken,
        environment: Option<(&str, &std::path::Path)>,
    ) -> CapturedExecution {
        let mut command = Command::new("bash");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some((name, value)) = environment {
            command.env(name, value);
        }
        let child = cancellation
            .spawn(&mut command)
            .unwrap()
            .expect("test command should start");
        wait_interactive(child, cancellation)
    }

    #[cfg(unix)]
    fn output_pid(output: &str, label: &str) -> i32 {
        output
            .lines()
            .find_map(|line| line.strip_prefix(label))
            .unwrap_or_else(|| panic!("missing {label:?} in {output:?}"))
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn terminal_transcript_removes_display_control_sequences() {
        assert_eq!(
            sanitize_terminal_transcript("\u{1b}[31mvalue\u{1b}[0m\rnext\u{8}!\n"),
            "value\nnex!\n"
        );
    }

    #[cfg(unix)]
    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal 0 does not deliver a signal and only probes the numeric PID.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(unix)]
    fn wait_until_gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !process_exists(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        !process_exists(pid)
    }

    #[test]
    fn bounded_output_preserves_head_and_tail() {
        let mut output = BoundedOutput::new(32, 8);
        output.push(&[b'a'; 24]);
        output.push(&[b'b'; 40]);

        let rendered = output.render();
        assert!(rendered.starts_with("aaaaaaaa"));
        assert!(rendered.ends_with("bbbbbbbb"));
        assert!(rendered.contains("输出已截断"));
    }

    #[cfg(unix)]
    #[test]
    fn interactive_leader_exit_terminates_residual_group_before_unregistering() {
        let cancellation = CancellationToken::default();
        let pid_file = std::env::temp_dir().join(format!(
            "zhsh-interactive-residual-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let started = Instant::now();
        let result = supervise_interactive(
            "(trap '' TERM; printf '%s' \"$BASHPID\" >\"$ZHSH_TEST_PID_FILE\"; while :; do sleep 30; done) & while [[ ! -s \"$ZHSH_TEST_PID_FILE\" ]]; do :; done",
            &cancellation,
            Some(("ZHSH_TEST_PID_FILE", &pid_file)),
        );
        let child: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = wait_until_gone(child);
        if !gone {
            // SAFETY: child is the PID written by this test fixture.
            unsafe {
                libc::kill(child, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_file(pid_file);

        assert_eq!(result.termination, CommandTermination::BackgroundTerminated);
        assert_eq!(result.exit_code, 0);
        assert!(started.elapsed() >= TERMINATION_GRACE);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(gone, "interactive background child {child} survived");
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn interactive_cancellation_is_bounded_and_has_status_130_priority() {
        let cancellation = std::sync::Arc::new(CancellationToken::default());
        let cancelling = std::sync::Arc::clone(&cancellation);
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancelling.cancel();
        });
        let started = Instant::now();
        let result = supervise_interactive(
            "trap '' INT TERM; while :; do sleep 30; done",
            &cancellation,
            None,
        );
        cancel_thread.join().unwrap();

        assert_eq!(result.termination, CommandTermination::Exited);
        assert_eq!(result.exit_code, 130);
        assert!(started.elapsed() >= TERMINATION_GRACE);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn leader_exit_terminates_descendant_that_inherits_pipes() {
        let cancellation = CancellationToken::default();
        let started = Instant::now();
        let result = supervise(
            "sleep 30 & child=$!; printf 'child:%s\\n' \"$child\"",
            &cancellation,
        );
        let child = output_pid(&result.output, "child:");

        assert_eq!(result.termination, CommandTermination::BackgroundTerminated);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(wait_until_gone(child), "background child {child} survived");
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn leader_exit_checks_group_even_when_descendant_closes_pipes() {
        let cancellation = CancellationToken::default();
        let result = supervise(
            "sleep 30 >/dev/null 2>&1 & child=$!; printf 'child:%s\\n' \"$child\"",
            &cancellation,
        );
        let child = output_pid(&result.output, "child:");

        assert_eq!(result.termination, CommandTermination::BackgroundTerminated);
        assert!(wait_until_gone(child), "background child {child} survived");
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn residual_group_that_ignores_term_is_killed_after_grace() {
        let cancellation = CancellationToken::default();
        let started = Instant::now();
        let result = supervise(
            "(trap '' TERM; printf 'child:%s\\n' \"$BASHPID\"; while :; do sleep 30; done) & sleep 0.1",
            &cancellation,
        );
        let child = output_pid(&result.output, "child:");

        assert_eq!(result.termination, CommandTermination::BackgroundTerminated);
        assert!(started.elapsed() >= TERMINATION_GRACE);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            wait_until_gone(child),
            "TERM-ignoring child {child} survived"
        );
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_registration_survives_leader_exit_until_group_cleanup() {
        let cancellation = std::sync::Arc::new(CancellationToken::default());
        let runner_cancellation = std::sync::Arc::clone(&cancellation);
        let (sender, receiver) = std::sync::mpsc::channel();
        let runner = std::thread::spawn(move || {
            let result = supervise(
                "(trap '' TERM; while :; do sleep 30; done) & sleep 0.1",
                &runner_cancellation,
            );
            sender.send(result).unwrap();
        });

        let spawn_deadline = Instant::now() + Duration::from_secs(2);
        while cancellation.active_process_group().is_none() {
            assert!(Instant::now() < spawn_deadline, "fixture did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            cancellation.active_process_group().is_some(),
            "leader exit cleared cancellation registration before residual cleanup"
        );

        let result = receiver.recv_timeout(Duration::from_secs(3)).unwrap();
        runner.join().unwrap();
        assert_eq!(result.termination, CommandTermination::BackgroundTerminated);
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn descendant_output_after_leader_exit_still_hits_hard_limit() {
        let cancellation = CancellationToken::default();
        let result = supervise(
            // Keep the leader alive until the descendant has installed its TERM trap, then have
            // the descendant wait for that exact leader to be reaped before emitting the payload.
            // Without this handshake the supervisor can legitimately deliver TERM before the
            // trap exists, making the fixture race between Background and OutputLimit.
            "parent=$BASHPID; (trap '' TERM; while kill -0 \"$parent\" 2>/dev/null; do :; done; printf '%1100000s' x; while :; do :; done) & sleep 0.05",
            &cancellation,
        );

        assert_eq!(result.termination, CommandTermination::OutputLimit);
        assert!(result.total_output_bytes > 1024 * 1024);
        assert!(result.output.len() <= AGENT_COMMAND_FEEDBACK_LIMIT);
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_wins_races_and_returns_status_130() {
        let cancellation = std::sync::Arc::new(CancellationToken::default());
        let cancelling = std::sync::Arc::clone(&cancellation);
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancelling.cancel();
        });
        let result = supervise(
            "trap '' INT TERM; while :; do printf 1234567890; done",
            &cancellation,
        );
        cancel_thread.join().unwrap();

        assert_eq!(result.exit_code, 130);
        assert_eq!(result.termination, CommandTermination::Exited);
        assert_eq!(cancellation.active_process_group(), None);
    }

    #[cfg(unix)]
    #[test]
    fn escaped_pipe_holder_returns_bounded_supervision_failure() {
        if Command::new("setsid")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return;
        }

        let cancellation = CancellationToken::default();
        let started = Instant::now();
        let result = supervise(
            "setsid sh -c 'printf \"escaped:%s\\n\" \"$$\"; exec sleep 30' & sleep 0.1",
            &cancellation,
        );
        let escaped = output_pid(&result.output, "escaped:");

        // This process deliberately escaped the supervised PGID. Clean it up even if assertions
        // below change, then verify the supervisor did not wait for its natural 30 second exit.
        // SAFETY: escaped is parsed from the fixture's own PID report.
        unsafe {
            libc::kill(escaped, libc::SIGKILL);
        }
        let gone = wait_until_gone(escaped);

        assert_eq!(result.termination, CommandTermination::SupervisionFailed);
        assert_eq!(result.exit_code, 125);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(gone, "escaped fixture {escaped} survived fallback cleanup");
        assert_eq!(cancellation.active_process_group(), None);
    }
}
