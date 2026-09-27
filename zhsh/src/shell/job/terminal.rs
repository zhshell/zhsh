//! Native terminal leases. Signal continuation is deliberately a separate operation.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
pub(super) struct Terminal {
    fd: OwnedFd,
    owner: i32,
    original: libc::termios,
    leased: bool,
}
impl Terminal {
    pub fn open(interactive: bool) -> Option<Self> {
        // SAFETY: constant C string and owned descriptor; terminal structures initialized by libc.
        unsafe {
            let fd = libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let fd = OwnedFd::from_raw_fd(fd);
            let mut original = std::mem::zeroed();
            if libc::tcgetattr(fd.as_raw_fd(), &mut original) != 0 {
                return None;
            }
            let mut terminal = Self {
                fd,
                owner: libc::getpgrp(),
                original,
                leased: false,
            };
            if interactive {
                terminal.await_foreground().ok()?;
                let pid = libc::getpid();
                if terminal.owner != pid && libc::setpgid(0, pid) < 0 {
                    return None;
                }
                terminal.owner = libc::getpgrp();
                terminal.set_group(terminal.owner).ok()?;
            }
            Some(terminal)
        }
    }
    pub fn await_foreground(&self) -> io::Result<()> {
        // A nested shell waits for its parent to give it the terminal; it must
        // never steal the terminal after being continued in the background.
        unsafe {
            loop {
                let foreground = libc::tcgetpgrp(self.fd.as_raw_fd());
                if foreground < 0 {
                    return Err(io::Error::last_os_error());
                }
                if foreground == libc::getpgrp() {
                    return Ok(());
                }
                let old = libc::signal(libc::SIGTTIN, libc::SIG_DFL);
                let rc = libc::kill(-libc::getpgrp(), libc::SIGTTIN);
                let error = io::Error::last_os_error();
                libc::signal(libc::SIGTTIN, old);
                if rc < 0 {
                    return Err(error);
                }
            }
        }
    }
    pub fn give(&mut self, group: i32, settings: Option<&libc::termios>) -> io::Result<()> {
        self.original = self.settings()?;
        self.set_group(group)?;
        self.leased = true;
        if let Some(s) = settings {
            if let Err(e) = self.set_settings(s) {
                let _ = self.restore();
                return Err(e);
            }
        }
        Ok(())
    }
    pub fn settings(&self) -> io::Result<libc::termios> {
        // SAFETY: libc writes a valid termios on success.
        unsafe {
            let mut s = std::mem::zeroed();
            if libc::tcgetattr(self.fd.as_raw_fd(), &mut s) < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(s)
            }
        }
    }
    fn set_settings(&self, settings: &libc::termios) -> io::Result<()> {
        // SAFETY: tcsetattr may run while the Shell temporarily belongs to the
        // background group; block SIGTTOU for this operation just as for tcsetpgrp.
        unsafe {
            let mut set = std::mem::zeroed();
            let mut old = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTTOU);
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            let rc = libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSADRAIN, settings);
            let error = io::Error::last_os_error();
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            if rc < 0 {
                Err(error)
            } else {
                Ok(())
            }
        }
    }
    fn set_group(&self, group: i32) -> io::Result<()> {
        // SAFETY: temporarily block SIGTTOU on this thread and restore the previous mask.
        unsafe {
            let mut set = std::mem::zeroed();
            let mut old = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTTOU);
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            let result = libc::tcsetpgrp(self.fd.as_raw_fd(), group);
            let error = io::Error::last_os_error();
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            if result < 0 {
                Err(error)
            } else {
                Ok(())
            }
        }
    }
    pub fn restore(&mut self) -> io::Result<()> {
        if !self.leased {
            return Ok(());
        }
        self.set_group(self.owner)?;
        self.set_settings(&self.original)?;
        self.leased = false;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
