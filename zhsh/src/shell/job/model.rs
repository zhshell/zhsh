//! Instance-local job identities and kernel observations.
use std::os::fd::OwnedFd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Running,
    Stopped(i32),
    Exited(i32),
}
impl Status {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Running => "Running".into(),
            Self::Stopped(libc::SIGTSTP) => "Stopped".into(),
            Self::Stopped(libc::SIGSTOP) => "Stopped (signal)".into(),
            Self::Stopped(libc::SIGTTIN) => "Stopped (tty input)".into(),
            Self::Stopped(libc::SIGTTOU) => "Stopped (tty output)".into(),
            Self::Stopped(_) => "Stopped".into(),
            Self::Exited(0) => "Done".into(),
            Self::Exited(c) => format!("Exit {c}"),
        }
    }
    pub(super) fn done(self) -> bool {
        matches!(self, Self::Exited(_))
    }
    pub(super) fn code(self) -> i32 {
        match self {
            Self::Running => 0,
            Self::Stopped(s) => 128 + s,
            Self::Exited(c) => c,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Binding {
    pub key: u64,
    pub id: u32,
    pub pgid: i32,
}
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub binding: Binding,
    pub pids: Vec<i32>,
    pub command: String,
    pub status: Status,
    pub mark: char,
    pub changed: bool,
}
pub(super) struct Member {
    pub pid: i32,
    pub status: Status,
}
pub(super) struct Job {
    pub key: u64,
    pub id: u32,
    pub pgid: i32,
    pub dedicated: bool,
    pub command: String,
    pub members: Vec<Member>,
    pub visible: bool,
    pub starting: bool,
    pub background: bool,
    pub no_hup: bool,
    pub changed: bool,
    pub next_consumed: bool,
    pub waitable: bool,
    pub settings: Option<libc::termios>,
    pub streams: Vec<OwnedFd>,
    pub bytes: Vec<u8>,
    pub total: usize,
    pub capture: bool,
    pub forward: bool,
    pub budget_start: usize,
    pub completion_order: u64,
    pub io_failed: bool,
}
impl Job {
    pub fn status(&self) -> Status {
        if self.members.iter().all(|m| m.status.done()) {
            return self.members.last().map_or(Status::Exited(0), |m| m.status);
        }
        if self
            .members
            .iter()
            .filter(|m| !m.status.done())
            .all(|m| matches!(m.status, Status::Stopped(_)))
        {
            return self
                .members
                .iter()
                .find_map(|m| match m.status {
                    Status::Stopped(s) => Some(Status::Stopped(s)),
                    _ => None,
                })
                .unwrap();
        }
        Status::Running
    }
    pub fn binding(&self) -> Binding {
        Binding {
            key: self.key,
            id: self.id,
            pgid: self.pgid,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
    pub monitor: bool,
    pub notify: bool,
    pub checkjobs: bool,
    pub huponexit: bool,
}
