//! Native instance-local job supervision. Only the worker reaps registered children.
mod model;
mod pid;
pub(crate) use pid::PidBinding;
mod signals;
pub(super) use signals::interrupt;
mod spawn;
mod terminal;
use super::{CapturedExecution, CommandTermination, OutputEvidence};
use crate::common::CancellationToken;
pub(crate) use model::{Binding, Options, Snapshot, Status};
use model::{Job, Member};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ffi::OsString,
    io::{self, Write},
    os::fd::AsRawFd,
    path::Path,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

pub(super) struct SpawnRequest<'a> {
    pub path: &'a Path,
    pub program: &'a str,
    pub args: &'a [OsString],
    pub cwd: &'a Path,
    pub env: &'a HashMap<String, String>,
}

struct State {
    jobs: BTreeMap<u64, Job>,
    next: u64,
    event_order: u64,
    current: Option<u64>,
    previous: Option<u64>,
    options: Options,
    interactive: bool,
    login: bool,
    last_async_pid: Option<i32>,
    stop: bool,
    notifications: BTreeMap<u64, String>,
    failure: Option<String>,
}
type NoticeSink = Arc<dyn Fn(String) + Send + Sync>;
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    notice_sink: Mutex<Option<NoticeSink>>,
    handle_hup: std::sync::atomic::AtomicBool,
}
pub(crate) struct JobRuntime {
    shared: Arc<Shared>,
    worker: Option<thread::JoinHandle<()>>,
    terminal: Arc<Mutex<Option<terminal::Terminal>>>,
}
impl JobRuntime {
    pub fn await_shell_foreground(&self) -> io::Result<()> {
        if let Some(t) = self.terminal.lock().unwrap().as_ref() {
            t.await_foreground()?;
        }
        Ok(())
    }
    pub fn new(interactive: bool) -> Self {
        let terminal = Arc::new(Mutex::new(terminal::Terminal::open(interactive)));
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                jobs: BTreeMap::new(),
                next: 1,
                event_order: 0,
                current: None,
                previous: None,
                options: Options {
                    monitor: interactive && terminal.lock().unwrap().is_some(),
                    notify: false,
                    checkjobs: false,
                    huponexit: false,
                },
                interactive,
                login: false,
                last_async_pid: None,
                stop: false,
                notifications: BTreeMap::new(),
                failure: None,
            }),
            changed: Condvar::new(),
            notice_sink: Mutex::new(None),
            handle_hup: std::sync::atomic::AtomicBool::new(false),
        });
        // Retained results are bounded without imposing a limit on live jobs.
        let result_limit = unsafe { libc::sysconf(libc::_SC_CHILD_MAX) }.max(4096) as usize;
        let s = Arc::clone(&shared);
        let (output_tx, output_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
        thread::spawn(move || {
            for bytes in output_rx {
                let _ = io::stdout().write_all(&bytes);
                let _ = io::stdout().flush();
            }
        });
        let worker_terminal = Arc::clone(&terminal);
        let worker = thread::spawn(move || {
            let mut output_pending = VecDeque::<Vec<u8>>::new();
            loop {
                while let Some(bytes) = output_pending.pop_front() {
                    match output_tx.try_send(bytes) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(bytes)) => {
                            output_pending.push_front(bytes);
                            break;
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
                let mut state = s.state.lock().unwrap();
                if state.stop {
                    break;
                }
                if s.handle_hup.load(std::sync::atomic::Ordering::Relaxed) && signals::take_hup() {
                    for j in state
                        .jobs
                        .values()
                        .filter(|j| j.visible && !j.no_hup && !j.status().done())
                    {
                        let _ = signal(j, libc::SIGHUP);
                        if matches!(j.status(), Status::Stopped(_)) {
                            let _ = signal(j, libc::SIGCONT);
                        }
                    }
                    if let Some(t) = worker_terminal.lock().unwrap().as_mut() {
                        let _ = t.restore();
                    }
                    // The process received HUP; no task may continue after forwarding it.
                    unsafe {
                        libc::_exit(128 + libc::SIGHUP);
                    }
                }
                let mut changes = Vec::new();
                let mut failure = None;
                for job in state.jobs.values_mut() {
                    let before = job.status();
                    for member in &mut job.members {
                        if member.status.done() {
                            continue;
                        }
                        loop {
                            let mut raw = 0;
                            // SAFETY: this worker alone waits for these registered direct children.
                            let r = unsafe {
                                libc::waitpid(
                                    member.pid,
                                    &mut raw,
                                    libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
                                )
                            };
                            if r == 0 {
                                break;
                            }
                            if r < 0 {
                                let e = io::Error::last_os_error();
                                if e.raw_os_error() == Some(libc::EINTR) {
                                    continue;
                                }
                                failure = Some(format!("waitpid {}: {e}", member.pid));
                                break;
                            }
                            member.status = if libc::WIFEXITED(raw) {
                                Status::Exited(libc::WEXITSTATUS(raw))
                            } else if libc::WIFSIGNALED(raw) {
                                Status::Exited(128 + libc::WTERMSIG(raw))
                            } else if libc::WIFSTOPPED(raw) {
                                Status::Stopped(libc::WSTOPSIG(raw))
                            } else {
                                Status::Running
                            };
                            if member.status.done() {
                                break;
                            }
                        }
                    }
                    let mut failed = false;
                    job.streams.retain(|fd| {
                        let mut buf = [0u8; 8192];
                        for _ in 0..16 {
                            if (job.background || job.settings.is_some() || job.forward)
                                && output_pending.len() >= 64
                            {
                                return true;
                            }
                            let n = unsafe {
                                libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
                            };
                            if n == 0 {
                                return false;
                            }
                            if n < 0 {
                                let e = io::Error::last_os_error();
                                if e.kind() == io::ErrorKind::Interrupted {
                                    continue;
                                }
                                if e.kind() != io::ErrorKind::WouldBlock {
                                    failed = true;
                                    return false;
                                }
                                return true;
                            }
                            let n = n as usize;
                            job.total = job.total.saturating_add(n);
                            let keep = n.min(
                                super::executor::AGENT_COMMAND_OUTPUT_LIMIT
                                    .saturating_sub(job.bytes.len()),
                            );
                            job.bytes.extend_from_slice(&buf[..keep]);
                            if job.background || job.settings.is_some() || job.forward {
                                output_pending.push_back(buf[..n].to_vec());
                            }
                        }
                        true
                    });
                    job.io_failed |= failed;
                    if before != job.status() {
                        job.changed = true;
                        changes.push((job.key, job.status(), job.visible, job.background));
                    }
                }
                if failure.is_some() {
                    state.failure = failure;
                }
                for (key, status, visible, background) in changes {
                    state.event_order = state.event_order.saturating_add(1);
                    if status.done() {
                        let order = state.event_order;
                        state.jobs.get_mut(&key).unwrap().completion_order = order;
                    }
                    if matches!(status, Status::Stopped(_)) {
                        state.previous = state.current.filter(|k| *k != key);
                        state.current = Some(key);
                    }
                    if visible && background {
                        let notice = job_notice(&state, key);
                        state.notifications.insert(key, notice);
                    }
                }
                // Completed, invisible records are a bounded PID-result cache, not signal targets.
                let completed = state
                    .jobs
                    .iter()
                    .filter(|(_, j)| {
                        !j.starting && !j.visible && j.status().done() && j.streams.is_empty()
                    })
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>();
                for key in completed
                    .iter()
                    .take(completed.len().saturating_sub(result_limit))
                {
                    state.jobs.remove(key);
                }
                s.changed.notify_all();
                let sink = s.notice_sink.lock().unwrap().clone();
                let notices = if state.options.notify && sink.is_some() {
                    take_notifications(&mut state)
                } else {
                    Vec::new()
                };
                drop(state);
                if let Some(sink) = sink {
                    for notice in notices {
                        sink(notice);
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            shared,
            worker: Some(worker),
            terminal,
        }
    }
    pub fn failure(&self) -> Option<String> {
        self.shared.state.lock().unwrap().failure.clone()
    }
    pub fn install_signals(&self) -> io::Result<()> {
        signals::install()?;
        self.shared
            .handle_hup
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    pub fn set_notice_sink(&self, sink: NoticeSink) {
        *self.shared.notice_sink.lock().unwrap() = Some(sink);
    }
    pub fn last_async_pid(&self) -> Option<i32> {
        self.shared.state.lock().unwrap().last_async_pid
    }
    pub fn login(&self) -> bool {
        self.shared.state.lock().unwrap().login
    }
    pub fn options(&self) -> Options {
        self.shared.state.lock().unwrap().options
    }
    pub fn set_option(&self, name: &str, value: bool) -> Result<(), String> {
        let mut s = self.shared.state.lock().unwrap();
        match name {
            "monitor" => s.options.monitor = value,
            "notify" => s.options.notify = value,
            "checkjobs" => s.options.checkjobs = value,
            "huponexit" => s.options.huponexit = value,
            _ => return Err(format!("unsupported option: {name}")),
        }
        Ok(())
    }
    pub fn notifications(&self) -> Vec<String> {
        take_notifications(&mut self.shared.state.lock().unwrap())
    }
    pub fn snapshots(&self) -> Vec<Snapshot> {
        let s = self.shared.state.lock().unwrap();
        s.jobs
            .values()
            .filter(|j| j.visible)
            .map(|j| Snapshot {
                binding: j.binding(),
                pids: j.members.iter().map(|m| m.pid).collect(),
                command: j.command.clone(),
                status: j.status(),
                mark: if s.current == Some(j.key) {
                    '+'
                } else if s.previous == Some(j.key) {
                    '-'
                } else {
                    ' '
                },
                changed: j.changed,
            })
            .collect()
    }
    pub fn acknowledge(&self, keys: &[u64]) {
        let mut s = self.shared.state.lock().unwrap();
        for k in keys {
            if let Some(j) = s.jobs.get_mut(k) {
                j.changed = false;
                if j.status().done() {
                    j.visible = false;
                }
            }
        }
        reselect(&mut s);
    }
    pub fn resolve(&self, spec: &str) -> Result<Binding, String> {
        let s = self.shared.state.lock().unwrap();
        resolve(&s, spec)
    }
    pub fn waitable(&self, b: &Binding) -> bool {
        self.shared
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&b.key)
            .is_some_and(|j| j.waitable && j.binding() == *b)
    }
    pub fn valid(&self, b: &Binding) -> bool {
        let s = self.shared.state.lock().unwrap();
        s.jobs
            .get(&b.key)
            .is_some_and(|j| j.visible && j.binding() == *b && !j.status().done())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        &self,
        path: &Path,
        program: &str,
        args: &[OsString],
        cwd: &Path,
        env: &HashMap<String, String>,
        display: &str,
        capture: bool,
        input: bool,
        background: bool,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<Binding> {
        self.launch_members_with_cancel(
            &[SpawnRequest {
                path,
                program,
                args,
                cwd,
                env,
            }],
            display,
            capture,
            input,
            background,
            cancel,
        )
    }
    #[cfg(test)]
    pub fn launch_members(
        &self,
        requests: &[SpawnRequest<'_>],
        display: &str,
        capture: bool,
        input: bool,
        background: bool,
    ) -> io::Result<Binding> {
        self.launch_members_with_cancel(requests, display, capture, input, background, None)
    }
    fn launch_members_with_cancel(
        &self,
        requests: &[SpawnRequest<'_>],
        display: &str,
        capture: bool,
        input: bool,
        background: bool,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<Binding> {
        if requests.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty job"));
        }
        let mut s = self.shared.state.lock().unwrap();
        let key = s.next;
        s.next = s
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("job identity exhausted"))?;
        let id = (1..u32::MAX)
            .find(|id| !s.jobs.values().any(|j| j.visible && j.id == *id))
            .ok_or_else(|| io::Error::other("job numbers exhausted"))?;
        let dedicated = s.options.monitor;
        let mut pending = Vec::new();
        let mut outcome = Ok(());
        for r in requests {
            let group = dedicated.then(|| pending.first().map_or(0, |p: &spawn::Pending| p.pid));
            let start = || {
                spawn::start(
                    r.path,
                    r.program,
                    r.args,
                    r.cwd,
                    r.env,
                    group,
                    capture,
                    input && (!background || dedicated),
                    background,
                )
            };
            let started = match cancel {
                Some(token) => token.run_if_active(start).unwrap_or_else(|| {
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Native launch cancelled",
                    ))
                }),
                None => start(),
            };
            match started {
                Ok(mut p) => {
                    let error = p.setup_error.take();
                    pending.push(p); // A successful fork always transfers reaping ownership.
                    if let Some(e) = error {
                        outcome = Err(e);
                        break;
                    }
                }
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        let Some(first) = pending.first() else {
            return outcome.map(|()| unreachable!());
        };
        let pid = first.pid;
        let binding = Binding { key, id, pgid: pid };
        let mut members = Vec::new();
        let mut streams = Vec::new();
        for p in &mut pending {
            members.push(Member {
                pid: p.pid,
                status: Status::Running,
            });
            streams.append(&mut p.streams);
        }
        s.jobs.insert(
            key,
            Job {
                key,
                id,
                pgid: pid,
                dedicated,
                command: display.into(),
                members,
                visible: false,
                starting: true,
                background,
                no_hup: false,
                changed: true,
                next_consumed: false,
                waitable: true,
                settings: None,
                streams,
                bytes: Vec::new(),
                total: 0,
                capture,
                forward: capture && input,
                budget_start: 0,
                completion_order: u64::MAX,
                io_failed: false,
            },
        );
        drop(s);
        if outcome.is_ok() && !background && dedicated {
            if let Some(t) = self.terminal.lock().unwrap().as_mut() {
                outcome = t.give(pid, None);
            }
        }
        if outcome.is_ok() {
            for (started, p) in pending.iter().enumerate() {
                if let Err(e) = spawn::release(p, cancel) {
                    outcome = Err(if started == 0 {
                        e
                    } else {
                        io::Error::other(format!(
                            "partial Native launch ({started} members started): {e}"
                        ))
                    });
                    break;
                }
            }
        }
        let mut s = self.shared.state.lock().unwrap();
        let job = s
            .jobs
            .get_mut(&key)
            .expect("building jobs cannot be evicted");
        job.starting = false;
        job.visible = outcome.is_ok();
        if outcome.is_err() {
            // The sole reaper has revoked signal authority for members already reaped.
            let _ = signal(job, libc::SIGKILL);
            if !background && dedicated {
                if let Some(t) = self.terminal.lock().unwrap().as_mut() {
                    if let Err(e) = t.restore() {
                        s.failure = Some(format!("terminal restore: {e}"));
                    }
                }
            }
        }
        if background && outcome.is_ok() {
            let job = &s.jobs[&key];
            if job.status() != Status::Running {
                let notice = job_notice(&s, key);
                s.notifications.insert(key, notice);
            }
            s.last_async_pid = s.jobs[&key].members.last().map(|m| m.pid);
            if !s
                .current
                .and_then(|k| s.jobs.get(&k))
                .is_some_and(|j| matches!(j.status(), Status::Stopped(_)))
            {
                s.previous = s.current;
                s.current = Some(key);
            } else {
                reselect(&mut s);
            }
        }
        self.shared.changed.notify_all();
        outcome.map(|()| binding)
    }
    pub fn wait_foreground(
        &self,
        b: &Binding,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<CapturedExecution> {
        let result = self.wait_foreground_inner(b, cancel);
        let restored = self
            .terminal
            .lock()
            .unwrap()
            .as_mut()
            .map_or(Ok(()), |t| t.restore());
        if let Err(e) = restored {
            self.shared.state.lock().unwrap().failure = Some(format!("terminal restore: {e}"));
            return Err(e);
        }
        // With TOSTOP the shell itself must own the terminal before printing.
        if result
            .as_ref()
            .is_ok_and(|r| r.termination == CommandTermination::StoppedRetained)
        {
            let s = self.shared.state.lock().unwrap();
            if s.jobs.contains_key(&b.key) {
                eprintln!("\n{}", job_notice(&s, b.key));
            }
        }
        result
    }
    fn wait_foreground_inner(
        &self,
        b: &Binding,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<CapturedExecution> {
        let mut cancelling = None;
        let mut stop_reason = CommandTermination::Interrupted;
        let mut done_at = None;
        let budget_start = self
            .shared
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&b.key)
            .map_or(0, |j| j.budget_start);
        loop {
            let mut s = self.shared.state.lock().unwrap();
            if let Some(e) = &s.failure {
                return Err(io::Error::other(e.clone()));
            }
            let job = s
                .jobs
                .get_mut(&b.key)
                .ok_or_else(|| io::Error::other("job disappeared"))?;
            if cancel.is_some_and(CancellationToken::is_cancelled)
                && cancelling.is_none()
                && !job.status().done()
            {
                signal(job, libc::SIGINT)?;
                cancelling = Some(Instant::now());
            }
            if cancel.is_some()
                && cancelling.is_none()
                && job.total.saturating_sub(budget_start)
                    > super::executor::AGENT_COMMAND_OUTPUT_LIMIT
            {
                if !job.status().done() {
                    signal(job, libc::SIGTERM)?;
                }
                cancelling = Some(Instant::now());
                stop_reason = CommandTermination::OutputLimit;
            }
            if cancelling.is_some_and(|t| t.elapsed() > Duration::from_millis(500))
                && !job.status().done()
            {
                signal(job, libc::SIGKILL)?;
            }
            let status = job.status();
            if matches!(status, Status::Stopped(_)) && cancelling.is_some() {
                signal(job, libc::SIGCONT)?;
                signal(job, libc::SIGTERM)?;
            } else if status != Status::Running {
                if status.done()
                    && !job.streams.is_empty()
                    && done_at.get_or_insert_with(Instant::now).elapsed()
                        < Duration::from_millis(200)
                {
                    drop(s);
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                if matches!(status, Status::Stopped(_)) {
                    job.background = true;
                    let mut t = self.terminal.lock().unwrap();
                    if let Some(t) = t.as_mut() {
                        job.settings = t.settings().ok();
                    }
                }
                let complete = status.done() && job.streams.is_empty();
                let evidence = if !job.capture {
                    OutputEvidence::Unavailable
                } else if job.io_failed {
                    OutputEvidence::CaptureFailed
                } else if !complete {
                    OutputEvidence::Partial
                } else if job.total > job.bytes.len() {
                    OutputEvidence::Truncated
                } else {
                    OutputEvidence::Complete
                };
                let result = CapturedExecution {
                    job: Some(*b),
                    output: String::from_utf8_lossy(&job.bytes).into_owned(),
                    total_output_bytes: job.total,
                    exit_code: status.code(),
                    termination: if matches!(status, Status::Stopped(_)) {
                        CommandTermination::StoppedRetained
                    } else if cancelling.is_some() {
                        stop_reason
                    } else {
                        CommandTermination::Exited
                    },
                    output_evidence: evidence,
                };
                if status.done() {
                    job.visible = false;
                }
                reselect(&mut s);
                drop(s);
                return Ok(result);
            }
            let _ = self
                .shared
                .changed
                .wait_timeout(s, Duration::from_millis(20))
                .unwrap();
        }
    }
    pub fn resume(
        &self,
        b: &Binding,
        foreground: bool,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<Option<CapturedExecution>> {
        {
            let mut s = self.shared.state.lock().unwrap();
            if !s.options.monitor {
                return Err(io::Error::other("job control is disabled"));
            }
            let interactive = s.interactive;
            let j = s
                .jobs
                .get_mut(&b.key)
                .filter(|j| j.visible && j.binding() == *b)
                .ok_or_else(|| io::Error::other("no such job"))?;
            if j.status().done() {
                return Err(io::Error::other("job has terminated"));
            }
            if !j.dedicated {
                return Err(io::Error::other("job was started without job control"));
            }
            if foreground {
                // Print before handing off the terminal, including when TOSTOP is set.
                // Agent execution retains its existing proposal/output presentation.
                if interactive && cancel.is_none() {
                    eprintln!("{}", j.command);
                }
                if let Some(t) = self.terminal.lock().unwrap().as_mut() {
                    t.give(j.pgid, j.settings.as_ref())?;
                }
            }
            if let Err(e) = signal(j, libc::SIGCONT) {
                if foreground {
                    if let Some(t) = self.terminal.lock().unwrap().as_mut() {
                        t.restore()?;
                    }
                }
                return Err(e);
            }
            for m in &mut j.members {
                if !m.status.done() {
                    m.status = Status::Running;
                }
            }
            j.background = !foreground;
            j.changed = true;
            if foreground {
                j.forward = j.capture;
                j.budget_start = j.total;
            }
            if !foreground {
                s.last_async_pid = j.members.last().map(|m| m.pid);
            }
            reselect(&mut s);
        }
        if foreground {
            self.wait_foreground(b, cancel).map(Some)
        } else {
            Ok(None)
        }
    }
    pub fn send(&self, b: &Binding, sig: i32) -> io::Result<()> {
        let s = self.shared.state.lock().unwrap();
        let j = s
            .jobs
            .get(&b.key)
            .filter(|j| j.visible && j.binding() == *b && !j.status().done())
            .ok_or_else(|| io::Error::other("stale job"))?;
        signal(j, sig)
    }
    pub fn disown(&self, b: &Binding, hup_only: bool) -> Result<(), String> {
        let mut s = self.shared.state.lock().unwrap();
        let j = s
            .jobs
            .get_mut(&b.key)
            .filter(|j| j.visible && j.binding() == *b)
            .ok_or("no such job")?;
        if hup_only {
            j.no_hup = true;
        } else {
            j.visible = false;
            j.next_consumed = true;
            j.waitable = false;
        }
        reselect(&mut s);
        Ok(())
    }
    #[cfg(test)]
    pub fn wait(
        &self,
        ids: &[String],
        next: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(i32, Option<i32>), String> {
        self.wait_bound(ids, next, force, cancel, &[])
    }
    pub fn check_wait_target(
        &self,
        id: &str,
        bindings: &[(String, Binding)],
    ) -> Result<(), String> {
        let s = self.shared.state.lock().unwrap();
        if id.starts_with('%') {
            let b = bindings
                .iter()
                .find(|(v, _)| v == id)
                .map(|(_, b)| *b)
                .map_or_else(|| resolve(&s, id), Ok)?;
            if s.jobs
                .get(&b.key)
                .is_some_and(|j| j.waitable && j.binding() == b)
            {
                Ok(())
            } else {
                Err("not a waitable job".into())
            }
        } else {
            let pid = id.parse::<i32>().map_err(|_| "invalid pid")?;
            if s.jobs
                .values()
                .any(|j| j.waitable && j.members.iter().any(|m| m.pid == pid))
            {
                Ok(())
            } else {
                Err("not a child of this shell".into())
            }
        }
    }
    pub fn wait_bound(
        &self,
        ids: &[String],
        next: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
        bindings: &[(String, Binding)],
    ) -> Result<(i32, Option<i32>), String> {
        let interrupt_epoch = signals::interrupt_epoch();
        let mut s = self.shared.state.lock().unwrap();
        let mut targets = Vec::new();
        if ids.is_empty() {
            targets = s
                .jobs
                .values()
                .filter(|j| j.visible && j.background && (!next || !j.next_consumed))
                .map(|j| (j.key, None))
                .collect();
        } else {
            for id in ids {
                if id.starts_with('%') {
                    let binding = if let Some((_, b)) = bindings.iter().find(|(spec, _)| spec == id)
                    {
                        if !s
                            .jobs
                            .get(&b.key)
                            .is_some_and(|j| j.waitable && j.binding() == *b)
                        {
                            return Err("PlanStale: wait target disappeared".into());
                        }
                        *b
                    } else {
                        resolve(&s, id)?
                    };
                    targets.push((binding.key, None));
                } else {
                    let pid = id.parse::<i32>().map_err(|_| "invalid pid")?;
                    let j = s
                        .jobs
                        .values()
                        .rev()
                        .find(|j| j.waitable && j.members.iter().any(|m| m.pid == pid))
                        .ok_or("not a child of this shell")?;
                    targets.push((j.key, Some(pid)));
                }
            }
        }
        if targets.is_empty() {
            return if next {
                Err("no unwaited children".into())
            } else {
                Ok((0, None))
            };
        }
        let mut last = (0, None);
        loop {
            if cancel.is_some_and(CancellationToken::is_cancelled)
                || signals::interrupt_epoch() != interrupt_epoch
            {
                return Ok((130, None));
            }
            let monitor = s.options.monitor;
            if next {
                targets.sort_by_key(|(key, _)| {
                    s.jobs.get(key).map_or(u64::MAX, |j| j.completion_order)
                });
            }
            let mut completed = Vec::new();
            for (i, (key, pid)) in targets.iter().enumerate() {
                let j = s.jobs.get_mut(key).ok_or("wait target disappeared")?;
                if next && j.next_consumed {
                    continue;
                }
                let status = pid
                    .and_then(|p| j.members.iter().find(|m| m.pid == p).map(|m| m.status))
                    .unwrap_or_else(|| j.status());
                if status.done()
                    || (!next && !force && monitor && matches!(status, Status::Stopped(_)))
                {
                    last = (
                        status.code(),
                        Some(pid.unwrap_or_else(|| j.members.last().unwrap().pid)),
                    );
                    if status.done() {
                        j.next_consumed = true;
                        if pid.is_none() {
                            j.visible = false;
                        }
                    }
                    if next {
                        return Ok(last);
                    }
                    completed.push(i);
                } else if !next {
                    break;
                }
            }
            for i in completed.into_iter().rev() {
                targets.remove(i);
            }
            if targets.is_empty() {
                if ids.is_empty() {
                    s.jobs
                        .retain(|_, j| !j.status().done() || !j.streams.is_empty());
                    return Ok((0, None));
                }
                return Ok(last);
            }
            if next
                && targets
                    .iter()
                    .all(|(k, _)| s.jobs.get(k).is_none_or(|j| j.next_consumed))
            {
                return Err("no unwaited children".into());
            }
            s = self
                .shared
                .changed
                .wait_timeout(s, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }
    pub fn exit(&self, warn: bool, hup: bool) -> Result<(), String> {
        let s = self.shared.state.lock().unwrap();
        if warn
            && s.interactive
            && s.options.monitor
            && s.jobs.values().any(|j| {
                j.visible
                    && (matches!(j.status(), Status::Stopped(_))
                        || s.options.checkjobs && !j.status().done())
            })
        {
            return Err("there are active jobs".into());
        }
        let hup = hup || (s.interactive && s.login && s.options.huponexit);
        for j in s.jobs.values().filter(|j| j.visible && !j.status().done()) {
            if hup && j.no_hup {
                continue;
            }
            if hup {
                let _ = signal(j, libc::SIGHUP);
            }
            if matches!(j.status(), Status::Stopped(_)) {
                if !hup {
                    let _ = signal(j, libc::SIGTERM);
                }
                let _ = signal(j, libc::SIGCONT);
            }
        }
        Ok(())
    }
}
fn take_notifications(s: &mut State) -> Vec<String> {
    let notices = std::mem::take(&mut s.notifications);
    for key in notices.keys() {
        if let Some(j) = s.jobs.get_mut(key) {
            j.changed = false;
            if j.status().done() {
                j.visible = false;
            }
        }
    }
    reselect(s);
    notices.into_values().collect()
}
fn resolve(s: &State, spec: &str) -> Result<Binding, String> {
    let value = spec.strip_prefix('%').ok_or("expected jobspec")?;
    let key = match value {
        "" | "%" | "+" => s.current,
        "-" => s.previous,
        _ => None,
    };
    if matches!(value, "" | "%" | "+" | "-") {
        return key
            .and_then(|k| s.jobs.get(&k))
            .filter(|j| j.visible)
            .map(Job::binding)
            .ok_or("no current job".into());
    }
    let found = s
        .jobs
        .values()
        .filter(|j| j.visible)
        .filter(|j| {
            if let Ok(id) = value.parse::<u32>() {
                j.id == id
            } else if let Some(q) = value.strip_prefix('?') {
                j.command.contains(q)
            } else {
                j.command.starts_with(value)
            }
        })
        .collect::<Vec<_>>();
    match found.as_slice() {
        [j] => Ok(j.binding()),
        [] => Err("no such job".into()),
        _ => Err("ambiguous jobspec".into()),
    }
}
fn reselect(s: &mut State) {
    let eligible = |key: u64| {
        s.jobs
            .get(&key)
            .is_some_and(|j| j.visible && j.background && !j.status().done())
    };
    let stopped = |key: u64| eligible(key) && matches!(s.jobs[&key].status(), Status::Stopped(_));
    let mut keys = s
        .jobs
        .values()
        .filter(|j| eligible(j.key))
        .map(|j| (j.id, j.key))
        .collect::<Vec<_>>();
    keys.sort();
    let current = s
        .current
        .filter(|k| stopped(*k))
        .or(s.previous.filter(|k| stopped(*k)))
        .or_else(|| {
            keys.iter()
                .rev()
                .find(|(_, k)| stopped(*k))
                .map(|(_, k)| *k)
        })
        .or_else(|| keys.last().map(|(_, k)| *k));
    let previous = s
        .previous
        .filter(|k| Some(*k) != current && stopped(*k))
        .or_else(|| {
            keys.iter()
                .rev()
                .find(|(_, k)| Some(*k) != current && stopped(*k))
                .map(|(_, k)| *k)
        })
        .or_else(|| {
            keys.iter()
                .rev()
                .find(|(_, k)| Some(*k) != current)
                .map(|(_, k)| *k)
        });
    s.current = current;
    s.previous = previous;
}
fn signal(job: &Job, sig: i32) -> io::Result<()> {
    if job.status().done() {
        return Err(io::Error::other("job has finished"));
    }
    if job.dedicated {
        if unsafe { libc::kill(-job.pgid, sig) } < 0 {
            return Err(io::Error::last_os_error());
        }
    } else {
        for m in job.members.iter().filter(|m| !m.status.done()) {
            if unsafe { libc::kill(m.pid, sig) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
fn job_notice(s: &State, key: u64) -> String {
    let job = &s.jobs[&key];
    let mark = if s.current == Some(key) {
        '+'
    } else if s.previous == Some(key) {
        '-'
    } else {
        ' '
    };
    format!(
        "[{}]{mark} {:<23} {}",
        job.id,
        job.status().label(),
        job.command
    )
}
impl Drop for JobRuntime {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().stop = true;
        self.shared.changed.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn eventually(runtime: &JobRuntime, b: &Binding, expected: impl Fn(Status) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let state = runtime.shared.state.lock().unwrap();
            let status = state.jobs[&b.key].status();
            if expected(status) {
                return;
            }
            drop(state);
            assert!(Instant::now() < deadline, "job did not change: {status:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn independent_runtimes_and_stale_ids_never_share_jobs() {
        let a = JobRuntime::new(false);
        let b = JobRuntime::new(false);
        a.set_option("monitor", true).unwrap();
        let env = HashMap::new();
        let args = vec![OsString::from("30")];
        let target = a
            .launch(
                Path::new("/usr/bin/sleep"),
                "sleep",
                &args,
                Path::new("/"),
                &env,
                "sleep 30",
                false,
                false,
                true,
                None,
            )
            .unwrap();
        assert_eq!(target.id, 1);
        assert!(b.snapshots().is_empty());
        assert!(b.resolve("%1").is_err());
        assert!(b
            .wait(&[target.pgid.to_string()], false, false, None)
            .is_err());
        a.send(&target, libc::SIGSTOP).unwrap();
        eventually(&a, &target, |s| matches!(s, Status::Stopped(_)));
        a.resume(&target, false, None).unwrap();
        a.send(&target, libc::SIGKILL).unwrap();
        eventually(&a, &target, Status::done);
        assert_eq!(
            a.resume(&target, true, None).err().unwrap().to_string(),
            "job has terminated"
        );
        a.acknowledge(&[target.key]);
        let newer = a
            .launch(
                Path::new("/usr/bin/sleep"),
                "sleep",
                &args,
                Path::new("/"),
                &env,
                "sleep 30",
                false,
                false,
                true,
                None,
            )
            .unwrap();
        assert_eq!(newer.id, target.id);
        assert_ne!(newer.key, target.key);
        assert!(!a.valid(&target));
        assert!(a.send(&target, libc::SIGTERM).is_err());
        a.send(&newer, libc::SIGKILL).unwrap();
        eventually(&a, &newer, Status::done);
    }
    #[test]
    fn group_aggregation_wait_cache_and_disown_reaping() {
        let r = JobRuntime::new(false);
        r.set_option("monitor", true).unwrap();
        let env = HashMap::new();
        let args = vec![OsString::from("30")];
        let requests = [
            SpawnRequest {
                path: Path::new("/usr/bin/sleep"),
                program: "sleep",
                args: &args,
                cwd: Path::new("/"),
                env: &env,
            },
            SpawnRequest {
                path: Path::new("/usr/bin/sleep"),
                program: "sleep",
                args: &args,
                cwd: Path::new("/"),
                env: &env,
            },
        ];
        let b = r
            .launch_members(&requests, "two members", false, false, true)
            .unwrap();
        let pids = r.snapshots()[0].pids.clone();
        unsafe {
            libc::kill(pids[0], libc::SIGSTOP);
        }
        {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let s = r.shared.state.lock().unwrap();
                if matches!(s.jobs[&b.key].members[0].status, Status::Stopped(_)) {
                    assert_eq!(s.jobs[&b.key].status(), Status::Running);
                    break;
                }
                drop(s);
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
        }
        r.send(&b, libc::SIGSTOP).unwrap();
        eventually(&r, &b, |s| matches!(s, Status::Stopped(_)));
        r.resume(&b, false, None).unwrap();
        r.send(&b, libc::SIGKILL).unwrap();
        eventually(&r, &b, Status::done);
        assert_eq!(r.wait(&[], true, false, None).unwrap().0, 137);
        assert_eq!(
            r.wait(&[pids[1].to_string()], false, false, None)
                .unwrap()
                .0,
            137
        );
        assert_eq!(
            r.wait(&[pids[1].to_string()], false, false, None)
                .unwrap()
                .0,
            137
        );

        assert!(r.wait(&[], true, false, None).is_err());
        let b = r
            .launch(
                Path::new("/usr/bin/sleep"),
                "sleep",
                &args,
                Path::new("/"),
                &env,
                "disowned",
                false,
                false,
                true,
                None,
            )
            .unwrap();
        r.disown(&b, false).unwrap();
        assert!(r.resolve(&format!("%{}", b.id)).is_err());
        unsafe {
            libc::kill(b.pgid, libc::SIGKILL);
        }
        eventually(&r, &b, Status::done);
    }
    #[test]
    fn monitor_exit_and_hup_policies_preserve_only_eligible_jobs() {
        let r = JobRuntime::new(false);
        assert!(!r.options().monitor);
        r.set_option("monitor", true).unwrap();
        let env = HashMap::new();
        let args = vec![OsString::from("30")];
        let launch = || {
            r.launch(
                Path::new("/usr/bin/sleep"),
                "sleep",
                &args,
                Path::new("/"),
                &env,
                "policy fixture",
                false,
                false,
                true,
                None,
            )
            .unwrap()
        };
        let stopped = launch();
        let running = launch();
        r.send(&stopped, libc::SIGSTOP).unwrap();
        eventually(&r, &stopped, |s| matches!(s, Status::Stopped(_)));
        r.set_option("monitor", false).unwrap();
        assert!(r.resume(&stopped, false, None).is_err());
        r.set_option("monitor", true).unwrap();
        r.shared.state.lock().unwrap().interactive = true;
        assert!(r.exit(true, false).is_err());
        r.exit(false, false).unwrap();
        eventually(&r, &stopped, Status::done);
        assert!(r.valid(&running)); // Normal non-login exit does not HUP running jobs.
        r.set_option("checkjobs", true).unwrap();
        assert!(r.exit(true, false).is_err());
        r.set_option("huponexit", true).unwrap();
        r.exit(false, false).unwrap();
        assert!(r.valid(&running)); // huponexit alone does not make this a login shell.
        r.disown(&running, true).unwrap();
        let hup_target = launch();
        r.shared.state.lock().unwrap().login = true;
        r.exit(false, false).unwrap();
        eventually(&r, &hup_target, Status::done);
        assert_eq!(
            r.wait(&[hup_target.pgid.to_string()], false, true, None)
                .unwrap()
                .0,
            129
        );
        assert!(r.valid(&running));
        r.exit(false, true).unwrap(); // Explicit HUP also honors disown -h.
        assert!(r.valid(&running));
        r.send(&running, libc::SIGKILL).unwrap();
        eventually(&r, &running, Status::done);
    }

    #[test]
    fn partial_launch_failure_keeps_every_child_owned_until_reaped() {
        let runtime = JobRuntime::new(false);
        runtime.set_option("monitor", true).unwrap();
        let env = HashMap::new();
        let args = vec![OsString::from("30")];
        let requests = [
            SpawnRequest {
                path: Path::new("/usr/bin/sleep"),
                program: "sleep",
                args: &args,
                cwd: Path::new("/"),
                env: &env,
            },
            SpawnRequest {
                path: Path::new("/nonexistent/zhsh-test-program"),
                program: "missing",
                args: &[],
                cwd: Path::new("/"),
                env: &env,
            },
        ];
        let error = runtime
            .launch_members(&requests, "partial", false, false, true)
            .unwrap_err();
        assert!(
            error.to_string().contains("partial Native launch"),
            "{error}"
        );
        let binding = runtime
            .shared
            .state
            .lock()
            .unwrap()
            .jobs
            .values()
            .next()
            .unwrap()
            .binding();
        eventually(&runtime, &binding, Status::done);
        assert!(runtime.snapshots().is_empty());
        assert!(runtime.failure().is_none());
    }

    #[test]
    fn captured_foreground_keeps_existing_agent_output_limit() {
        let runtime = JobRuntime::new(false);
        runtime.set_option("monitor", true).unwrap();
        let env = HashMap::new();
        let binding = runtime
            .launch(
                Path::new("/usr/bin/yes"),
                "yes",
                &[],
                Path::new("/"),
                &env,
                "yes",
                true,
                false,
                false,
                None,
            )
            .unwrap();
        let result = runtime
            .wait_foreground(&binding, Some(&CancellationToken::default()))
            .unwrap();
        assert_eq!(result.termination, CommandTermination::OutputLimit);
        assert!(result.output.len() <= super::super::executor::AGENT_COMMAND_OUTPUT_LIMIT);
        assert!(!runtime.valid(&binding));
    }
    #[test]
    fn kernel_rejects_text_and_capture_preserves_arguments() {
        let r = JobRuntime::new(false);
        let env = HashMap::new();
        let args = vec![
            OsString::from("[%s]"),
            OsString::from("a b"),
            OsString::from(""),
        ];
        let b = r
            .launch(
                Path::new("/usr/bin/printf"),
                "printf",
                &args,
                Path::new("/"),
                &env,
                "printf",
                true,
                false,
                false,
                None,
            )
            .unwrap();
        let output = r.wait_foreground(&b, None).unwrap();
        assert_eq!(output.output, "[a b][]");
        assert_eq!(output.output_evidence, OutputEvidence::Complete);
        let path =
            std::env::temp_dir().join(format!("zhsh-native-no-fallback-{}", std::process::id()));
        std::fs::write(&path, "exit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = r
            .launch(
                &path,
                "test",
                &[],
                Path::new("/"),
                &env,
                "text",
                false,
                false,
                false,
                None,
            )
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOEXEC));
        std::fs::remove_file(path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if r.shared
                .state
                .lock()
                .unwrap()
                .jobs
                .values()
                .all(|j| j.status().done())
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }
}
