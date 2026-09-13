#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
                std::thread::sleep(Duration::from_millis(5));
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

#[test]
fn prompt_ignores_ctrl_z_and_fg_resumes_one_stopped_foreground_command() {
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

    let mut master = unsafe { File::from_raw_fd(master_fd) };
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
        "zhsh-foreground-pty-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&home).unwrap();

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
    // SAFETY: 子进程中只调用异步信号安全的 setsid/ioctl；PTY slave 已映射到 stdin。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();

    let startup = read_until(&mut master, b"$ ", Duration::from_secs(2));
    assert!(contains(&startup, b"$ "), "未读取到 zhsh 提示符");

    master.write_all(b"\x1a").unwrap();
    master.write_all(b"printf PROMPT_ALIVE\r").unwrap();
    let prompt_alive = read_until(&mut master, b"PROMPT_ALIVE", Duration::from_secs(2));
    assert!(
        contains(&prompt_alive, b"PROMPT_ALIVE"),
        "提示符处 Ctrl-Z 暂停了 zhsh: {:?}",
        String::from_utf8_lossy(&prompt_alive)
    );
    let _ = read_until(&mut master, b"$ ", Duration::from_secs(2));

    master
        .write_all(b"sh -c 'printf JOB_READY; read value; printf \"RESUMED:%s\\n\" \"$value\"'\r")
        .unwrap();
    let ready = read_until(&mut master, b"JOB_READY", Duration::from_secs(2));
    assert!(contains(&ready, b"JOB_READY"));
    master.write_all(b"\x1a").unwrap();
    let stopped = read_until(
        &mut master,
        "运行 fg 恢复".as_bytes(),
        Duration::from_secs(2),
    );
    assert!(
        contains(&stopped, "运行 fg 恢复".as_bytes()),
        "前台命令停止后没有归还提示符: {:?}",
        String::from_utf8_lossy(&stopped)
    );
    let _ = read_until(&mut master, b"$ ", Duration::from_secs(2));

    master.write_all(b"fg\r").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    master.write_all(b"continued\r").unwrap();
    let resumed = read_until(&mut master, b"RESUMED:continued", Duration::from_secs(2));

    let child_pid = child.id() as libc::pid_t;
    // SAFETY: child_pid 是本测试创建的独立会话/进程组 leader。
    unsafe {
        libc::kill(-child_pid, libc::SIGKILL);
    }
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        contains(&resumed, b"RESUMED:continued"),
        "fg 没有恢复暂停的整行命令: {:?}",
        String::from_utf8_lossy(&resumed)
    );
}

#[test]
fn interactive_mode_uses_ascii_and_non_ascii_first_character_routing() {
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

    let mut master = unsafe { File::from_raw_fd(master_fd) };
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
    let home = std::env::temp_dir().join(format!("zhsh-route-pty-{}-{unique}", std::process::id()));
    std::fs::create_dir(&home).unwrap();

    let stdin = slave.try_clone().unwrap();
    let stdout = slave.try_clone().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();

    let startup = read_until(&mut master, b"$ ", Duration::from_secs(1));
    assert!(contains(&startup, b"$ "), "未读取到 zhsh 提示符");

    let shell_input = "printf 'shell-中文参数'\r";
    let written = unsafe {
        libc::write(
            master.as_raw_fd(),
            shell_input.as_ptr().cast::<libc::c_void>(),
            shell_input.len(),
        )
    };
    assert_eq!(written, shell_input.len() as isize);
    let shell_marker = "\r\nshell-中文参数";
    let shell_output = read_until(&mut master, shell_marker.as_bytes(), Duration::from_secs(1));
    assert!(
        contains(&shell_output, shell_marker.as_bytes()),
        "ASCII 首字符输入没有进入 Shell: {:?}",
        String::from_utf8_lossy(&shell_output)
    );

    let prompt = if contains(&shell_output, b"$ ") {
        Vec::new()
    } else {
        read_until(&mut master, b"$ ", Duration::from_secs(1))
    };
    assert!(
        contains(&shell_output, b"$ ") || contains(&prompt, b"$ "),
        "Shell 完成后未恢复提示符"
    );
    let agent_input = "Проверить систему\r";
    let written = unsafe {
        libc::write(
            master.as_raw_fd(),
            agent_input.as_ptr().cast::<libc::c_void>(),
            agent_input.len(),
        )
    };
    assert_eq!(written, agent_input.len() as isize);
    let agent_output = read_until(
        &mut master,
        "Agent 不可用".as_bytes(),
        Duration::from_secs(2),
    );

    let _ = child.kill();
    let _ = child.wait();
    drop(master);
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        contains(&agent_output, "Agent 不可用".as_bytes()),
        "非 ASCII 首字符输入没有进入 Agent: {:?}",
        String::from_utf8_lossy(&agent_output)
    );
    assert!(!contains(&agent_output, b"command not found"));
}

#[test]
fn renders_a_batched_utf8_input_without_waiting_for_another_key() {
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

    let mut master = unsafe { File::from_raw_fd(master_fd) };
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
    let home = std::env::temp_dir().join(format!("zhsh-pty-{}-{unique}", std::process::id()));
    std::fs::create_dir(&home).unwrap();

    let stdin = slave.try_clone().unwrap();
    let stdout = slave.try_clone().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();

    let startup = read_until(&mut master, b"$ ", Duration::from_secs(1));
    assert!(contains(&startup, b"$ "), "未读取到 zhsh 提示符");

    // 输入法会在一次 write 中提交整段 UTF-8。曾经启用的 rustyline
    // signal-hook 路径只消费第一个字符，余下字符要等下一次按键才会显示。
    let input = "检查系统状态，快速响应".as_bytes();
    let written = unsafe {
        libc::write(
            master.as_raw_fd(),
            input.as_ptr().cast::<libc::c_void>(),
            input.len(),
        )
    };
    assert_eq!(written, input.len() as isize);

    let rendered = read_until(&mut master, input, Duration::from_secs(1));
    let _ = child.kill();
    let _ = child.wait();
    drop(master);
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        contains(&rendered, input),
        "整批 UTF-8 输入未立即完整显示: {:?}",
        String::from_utf8_lossy(&rendered)
    );
}

