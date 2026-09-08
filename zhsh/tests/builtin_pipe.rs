use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temporary_home(label: &str) -> PathBuf {
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("zhsh-{label}-{}-{sequence}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn run(home: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .env("HOME", home)
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
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn invalid_export_does_not_crash_or_stop_following_input() {
    let home = temporary_home("invalid-export");
    let output = run(&home, "export =value\nprintf alive\nexit\n");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "alive");
    assert!(String::from_utf8_lossy(&output.stderr).contains("不是有效的标识符"));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn pipe_exit_uses_status_and_stops_remaining_lines() {
    let home = temporary_home("pipe-exit");
    let output = run(&home, "printf before\nexit 7\nprintf SHOULD_NOT_RUN\n");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "before");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SHOULD_NOT_RUN"));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn bash_for_loop_with_chinese_filenames_bypasses_the_agent() {
    let home = temporary_home("chinese-for-loop");
    let work = home.join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("海贼王0001.mkv"), "one").unwrap();
    std::fs::write(work.join("海贼王0002.mp4"), "two").unwrap();

    let script = r#"for f in 海贼王[0-9][0-9][0-9][0-9].{mkv,mp4}; do mv "$f" "${f#海贼王}"; done"#;
    let output = run(&home, &format!("cd '{}'\n{script}\n", work.display()));

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(work.join("0001.mkv").is_file());
    assert!(work.join("0002.mp4").is_file());
    assert!(!work.join("海贼王0001.mkv").exists());
    assert!(!work.join("海贼王0002.mp4").exists());
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn pipe_mode_uses_ascii_and_non_ascii_first_character_routing() {
    let home = temporary_home("pipe-binary-route");
    let shell = run(&home, "printf 'shell-中文参数'");
    assert!(shell.status.success());
    assert_eq!(String::from_utf8_lossy(&shell.stdout), "shell-中文参数");
    assert!(String::from_utf8_lossy(&shell.stderr).is_empty());

    let agent = run(&home, "Проверить систему");
    assert_eq!(agent.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&agent.stderr);
    assert!(stderr.contains("Agent 不可用"), "{stderr}");
    assert!(!stderr.contains("command not found"), "{stderr}");

    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn source_persists_bash_environment_directory_and_aliases() {
    let home = temporary_home("source");
    let work = home.join("work");
    std::fs::create_dir(&work).unwrap();
    let script = home.join("session.sh");
    std::fs::write(
        &script,
        format!(
            "export ZHSH_SOURCE_VALUE='value with space'\nSOURCE_LOCAL=sourced-function\ncd '{}'\nalias hi='printf sourced-alias'\ndev() {{ printf \"$SOURCE_LOCAL\"; }}\n",
            work.display()
        ),
    )
    .unwrap();
    let output = run(
        &home,
        &format!(
            "source '{}'\nprintf '<%s>\\n' \"$ZHSH_SOURCE_VALUE\"\npwd\nhi\ndev\nexport SOURCE_LOCAL\nbash -c 'printf exported-local:%s \"$SOURCE_LOCAL\"'\nexport SOURCE_LOCAL=overridden\nprintf ' overridden:%s' \"$SOURCE_LOCAL\"\nexit 9\nprintf SHOULD_NOT_RUN\n",
            script.display()
        ),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(9),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("<value with space>\n"), "{stdout}");
    assert!(
        stdout.contains(&format!("{}\n", work.display())),
        "{stdout}"
    );
    assert!(stdout.contains("sourced-alias"), "{stdout}");
    assert!(stdout.contains("sourced-function"), "{stdout}");
    assert!(
        stdout.contains("exported-local:sourced-function"),
        "{stdout}"
    );
    assert!(stdout.contains("overridden:overridden"), "{stdout}");
    assert!(!stdout.contains("SHOULD_NOT_RUN"), "{stdout}");
    let _ = std::fs::remove_dir_all(home);
}
