//! 内置的数据披露规则。
//!
//! 与程序名有关的策略留在 builtin 插件包内；安全聚合器只负责合并两个正交维度。

use super::super::{lex, DisclosureClass, SafetyAssessment};
use crate::shell::{
    AgentCommandPlan, AgentExecutionTarget, BoundCommand, BoundExpression, BoundTarget,
    QueryBuiltin,
};
use std::path::Path;

pub(in super::super) fn apply(
    mut assessment: SafetyAssessment,
    plan: &AgentCommandPlan,
    task_root: &Path,
) -> SafetyAssessment {
    let script = match &plan.executable {
        AgentExecutionTarget::ZhshBuiltin { name, arguments } => {
            let mut words = Vec::with_capacity(arguments.len() + 1);
            words.push(name.clone());
            words.extend(arguments.iter().cloned());
            vec![words]
        }
        AgentExecutionTarget::External {
            path, arguments, ..
        } => {
            let mut words = Vec::with_capacity(arguments.len() + 1);
            words.push(
                plan.invocations
                    .first()
                    .map(|invocation| invocation.semantic_name().to_owned())
                    .unwrap_or_else(|| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_owned()
                    }),
            );
            words.extend(
                arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned()),
            );
            vec![words]
        }
        AgentExecutionTarget::BoundCompound { expression } => {
            let mut words = Vec::new();
            collect_bound_words(expression, &mut words);
            words
        }
        AgentExecutionTarget::Bash { script } => lex(script).segments,
    };
    for words in script {
        let Some((program, arguments)) = words.split_first() else {
            continue;
        };
        let program = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        if is_network_program(program) {
            assessment.network = true;
            assessment = assessment
                .with_disclosure(
                    DisclosureClass::SensitiveOrUnbounded,
                    format!("{program} 可能把本地数据发送到网络目标"),
                )
                .force_confirmation("网络发送行为必须明确确认");
            continue;
        }
        if program == "journalctl" {
            assessment = assessment.with_disclosure(
                DisclosureClass::OperationalContent,
                "journalctl_operational_content",
            );
        }
        if program == "printenv"
            || program == "alias"
            || program == "export" && arguments == ["-p"]
            || program == "ps" && arguments.iter().any(|argument| argument == "e")
            || arguments
                .iter()
                .any(|argument| argument.contains("/proc/") && argument.contains("/environ"))
        {
            assessment = assessment.with_disclosure(
                DisclosureClass::SensitiveOrUnbounded,
                "命令可能读取完整进程环境，其中常包含访问令牌或凭据",
            );
            continue;
        }

        let path_mode = if is_content_reader(program) {
            Some(DisclosureClass::WorkspaceContent)
        } else if is_metadata_reader(program) {
            Some(DisclosureClass::Metadata)
        } else {
            None
        };
        let Some(path_mode) = path_mode else {
            continue;
        };
        if arguments.is_empty() && is_content_reader(program) {
            assessment = assessment.with_disclosure(
                DisclosureClass::SensitiveOrUnbounded,
                format!("{program} 的输入范围无法静态证明"),
            );
            continue;
        }

        let mut saw_path = false;
        for argument in reader_paths(program, arguments) {
            saw_path = true;
            let (class, evidence) = classify_path(program, argument, &plan.cwd, task_root);
            assessment = assessment.with_disclosure(path_mode.max(class), evidence);
        }
        if !saw_path {
            assessment = assessment.with_disclosure(
                path_mode,
                format!("{program} 的输出将反馈给当前 LLM Provider"),
            );
        }
    }
    assessment
}

fn reader_paths<'a>(program: &str, arguments: &'a [String]) -> Vec<&'a str> {
    match program {
        "sed" => sed_input_paths(arguments),
        "awk" => awk_input_paths(arguments),
        "which" | "whereis" | "type" => Vec::new(),
        _ => arguments
            .iter()
            .filter(|argument| !argument.starts_with('-'))
            .map(String::as_str)
            .collect(),
    }
}

fn sed_input_paths(arguments: &[String]) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut index = 0;
    let mut has_explicit_expression = false;
    let mut implicit_expression_seen = false;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(argument, "-e" | "--expression" | "-f" | "--file") {
            has_explicit_expression |= matches!(argument, "-e" | "--expression");
            index += 2;
            continue;
        }
        if argument.starts_with("--expression=") || argument.starts_with("-e") && argument.len() > 2
        {
            has_explicit_expression = true;
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            index += 1;
            continue;
        }
        if !has_explicit_expression && !implicit_expression_seen {
            implicit_expression_seen = true;
        } else {
            paths.push(argument);
        }
        index += 1;
    }
    paths.extend(arguments[index..].iter().map(String::as_str));
    paths
}

fn awk_input_paths(arguments: &[String]) -> Vec<&str> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(argument, "-v" | "-F") {
            index += 2;
        } else if argument.starts_with('-') {
            index += 1;
        } else {
            break;
        }
    }
    // 第一个非选项是 awk 程序；后续 NAME=value 是变量赋值，不是输入文件。
    arguments
        .get(index + 1..)
        .unwrap_or_default()
        .iter()
        .filter(|argument| !argument.contains('='))
        .map(String::as_str)
        .collect()
}

