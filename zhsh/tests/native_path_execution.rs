#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn native_lookup_matches_bash_permissions_and_failure_status() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("zhsh-native-permissions-{}", std::process::id()));
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::create_dir_all(root.join("directory")).unwrap();
    for (name, mode) in [
        ("noexec", 0o600),
        ("execonly", 0o100),
        ("a/probe", 0o401),
        ("b/probe", 0o700),
    ] {
        fs::copy("/usr/bin/true", root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(mode)).unwrap();
    }
    for (name, contents) in [
        ("script", "#!/bin/sh\nexit 7\n"),
        (
            "missing-interpreter",
            "#!/nonexistent/zhsh-interpreter\nexit 0\n",
        ),
        ("bad", "\x7fELFbroken"),
    ] {
        fs::write(root.join(name), contents).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let search = format!(
        "{}:{}:{}",
        root.join("a").display(),
        root.join("b").display(),
        root.display()
    );
    for (input, expected) in [
        ("./execonly", 0),
        ("./script", 7),
        ("./missing-interpreter", 127),
        ("./bad", 126),
        ("./noexec", 126),
        ("noexec", 126),
        ("./directory", 126),
        ("./missing", 127),
        ("probe", 0),
    ] {
        // Root can execute any file with an execute bit; the PATH skip case requires a normal user.
        if input == "probe" && unsafe { libc::geteuid() } == 0 {
            continue;
        }
        for native in [false, true] {
            let mut command = if native {
                let mut command = Command::new(env!("CARGO_BIN_EXE_zhsh"));
                command.arg("--native");
                command
            } else {
                let mut command = Command::new("/bin/bash");
                command.args(["--noprofile", "--norc", "-c", input]);
                command
            };
            let mut child = command
                .env_clear()
                .env("HOME", &root)
                .env("PATH", &search)
                .env("LC_ALL", "C")
                .env("ZHSH_TEST_SYSTEM_CODEC_DIR", root.join("no-codecs"))
                .current_dir(&root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            if native {
                writeln!(child.stdin.take().unwrap(), "{input}").unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(expected),
                "native={native}, input={input}, {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    fs::remove_dir_all(root).unwrap();
}

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
    let absolute = root.join("bin/probe").to_string_lossy().into_owned();
    for program in [
        "probe",
        "./bin/probe",
        "./bin/../bin/probe",
        absolute.as_str(),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
            .arg("--native")
            .env("HOME", &root)
            .env(
                "PATH",
                if program == "probe" {
                    "bin"
                } else {
                    "/nonexistent"
                },
            )
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
            .write_all(format!("{program} -n \"a b\" 'c d' e\\ f \"\" '$HOME'\n").as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(7),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.starts_with(&format!("ARG:{program:?}\nARG:\"-n\"\nARG:\"a b\"\nARG:\"c d\"\nARG:\"e f\"\nARG:\"\"\nARG:\"$HOME\"\n")), "{stdout}");
        assert!(
            stdout.contains(&format!("CWD:{}\nENV:session-value", root.display())),
            "{stdout}"
        );
        assert_eq!(fs::read_to_string(root.join("visible")).unwrap(), "native");
    }
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

#[test]
fn native_simple_units_execute_multiline_source_and_preserve_literal_argv() {
    let root = std::env::temp_dir().join(format!("zhsh-s1-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    // 两种 Shell 均调用同一个外部 printf，避免内建实现差异影响参数传递的比较。
    let text="/usr/bin/printf '<%s>\\n' \"a b\" '' a\\ b a\"b\"'c' x\\ \n/usr/bin/printf '<%s>\\n' '中文\n值' ab\\\ncd # ignored $x |\n";
    let run = |native: bool, input: &str| {
        let mut command = if native {
            let mut c = Command::new(env!("CARGO_BIN_EXE_zhsh"));
            c.args(["--native", "--norc"]);
            c
        } else {
            let mut c = Command::new("/bin/bash");
            c.args(["--noprofile", "--norc"]);
            c
        };
        let mut child = command
            .env_clear()
            .env("HOME", &root)
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C.UTF-8")
            .env("ZHSH_TEST_SYSTEM_CODEC_DIR", root.join("missing"))
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
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let reference = run(false, text);
    let native = run(true, text);
    assert!(
        native.status.success(),
        "{}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(native.stdout, reference.stdout);
    fs::create_dir_all(root.join("child")).unwrap();
    fs::write(root.join("commands"),"cd child\nexport S1_FILE='line1\nline2'\n/usr/bin/false\n/usr/bin/printf 'SOURCE_OK\\n'\n/usr/bin/printf BAD > marker\nexport S1_LATE=yes\n").unwrap();
    let output = run(
        true,
        "source commands\npwd\n/usr/bin/printenv S1_FILE\n/usr/bin/printenv S1_LATE\n",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("SOURCE_OK\n"), "{stdout}");
    assert!(
        stdout.contains(root.join("child").to_str().unwrap()),
        "{stdout}"
    );
    assert!(stdout.contains("line1\nline2\n"), "{stdout}");
    assert!(!stdout.contains("BAD"));
    assert!(!stdout.contains("yes"));
    assert!(!root.join("child/marker").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("第 6 行"));
    // 启动文件共用完整命令单元读取器，其状态修改对后续标准输入命令可见。
    fs::write(root.join(".zhshrc"), "export S1_BOOT='boot\nvalue'\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_zhsh"))
        .arg("--native")
        .env_clear()
        .env("HOME", &root)
        .env("PATH", "/usr/bin:/bin")
        .env("ZHSH_TEST_SYSTEM_CODEC_DIR", root.join("missing"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/usr/bin/printenv S1_BOOT\n")
        .unwrap();
    let startup = child.wait_with_output().unwrap();
    assert_eq!(startup.stdout, b"boot\nvalue\n");
    fs::remove_dir_all(root).unwrap();
}
