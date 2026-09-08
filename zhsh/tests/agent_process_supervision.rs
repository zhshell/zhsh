#![cfg(target_os = "linux")]

use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn read_http_body(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let size = stream.read(&mut buffer).unwrap();
        assert!(size > 0, "HTTP request ended before its headers");
        request.extend_from_slice(&buffer[..size]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header_end = end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (header_end, content_length);
        }
    };
    while request.len() < header_end + content_length {
        let size = stream.read(&mut buffer).unwrap();
        assert!(size > 0, "HTTP request body ended early");
        request.extend_from_slice(&buffer[..size]);
    }
    String::from_utf8(request[header_end..header_end + content_length].to_vec()).unwrap()
}

fn reply(stream: &mut TcpStream, model_output: &str) {
    let model_output = serde_json::from_str::<serde_json::Value>(model_output)
        .ok()
        .filter(|value| value.get("action").is_some() && value.get("response").is_none())
        .map(|value| json!({"response": value}).to_string())
        .unwrap_or_else(|| model_output.to_string());
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": model_output}]
        }]
    })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

fn accept_until(listener: &TcpListener, deadline: Instant) -> TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for Agent request"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("mock Agent listener failed: {error}"),
        }
    }
}

fn wait_bounded(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().unwrap() {
            Some(_) => return child.wait_with_output().unwrap(),
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "zhsh did not finish process supervision in {timeout:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

fn read_until(master: &mut File, expected: &[u8], timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    while Instant::now() < deadline {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                output.extend_from_slice(&buffer[..size]);
                if output
                    .windows(expected.len())
                    .any(|window| window == expected)
                {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("failed to read zhsh PTY: {error}"),
        }
    }
    output
}

fn secure_write(path: &std::path::Path, contents: impl AsRef<[u8]>) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn fixture_pid(request: &str) -> i32 {
    let value: Value = serde_json::from_str(request).unwrap();
    let transcript = value["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let pid = transcript
        .rsplit_once("\noutput:\nfixture:")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.lines().next())
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("fixture did not report its PID: {transcript}"));
    assert!(
        transcript.contains("result:background_terminated"),
        "Agent received an unstable termination result: {transcript}"
    );
    pid
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct FixtureProcess(Option<i32>);

impl FixtureProcess {
    fn wait_until_gone(&mut self) -> bool {
        let pid = self.0.expect("fixture PID should be available");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            // SAFETY: signal 0 only probes the fixture PID reported by the fixture itself.
            if unsafe { libc::kill(pid, 0) } < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                self.0 = None;
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let Some(pid) = self.0 else {
            return;
        };
        // SAFETY: PID came from the fixture itself; SIGKILL is test fallback cleanup.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[test]
fn agent_reaps_term_ignoring_descendant_after_leader_exit() {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            assert_ne!(
                std::env::var("ZHSH_RELEASE_IN_CONTAINER").as_deref(),
                Ok("1"),
                "发布门禁环境必须允许随机 loopback Mock LLM"
            );
            eprintln!("sandbox forbids local listeners; skipping Agent supervision test");
            return;
        }
        Err(error) => panic!("cannot start mock Agent service: {error}"),
    };
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    // The helper reaper moves itself to another session but leaves its worker in the Agent PGID.
    // It closes captured pipes, waits/reaps the worker after supervision kills it, and then exits.
    // This avoids relying on PID 1 to reap an orphan before kill(-pgid, 0) can report the group gone.
    let python_source = r#"import os
import signal
import time

ready_r, ready_w = os.pipe()
detached_r, detached_w = os.pipe()
reaper = os.fork()
if reaper == 0:
    worker = os.fork()
    if worker == 0:
        os.close(ready_r)
        os.close(detached_r)
        os.close(detached_w)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        os.write(ready_w, str(os.getpid()).encode() + b'\n')
        os.close(ready_w)
        time.sleep(30)
        os._exit(0)
    os.close(ready_r)
    os.close(ready_w)
    os.close(detached_r)
    os.setsid()
    os.write(detached_w, b'x')
    os.close(detached_w)
    os.close(1)
    os.close(2)
    os.waitpid(worker, 0)
    os._exit(0)

os.close(ready_w)
os.close(detached_w)
worker_pid = os.read(ready_r, 32).decode().strip()
os.close(ready_r)
os.read(detached_r, 1)
os.close(detached_r)
print('fixture:' + worker_pid, flush=True)
os._exit(0)
"#;
    let python = format!("/usr/bin/python3 -c {}", shell_quote(python_source));
    let first = json!({
        "action": "run",
        "purpose": "test captured process supervision",
        "command": python,
    })
    .to_string();
    let second = json!({"action": "done", "answer": "supervision complete"}).to_string();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut requests = Vec::new();
        for response in [first, second] {
            let mut stream = accept_until(&listener, deadline);
            requests.push(read_http_body(&mut stream));
            reply(&mut stream, &response);
        }
        requests
    });

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = std::env::temp_dir().join(format!(
        "zhsh-process-supervision-{}-{unique}",
        std::process::id()
    ));
    let zhsh_dir = home.join(".zhsh");
    let config_dir = zhsh_dir.join("llm");
    let plugin_dir = zhsh_dir.join("plugins/llm");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&plugin_dir).unwrap();
    for directory in [&home, &zhsh_dir, &config_dir, &plugin_dir] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let user_plugin = plugin_dir.join("openai@0.3.0.zhcodec");
    secure_write(
        &user_plugin,
        include_bytes!("../assets/llm-codecs/openai@0.3.0.zhcodec"),
    );
    secure_write(
        &config_dir.join("test.llm"),
        format!(
            "NAME=test\nURL=http://{address}\nFORMAT=openai@0.3.0\nACCESS_TOKEN=1234567890123456\nFLASH=test\nSTANDARD=test\nMAX=test\nTIER=flash\n"
        ),
    );
    secure_write(&zhsh_dir.join("active-llm"), "test\n");

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(opened, 0, "failed to create test PTY");
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );

    let started = Instant::now();
    let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
    command
        .env("HOME", &home)
        .env("ZHSH_AGENT_TRUST", "trusted")
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    let _ = read_until(&mut master, b"$ ", Duration::from_secs(2));
    master.write_all("测试进程监督\r".as_bytes()).unwrap();
    let confirmation = read_until(&mut master, b"[y/N]", Duration::from_secs(3));
    assert!(
        confirmation
            .windows(b"[y/N]".len())
            .any(|window| window == b"[y/N]"),
        "Agent command did not reach confirmation: {}",
        String::from_utf8_lossy(&confirmation)
    );
    master.write_all(b"y\r").unwrap();
    let terminal = read_until(&mut master, b"supervision complete", Duration::from_secs(5));
    assert!(
        terminal
            .windows(b"supervision complete".len())
            .any(|window| window == b"supervision complete"),
        "Agent supervision did not complete: {}",
        String::from_utf8_lossy(&terminal)
    );
    master.write_all(b"exit\r").unwrap();
    let output = wait_bounded(child, Duration::from_secs(5));
    let requests = server.join().unwrap();
    let _ = fs::remove_dir_all(&home);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(requests.len(), 2);
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut fixture = FixtureProcess(Some(fixture_pid(&requests[1])));
    assert!(
        fixture.wait_until_gone(),
        "TERM-ignoring fixture survived TERM-to-KILL supervision"
    );
}
