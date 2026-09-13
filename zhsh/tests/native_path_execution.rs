#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn native_cli_executes_a_real_binary_with_literal_arguments_and_environment() {
    let root = std::env::temp_dir().join(format!(
        "zhsh-native-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("bin")).unwrap();
    let source = root.join("probe.rs");
    fs::write(
        &source,
        r#"
        fn main() {
            for arg in std::env::args() { println!("ARG:{:?}", arg); }
            println!("CWD:{}", std::env::current_dir().unwrap().display());
            println!("ENV:{}", std::env::var("NATIVE_FIXTURE").unwrap());
            std::fs::write("visible", "native").unwrap();
            std::process::exit(7);
        }
    "#,
    )
    .unwrap();
    let built = Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(root.join("bin/probe"))
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .arg("--native")
        .env("HOME", &root)
        .env("PATH", "bin")
        .env("NATIVE_FIXTURE", "session-value")
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"probe -n \"a b\" 'c d' e\\ f \"\" '$HOME'\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("ARG:\"probe\"\nARG:\"-n\"\nARG:\"a b\"\nARG:\"c d\"\nARG:\"e f\"\nARG:\"\"\nARG:\"$HOME\"\n"), "{stdout}");
    assert!(
        stdout.contains(&format!("CWD:{}\nENV:session-value", root.display())),
        "{stdout}"
    );
    assert_eq!(fs::read_to_string(root.join("visible")).unwrap(), "native");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_cli_builtins_update_directory_environment_and_exit_status() {
    let root =
        std::env::temp_dir().join(format!("zhsh-native-cli-builtins-{}", std::process::id()));
    fs::create_dir_all(root.join("child")).unwrap();
    fs::write(
        root.join("commands"),
        "export FROM_FILE=yes\ncd child\nalias show='pwd'\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .arg("--native")
        .env("HOME", &root)
        .env("PATH", "/usr/bin:/bin")
        .env(
            "ZHSH_TEST_SYSTEM_CODEC_DIR",
            "/tmp/zhsh-test-no-system-codecs",
        )
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"source commands\nshow\nprintenv FROM_FILE\ncd ~\npwd\nzh status\nhelp source\nexit 13\nexport NEVER=yes\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(13),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with(&format!(
            "{}\nyes\n{}\n",
            root.join("child").display(),
            root.display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("授信:"), "{stdout}");
    assert!(stdout.contains("不调用 Bash"), "{stdout}");
    fs::remove_dir_all(root).unwrap();
}
