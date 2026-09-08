//! 保证公开源码中的运行时契约与关键代码集合保持一致。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(root().join(path)).unwrap()
}

const RUNTIME_CONTRACT: &str = r#"input_kinds=UserCommand,AgentInput
command_terminations=Exited,OutputLimit,BackgroundTerminated,SupervisionFailed,StoppedTerminated,Interrupted
output_evidence=Complete,Truncated,Partial,Unavailable,CaptureFailed
model_tiers=Flash,Standard,Max
error_kinds=Input,Io,Http,Protocol,Cancelled,Internal
builtin_commands=.,alias,cd,dirs,exit,export,fg,help,history,popd,pushd,pwd,source,type,unalias,umask,unset,zh
zh_tokens=status,ls,use,llm,tier,trust,safety,codec,help,--help,-h
agent_actions=run,clarify,done
terminal_mode_routers=sudo,doas,pkexec,su,ssh,rsync,docker,kubectl
line_interactive_programs=passwd,chsh,chfn,cryptsetup,read,select,scp
opaque_terminal_programs=mosh,sftp,ftp,telnet,gpg,pinentry,less,more,man,info,vim,vi,nvim,nano,pico,emacs,top,htop,btop,glances,watch,tmux,screen,ranger,mc,nnn,lf,nmtui,alsamixer,pulsemixer,dialog,whiptail,gdb,mysql,mariadb,psql,sqlite3
max_agent_rounds=6
max_agent_clarifications=3
history_limit=10000
agent_command_output_limit=1024*1024
agent_command_feedback_limit=64*1024
agent_task_feedback_limit=256*1024
source_files=zhsh/src/main.rs,zhsh/src/repl/mod.rs,zhsh/src/repl/input.rs,zhsh/src/repl/completion.rs,zhsh/src/repl/llm_wizard.rs,zhsh/src/shell/mod.rs,zhsh/src/shell/trust.rs,zhsh/src/shell/safety_management.rs,zhsh/src/shell/command/mod.rs,zhsh/src/shell/command/args.rs,zhsh/src/shell/builtin/trust.rs,zhsh/src/shell/builtin/safety.rs,zhsh/src/shell/executor/mod.rs,zhsh/src/shell/executor/captured.rs,zhsh/src/shell/executor/interactive.rs,zhsh/src/common/cancellation.rs,zhsh/src/agent/mod.rs,zhsh/src/agent/operation_log.rs,zhsh/src/agent/protocol.rs,zhsh/src/agent/safety/mod.rs,zhsh/src/agent/safety/runtime.rs,zhsh/src/agent/safety/builtin/mod.rs,zhsh/src/agent/safety/builtin/rules.rs,zhsh/src/agent/safety/external.rs,zhsh/src/agent/terminal.rs,zhsh/src/llm/mod.rs,zhsh/src/llm/model.rs,zhsh/src/application/llm_config.rs"#;

fn contract() -> HashMap<String, String> {
    RUNTIME_CONTRACT
        .lines()
        .map(|line| {
            let (key, value) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("无效状态机契约行: {line}"));
            (key.to_string(), value.to_string())
        })
        .collect()
}

fn csv(values: &HashMap<String, String>, key: &str) -> Vec<String> {
    values
        .get(key)
        .unwrap_or_else(|| panic!("状态机契约缺少 {key}"))
        .split(',')
        .map(str::to_string)
        .collect()
}

fn quoted_strings(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut value = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for character in input.chars() {
        if !in_string {
            if character == '"' {
                in_string = true;
                value.clear();
            }
            continue;
        }
        if escaped {
            value.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            values.push(std::mem::take(&mut value));
            in_string = false;
        } else {
            value.push(character);
        }
    }
    values
}

fn builtin_names() -> Vec<String> {
    let source = read("zhsh/src/shell/command/mod.rs");
    let registry = source
        .split_once("const COMMANDS: &[CommandSpec] = &[")
        .and_then(|(_, rest)| rest.split_once("\n];"))
        .map(|(body, _)| body)
        .expect("无法定位内建命令注册表");
    registry
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("name: \"")
                .and_then(|value| value.strip_suffix("\","))
                .map(str::to_string)
        })
        .collect()
}

fn enum_variants(path: &str, declaration: &str) -> Vec<String> {
    let source = read(path);
    let body = source
        .split_once(declaration)
        .and_then(|(_, rest)| rest.split_once('\n'))
        .and_then(|(_, rest)| rest.split_once("\n}"))
        .map(|(body, _)| body)
        .unwrap_or_else(|| panic!("无法定位 enum: {declaration}"));
    body.lines()
        .filter_map(|line| {
            let candidate = line.trim().strip_suffix(',')?;
            let candidate = candidate
                .split(['(', '{'])
                .next()
                .unwrap_or(candidate)
                .trim();
            (!candidate.is_empty()
                && candidate
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric()))
            .then(|| candidate.to_string())
        })
        .collect()
}