#[test]
fn history_builtin_reads_persisted_user_inputs() {
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

    let mut master = unsafe { File::from_raw_fd(master_fd) };
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
    let home =
        std::env::temp_dir().join(format!("zhsh-history-pty-{}-{unique}", std::process::id()));
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join(".zh_history"),
        "first-user-input\nsecond-user-input\n",
    )
    .unwrap();

    let stdin = slave.try_clone().unwrap();
    let stdout = slave.try_clone().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();

    let startup = read_until(&mut master, b"$ ", Duration::from_secs(1));
    assert!(contains(&startup, b"$ "), "未读取到 zhsh 提示符");

    let input = b"history 2\r";
    let written = unsafe {
        libc::write(
            master.as_raw_fd(),
            input.as_ptr().cast::<libc::c_void>(),
            input.len(),
        )
    };
    assert_eq!(written, input.len() as isize);

    let rendered = read_until(&mut master, b"3  history 2", Duration::from_secs(1));
    let _ = child.kill();
    let _ = child.wait();
    drop(master);
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        contains(&rendered, b"2  second-user-input"),
        "未显示已持久化的用户输入: {:?}",
        String::from_utf8_lossy(&rendered)
    );
    assert!(contains(&rendered, b"3  history 2"));
    assert!(!contains(&rendered, b"1  first-user-input"));
}

#[test]
fn tab_completion_escapes_a_directory_with_spaces_before_cd() {
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

    let mut master = unsafe { File::from_raw_fd(master_fd) };
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
    let root = std::env::temp_dir().join(format!(
        "zhsh-completion-pty-{}-{unique}",
        std::process::id()
    ));
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(workspace.join("Season 23")).unwrap();

    let stdin = slave.try_clone().unwrap();
    let stdout = slave.try_clone().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .current_dir(&workspace)
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();

    let startup = read_until(&mut master, b"$ ", Duration::from_secs(1));
    assert!(contains(&startup, b"$ "), "未读取到 zhsh 提示符");

    let input = b"cd Sea\t\r";
    let written = unsafe {
        libc::write(
            master.as_raw_fd(),
            input.as_ptr().cast::<libc::c_void>(),
            input.len(),
        )
    };
    assert_eq!(written, input.len() as isize);

    let rendered = read_until(&mut master, b"Season 23\x1b[0m$ ", Duration::from_secs(2));
    let _ = child.kill();
    let _ = child.wait();
    drop(master);
    let _ = std::fs::remove_dir_all(root);

    assert!(
        contains(&rendered, b"cd Season\\ 23/"),
        "补全结果没有转义空格: {:?}",
        String::from_utf8_lossy(&rendered)
    );
    assert!(
        contains(&rendered, b"Season 23\x1b[0m$ "),
        "cd 没有进入补全目录: {:?}",
        String::from_utf8_lossy(&rendered)
    );
    assert!(!contains(&rendered, "参数过多".as_bytes()));
}

#[test]
fn native_interrupt_stop_and_fg_preserve_terminal_control() {
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

    let mut master = unsafe { File::from_raw_fd(master_fd) };
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
        "zhsh-native-foreground-pty-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&home).unwrap();

    let stdin = slave.try_clone().unwrap();
    let stdout = slave.try_clone().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
    command
        .arg("--native")
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    // SAFETY: 子进程中只调用异步信号安全的 setsid/ioctl；PTY slave 已映射到 stdin。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();

    let startup = read_until(&mut master, b"$ ", Duration::from_secs(2));
    assert!(contains(&startup, b"$ "), "未读取到 zhsh 提示符");

    // Wait for the real foreground process group rather than the echoed command text.
    for signal in *b"\x03\x1a" {
        master.write_all(b"sleep 30\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::tcgetpgrp(master.as_raw_fd()) } == child.id() as i32
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(
            unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
            child.id() as i32
        );
        master.write_all(&[signal]).unwrap();
        let returned = read_until(&mut master, b"$ ", Duration::from_secs(3));
        assert!(
            contains(&returned, b"$ "),
            "{}",
            String::from_utf8_lossy(&returned)
        );
        if signal == b'\x1a' {
            assert!(contains(&returned, "运行 fg 恢复".as_bytes()));
            master.write_all(b"fg\r").unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while unsafe { libc::tcgetpgrp(master.as_raw_fd()) } == child.id() as i32
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_ne!(
                unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
                child.id() as i32
            );
            master.write_all(b"\x03").unwrap();
            let resumed = read_until(&mut master, b"$ ", Duration::from_secs(3));
            assert!(contains(&resumed, b"$ "));
        }
        assert_eq!(
            unsafe { libc::tcgetpgrp(master.as_raw_fd()) },
            child.id() as i32
        );
    }
    let child_pid = child.id() as libc::pid_t;
    // SAFETY: child_pid 是本测试创建的独立会话/进程组 leader。
    unsafe {
        libc::kill(-child_pid, libc::SIGKILL);
    }
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);
}
