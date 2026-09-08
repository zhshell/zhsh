#![cfg(unix)]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
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
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("读取伪终端失败: {error}"),
        }
    }
    output
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

fn install_user_codec(home: &Path) {
    let directory = home.join(".zhsh/plugins/llm");
    fs::create_dir_all(&directory).unwrap();
    let artifact = directory.join("openai@0.3.0.zhcodec");
    fs::write(
        &artifact,
        include_bytes!("../assets/llm-codecs/openai@0.3.0.zhcodec"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&artifact, fs::Permissions::from_mode(0o600)).unwrap();
}

fn temporary_home(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = std::env::temp_dir().join(format!(
        "zhsh-llm-wizard-{label}-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&home).unwrap();
    install_user_codec(&home);
    home
}

fn start(label: &str) -> Session {
    start_with_home(temporary_home(label))
}

fn append_until(session: &mut Session, output: &mut Vec<u8>, expected: &str) {
    let chunk = read_until(
        &mut session.master,
        expected.as_bytes(),
        Duration::from_secs(3),
    );
    assert!(
        chunk
            .windows(expected.len())
            .any(|window| window == expected.as_bytes()),
        "终端未出现 {expected:?}: {}",
        String::from_utf8_lossy(&chunk)
    );
    output.extend(chunk);
}

#[test]
fn write_quit_saves_an_incomplete_draft_from_the_first_editable_field() {
    let mut session = start("draft");
    let mut terminal = Vec::new();
    append_until(&mut session, &mut terminal, "$ ");
    session.master.write_all(b"zh llm draft\r").unwrap();

    append_until(&mut session, &mut terminal, "› URL=");
    session.master.write_all(b"\x1b:wq\r").unwrap();
    append_until(&mut session, &mut terminal, "配置草稿已保存: draft");

    let rendered = String::from_utf8_lossy(&terminal);
    assert!(rendered.contains("配置不完整，尚不能用于 Agent"));
    let config = fs::read_to_string(session.home.join(".zhsh/llm/draft.llm")).unwrap();
    assert!(config.contains("NAME=draft\n"));
    assert!(config.contains("FORMAT=openai@0.3.0\n"));
    assert!(config.contains("URL=\n"));
}

#[test]
fn create_allows_an_empty_token_for_a_local_http_provider() {
    let mut session = start("empty-token");
    let mut terminal = Vec::new();
    append_until(&mut session, &mut terminal, "$ ");
    session.master.write_all(b"zh llm anonymous\r").unwrap();

    append_until(&mut session, &mut terminal, "› URL=");
    session
        .master
        .write_all(b"http://127.0.0.1:11434\r")
        .unwrap();
    append_until(&mut session, &mut terminal, "› ACCESS_TOKEN=");
    session.master.write_all(b"\r").unwrap();
    append_until(&mut session, &mut terminal, "› FLASH=");
    session.master.write_all(b"local-model\r").unwrap();
    append_until(&mut session, &mut terminal, "› STANDARD=local-model");
    session.master.write_all(b"\r").unwrap();
    append_until(&mut session, &mut terminal, "› MAX=local-model");
    session.master.write_all(b"\r").unwrap();
    append_until(&mut session, &mut terminal, "› TIER=flash");
    session.master.write_all(b"\r").unwrap();
    append_until(&mut session, &mut terminal, "下一步");
    session.master.write_all(b"\r").unwrap();
    append_until(&mut session, &mut terminal, "配置已保存: anonymous");

    let rendered = String::from_utf8_lossy(&terminal);
    assert!(rendered.contains("传输: HTTP（本机）"));
    assert!(rendered.contains("token: (unset)"));
    let config = fs::read_to_string(session.home.join(".zhsh/llm/anonymous.llm")).unwrap();
    assert!(config.contains("ACCESS_TOKEN=\n"));
}

#[test]
fn modify_without_a_name_defaults_to_the_current_configuration() {
    let home = temporary_home("modify");
    let config_dir = home.join(".zhsh/llm");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("current.llm");
    fs::write(
        &config_file,
        "NAME=current\nURL=https://example.com/gateway\nFORMAT=openai@0.3.0\nACCESS_TOKEN=1234567890123456\nFLASH=fast\nSTANDARD=standard\nMAX=max\nTIER=flash\n",
    )
    .unwrap();
    let active_file = home.join(".zhsh/active-llm");
    fs::write(&active_file, "current\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(home.join(".zhsh"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&active_file, fs::Permissions::from_mode(0o600)).unwrap();
    let mut session = start_with_home(home);

    let mut terminal = Vec::new();
    append_until(&mut session, &mut terminal, "$ ");
    session.master.write_all(b"zh llm -m\r").unwrap();
    append_until(&mut session, &mut terminal, "? 修改配置");
    session.master.write_all(b"\r").unwrap();
    append_until(&mut session, &mut terminal, "› FORMAT=openai@0.3.0");
    session.master.write_all(b"\r\r\r\r\r\r\r\r").unwrap();
    append_until(&mut session, &mut terminal, "下一步");
    session.master.write_all(b"\r").unwrap();
    append_until(&mut session, &mut terminal, "配置已保存并启用: current");

    let rendered = String::from_utf8_lossy(&terminal);
    assert!(!rendered.contains("1234567890123456"));
    assert!(rendered.contains("ACCESS_TOKEN=••••••••••••••••"));
}

#[test]
fn escape_from_an_unsubmitted_colon_command_returns_to_normal_mode() {
    let mut session = start("command-escape");
    let mut terminal = Vec::new();
    append_until(&mut session, &mut terminal, "$ ");
    session.master.write_all(b"zh llm\r").unwrap();
    append_until(&mut session, &mut terminal, "› NAME=");
    session
        .master
        .write_all(b"command-escape\x1b:q\x1b:q\r")
        .unwrap();
    append_until(&mut session, &mut terminal, "已取消");

    assert!(!session.home.join(".zhsh/llm/command-escape.llm").exists());
}

#[test]
fn editor_uses_the_header_for_mode_and_filters_choice_options() {
    let mut session = start("choice-options");
    let mut terminal = Vec::new();
    append_until(&mut session, &mut terminal, "$ ");
    session
        .master
        .write_all(b"zh llm choice-options\r")
        .unwrap();

    append_until(
        &mut session,
        &mut terminal,
        "LLM 配置 · INSERT · Esc: normal · Enter: next line",
    );
    session.master.write_all(b"\x1b\x1b[A").unwrap();
    append_until(&mut session, &mut terminal, "LLM 配置 · NORMAL");
    session.master.write_all(b"i\x15o").unwrap();
    append_until(&mut session, &mut terminal, "  on");
    session.master.write_all(b"n\r").unwrap();
    append_until(&mut session, &mut terminal, "› URL=");
    session.master.write_all(b"\x1b:wq\r").unwrap();
    append_until(
        &mut session,
        &mut terminal,
        "配置草稿已保存: choice-options",
    );

    let rendered = String::from_utf8_lossy(&terminal);
    assert!(rendered.contains("LLM 配置 · INSERT · Esc: normal · Enter: confirm · Tab: complete"));
    assert!(rendered.contains("Options:"));
    assert!(rendered.contains("  off"));
    assert!(!rendered.contains("-- INSERT --"));
    assert!(!rendered.contains("-- NORMAL --"));
    assert!(!rendered.contains("-- COMMAND --"));
}

#[test]
fn use_can_select_an_incomplete_profile_without_disabling_the_shell() {
    let home = temporary_home("use-incomplete");
    let config_dir = home.join(".zhsh/llm");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("draft.llm");
    fs::write(
        &config_file,
        "NAME=draft\nURL=\nFORMAT=openai@0.3.0\nJSON_SCHEMA=off\nACCESS_TOKEN=\nFLASH=\nSTANDARD=\nMAX=\nTIER=flash\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(home.join(".zhsh"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600)).unwrap();
    let mut session = start_with_home(home);
    let mut terminal = Vec::new();

    append_until(&mut session, &mut terminal, "$ ");
    session.master.write_all(b"zh use draft\r").unwrap();
    append_until(&mut session, &mut terminal, "是否进入修复流程");
    session.master.write_all(b"\x1b[B\r").unwrap();
    append_until(&mut session, &mut terminal, "Agent: unavailable");
    session.master.write_all(b"zh status\r").unwrap();
    append_until(&mut session, &mut terminal, "配置: draft");
    session.master.write_all(b"printf shell-ok\r").unwrap();
    append_until(&mut session, &mut terminal, "shell-ok");

    assert_eq!(
        fs::read_to_string(session.home.join(".zhsh/active-llm")).unwrap(),
        "draft\n"
    );
}

fn start_with_home(home: PathBuf) -> Session {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let window = libc::winsize {
        ws_row: 30,
        ws_col: 140,
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
    assert_eq!(opened, 0);
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
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
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    Session {
        master,
        child,
        home,
    }
}