fn zh_tokens() -> Vec<String> {
    let source = read("zhsh/src/shell/builtin/zh.rs");
    let execute = source
        .split_once("pub(crate) fn execute(")
        .map(|(_, rest)| rest)
        .expect("无法定位 zh::execute");
    let arms = execute
        .split_once("match command.as_str() {")
        .and_then(|(_, rest)| rest.split_once("\n        _ =>"))
        .map(|(body, _)| body)
        .expect("无法定位 zh 子命令 match");
    arms.lines()
        .filter_map(|line| line.split_once("=>").map(|(pattern, _)| pattern))
        .flat_map(quoted_strings)
        .collect()
}

fn terminal_programs(function: &str, end: &str) -> Vec<String> {
    let source = read("zhsh/src/shell/executor/interactive.rs");
    let body = source
        .split_once(function)
        .and_then(|(_, rest)| rest.split_once(end))
        .map(|(body, _)| body)
        .expect("无法定位交互程序集合");
    quoted_strings(body)
}

#[test]
fn runtime_sets_equal_the_code_sets() {
    let values = contract();

    assert_eq!(
        csv(&values, "input_kinds"),
        enum_variants("zhsh/src/repl/input.rs", "pub(crate) enum InputKind {")
    );
    assert_eq!(
        csv(&values, "command_terminations"),
        enum_variants(
            "zhsh/src/shell/executor/mod.rs",
            "pub(crate) enum CommandTermination {"
        )
    );
    assert_eq!(
        csv(&values, "model_tiers"),
        enum_variants("zhsh/src/llm/model.rs", "pub enum ModelTier {")
    );
    assert_eq!(
        csv(&values, "error_kinds"),
        enum_variants("zhsh/src/common/mod.rs", "pub(crate) enum ErrorKind {")
    );
    assert_eq!(csv(&values, "builtin_commands"), builtin_names());
    assert_eq!(csv(&values, "zh_tokens"), zh_tokens());
    assert_eq!(
        csv(&values, "output_evidence"),
        enum_variants(
            "zhsh/src/shell/executor/mod.rs",
            "pub(crate) enum OutputEvidence {"
        )
    );
    assert_eq!(
        csv(&values, "terminal_mode_routers"),
        terminal_programs("    match program {", "\n        _ =>")
    );
    assert_eq!(
        csv(&values, "line_interactive_programs"),
        terminal_programs(
            "fn is_line_interactive_program(program: &str) -> bool {",
            "\n}\n\nfn is_opaque_program"
        )
    );
    assert_eq!(
        csv(&values, "opaque_terminal_programs"),
        terminal_programs(
            "fn is_opaque_program(program: &str) -> bool {",
            "\n}\n\n#[derive"
        )
    );
    let actions = csv(&values, "agent_actions");
    let protocol = read("zhsh/src/agent/protocol.rs");
    assert_eq!(actions, ["run", "clarify", "done"]);
    for action in actions {
        assert!(
            protocol.contains(&format!(r#"{{"action":"{action}""#)),
            "Prompt 缺少动作协议: {action}"
        );
    }
}

#[test]
fn limits_and_rounds_equal_the_code_constants() {
    let values = contract();
    let agent = read("zhsh/src/agent/mod.rs");
    let protocol = read("zhsh/src/agent/protocol.rs");
    let executor = read("zhsh/src/shell/executor/mod.rs");
    let compact_executor: String = executor.split_whitespace().collect();
    let repl = read("zhsh/src/repl/mod.rs");

    let turns = values.get("max_agent_rounds").unwrap();
    assert!(protocol.contains(&format!("const MAX_ROUNDS: i32 = {turns};")));
    assert!(agent.contains("let turn = task.phase_turns + 1;"));
    assert!(agent.contains("if turn > MAX_ROUNDS"));
    assert!(!agent.contains("repair_final_answer"));

    let clarifications = values.get("max_agent_clarifications").unwrap();
    assert!(protocol.contains(&format!("const MAX_CLARIFICATIONS: u8 = {clarifications};")));

    let history_limit = values.get("history_limit").unwrap();
    assert!(repl.contains(&format!("set_max_history_size({history_limit})")));

    for (contract_key, rust_name) in [
        ("agent_command_output_limit", "AGENT_COMMAND_OUTPUT_LIMIT"),
        (
            "agent_command_feedback_limit",
            "AGENT_COMMAND_FEEDBACK_LIMIT",
        ),
        ("agent_task_feedback_limit", "AGENT_TASK_FEEDBACK_LIMIT"),
    ] {
        let expression = values.get(contract_key).unwrap();
        assert!(compact_executor.contains(&format!("{rust_name}:usize={expression};")));
    }
}

#[test]
fn every_contract_source_is_present() {
    let values = contract();

    for source in csv(&values, "source_files") {
        assert!(root().join(&source).is_file(), "契约源码不存在: {source}");
    }
}
