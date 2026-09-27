#![cfg(target_os = "linux")]
use std::{
    fs::File,
    io::{Read, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Terminal {
    master: File,
    child: Child,
    home: std::path::PathBuf,
}
impl Terminal {
    fn new() -> Self {
        Self::with_tostop(false)
    }
    fn with_tostop(tostop: bool) -> Self {
        let mut m = -1;
        let mut s = -1;
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 300,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut m,
                    &mut s,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        if tostop {
            unsafe {
                let mut settings = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(s, &mut settings), 0);
                settings.c_lflag |= libc::TOSTOP;
                assert_eq!(libc::tcsetattr(s, libc::TCSANOW, &settings), 0);
            }
        }
        let master = unsafe { File::from_raw_fd(m) };
        let slave = unsafe { File::from_raw_fd(s) };
        unsafe {
            libc::fcntl(m, libc::F_SETFL, libc::O_NONBLOCK);
        }
        let home = std::env::temp_dir().join(format!(
            "zhsh-job-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&home).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
        command
            .arg("--native")
            .env("HOME", &home)
            .env("TERM", "xterm")
            .env("ZHSH_TEST_SYSTEM_CODEC_DIR", "/nonexistent")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let mut result = Self {
            master,
            child,
            home,
        };
        result.prompt();
        result
    }
    fn prompt(&mut self) -> String {
        self.until("\x1b[?2004h")
    }
    fn until(&mut self, expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut bytes = Vec::new();
        let mut buffer = [0; 16384];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.extend_from_slice(&buffer[..n]);
                    if String::from_utf8_lossy(&bytes).contains(expected) {
                        return String::from_utf8_lossy(&bytes).into_owned();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("PTY read: {e}: {}", String::from_utf8_lossy(&bytes)),
            }
            assert!(
                Instant::now() < deadline,
                "missing {expected:?}: {}",
                String::from_utf8_lossy(&bytes)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("unexpected terminal EOF")
    }
    fn command(&mut self, text: &str) -> String {
        self.master
            .write_all(format!("{text}\r").as_bytes())
            .unwrap();
        self.prompt()
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.master.write_all(b"kill -KILL %1 %2 %3\r");
        let _ = self.master.write_all(b"exit\rexit\r");
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                let _ = std::fs::remove_dir_all(&self.home);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}
#[test]
fn two_native_instances_keep_jobs_and_foreground_control_separate() {
    let mut a = Terminal::new();
    let mut b = Terminal::new();
    let stopped = a.command("sh -c 'kill -STOP $$; echo READING_A; read x; echo RESUMED_A' ");
    assert!(stopped.contains("[1]+ Stopped"), "{stopped}");
    let empty = b.command("jobs");
    assert!(!empty.contains("[1]"), "{empty}");
    let missing = b.command("fg %1");
    assert!(missing.contains("no such job"), "{missing}");
    let stopped = b.command("sh -c 'kill -STOP $$; echo READING_B; read x; echo RESUMED_B'");
    assert!(stopped.contains("[1]+ Stopped"), "{stopped}");
    let jobs = a.command("jobs");
    assert!(
        jobs.contains("Stopped") && jobs.contains("RESUMED_A") && !jobs.contains("RESUMED_B"),
        "{jobs}"
    );
    b.master.write_all(b"fg %1\r").unwrap();
    b.until("READING_B\r\n");
    b.master.write_all(b"hello\r").unwrap();
    let resumed = b.prompt();
    assert!(resumed.contains("RESUMED_B"), "{resumed}");
    let jobs = a.command("jobs");
    assert!(jobs.contains("Stopped"), "{jobs}");
    a.master.write_all(b"fg %1\r").unwrap();
    a.until("READING_A\r\n");
    a.master.write_all(b"hello\r").unwrap();
    let resumed = a.prompt();
    assert!(resumed.contains("RESUMED_A"), "{resumed}");
}
#[test]
fn multiple_stops_jobspec_substring_and_mode_metadata() {
    let mut t = Terminal::new();
    for word in ["FIRST", "SECOND"] {
        let s = t.command(&format!("sh -c 'kill -STOP $$; echo {word}'"));
        assert!(s.contains("Stopped"), "{s}");
    }
    let s = t.command("jobs -l");
    assert!(s.contains("[1]") && s.contains("[2]"), "{s}");
    let s = t.command("fg %?FIRST");
    assert!(s.contains("FIRST") && !s.contains("PlanStale"), "{s}");
    let s = t.command("fg %?SECOND");
    assert!(s.contains("SECOND"), "{s}");
    let s = t.command("help fg");
    assert!(s.contains("fg [jobspec]"), "{s}");
    let s = t.command("type jobs bg wait");
    assert_eq!(s.matches("是 zhsh 内建命令").count(), 3, "{s}");
    let s = t.command("printf nope &");
    assert!(s.contains("不支持"), "{s}");
}

#[test]
fn immediate_notifications_do_not_stall_batched_utf8_input() {
    let mut t = Terminal::new();
    t.command("set -b");
    let s = t.command("sh -c 'kill -STOP $$; exit 0'");
    assert!(s.contains("Stopped"), "{s}");
    let s = t.command("bg %1");
    if !s.contains("Done") {
        t.until("Done");
    }
    let s = t.command("/usr/bin/printf '中文验证\\n'");
    assert!(s.contains("中文验证\r\n"), "{s}");
    let s = t.command("/usr/bin/printf 'still-responsive\\n'");
    assert!(s.contains("still-responsive\r\n"), "{s}");
}

#[test]
fn ctrl_c_interrupts_wait_without_terminating_its_background_job() {
    let mut t = Terminal::new();
    let s = t.command("sh -c 'kill -STOP $$; exec sleep 30'");
    assert!(s.contains("Stopped"));
    t.command("bg %1");
    t.master.write_all(b"wait -f %1\r").unwrap();
    t.until("\x1b[?2004l");
    t.master.write_all(b"\x03").unwrap();
    t.prompt();
    let s = t.command("jobs");
    assert!(s.contains("Running"), "{s}");
}

#[test]
fn nested_native_shell_suspend_returns_terminal_to_parent() {
    let mut t = Terminal::new();
    t.command(&format!("{} --native", env!("CARGO_BIN_EXE_zhsh")));
    let stopped = t.command("suspend");
    assert!(stopped.contains("Stopped"), "{stopped}");
    let jobs = t.command("jobs");
    assert!(
        jobs.contains("--native") && jobs.contains("Stopped"),
        "{jobs}"
    );
    t.command("fg %1");
    // The resumed inner instance cannot see its parent's job record.
    let jobs = t.command("jobs");
    assert!(!jobs.contains("[1]"), "{jobs}");
    t.command("exit");
    let result = t.command("/usr/bin/printf 'parent-restored\\n'");
    assert!(result.contains("parent-restored\r\n"), "{result}");
}

#[test]
fn background_terminal_read_stops_until_fg_restores_the_terminal() {
    let mut t = Terminal::new();
    t.command("sh -c 'kill -STOP $$; read x; echo INPUT_RECEIVED'");
    t.command("bg %1");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let jobs = t.command("jobs");
        if jobs.contains("Stopped") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background reader did not stop: {jobs}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    t.master.write_all(b"fg %1\r").unwrap();
    t.until("\x1b[?2004l");
    // Wait until the foreground group actually belongs to the job.
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::tcgetpgrp(std::os::fd::AsRawFd::as_raw_fd(&t.master)) }
        == t.child.id() as i32
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    t.master.write_all(b"hello\r").unwrap();
    let output = t.prompt();
    assert!(output.contains("INPUT_RECEIVED"), "{output}");
}

#[test]
fn tostop_background_writer_resumes_in_foreground() {
    let mut t = Terminal::with_tostop(true);
    t.command("sh -c 'kill -STOP $$; echo BACKGROUND_WRITE_COMPLETED'");
    t.command("bg %1");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let jobs = t.command("jobs");
        if jobs.contains("Stopped (tty output)") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TOSTOP did not stop writer: {jobs}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = t.command("fg %1");
    assert!(
        output.contains("BACKGROUND_WRITE_COMPLETED\r\n"),
        "{output}"
    );
}

#[test]
fn exit_and_eof_warn_once_for_stopped_jobs() {
    let mut t = Terminal::new();
    t.command("sh -c 'kill -STOP $$; exit 0'");
    // Listing jobs before the first exit must not count as an exit warning.
    assert!(t.command("jobs").contains("Stopped"));
    let warning = t.command("exit");
    assert!(warning.contains("there are active jobs"), "{warning}");
    // jobs does not reset the consecutive-exit warning; EOF shares the same policy.
    assert!(t.command("jobs").contains("Stopped"));
    t.master.write_all(b"\x04").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if t.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "second exit was not accepted");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn stopped_sleep_can_resume_in_background_then_foreground() {
    let mut t = Terminal::new();
    t.master.write_all(b"sleep 100\r").unwrap();
    t.until("\x1b[?2004l");
    std::thread::sleep(Duration::from_millis(100));
    t.master.write_all(b"\x1a").unwrap();
    let stopped = t.prompt();
    assert!(stopped.contains("\r\n[1]+ Stopped"), "{stopped}");
    assert!(!stopped.contains("运行 fg"), "{stopped}");
    let jobs = t.command("jobs");
    assert!(
        jobs.contains("[1]+ Stopped                 sleep 100"),
        "{jobs}"
    );
    let background = t.command("bg");
    assert!(background.contains("[1]+ sleep 100"), "{background}");
    t.master.write_all(b"fg\r").unwrap();
    let mut foreground = t.until("sleep 100\r\n");
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::tcgetpgrp(std::os::fd::AsRawFd::as_raw_fd(&t.master)) }
        == t.child.id() as i32
    {
        assert!(Instant::now() < deadline, "fg did not acquire the terminal");
        std::thread::sleep(Duration::from_millis(5));
    }
    t.master.write_all(b"\x03").unwrap();
    foreground.push_str(&t.prompt());
    assert!(!foreground.contains("stale job"), "{foreground}");
    assert!(!t.command("jobs").contains("sleep 100"));
}
