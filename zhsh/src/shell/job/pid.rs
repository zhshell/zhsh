//! Stable Linux process handles for an approved numeric signal target.
use std::{
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::Arc,
};
#[derive(Debug, Clone)]
pub(crate) struct PidBinding {
    pub pid: i32,
    birth: String,
    fd: Option<Arc<OwnedFd>>,
}
impl PartialEq for PidBinding {
    fn eq(&self, other: &Self) -> bool {
        self.pid == other.pid && self.birth == other.birth
    }
}
impl Eq for PidBinding {}
fn birth(pid: i32) -> io::Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("invalid process identity"))
}
impl PidBinding {
    pub fn open(pid: i32) -> io::Result<Self> {
        let before = birth(pid)?;
        // SAFETY: pidfd_open returns a new owned descriptor or errno; flags are zero.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        let fd = if fd >= 0 {
            Some(Arc::new(unsafe { OwnedFd::from_raw_fd(fd as i32) }))
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ENOSYS) && e.raw_os_error() != Some(libc::EINVAL) {
                return Err(e);
            }
            None
        };
        if birth(pid)? != before {
            return Err(io::Error::other("process identity changed"));
        }
        Ok(Self {
            pid,
            birth: before,
            fd,
        })
    }
    pub fn send(&self, signal: i32) -> io::Result<()> {
        let result = if let Some(fd) = &self.fd {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            }
        } else {
            if birth(self.pid)? != self.birth {
                return Err(io::Error::other("PlanStale: process identity changed"));
            }
            unsafe { libc::kill(self.pid, signal) as libc::c_long }
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
