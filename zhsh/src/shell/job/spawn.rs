//! Small fork/exec boundary. The child performs only async-signal-safe syscalls.
use std::collections::HashMap;
use std::{
    ffi::{CString, OsString},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};
pub(super) struct Pending {
    pub pid: i32,
    pub go: OwnedFd,
    pub error: OwnedFd,
    pub streams: Vec<OwnedFd>,
    pub setup_error: Option<io::Error>,
}
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0; 2];
    // SAFETY: writable two-element array; ownership transferred once.
    unsafe {
        if libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        let promote = |fd: OwnedFd| -> io::Result<OwnedFd> {
            if fd.as_raw_fd() >= 3 {
                return Ok(fd);
            }
            let duplicate = libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3);
            if duplicate < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(OwnedFd::from_raw_fd(duplicate))
            }
        };
        let read = OwnedFd::from_raw_fd(p[0]);
        let write = OwnedFd::from_raw_fd(p[1]);
        Ok((promote(read)?, promote(write)?))
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn start(
    path: &Path,
    program: &str,
    args: &[OsString],
    cwd: &Path,
    env: &HashMap<String, String>,
    group: Option<i32>,
    capture: bool,
    input: bool,
    background: bool,
) -> io::Result<Pending> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "NUL in execution request");
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid())?;
    let cwd = CString::new(cwd.as_os_str().as_bytes()).map_err(|_| invalid())?;
    let args = std::iter::once(std::ffi::OsStr::new(program))
        .chain(args.iter().map(OsString::as_os_str))
        .map(|a| CString::new(a.as_bytes()).map_err(|_| invalid()))
        .collect::<io::Result<Vec<_>>>()?;
    let env = env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).map_err(|_| invalid()))
        .collect::<io::Result<Vec<_>>>()?;
    let argv = args
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect::<Vec<_>>();
    let envp = env
        .iter()
        .map(|a| a.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect::<Vec<_>>();
    let (go_r, go_w) = pipe()?;
    let (err_r, err_w) = pipe()?;
    let out = if capture { Some(pipe()?) } else { None };
    let err = if capture { Some(pipe()?) } else { None };
    // SAFETY: all child data is allocated before fork. Child does not return into Rust.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            let fail = || -> ! {
                let errno = *libc::__errno_location();
                libc::write(err_w.as_raw_fd(), (&errno as *const i32).cast(), 4);
                libc::_exit(126)
            };
            libc::close(go_w.as_raw_fd());
            libc::close(err_r.as_raw_fd());
            if let Some(g) = group {
                if libc::setpgid(0, g) < 0 {
                    fail();
                }
            }
            if libc::chdir(cwd.as_ptr()) < 0 {
                fail();
            }
            for sig in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGPIPE,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            if background && group.is_none() {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
                libc::signal(libc::SIGQUIT, libc::SIG_IGN);
            }
            let mut mask = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
            if !input {
                let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
                if fd < 0 || libc::dup2(fd, 0) < 0 {
                    fail();
                }
                if fd > 2 {
                    libc::close(fd);
                }
            }
            for (pair, target) in [(&out, 1), (&err, 2)] {
                if let Some((r, w)) = pair {
                    libc::close(r.as_raw_fd());
                    if libc::dup2(w.as_raw_fd(), target) < 0 {
                        fail();
                    }
                    libc::close(w.as_raw_fd());
                }
            }
            let mut byte = 0u8;
            loop {
                let n = libc::read(go_r.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
                if n == 1 {
                    break;
                }
                if n < 0 && *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                libc::_exit(126);
            }
            libc::close(go_r.as_raw_fd());
            libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
            fail();
        }
    }
    drop(go_r);
    drop(err_w);
    let mut setup_error = None;
    if let Some(g) = group {
        // Child also calls setpgid before accessing the terminal.
        unsafe {
            if libc::setpgid(pid, if g == 0 { pid } else { g }) < 0 {
                setup_error = Some(io::Error::last_os_error());
            }
        }
    }
    let mut streams = Vec::new();
    for pair in [out, err].into_iter().flatten() {
        drop(pair.1);
        unsafe {
            if libc::fcntl(pair.0.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) < 0 {
                setup_error.get_or_insert_with(io::Error::last_os_error);
            }
        }
        streams.push(pair.0);
    }
    Ok(Pending {
        pid,
        go: go_w,
        error: err_r,
        streams,
        setup_error,
    })
}
pub(super) fn release(
    p: &Pending,
    cancel: Option<&crate::common::CancellationToken>,
) -> io::Result<()> {
    // SAFETY: owned pipe descriptors; protocol is one release byte followed by exec errno or EOF.
    unsafe {
        let b = 1u8;
        if libc::write(p.go.as_raw_fd(), (&b as *const u8).cast(), 1) != 1 {
            return Err(io::Error::last_os_error());
        }
        let mut errno = 0i32;
        loop {
            if cancel.is_some_and(crate::common::CancellationToken::is_cancelled) {
                // GO may already have reached exec; never report definite NotStarted.
                return Err(io::Error::other("Native launch cancelled after dispatch"));
            }
            let mut descriptor = libc::pollfd {
                fd: p.error.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = libc::poll(&mut descriptor, 1, 10);
            if ready == 0 {
                continue;
            }
            if ready < 0 {
                if *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                return Err(io::Error::last_os_error());
            }
            let n = libc::read(p.error.as_raw_fd(), (&mut errno as *mut i32).cast(), 4);
            if n == 0 {
                return Ok(());
            }
            if n == 4 {
                return Err(io::Error::from_raw_os_error(errno));
            }
            if n < 0 && *libc::__errno_location() == libc::EINTR {
                continue;
            }
            return Err(io::Error::other("invalid exec handshake"));
        }
    }
}
