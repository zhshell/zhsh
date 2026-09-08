//! 防止已经消除的跨包依赖与顶层零散模块在后续修改中回流。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    root().parent().unwrap().to_path_buf()
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(root().join(path)).unwrap()
}

fn read_workspace(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(workspace_root().join(path)).unwrap()
}

fn rust_sources(directory: impl AsRef<Path>) -> Vec<PathBuf> {
    fn visit(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(&root().join(directory), &mut files);
    files.sort();
    files
}

fn assert_package_avoids(package: &str, forbidden: &[&str]) {
    for path in rust_sources(format!("src/{package}")) {
        let source = std::fs::read_to_string(&path).unwrap();
        for dependency in forbidden {
            assert!(
                !source.contains(&format!("crate::{dependency}")),
                "{} 不应依赖更高层 package `{dependency}`",
                path.display()
            );
        }
    }
}

#[test]
fn src_root_contains_only_crate_entry_points() {
    let mut root_files = Vec::new();
    for entry in std::fs::read_dir(root().join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            root_files.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    root_files.sort();
    assert_eq!(root_files, ["lib.rs", "main.rs"]);

    for package in ["common", "llm", "application", "shell", "agent", "repl"] {
        assert!(root().join("src").join(package).join("mod.rs").is_file());
    }
}

#[test]
fn package_dependencies_follow_the_extraction_order() {
    for package in ["common", "llm", "application", "shell", "agent", "repl"] {
        for path in rust_sources(format!("src/{package}")) {
            let source = std::fs::read_to_string(&path).unwrap();
            assert!(
                !source.contains("use crate::{"),
                "{} 应显式写出 package 根路径，避免隐藏跨包依赖",
                path.display()
            );
            assert!(
                !source.contains(&format!("crate::{package}")),
                "{} 应使用 package 内相对路径，避免提取 crate 时批量改写",
                path.display()
            );
        }
    }
    assert_package_avoids("common", &["llm", "application", "shell", "agent", "repl"]);
    assert_package_avoids("llm", &["application", "shell", "agent", "repl"]);
    assert_package_avoids("application", &["shell", "agent", "repl"]);
    assert_package_avoids("shell", &["agent", "repl"]);
    assert_package_avoids("agent", &["application", "repl"]);
}

#[test]
fn builtins_use_shell_ports_instead_of_process_or_repl_details() {
    for path in rust_sources("src/shell/builtin") {
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("std::process::Command"),
            "{} 在 builtin 中直接创建进程",
            path.display()
        );
        assert!(
            !source.contains("crate::repl"),
            "{} 反向依赖 REPL",
            path.display()
        );
        assert!(
            !source.contains("llm::store"),
            "{} 绕过应用服务访问配置仓库",
            path.display()
        );
    }
}

#[test]
fn state_protocol_ui_and_persistence_boundaries_remain_separate() {
    let session = read("src/shell/session.rs");
    assert!(!session.contains("std::process"));
    assert!(!session.contains("llm::store"));
    assert!(!session.contains("rustyline"));

    let wizard = read("src/repl/llm_wizard.rs");
    assert!(!wizard.contains("llm::store"));
    assert!(!wizard.contains("SessionState"));

    let service = read("src/application/llm_config.rs");
    assert!(!service.contains("SessionState"));
    assert!(service.contains("trait LlmConfigUi"));

    let agent = read("src/agent/mod.rs");
    assert!(!agent.contains("termios"));
    assert!(!agent.contains("Deserialize"));
    assert!(root().join("src/agent/operation_log.rs").is_file());
    assert!(root().join("src/agent/protocol.rs").is_file());
    assert!(root().join("src/agent/safety/mod.rs").is_file());
    assert!(root().join("src/agent/safety/builtin/mod.rs").is_file());
    assert!(root().join("src/agent/safety/builtin/rules.rs").is_file());
    assert!(root().join("src/agent/safety/external.rs").is_file());
    assert!(root().join("src/agent/terminal.rs").is_file());

    let protocol = read("src/agent/protocol.rs");
    assert!(protocol.contains("purpose: String"));
    let terminal = read("src/agent/terminal.rs");
    assert!(terminal.contains("enum AgentEvent"));
    assert!(terminal.contains("FinalStatus"));
    let repl = read("src/repl/mod.rs");
    assert!(!repl.contains("eprintln!(\"  第 {phase}"));
    assert!(!repl.contains("eprintln!(\"  {}轮"));
}

#[test]
fn safety_core_aggregates_plugins_without_program_special_cases() {
    let core = read("src/agent/safety/mod.rs");
    let production = core.rsplit_once("#[cfg(test)]\nmod tests").unwrap().0;

    assert!(!production.contains("match program"));
    assert!(!production.contains("program =="));
    for program in ["java", "javac"] {
        assert!(
            !production.contains(program),
            "具体程序 `{program}` 的规则不应写入安全聚合器"
        );
    }
    let builtin = read("src/agent/safety/builtin/mod.rs");
    assert!(builtin.contains("impl CommandSafetyAnalyzer"));
}

#[test]
fn history_has_one_owner_and_registry_has_one_source() {
    for path in rust_sources("src") {
        assert!(!std::fs::read_to_string(path)
            .unwrap()
            .contains("user_history"));
    }
    assert!(read("src/shell/command/mod.rs").contains("const COMMANDS"));
    assert!(!read("src/shell/builtin/mod.rs").contains("const COMMANDS"));
    assert!(read("src/repl/completion.rs").contains("Shell::builtin_names()"));
    let input = read("src/repl/input.rs");
    let compact_input: String = input.split_whitespace().collect();
    assert!(!input.contains("is_known_command"));
    assert!(!input.contains("executable_paths"));
    assert!(compact_input.contains("original.chars().next().is_some_and(|first|first.is_ascii())"));
    assert!(!read("src/agent/mod.rs").contains("executable_paths"));
}

#[test]
fn user_and_agent_execution_paths_remain_separate() {
    let entry = read("src/repl/mod.rs");
    let input = read("src/repl/input.rs");
    let shell = read("src/shell/mod.rs");
    let executor = read("src/shell/executor/mod.rs");

    assert!(entry.contains("input::route"));
    assert!(!input.contains("run_agent_command"));
    assert!(shell.contains("run_interactive"));
    assert!(shell.contains("run_agent"));
    assert!(executor.contains("AGENT_COMMAND_OUTPUT_LIMIT"));
    assert!(!executor.contains("USER_COMMAND_OUTPUT_LIMIT"));
}

#[test]
fn llm_codecs_remain_declarative_and_do_not_pull_a_general_runtime() {
    let manifest = read("Cargo.toml");
    for dependency in ["wasmtime", "cranelift", "wit-component"] {
        assert!(
            !manifest.contains(dependency),
            "声明式 LLM Codec 不应依赖通用运行时 `{dependency}`"
        );
    }
    let workspace = read_workspace("Cargo.toml");
    let release = workspace
        .split_once("[profile.release]")
        .map(|(_, release)| release)
        .expect("Workspace 必须声明 release profile");
    for setting in [
        "opt-level = \"z\"",
        "lto = \"fat\"",
        "codegen-units = 1",
        "strip = \"symbols\"",
        "panic = \"unwind\"",
    ] {
        assert!(
            release.contains(setting),
            "release 构建必须保留体积优化与终端清理语义: {setting}"
        );
    }

    let codec = read("src/llm/codec/declarative.rs");
    assert!(!codec.contains("std::process"));
    assert!(!codec.contains("unsafe"));
    assert!(codec.contains("fn encode"));
    assert!(codec.contains("fn decode"));
    assert!(codec.contains("fn decode_error"));

    let plugin_loader = read("src/llm/plugin/mod.rs");
    let production = plugin_loader
        .rsplit_once("#[cfg(test)]\nmod tests")
        .unwrap()
        .0;
    assert!(production.contains("official-codec.pub"));
    assert!(production.contains("official_codecs.rs"));
    assert_eq!(
        production.matches("include_bytes!").count(),
        1,
        "LLM 插件生产代码只能嵌入官方发布公钥"
    );
    assert!(production.contains("OFFICIAL_CODEC_LOCKS"));
    assert!(production.contains("expected_official_codec"));
    assert!(
        !production.contains("/assets/llm-codecs/"),
        "LLM 插件生产代码不得嵌入官方 .zhcodec"
    );

    let lock: serde_json::Value =
        serde_json::from_str(&read("assets/llm-codecs/official-codecs.lock")).unwrap();
    let locked = lock["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["file"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    let discovered = std::fs::read_dir(root().join("assets/llm-codecs"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("zhcodec"))
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(locked, discovered);
    let build_script = read("build.rs");
    assert!(build_script.contains("lock/directory mismatch"));
    assert!(build_script.contains("artifact_len"));
    assert!(build_script.contains("sha256"));
    assert!(
        !build_script.contains("include_bytes!"),
        "构建脚本只能生成官方 Codec 的长度与摘要表"
    );
    assert!(root().join("src/llm/plugin/container.rs").is_file());
    assert!(root().join("src/llm/plugin/package.rs").is_file());
    assert!(root().join("assets/official-codec.pub").is_file());
    assert!(!root().join("src/llm/plugin/manifest.rs").exists());
}

#[test]
fn workspace_contains_only_the_zhsh_source_member() {
    let manifest = read_workspace("Cargo.toml");
    assert!(manifest.contains("[workspace]"));
    assert!(manifest.contains("members = [\"zhsh\"]"));
    assert!(manifest.contains("default-members = [\"zhsh\"]"));
    assert!(!manifest.contains("[workspace.dependencies]"));
    assert!(workspace_root().join("Cargo.lock").is_file());

    let runtime_manifest = read("Cargo.toml");
    let runtime_dependencies = runtime_manifest
        .split_once("[dependencies]")
        .and_then(|(_, rest)| rest.split_once("[package.metadata.deb]"))
        .map(|(dependencies, _)| dependencies)
        .unwrap();
    assert!(!runtime_dependencies.contains("zhcodec"));
}
