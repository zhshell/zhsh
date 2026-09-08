#![cfg(unix)]

use serde_json::json;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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

fn read_until(master: &mut File, expected: &[u8], duration: Duration) -> Vec<u8> {
    let deadline = Instant::now() + duration;
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    while Instant::now() < deadline {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                output.extend_from_slice(&buffer[..size]);
                if contains(&output, expected) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("读取伪终端失败: {error}"),
        }
    }
    output
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn wait_until_process_is_gone(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        // SAFETY: signal 0 only probes the PID written by this test's helper process.
        if unsafe { libc::kill(pid, 0) } < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

struct Session {
    master: File,
    child: std::process::Child,
    home: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn start(outputs: &[&str]) -> Option<(Session, thread::JoinHandle<Vec<String>>)> {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            assert_ne!(
                std::env::var("ZHSH_RELEASE_IN_CONTAINER").as_deref(),
                Ok("1"),
                "发布门禁环境必须允许随机 loopback Mock LLM"
            );
            eprintln!("当前沙箱禁止本地监听，跳过 Agent 交互终端测试");
            return None;
        }
        Err(error) => panic!("无法启动模拟 LLM 服务: {error}"),
    };
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let outputs: Vec<_> = outputs.iter().map(ToString::to_string).collect();
    let server = thread::spawn(move || {
        let mut requests = Vec::with_capacity(outputs.len());
        for output in outputs {
            let deadline = Instant::now() + Duration::from_secs(8);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(accepted) => break accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "等待模拟 LLM 请求超时；终端流程可能停在未处理的确认提示"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("接受模拟 LLM 请求失败: {error}"),
                }
            };
            requests.push(read_http_body(&mut stream));
            reply(&mut stream, &output);
        }
        requests
    });

    let mut master_fd = -1;
    let mut slave_fd = -1;
    let window = libc::winsize {
        ws_row: 24,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            &window,
        )
    };
    assert_eq!(opened, 0, "无法创建伪终端");
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = std::env::temp_dir().join(format!(
        "zhsh-agent-interactive-{}-{unique}",
        std::process::id()
    ));
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
        fs::set_permissions(&plugin_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&user_plugin, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config_file = config_dir.join("test.llm");
    fs::write(
        &config_file,
        format!(
            "NAME=test\nURL=http://{address}\nFORMAT=openai@0.3.0\nACCESS_TOKEN=1234567890123456\nFLASH=test\nSTANDARD=test\nMAX=test\nTIER=flash\n"
        ),
    )
    .unwrap();
    let active_file = home.join(".zhsh/active-llm");
    fs::write(&active_file, "test\n").unwrap();
    #[cfg(unix)]
    {
        fs::set_permissions(home.join(".zhsh"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&active_file, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let stdin = slave.try_clone().unwrap();
    let stdout = slave.try_clone().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
    command
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    // SAFETY: pre_exec 中只调用异步信号安全的 libc 系统调用。stdin 已在子进程中
    // 映射到 PTY slave；新会话随后把它设为控制终端。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    Some((
        Session {
            master,
            child,
            home,
        },
        server,
    ))
}

#[test]
fn agent_interactive_command_reads_without_echo_and_returns_terminal() {
    let Some((mut session, server)) = start(&[
        r#"{"action":"run","purpose":"读取交互输入","command":"read -rsp 'Secret: ' secret; printf '\\naccepted\\n'"}"#,
        r#"{"action":"done","answer":"交互完成"}"#,
    ]) else {
        return;
    };

    let mut terminal = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
    session
        .master
        .write_all("测试交互输入\r".as_bytes())
        .unwrap();
    terminal.extend(read_until(
        &mut session.master,
        b"[y/N]",
        Duration::from_secs(3),
    ));
    session.master.write_all(b"y\r").unwrap();
    terminal.extend(read_until(
        &mut session.master,
        b"\r\nSecret: ",
        Duration::from_secs(3),
    ));
    let secret = b"sensitive-value";
    session.master.write_all(secret).unwrap();
    session.master.write_all(b"\r").unwrap();
    terminal.extend(read_until(
        &mut session.master,
        "交互完成".as_bytes(),
        Duration::from_secs(3),
    ));

    let requests = server.join().unwrap();

    let hidden = terminal
        .windows(b"\x1b[?25l".len())
        .position(|window| window == b"\x1b[?25l")
        .expect("等待 LLM 响应时应隐藏光标");
    let shown = terminal
        .windows(b"\x1b[?25h".len())
        .position(|window| window == b"\x1b[?25h")
        .expect("离开等待态时应恢复光标");
    let status = terminal
        .windows("阶段 1".len())
        .position(|window| window == "阶段 1".as_bytes())
        .expect("等待态应显示当前阶段");
    let confirmation = terminal
        .windows(b"[y/N]".len())
        .position(|window| window == b"[y/N]")
        .expect("交互命令应进入确认输入态");
    assert!(hidden < status && status < shown, "等待态必须保持隐藏光标");
    assert!(shown < confirmation, "显示确认输入前必须恢复光标");
    assert!(contains(&terminal, b"Secret: "));
    assert!(contains(&terminal, b"accepted"));
    assert!(contains(&terminal, "交互完成".as_bytes()));
    assert!(
        !contains(&terminal, secret),
        "保密输入被终端回显: {:?}",
        String::from_utf8_lossy(&terminal)
    );
    assert!(requests[1].contains("output_evidence:complete"));
    assert!(requests[1].contains("accepted"));
    assert!(
        !requests[1].contains(std::str::from_utf8(secret).unwrap()),
        "保密输入进入了 Agent result: {}",
        requests[1]
    );
}

#[test]
fn ssh_line_command_is_captured_while_interactive_session_stays_opaque() {
    for (arguments, expected_evidence, model_may_see_output) in [
        ("host status", "output_evidence:complete", true),
        (
            "host status && printf 'compound-visible-result\\n'",
            "output_evidence:complete",
            true,
        ),
        ("host", "output_evidence:unavailable", false),
    ] {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture_root =
            std::env::temp_dir().join(format!("zhsh-ssh-evidence-{}-{unique}", std::process::id()));
        fs::create_dir_all(&fixture_root).unwrap();
        let ssh = fixture_root.join("ssh");
        fs::write(&ssh, "#!/bin/sh\nprintf 'remote-visible-result\\n'\n").unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        let command = format!("{} {arguments}", ssh.display());
        let run = json!({
            "action": "run",
            "purpose": "检查远端返回",
            "command": command,
        })
        .to_string();
        let done = json!({"action": "done", "answer": "远端检查结束"}).to_string();
        let Some((mut session, server)) = start(&[run.as_str(), done.as_str()]) else {
            let _ = fs::remove_dir_all(&fixture_root);
            return;
        };

        let _ = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
        session
            .master
            .write_all("运行远端会话测试\r".as_bytes())
            .unwrap();
        let confirmation = read_until(&mut session.master, b"[y/N]", Duration::from_secs(3));
        assert!(contains(&confirmation, b"[y/N]"));
        session.master.write_all(b"y\r").unwrap();
        let terminal = read_until(
            &mut session.master,
            "远端检查结束".as_bytes(),
            Duration::from_secs(3),
        );
        let requests = server.join().unwrap();
        let _ = fs::remove_dir_all(&fixture_root);

        assert!(contains(&terminal, b"remote-visible-result"));
        assert!(requests[1].contains(expected_evidence), "{}", requests[1]);
        assert_eq!(
            requests[1].contains("remote-visible-result"),
            model_may_see_output,
            "{}",
            requests[1]
        );
        if arguments.contains("compound-visible-result") {
            assert!(contains(&terminal, b"compound-visible-result"));
            assert!(requests[1].contains("compound-visible-result"));
        }
    }
}

#[test]
fn foreground_evidence_limit_does_not_terminate_the_command() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture_root = std::env::temp_dir().join(format!(
        "zhsh-foreground-output-limit-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&fixture_root).unwrap();
    let ssh = fixture_root.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nhead -c 70000 /dev/zero | tr '\\000' x\nprintf '\\nlarge-output-finished\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let command = format!(
        "{} host status && printf '\\ncompound-after-large-output\\n'",
        ssh.display()
    );
    let run = json!({
        "action": "run",
        "purpose": "验证前台输出限制",
        "command": command,
    })
    .to_string();
    let done = json!({"action": "done", "answer": "大输出命令已结束"}).to_string();
    let Some((mut session, server)) = start(&[run.as_str(), done.as_str()]) else {
        let _ = fs::remove_dir_all(&fixture_root);
        return;
    };

    let _ = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
    session
        .master
        .write_all("运行大输出前台测试\r".as_bytes())
        .unwrap();
    let confirmation = read_until(&mut session.master, b"[y/N]", Duration::from_secs(3));
    assert!(contains(&confirmation, b"[y/N]"));
    session.master.write_all(b"y\r").unwrap();
    let terminal = read_until(
        &mut session.master,
        "大输出命令已结束".as_bytes(),
        Duration::from_secs(5),
    );
    let requests = server.join().unwrap();
    let _ = fs::remove_dir_all(&fixture_root);

    assert!(contains(&terminal, b"large-output-finished"));
    assert!(contains(&terminal, b"compound-after-large-output"));
    assert!(contains(&terminal, "大输出命令已结束".as_bytes()));
    assert!(requests[1].contains("result:exited"));
    assert!(requests[1].contains("output_evidence:truncated"));
    assert!(requests[1].contains("large-output-finished"));
}

#[test]
fn ctrl_c_cancels_an_agent_foreground_interaction_and_restores_repl() {
    let Some((mut session, server)) = start(&[
        r#"{"action":"run","purpose":"读取交互输入","command":"read -rp 'Value: ' value"}"#,
    ]) else {
        return;
    };

    let mut terminal = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
    session
        .master
        .write_all("测试取消交互\r".as_bytes())
        .unwrap();
    terminal.extend(read_until(
        &mut session.master,
        b"[y/N]",
        Duration::from_secs(3),
    ));
    session.master.write_all(b"y\r").unwrap();
    terminal.extend(read_until(
        &mut session.master,
        b"\r\nValue: ",
        Duration::from_secs(3),
    ));
    session.master.write_all(b"\x03").unwrap();
    terminal.extend(read_until(
        &mut session.master,
        "已取消".as_bytes(),
        Duration::from_secs(3),
    ));

    server.join().unwrap();
    assert!(contains(&terminal, b"Value: "));
    assert!(contains(&terminal, "已取消".as_bytes()));
}

#[test]
fn ctrl_z_terminates_a_stopped_agent_foreground_group_and_restores_repl() {
    let Some((mut session, server)) = start(&[
        r#"{"action":"run","purpose":"读取交互输入","command":"read -rp 'AgentValue: ' value"}"#,
        r#"{"action":"done","answer":"终端已收回"}"#,
    ]) else {
        return;
    };

    let _ = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
    session
        .master
        .write_all("测试暂停交互\r".as_bytes())
        .unwrap();
    let confirmation = read_until(&mut session.master, b"[y/N]", Duration::from_secs(3));
    assert!(contains(&confirmation, b"[y/N]"));
    session.master.write_all(b"y\r").unwrap();
    let prompt = read_until(
        &mut session.master,
        b"\r\nAgentValue: ",
        Duration::from_secs(3),
    );
    assert!(contains(&prompt, b"AgentValue: "));
    session.master.write_all(b"\x1a").unwrap();
    let terminal = read_until(
        &mut session.master,
        "终端已收回".as_bytes(),
        Duration::from_secs(3),
    );

    let requests = server.join().unwrap();
    assert!(contains(&terminal, "终端已收回".as_bytes()));
    assert!(
        requests[1].contains("result:stopped_terminated"),
        "Agent 未收到停止态清理结果: {}",
        requests[1]
    );
}

#[test]
fn both_interactive_execution_paths_clean_residual_process_groups() {
    for direct_external in [false, true] {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture_root = std::env::temp_dir().join(format!(
            "zhsh-interactive-supervision-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&fixture_root).unwrap();
        let executable = fixture_root.join(if direct_external { "ssh" } else { "helper" });
        let pid_file = fixture_root.join("worker.pid");
        fs::write(
            &executable,
            "#!/usr/bin/env bash\n\
             (trap '' TERM; printf '%s' \"$BASHPID\" >\"$1\"; while :; do sleep 30; done) &\n\
             while [[ ! -s \"$1\" ]]; do :; done\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let command = if direct_external {
            format!("{} {}", executable.display(), pid_file.display())
        } else {
            // `read` selects the prepared Bash terminal path. The helper hides its background
            // implementation from the Agent command preflight so runtime supervision is tested.
            format!("read -t 0; {} {}", executable.display(), pid_file.display())
        };
        let run = json!({
            "action": "run",
            "purpose": "test interactive residual process cleanup",
            "command": command,
        })
        .to_string();
        let done = json!({"action": "done", "answer": "cleanup complete"}).to_string();
        let Some((mut session, server)) = start(&[run.as_str(), done.as_str()]) else {
            let _ = fs::remove_dir_all(&fixture_root);
            return;
        };

        let _ = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
        session
            .master
            .write_all("测试交互进程组清理\r".as_bytes())
            .unwrap();
        let confirmation = read_until(&mut session.master, b"[y/N]", Duration::from_secs(3));
        assert!(
            contains(&confirmation, b"[y/N]"),
            "interactive fixture was not confirmed: {}",
            String::from_utf8_lossy(&confirmation)
        );
        session.master.write_all(b"y\r").unwrap();
        let terminal = read_until(
            &mut session.master,
            b"cleanup complete",
            Duration::from_secs(4),
        );
        let requests = server.join().unwrap();
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = wait_until_process_is_gone(pid);
        if !gone {
            // SAFETY: pid came from this test fixture and is only a fallback cleanup.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = fs::remove_dir_all(&fixture_root);

        assert!(contains(&terminal, b"cleanup complete"));
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].contains("result:background_terminated"),
            "unexpected Agent feedback for {command}: {}",
            requests[1]
        );
        assert!(gone, "interactive worker {pid} survived for {command}");
    }
}

#[test]
fn destructive_agent_command_requires_explicit_terminal_confirmation() {
    let Some((mut session, server)) =
        start(&[r#"{"action":"run","purpose":"删除测试文件","command":"rm -- \"$HOME/victim\""}"#])
    else {
        return;
    };
    let victim = session.home.join("victim");
    fs::write(&victim, "must remain").unwrap();

    let mut terminal = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
    session
        .master
        .write_all("删除测试文件\r".as_bytes())
        .unwrap();
    terminal.extend(read_until(
        &mut session.master,
        b"[y/N]",
        Duration::from_secs(3),
    ));
    session.master.write_all(b"n\r").unwrap();
    terminal.extend(read_until(
        &mut session.master,
        "已取消".as_bytes(),
        Duration::from_secs(3),
    ));

    server.join().unwrap();
    assert!(contains(&terminal, "将删除、覆盖或移动现有状态".as_bytes()));
    assert!(contains(&terminal, b"[y/N]"));
    assert!(contains(&terminal, "已取消".as_bytes()));
    assert!(!contains(&terminal, "失败".as_bytes()));
    assert!(victim.exists());
}

#[test]
fn ctrl_c_cancels_agent_command_confirmation_without_executing() {
    let Some((mut session, server)) =
        start(&[r#"{"action":"run","purpose":"删除测试文件","command":"rm -- \"$HOME/victim\""}"#])
    else {
        return;
    };
    let victim = session.home.join("victim");
    fs::write(&victim, "must remain").unwrap();

    let mut terminal = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
    session
        .master
        .write_all("删除测试文件\r".as_bytes())
        .unwrap();
    terminal.extend(read_until(
        &mut session.master,
        b"[y/N]",
        Duration::from_secs(3),
    ));
    session.master.write_all(b"\x03").unwrap();
    terminal.extend(read_until(
        &mut session.master,
        "已取消".as_bytes(),
        Duration::from_secs(3),
    ));

    server.join().unwrap();
    assert!(contains(&terminal, "已取消".as_bytes()));
    assert!(victim.exists());
}

#[test]
fn clarification_shows_free_input_with_options_and_accepts_it_as_an_alternative() {
    let Some((mut session, server)) = start(&[
        r#"{"action":"clarify","questions":[{"id":"pattern","prompt":"请提供模式？","multiple":false,"choices":[{"id":"suffix","label":"按文件后缀"}]}]}"#,
        r#"{"action":"done","answer":"模式已记录"}"#,
    ]) else {
        return;
    };

    let _ = read_until(&mut session.master, b"$ ", Duration::from_secs(2));
    session
        .master
        .write_all("执行模式匹配\r".as_bytes())
        .unwrap();
    let question = read_until(
        &mut session.master,
        "自由输入（可替代选项）: ".as_bytes(),
        Duration::from_secs(3),
    );
    assert!(contains(&question, "按文件后缀".as_bytes()));

    let mut input_render = Vec::new();
    let split_character = "中".as_bytes();
    session.master.write_all(&split_character[..1]).unwrap();
    thread::sleep(Duration::from_millis(10));
    session.master.write_all(&split_character[1..]).unwrap();
    input_render.extend(read_until(
        &mut session.master,
        "中".as_bytes(),
        Duration::from_secs(1),
    ));
    session.master.write_all("文".as_bytes()).unwrap();
    input_render.extend(read_until(
        &mut session.master,
        "文".as_bytes(),
        Duration::from_secs(1),
    ));
    session.master.write_all(b"\x7f").unwrap();
    input_render.extend(read_until(
        &mut session.master,
        b"\x08 \x08\x08 \x08",
        Duration::from_secs(1),
    ));
    session.master.write_all(b"\r").unwrap();
    let terminal = read_until(
        &mut session.master,
        "模式已记录".as_bytes(),
        Duration::from_secs(3),
    );

    let requests = server.join().unwrap();
    assert!(
        contains(&input_render, b"\x08 \x08\x08 \x08"),
        "一次退格应清除中文字符占用的两个终端列: {:?}",
        String::from_utf8_lossy(&input_render)
    );
    assert_eq!(
        input_render
            .windows("中".len())
            .filter(|part| *part == "中".as_bytes())
            .count(),
        1,
        "澄清输入不应被重复回显: {:?}",
        String::from_utf8_lossy(&input_render)
    );
    assert_eq!(
        input_render
            .windows("文".len())
            .filter(|part| *part == "文".as_bytes())
            .count(),
        1,
        "澄清输入不应被重复回显: {:?}",
        String::from_utf8_lossy(&input_render)
    );
    assert!(contains(&terminal, "模式已记录".as_bytes()));
    assert!(
        contains(&terminal, b"\x1b[J"),
        "提交澄清后应清除临时选项和自由输入"
    );

    let request: serde_json::Value = serde_json::from_str(&requests[1]).unwrap();
    let clarification = request["input"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(
        clarification.contains(r#""selected_choice_ids":[],"free_text":"中""#),
        "直接输入替代答案时不应强制提交当前选项: {clarification}"
    );
}
