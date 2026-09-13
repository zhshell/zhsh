#![cfg(unix)]

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
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
        assert!(size > 0, "HTTP 请求在请求头完成前结束");
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
        assert!(size > 0, "HTTP 请求体提前结束");
        request.extend_from_slice(&buffer[..size]);
    }
    String::from_utf8(request[header_end..header_end + content_length].to_vec()).unwrap()
}

fn reply(stream: &mut TcpStream, model_output: &str) {
    let model_output = serde_json::from_str::<Value>(model_output)
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

#[test]
fn sixth_run_is_rejected_without_a_seventh_request() {
    run_request_limit(false);
}

#[test]
fn native_run_finishes_after_one_request() {
    run_request_limit(true);
}

fn run_request_limit(native: bool) {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            assert_ne!(
                std::env::var("ZHSH_RELEASE_IN_CONTAINER").as_deref(),
                Ok("1"),
                "发布门禁环境必须允许随机 loopback Mock LLM"
            );
            eprintln!("当前沙箱禁止本地监听，跳过严格六轮 HTTP 集成测试");
            return;
        }
        Err(error) => panic!("无法启动模拟 LLM 服务: {error}"),
    };
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let hard_deadline = Instant::now() + Duration::from_secs(5);
        let mut quiet_deadline = None;
        let mut request_bodies = Vec::new();
        while request_bodies.len() < 7 && Instant::now() < quiet_deadline.unwrap_or(hard_deadline) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    request_bodies.push(read_http_body(&mut stream));
                    reply(
                        &mut stream,
                        r#"{"action":"run","purpose":"执行测试命令","command":"/usr/bin/true"}"#,
                    );
                    if request_bodies.len() == if native { 1 } else { 6 } {
                        quiet_deadline = Some(Instant::now() + Duration::from_millis(300));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("模拟 LLM 服务失败: {error}"),
            }
        }
        request_bodies
    });

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home =
        std::env::temp_dir().join(format!("zhsh-final-repair-{}-{unique}", std::process::id()));
    let config_dir = home.join(".zhsh/llm");
    fs::create_dir_all(&config_dir).unwrap();
    let plugin_dir = home.join(".zhsh/plugins/llm");
    fs::create_dir_all(&plugin_dir).unwrap();
    let user_plugin = plugin_dir.join("openai@0.3.0.zhcodec");
    fs::write(
        &user_plugin,
        include_bytes!("../assets/llm-codecs/openai@0.3.0.zhcodec"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&plugin_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&user_plugin, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config_file = config_dir.join("test.llm");
    fs::write(
        &config_file,
        format!(
            "NAME=test\nURL=http://{address}\nFORMAT=openai@0.3.0\nACCESS_TOKEN=\nFLASH=test\nSTANDARD=test\nMAX=test\nTIER=flash\n"
        ),
    )
    .unwrap();
    let active_file = home.join(".zhsh/active-llm");
    fs::write(&active_file, "test\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(home.join(".zhsh"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&active_file, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
    if native {
        command.arg("--native");
    }
    let mut child = command
        .env("HOME", &home)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
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
        .write_all("检查系统状态\n".as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let requests = server.join().unwrap();
    let _ = fs::remove_dir_all(&home);
    let terminal = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    if native {
        assert_eq!(requests.len(), 1, "{terminal}");
        assert!(
            terminal.contains("Native 空执行路径尚未执行命令"),
            "{terminal}"
        );
        assert!(!terminal.contains("> /usr/bin/true"));
        assert!(!terminal.contains("结果已反馈模型"));
        return;
    }

    assert_eq!(
        requests.len(),
        6,
        "任何路径都不得发起第七次模型请求；终端输出: {terminal}"
    );
    assert_eq!(
        terminal.matches("> /usr/bin/true").count(),
        6,
        "六轮命令都应完整展示，但第六轮不得执行"
    );
    assert!(!terminal.contains("执行测试命令"));
    assert!(!terminal.contains("退出码"));
    assert!(!terminal.contains("结果已反馈模型"));
    assert!(terminal.contains("第6轮禁止执行新命令"));
    assert!(
        terminal.contains("未完成 · 第6轮禁止执行新命令"),
        "终端输出: {terminal}"
    );
    assert!(terminal.contains("6轮"));
    assert!(!terminal.contains("失败"));
    assert!(!terminal.contains("格式修复"));
    assert!(!terminal.contains('\u{1b}'), "非 TTY 输出不得包含 ANSI");
    assert!(
        terminal
            .lines()
            .filter(|line| line.contains("> /usr/bin/true"))
            .all(|line| !line.starts_with(' ')),
        "顶层 Agent 事件必须从第一列开始: {terminal}"
    );

    let sixth: Value = serde_json::from_str(&requests[5]).unwrap();
    assert!(sixth["instructions"]
        .as_str()
        .unwrap()
        .contains("最后一轮禁止返回 run"));
    assert!(sixth["instructions"]
        .as_str()
        .unwrap()
        .contains("不会再发起格式修复或其他模型请求"));
}
