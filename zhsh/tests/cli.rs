use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn zhsh() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zhsh"))
}

#[test]
fn version_matches_cargo_package_version() {
    let output = zhsh().arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("zhsh {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn help_does_not_start_the_repl() {
    let output = zhsh().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("用法: zhsh [--help | --version | --trace-agent | --native]"));
    assert!(stdout.contains("--trace-agent  临时记录"));
    assert!(stdout.contains("--native  ASCII 输入进入空处理旁路"));
    assert!(output.stderr.is_empty());
}

#[test]
fn trace_agent_is_explicit_and_does_not_create_a_log_without_an_invalid_response() {
    let home = std::env::temp_dir().join(format!(
        "zhsh-cli-trace-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&home).unwrap();

    let output = zhsh()
        .arg("--trace-agent")
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Agent 原始响应诊断已启用"));
    assert!(stderr.contains("invalid-agent-responses.jsonl"));
    assert!(!home
        .join(".zhsh/diagnostics/invalid-agent-responses.jsonl")
        .exists());

    fs::remove_dir_all(home).unwrap();
}

#[test]
fn unknown_or_extra_arguments_are_rejected() {
    for arguments in [["--unknown"].as_slice(), ["--help", "extra"].as_slice()] {
        let output = zhsh().args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("未知参数"));
    }
}

#[test]
fn native_bypasses_ascii_commands_but_keeps_agent_routing() {
    let home = std::env::temp_dir().join(format!(
        "zhsh-cli-native-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&home).unwrap();
    let run = |native: bool, input: &str| {
        let mut command = zhsh();
        if native {
            command.arg("--native");
        }
        let mut child = command
            .env("HOME", &home)
            .current_dir(&home)
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
    };
    let baseline = run(true, "");
    let bypassed = run(
        true,
        "printf UNEXPECTED\npwd\ncd /\nexit 9\nprintf x > sentinel\nprintf 'unfinished\n",
    );
    assert_eq!(bypassed.status.code(), baseline.status.code());
    assert_eq!(bypassed.stdout, baseline.stdout);
    assert_eq!(bypassed.stderr, baseline.stderr);
    assert!(!home.join("sentinel").exists());

    // An unconfigured Agent still reports its existing error. The ASCII exit
    // after it must neither terminate with 9 nor overwrite that error status.
    let agent = run(true, "检查状态\nexit 9\n");
    assert_eq!(agent.status.code(), Some(1));
    assert!(String::from_utf8(agent.stderr)
        .unwrap()
        .contains("Agent 不可用"));
    let default = run(false, "printf DEFAULT_OK\n");
    assert!(default.status.success());
    assert_eq!(default.stdout, b"DEFAULT_OK");
    fs::remove_dir_all(home).unwrap();
}
