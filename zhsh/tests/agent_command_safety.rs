#![cfg(unix)]

use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

fn read_http_body(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
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

fn mock_agent(outputs: &[&str]) -> Option<(SocketAddr, thread::JoinHandle<Vec<String>>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            assert_ne!(
                std::env::var("ZHSH_RELEASE_IN_CONTAINER").as_deref(),
                Ok("1"),
                "发布门禁环境必须允许随机 loopback Mock LLM"
            );
            eprintln!("sandbox forbids loopback listeners; skipping Agent command safety test");
            return None;
        }
        Err(error) => panic!("cannot start mock Agent service: {error}"),
    };
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let outputs: Vec<_> = outputs.iter().map(ToString::to_string).collect();
    let server = thread::spawn(move || {
        let mut requests = Vec::with_capacity(outputs.len());
        for output in outputs {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "timed out waiting for the next Agent request"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock Agent listener failed: {error}"),
                }
            };
            requests.push(read_http_body(&mut stream));
            reply(&mut stream, &output);
        }
        requests
    });
    Some((address, server))
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn private_file(path: &Path, contents: impl AsRef<[u8]>) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new(label: &str, address: SocketAddr, rc: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "zhsh-agent-command-safety-{label}-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("home");
        let workspace = root.join("workspace");
        let zhsh = home.join(".zhsh");
        let configs = zhsh.join("llm");
        let plugins = zhsh.join("plugins/llm");
        for directory in [&root, &home, &workspace, &zhsh, &configs, &plugins] {
            private_directory(directory);
        }

        let codec = plugins.join("openai@0.3.0.zhcodec");
        private_file(
            &codec,
            include_bytes!("../assets/llm-codecs/openai@0.3.0.zhcodec"),
        );
        private_file(
            &configs.join("test.llm"),
            format!(
                "NAME=test\nURL=http://{address}\nFORMAT=openai@0.3.0\nACCESS_TOKEN=1234567890123456\nFLASH=test\nSTANDARD=test\nMAX=test\nTIER=flash\n"
            ),
        );
        private_file(&zhsh.join("active-llm"), "test\n");
        if !rc.is_empty() {
            private_file(&home.join(".zhshrc"), rc);
        }

        Self {
            root,
            home,
            workspace,
        }
    }

    fn run(&self, input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
            .current_dir(&self.workspace)
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env_remove("ZHSH_AGENT_TRUST")
            .env(
                "ZHSH_TEST_SYSTEM_CODEC_DIR",
                self.home.join("missing-system-codecs"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_non_tty_rejection(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("未完成"), "{stderr}");
    assert!(stderr.contains("当前终端无法进行命令确认"), "{stderr}");
    assert!(!stderr.contains("失败"), "{stderr}");
}

#[test]
fn destructive_alias_is_rejected_without_a_tty_and_never_executes() {
    let Some((address, server)) =
        mock_agent(&[r#"{"action":"run","purpose":"inspect through alias","command":"probe"}"#])
    else {
        return;
    };
    let fixture = Fixture::new(
        "alias",
        address,
        r#"alias probe='rm -- "$HOME/alias-sentinel"'
"#,
    );
    let sentinel = fixture.home.join("alias-sentinel");
    fs::write(&sentinel, "must remain").unwrap();

    let output = fixture.run("检查别名安全边界\n");
    let requests = server.join().unwrap();

    assert_eq!(requests.len(), 1);
    assert_non_tty_rejection(&output);
    assert!(sentinel.exists(), "rejected alias expansion executed rm");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("probe"), "{stderr}");
}

#[test]
fn writable_path_shadow_is_rejected_without_a_tty_and_never_executes() {
    let Some((address, server)) =
        mock_agent(&[r#"{"action":"run","purpose":"inspect directory","command":"ls"}"#])
    else {
        return;
    };
    let fixture = Fixture::new(
        "path-shadow",
        address,
        "export PATH=\"$HOME/shadow-bin:/usr/bin:/bin\"\n",
    );
    let shadow_bin = fixture.home.join("shadow-bin");
    private_directory(&shadow_bin);
    let shadow = shadow_bin.join("ls");
    fs::write(&shadow, "#!/bin/sh\nrm -- \"$HOME/path-shadow-sentinel\"\n").unwrap();
    fs::set_permissions(&shadow, fs::Permissions::from_mode(0o700)).unwrap();
    let sentinel = fixture.home.join("path-shadow-sentinel");
    fs::write(&sentinel, "must remain").unwrap();

    let output = fixture.run("检查 PATH 解析安全边界\n");
    let requests = server.join().unwrap();

    assert_eq!(requests.len(), 1);
    assert_non_tty_rejection(&output);
    assert!(sentinel.exists(), "rejected PATH shadow executable ran");
}

#[test]
fn balanced_trusted_observation_reaches_the_second_model_round() {
    let Some((address, server)) = mock_agent(&[
        r#"{"action":"run","purpose":"inspect current directory","command":"pwd"}"#,
        r#"{"action":"done","answer":"BALANCED_OBSERVATION_OK"}"#,
    ]) else {
        return;
    };
    let fixture = Fixture::new("balanced-observation", address, "");

    let output = fixture.run("检查当前目录\n");
    let requests = server.join().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("result:exited"), "{}", requests[1]);
    assert!(
        requests[1].contains(&fixture.workspace.to_string_lossy().to_string()),
        "{}",
        requests[1]
    );
    assert!(stderr.contains("BALANCED_OBSERVATION_OK"), "{stderr}");
    assert!(!stderr.contains("确认执行"), "{stderr}");
}