fn collect_bound_words(expression: &BoundExpression, output: &mut Vec<Vec<String>>) {
    match expression {
        BoundExpression::Command(command) => output.push(bound_words(command)),
        BoundExpression::Pipeline(commands) => {
            output.extend(commands.iter().map(bound_words));
        }
        BoundExpression::And(left, right) | BoundExpression::Or(left, right) => {
            collect_bound_words(left, output);
            collect_bound_words(right, output);
        }
        BoundExpression::Sequence(expressions) => {
            for expression in expressions {
                collect_bound_words(expression, output);
            }
        }
    }
}

fn bound_words(command: &BoundCommand) -> Vec<String> {
    let program = match &command.target {
        BoundTarget::External { .. } => command.invocation.semantic_name(),
        BoundTarget::ZhshQueryBuiltin(query) => match query {
            QueryBuiltin::Pwd => "pwd",
            QueryBuiltin::Type => "type",
            QueryBuiltin::CommandV => "command",
            QueryBuiltin::LiteralEcho => "echo",
        },
    };
    std::iter::once(program.to_owned())
        .chain(
            command
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        )
        .collect()
}

fn classify_path(
    program: &str,
    argument: &str,
    cwd: &Path,
    task_root: &Path,
) -> (DisclosureClass, String) {
    if argument == "-"
        || argument.starts_with('~')
        || argument.contains(['$', '`', '*', '?'])
        || is_sensitive_path(argument)
    {
        return (
            DisclosureClass::SensitiveOrUnbounded,
            format!("读取目标 {argument} 敏感、动态或无界"),
        );
    }
    let path = Path::new(argument);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let candidate = std::fs::canonicalize(&candidate).unwrap_or(candidate);
    if is_sensitive_path(&candidate.to_string_lossy()) {
        return (
            DisclosureClass::SensitiveOrUnbounded,
            format!("读取目标位于敏感配置或凭据路径: {}", candidate.display()),
        );
    }
    if program == "df" {
        return (
            DisclosureClass::Metadata,
            format!("df 只观察目标文件系统元数据: {}", candidate.display()),
        );
    }
    if is_public_system_metadata_path(program, &candidate) {
        return (
            DisclosureClass::Metadata,
            format!("读取公共系统软件路径元数据: {}", candidate.display()),
        );
    }
    if candidate.starts_with(task_root) {
        (
            DisclosureClass::WorkspaceContent,
            format!("读取任务根内路径 {}", candidate.display()),
        )
    } else {
        (
            DisclosureClass::SensitiveOrUnbounded,
            format!("读取目标越出任务根: {}", candidate.display()),
        )
    }
}

fn is_public_system_metadata_path(program: &str, path: &Path) -> bool {
    if !matches!(
        program,
        "ls" | "dir" | "vdir" | "stat" | "file" | "readlink" | "realpath" | "du"
    ) {
        return false;
    }
    path == Path::new("/")
        || [
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/usr/bin",
            "/usr/sbin",
            "/usr/lib",
            "/usr/lib64",
            "/usr/share",
        ]
        .iter()
        .any(|root| path.starts_with(root))
}

fn is_sensitive_path(path: &str) -> bool {
    let normalized = path.to_ascii_lowercase();
    [
        "/.ssh/",
        "/.gnupg/",
        "/.aws/",
        "/.azure/",
        "/.config/gcloud/",
        "/.zhsh/llm/",
        "/.zhsh/active-llm",
        "id_rsa",
        "id_ed25519",
        "credentials",
        ".netrc",
        "/.env",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn is_content_reader(program: &str) -> bool {
    matches!(
        program,
        "cat"
            | "tac"
            | "head"
            | "tail"
            | "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "find"
            | "sed"
            | "awk"
            | "less"
            | "more"
    )
}

fn is_metadata_reader(program: &str) -> bool {
    matches!(
        program,
        "ls" | "dir"
            | "vdir"
            | "stat"
            | "file"
            | "readlink"
            | "realpath"
            | "du"
            | "df"
            | "pwd"
            | "dirs"
            | "type"
            | "which"
            | "whereis"
    )
}

fn is_network_program(program: &str) -> bool {
    matches!(
        program,
        "curl" | "wget" | "scp" | "ssh" | "sftp" | "rsync" | "nc" | "ncat" | "netcat" | "socat"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_system_metadata_is_not_treated_as_sensitive_content() {
        let task_root = Path::new("/workspace/project");
        assert_eq!(
            classify_path("ls", "/usr/lib/jvm", task_root, task_root).0,
            DisclosureClass::Metadata
        );
        for path in ["/", "/home"] {
            assert_eq!(
                classify_path("df", path, task_root, task_root).0,
                DisclosureClass::Metadata
            );
        }
        assert_eq!(
            classify_path("cat", "/etc/shadow", task_root, task_root).0,
            DisclosureClass::SensitiveOrUnbounded
        );
    }
}
