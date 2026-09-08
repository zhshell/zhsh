#![cfg(unix)]

use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Request {
    headers: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Request {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let size = stream.read(&mut buffer).unwrap();
        assert!(size > 0, "HTTP 请求头提前结束");
        request.extend_from_slice(&buffer[..size]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header_end = end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (header_end, length);
        }
    };
    while request.len() < header_end + content_length {
        let size = stream.read(&mut buffer).unwrap();
        assert!(size > 0, "HTTP 请求体提前结束");
        request.extend_from_slice(&buffer[..size]);
    }
    Request {
        headers: String::from_utf8_lossy(&request[..header_end]).into_owned(),
        body: String::from_utf8(request[header_end..header_end + content_length].to_vec()).unwrap(),
    }
}

fn reply(stream: &mut TcpStream, content: &str) {
    let content = serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .filter(|value| value.get("action").is_some() && value.get("response").is_none())
        .map(|value| json!({"response": value}).to_string())
        .unwrap_or_else(|| content.to_string());
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": content}]
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

#[test]
fn known_token_reaches_only_the_authorization_header_not_the_next_transcript() {
    const TOKEN: &str = "secret-token-1234";
    const INACTIVE_TOKEN: &str = "inactive-secret-5678";
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            assert_ne!(
                std::env::var("ZHSH_RELEASE_IN_CONTAINER").as_deref(),
                Ok("1"),
                "发布门禁环境必须允许随机 loopback Mock LLM"
            );
            eprintln!("当前沙箱禁止本地监听，跳过 Agent 脱敏集成测试");
            return;
        }
        Err(error) => panic!("无法启动模拟 LLM 服务: {error}"),
    };
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in [
            r#"{"action":"run","purpose":"验证终端和反馈脱敏","command":"type inactive-secret-5678"}"#,
            r#"{"action":"done","answer":"AGENT_REDACTION_OK"}"#,
        ] {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return requests;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("模拟 LLM 服务失败: {error}"),
                }
            };
            requests.push(read_request(&mut stream));
            reply(&mut stream, response);
        }
        requests
    });

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = std::env::temp_dir().join(format!(
        "zhsh-agent-redaction-{}-{unique}",
        std::process::id()
    ));
    let configs = home.join(".zhsh/llm");
    let plugins = home.join(".zhsh/plugins/llm");
    fs::create_dir_all(&configs).unwrap();
    fs::create_dir_all(&plugins).unwrap();
    let codec = plugins.join("openai@0.3.0.zhcodec");
    fs::write(
        &codec,
        include_bytes!("../assets/llm-codecs/openai@0.3.0.zhcodec"),
    )
    .unwrap();
    let config = configs.join("test.llm");
    fs::write(
        &config,
        format!(
            "NAME=test\nURL=http://{address}\nFORMAT=openai@0.3.0\nACCESS_TOKEN={TOKEN}\nFLASH=test\nSTANDARD=test\nMAX=test\nTIER=flash\n"
        ),
    )
    .unwrap();
    let inactive_config = configs.join("inactive.llm");
    fs::write(
        &inactive_config,
        format!(
            "NAME=inactive\nURL=http://{address}\nFORMAT=openai@0.3.0\nACCESS_TOKEN={INACTIVE_TOKEN}\nFLASH=test\nSTANDARD=test\nMAX=test\nTIER=flash\n"
        ),
    )
    .unwrap();
    let active = home.join(".zhsh/active-llm");
    fs::write(&active, "test\n").unwrap();
    for directory in [home.join(".zhsh"), configs, plugins] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for file in [codec, config, inactive_config, active] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .current_dir(&home)
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            home.join("missing-system-codecs"),
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
        .write_all("验证反馈脱敏\n".as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let requests = server.join().unwrap();
    let terminal = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "Agent 脱敏流程失败: {terminal}");
    assert_eq!(requests.len(), 2, "Agent 未完成第二轮请求: {terminal}");
    assert!(requests.iter().all(|request| {
        request
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {TOKEN}"))
    }));
    assert!(requests.iter().all(|request| !request.body.contains(TOKEN)));
    assert!(requests
        .iter()
        .all(|request| !request.body.contains(INACTIVE_TOKEN)));
    assert!(requests[1].body.contains("[zhsh: 已脱敏 LLM access-token]"));
    assert!(terminal.contains("AGENT_REDACTION_OK"), "{terminal}");
    assert!(!terminal.contains(TOKEN));
    assert!(!terminal.contains(INACTIVE_TOKEN));
    let _ = fs::remove_dir_all(home);
}
