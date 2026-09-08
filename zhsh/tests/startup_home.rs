#![cfg(unix)]

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fixture(label: &str) -> PathBuf {
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "zhsh-startup-home-{label}-{}-{sequence}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn prepare_probes(root: &Path) -> PathBuf {
    let sentinel = root.join("SHOULD_NOT_EXIST");
    let startup = format!("touch '{}'\n", sentinel.display());
    std::fs::write(root.join(".zhshrc"), &startup).unwrap();
    std::fs::write(root.join(".bashrc"), &startup).unwrap();
    let relative = root.join("relative/home");
    std::fs::create_dir_all(&relative).unwrap();
    std::fs::write(relative.join(".zhshrc"), &startup).unwrap();
    std::fs::write(relative.join(".bashrc"), &startup).unwrap();

    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let bash = bin.join("bash");
    std::fs::write(
        &bash,
        format!(
            "#!/bin/sh\n/usr/bin/touch '{}'\nexit 0\n",
            sentinel.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bash, std::fs::Permissions::from_mode(0o700)).unwrap();
    sentinel
}

fn run(root: &Path, configure_home: impl FnOnce(&mut Command)) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
    command
        .current_dir(root)
        .env("PATH", root.join("bin"))
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            root.join("missing-system-codecs"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_home(&mut command);
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"pwd\nexit\n")
        .unwrap();
    child.wait_with_output().unwrap()
}

fn assert_degraded(root: &Path, sentinel: &Path, output: Output) {
    assert!(output.status.success(), "{:?}", output.status);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{}\n", root.display())
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("用户状态已禁用"), "{stderr}");
    assert!(!sentinel.exists(), "无效 HOME 仍执行了启动文件或 bash -ic");
    assert!(!root.join(".zh_history").exists());
    assert!(!root.join(".zhsh").exists());
}

#[test]
fn missing_empty_and_relative_home_do_not_load_user_state() {
    for (label, value) in [
        ("missing", None),
        ("empty", Some(OsString::new())),
        ("relative", Some(OsString::from("relative/home"))),
    ] {
        let root = fixture(label);
        let sentinel = prepare_probes(&root);
        let output = run(&root, |command| {
            command.env_remove("HOME");
            if let Some(value) = value {
                command.env("HOME", value);
            }
        });
        assert_degraded(&root, &sentinel, output);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn non_utf8_home_degrades_without_panicking_or_lossy_lookup() {
    let root = fixture("non-utf8");
    let sentinel = prepare_probes(&root);
    let mut value = root.as_os_str().as_encoded_bytes().to_vec();
    value.extend_from_slice(&[b'/', 0xff]);
    let output = run(&root, |command| {
        command.env("HOME", OsString::from_vec(value));
    });
    assert_degraded(&root, &sentinel, output);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn user_state_root_does_not_follow_session_home_changes() {
    let root = fixture("fixed-root");
    let home = root.join("home");
    let replacement = root.join("replacement");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&replacement).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
    command
        .current_dir(&root)
        .env("HOME", &home)
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            root.join("missing-system-codecs"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "export HOME={}\nzh trust -w confirm\nexit",
        replacement.display()
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let startup = std::fs::read_to_string(home.join(".zhshrc")).unwrap();
    assert!(startup.contains("ZHSH_AGENT_TRUST=confirm"));
    assert!(!replacement.join(".zhshrc").exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn valid_absolute_home_still_loads_zhshrc() {
    let root = fixture("valid-home");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".zhshrc"),
        "export ZHSH_VALID_HOME_SENTINEL=loaded\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .current_dir(&root)
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            root.join("missing-system-codecs"),
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
        .write_all(b"printf '%s\\n' \"$ZHSH_VALID_HOME_SENTINEL\"\nexit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "loaded\n");
    let _ = std::fs::remove_dir_all(root);
}
