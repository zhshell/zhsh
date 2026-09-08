#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fixture(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "zhsh-codec-isolation-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".zhsh")).unwrap();
    std::fs::set_permissions(root.join(".zhsh"), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn private_directory(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn private_file(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn run(home: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .env("HOME", home)
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
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn an_unrelated_bad_codec_is_reported_but_ascii_shell_stays_available() {
    let home = fixture("unrelated-bad");
    let plugins = home.join(".zhsh/plugins/llm");
    private_directory(&plugins);
    private_file(
        &plugins.join("openai@0.3.0.zhcodec"),
        include_bytes!("../assets/llm-codecs/openai@0.3.0.zhcodec"),
    );
    private_file(
        &plugins.join("broken@1.0.0.zhcodec"),
        b"PLUGIN_SECRET_MUST_NOT_BE_PRINTED",
    );

    let output = run(&home, "printf 'SHELL_CODEC_OK\\n'\nexit\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert_eq!(stdout, "SHELL_CODEC_OK\n");
    assert!(stderr.contains("Codec 加载诊断"), "{stderr}");
    assert!(stderr.contains("broken@1.0.0.zhcodec"), "{stderr}");
    assert!(!stderr.contains("PLUGIN_SECRET_MUST_NOT_BE_PRINTED"));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn an_active_missing_codec_disables_only_agent_input() {
    let home = fixture("active-missing");
    let configs = home.join(".zhsh/llm");
    private_directory(&configs);
    private_file(
        &configs.join("broken.llm"),
        b"NAME=broken\nURL=https://example.com\nFORMAT=missing@1.0.0\nACCESS_TOKEN=never-print-this-token\nFLASH=x\nSTANDARD=x\nMAX=x\nTIER=flash\n",
    );
    private_file(&home.join(".zhsh/active-llm"), b"broken\n");

    let output = run(
        &home,
        "检查当前状态\nprintf 'AFTER_AGENT_FAILURE\\n'\nzh status\nexit\n",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr}");
    assert!(stdout.starts_with("AFTER_AGENT_FAILURE\n"), "{stdout}");
    assert!(stdout.contains("Agent: 不可用"), "{stdout}");
    assert!(stdout.contains("Codec: 无"), "{stdout}");
    assert!(stderr.contains("Agent 不可用"), "{stderr}");
    assert!(stderr.contains("missing@1.0.0"), "{stderr}");
    assert!(!stderr.contains("never-print-this-token"));
    let _ = std::fs::remove_dir_all(home);
}
