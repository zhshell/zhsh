use std::fs;
use std::process::Command;
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
    assert!(stdout.contains("用法: zhsh [--help | --version | --trace-agent]"));
    assert!(stdout.contains("--trace-agent  临时记录"));
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
