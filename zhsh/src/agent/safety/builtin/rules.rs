//! Linux 核心工具的程序与参数规则；应用级工具由外部插件负责。

use super::super::{
    assess_builtin_script, DisclosureClass, SafetyAssessment, SafetyLevel, SupervisionClass,
};

const CORE_PROGRAMS: &[&str] = &[
    ":",
    "[",
    "[[",
    "awk",
    "bash",
    "basename",
    "cat",
    "chgrp",
    "chmod",
    "chown",
    "comm",
    "command",
    "cp",
    "cut",
    "dash",
    "date",
    "dd",
    "df",
    "dir",
    "dirname",
    "du",
    "echo",
    "egrep",
    "env",
    "exec",
    "false",
    "fgrep",
    "file",
    "find",
    "fish",
    "free",
    "grep",
    "groups",
    "head",
    "id",
    "install",
    "join",
    "journalctl",
    "kill",
    "killall",
    "last",
    "less",
    "ln",
    "locate",
    "ls",
    "mkdir",
    "mkfifo",
    "mknod",
    "more",
    "mount",
    "mv",
    "nohup",
    "paste",
    "patch",
    "pgrep",
    "pidof",
    "ping",
    "ping6",
    "pkill",
    "printenv",
    "printf",
    "ps",
    "pwd",
    "read",
    "readlink",
    "realpath",
    "rm",
    "rmdir",
    "sed",
    "setfacl",
    "sh",
    "shred",
    "sleep",
    "sort",
    "stat",
    "su",
    "sudo",
    "systemctl",
    "tac",
    "tail",
    "tee",
    "test",
    "time",
    "top",
    "touch",
    "tr",
    "true",
    "truncate",
    "type",
    "uname",
    "uniq",
    "unlink",
    "uptime",
    "vdir",
    "w",
    "watch",
    "wc",
    "whereis",
    "which",
    "who",
    "whoami",
    "xargs",
    "zh",
    "zsh",
];

pub(super) fn programs() -> &'static [&'static str] {
    CORE_PROGRAMS
}

pub(super) fn assess_program(
    program: &str,
    arguments: &[String],
    depth: usize,
) -> Option<SafetyAssessment> {
    Some(match program {
        "command" | "exec" | "nohup" | "time" | "env" => {
            let index = arguments
                .iter()
                .position(|argument| !argument.starts_with('-') && !is_assignment(argument));
            let Some(index) = index else {
                return Some(SafetyAssessment::new(
                    SafetyLevel::Unknown,
                    "命令包装器缺少要执行的程序",
                ));
            };
            assess_program(&arguments[index], &arguments[index + 1..], depth + 1).unwrap_or_else(
                || SafetyAssessment::new(SafetyLevel::Unknown, "内置插件无法识别包装器中的命令"),
            )
        }
        "sudo" | "su" => SafetyAssessment::privilege("包含提权或切换身份后执行的命令"),
        "rm" | "unlink" | "rmdir" | "shred" | "mv" | "truncate" | "kill" | "pkill" | "killall" => {
            SafetyAssessment::new(
                SafetyLevel::Destructive,
                "包含删除、移动、截断或终止目标的命令",
            )
        }
        "cp" | "install" | "mkdir" | "mkfifo" | "mknod" | "touch" | "ln" | "dd" | "chmod"
        | "chown" | "chgrp" | "setfacl" | "patch" => {
            SafetyAssessment::new(SafetyLevel::StateChanging, "包含创建或修改持久状态的命令")
        }
        "tee" => assess_tee(arguments),
        "find" => assess_find(arguments, depth),
        "sort" => assess_sort(arguments),
        "uniq" => assess_uniq(arguments),
        "sed" => assess_sed(arguments),
        "awk" => assess_awk(arguments),
        "bash" | "sh" | "dash" | "zsh" | "fish" => assess_nested_shell(arguments, depth),
        "xargs" => assess_xargs(arguments, depth),
        "systemctl" => assess_systemctl(arguments),
        "top" => assess_top(arguments),
        "tail" => assess_tail(arguments),
        "journalctl" => assess_journalctl(arguments),
        "ping" | "ping6" => assess_ping(arguments),
        "watch" => potentially_unbounded("watch_continuous"),
        "less" | "more" => potentially_unbounded("interactive_pager"),
        "sleep" if arguments.iter().any(|argument| argument == "infinity") => {
            potentially_unbounded("sleep_infinity")
        }
        "mount" if arguments.is_empty() => SafetyAssessment::read_only(),
        "date" if !sets_date(arguments) => SafetyAssessment::read_only(),
        program if is_read_only_program(program) => SafetyAssessment::read_only(),
        _ => return None,
    })
}

pub(super) fn handles_program(program: &str) -> bool {
    CORE_PROGRAMS.contains(&program)
}

pub(super) fn known_write_targets<'a>(program: &str, arguments: &'a [String]) -> Vec<&'a str> {
    if matches!(program, "cp" | "mv" | "ln" | "install") {
        if let Some(index) = arguments
            .iter()
            .position(|argument| argument == "-t" || argument == "--target-directory")
        {
            return arguments
                .get(index + 1)
                .map(String::as_str)
                .into_iter()
                .collect();
        }
        if let Some(target) = arguments.iter().find_map(|argument| {
            argument
                .strip_prefix("--target-directory=")
                .filter(|target| !target.is_empty())
        }) {
            return vec![target];
        }
    }
    let positional: Vec<_> = arguments
        .iter()
        .filter(|argument| !argument.starts_with('-'))
        .map(String::as_str)
        .collect();
    match program {
        "touch" | "mkdir" | "mkfifo" | "mknod" | "chmod" | "chown" | "chgrp" | "setfacl" | "rm"
        | "unlink" | "rmdir" | "shred" | "truncate" | "tee" => positional,
        "install"
            if arguments
                .iter()
                .any(|argument| argument == "-d" || argument == "--directory") =>
        {
            positional
        }
        "cp" | "mv" | "ln" | "install" => positional.last().copied().into_iter().collect(),
        _ => Vec::new(),
    }
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn has_in_place(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        argument == "-i"
            || argument.starts_with("-i")
            || argument == "--in-place"
            || argument.starts_with("--in-place=")
    })
}

fn assess_sed(arguments: &[String]) -> SafetyAssessment {
    if has_in_place(arguments) {
        return SafetyAssessment::new(SafetyLevel::StateChanging, "包含原地修改文件的选项");
    }
    let mut scripts = Vec::new();
    let mut index = 0;
    let mut explicit_script = false;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(argument, "-f" | "--file") || argument.starts_with("--file=") {
            return SafetyAssessment::new(SafetyLevel::Unknown, "sed 使用外部脚本文件");
        }
        if matches!(argument, "-e" | "--expression") {
            let Some(script) = arguments.get(index + 1) else {
                return SafetyAssessment::new(SafetyLevel::Unknown, "sed 表达式参数缺失");
            };
            scripts.push(script.as_str());
            explicit_script = true;
            index += 2;
            continue;
        }
        if let Some(script) = argument.strip_prefix("--expression=") {
            scripts.push(script);
            explicit_script = true;
            index += 1;
            continue;
        }
        if let Some(script) = argument
            .strip_prefix("-e")
            .filter(|value| !value.is_empty())
        {
            scripts.push(script);
            explicit_script = true;
            index += 1;
            continue;
        }
        if matches!(
            argument,
            "-n" | "--quiet"
                | "--silent"
                | "-E"
                | "-r"
                | "--regexp-extended"
                | "-s"
                | "--separate"
                | "-u"
                | "--unbuffered"
                | "-z"
                | "--null-data"
        ) {
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            return SafetyAssessment::new(SafetyLevel::Unknown, "sed 使用未知或未支持选项");
        }
        break;
    }
    if !explicit_script {
        let Some(script) = arguments.get(index) else {
            return SafetyAssessment::new(SafetyLevel::Unknown, "sed 缺少脚本");
        };
        scripts.push(script);
    }
    if scripts.iter().all(|script| safe_sed_script(script)) {
        SafetyAssessment::read_only()
    } else {
        SafetyAssessment::new(SafetyLevel::Unknown, "sed 脚本可能执行命令或写入文件")
    }
}

fn safe_sed_script(script: &str) -> bool {
    script.split([';', '\n']).all(|command| {
        let command = command.trim();
        if command.is_empty() || matches!(command, "p" | "d" | "q" | "Q" | "=") {
            return true;
        }
        let mut characters = command.chars();
        if characters.next() != Some('s') {
            return false;
        }
        let Some(delimiter) = characters.next() else {
            return false;
        };
        if delimiter.is_ascii_alphanumeric() || delimiter == '\\' {
            return false;
        }
        let rest: String = characters.collect();
        let mut escaped = false;
        let mut delimiters = 0;
        let mut flags = String::new();
        for character in rest.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                delimiters += 1;
            } else if delimiters >= 2 {
                flags.push(character);
            }
        }
        delimiters >= 2
            && flags.chars().all(|flag| {
                flag.is_ascii_digit() || matches!(flag, 'g' | 'p' | 'i' | 'I' | 'm' | 'M')
            })
    })
}

fn assess_awk(arguments: &[String]) -> SafetyAssessment {
    let mut index = 0;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(argument, "-f" | "--file") || argument.starts_with("--file=") {
            return SafetyAssessment::new(SafetyLevel::Unknown, "awk 使用外部程序文件");
        }
        if matches!(argument, "-v" | "-F") {
            if arguments.get(index + 1).is_none() {
                return SafetyAssessment::new(SafetyLevel::Unknown, "awk 选项参数缺失");
            }
            index += 2;
            continue;
        }
        if argument.starts_with("-v") && argument.len() > 2
            || argument.starts_with("-F") && argument.len() > 2
        {
            index += 1;
            continue;
        }
        if argument.starts_with('-') {
            return SafetyAssessment::new(SafetyLevel::Unknown, "awk 使用未知或未支持选项");
        }
        break;
    }
    let Some(program) = arguments.get(index) else {
        return SafetyAssessment::new(SafetyLevel::Unknown, "awk 缺少程序文本");
    };
    let lowered = program.to_ascii_lowercase();
    if lowered.contains("system")
        || lowered.contains("getline")
        || unquoted_awk_redirection(program)
    {
        SafetyAssessment::new(SafetyLevel::Unknown, "awk 程序可能执行命令或访问额外文件")
    } else {
        SafetyAssessment::read_only()
    }
}

fn unquoted_awk_redirection(program: &str) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for character in program.chars() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if !quoted && matches!(character, '>' | '|') {
            return true;
        }
    }
    false
}

fn sets_date(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        argument == "-s"
            || argument.starts_with("-s") && !argument.starts_with("--")
            || argument == "--set"
            || argument.starts_with("--set=")
    })
}

fn assess_tee(arguments: &[String]) -> SafetyAssessment {
    let has_target = arguments.iter().any(|argument| !argument.starts_with('-'));
    if !has_target {
        SafetyAssessment::read_only()
    } else if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "-a" | "--append"))
    {
        SafetyAssessment::new(SafetyLevel::StateChanging, "tee 会追加或创建输出文件")
            .with_file_output()
    } else {
        SafetyAssessment::new(SafetyLevel::Destructive, "tee 可能创建、覆盖或截断输出文件")
            .with_file_output()
    }
}

fn assess_find(arguments: &[String], depth: usize) -> SafetyAssessment {
    if arguments.iter().any(|argument| argument == "-delete") {
        return SafetyAssessment::new(SafetyLevel::Destructive, "find 使用了 -delete");
    }
    if arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "-fprint" | "-fprint0" | "-fprintf" | "-fls"
        )
    }) {
        return SafetyAssessment::new(SafetyLevel::StateChanging, "find 会把结果写入文件")
            .with_file_output();
    }
    let mut assessment = SafetyAssessment::read_only();
    for (index, argument) in arguments.iter().enumerate() {
        if matches!(argument.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
            let Some((program, nested_arguments)) = arguments
                .get(index + 1..)
                .and_then(|nested| nested.split_first())
            else {
                return SafetyAssessment::new(SafetyLevel::Unknown, "find 的嵌套命令不完整");
            };
            let nested =
                assess_program(program, nested_arguments, depth + 1).unwrap_or_else(|| {
                    SafetyAssessment::new(SafetyLevel::Unknown, "内置插件无法识别 find 的嵌套命令")
                });
            assessment = assessment.merge(nested);
        }
    }
    assessment
}

fn assess_sort(arguments: &[String]) -> SafetyAssessment {
    if arguments.iter().any(|argument| {
        argument == "-o"
            || argument.starts_with("-o") && !argument.starts_with("--")
            || argument == "--output"
            || argument.starts_with("--output=")
    }) {
        SafetyAssessment::new(SafetyLevel::Destructive, "sort 会覆盖输出文件").with_file_output()
    } else if arguments
        .iter()
        .any(|argument| argument.starts_with("--compress-program"))
    {
        SafetyAssessment::new(SafetyLevel::Unknown, "sort 会启动外部压缩程序")
    } else {
        SafetyAssessment::read_only()
    }
}

fn assess_uniq(arguments: &[String]) -> SafetyAssessment {
    if arguments
        .iter()
        .filter(|argument| !argument.starts_with('-'))
        .count()
        >= 2
    {
        SafetyAssessment::new(SafetyLevel::Destructive, "uniq 会覆盖指定输出文件")
            .with_file_output()
    } else {
        SafetyAssessment::read_only()
    }
}

fn assess_nested_shell(arguments: &[String], depth: usize) -> SafetyAssessment {
    let Some(position) = arguments.iter().position(|argument| argument == "-c") else {
        return SafetyAssessment::new(SafetyLevel::Unknown, "无法分析解释器将执行的脚本");
    };
    let Some(script) = arguments.get(position + 1) else {
        return SafetyAssessment::new(SafetyLevel::Unknown, "解释器 -c 缺少脚本文本");
    };
    assess_builtin_script(script, depth + 1)
}

fn assess_xargs(arguments: &[String], depth: usize) -> SafetyAssessment {
    let Some(index) = arguments
        .iter()
        .position(|argument| !argument.starts_with('-'))
    else {
        return SafetyAssessment::read_only();
    };
    assess_program(&arguments[index], &arguments[index + 1..], depth + 1).unwrap_or_else(|| {
        SafetyAssessment::new(SafetyLevel::Unknown, "内置插件无法识别 xargs 的命令")
    })
}

fn assess_systemctl(arguments: &[String]) -> SafetyAssessment {
    if arguments
        .iter()
        .any(|argument| is_remote_systemctl_option(argument))
    {
        return SafetyAssessment::new(SafetyLevel::Unknown, "systemctl 指定了远程 host 或 machine")
            .with_disclosure(
                DisclosureClass::SensitiveOrUnbounded,
                "远程 systemctl 可能向其他主机披露请求和结果",
            )
            .force_confirmation("systemctl 远程目标必须明确确认");
    }
    if has_unknown_systemctl_option(arguments) {
        return SafetyAssessment::new(SafetyLevel::Unknown, "systemctl 使用了未知全局选项")
            .force_confirmation("无法可靠定位 systemctl subcommand");
    }
    let mut index = 0;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            index += 1;
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            break;
        }
        match systemctl_option_arity(argument) {
            Some(0) => index += 1,
            Some(1) if argument.contains('=') || short_option_has_attached_value(argument) => {
                index += 1
            }
            Some(1) if arguments.get(index + 1).is_some() => index += 2,
            Some(1) => {
                return SafetyAssessment::new(SafetyLevel::Unknown, "systemctl 全局选项缺少参数")
                    .force_confirmation("无法确定 systemctl subcommand");
            }
            _ => {
                return SafetyAssessment::new(SafetyLevel::Unknown, "systemctl 使用了未知全局选项")
                    .force_confirmation("无法可靠定位 systemctl subcommand");
            }
        }
    }

    let Some(subcommand) = arguments.get(index).map(String::as_str) else {
        return SafetyAssessment::new(SafetyLevel::Unknown, "systemctl 缺少 subcommand")
            .force_confirmation("无法确定 systemctl 操作");
    };
    if matches!(
        subcommand,
        "status" | "show" | "is-active" | "is-enabled" | "list-units" | "list-unit-files"
    ) {
        SafetyAssessment::read_only()
    } else if matches!(
        subcommand,
        "start"
            | "stop"
            | "restart"
            | "reload"
            | "enable"
            | "disable"
            | "mask"
            | "unmask"
            | "edit"
            | "set-property"
            | "daemon-reload"
    ) {
        SafetyAssessment::new(
            SafetyLevel::StateChanging,
            format!("systemctl {subcommand} 会或可能改变服务状态"),
        )
    } else {
        SafetyAssessment::new(
            SafetyLevel::Unknown,
            format!("systemctl subcommand {subcommand} 未列入只读白名单"),
        )
    }
}

fn potentially_unbounded(reason: &str) -> SafetyAssessment {
    let mut assessment = SafetyAssessment::new(SafetyLevel::ReadOnly, reason);
    assessment.supervision = SupervisionClass::PotentiallyUnbounded;
    assessment
}

fn assess_top(arguments: &[String]) -> SafetyAssessment {
    let batch = arguments
        .iter()
        .any(|argument| argument == "-b" || argument == "--batch" || argument.starts_with("-b"));
    let finite = arguments.iter().enumerate().any(|(index, argument)| {
        argument
            .strip_prefix("--iterations=")
            .or_else(|| {
                argument
                    .strip_prefix("-n")
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                (argument == "-n")
                    .then(|| arguments.get(index + 1))
                    .flatten()
                    .map(String::as_str)
            })
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|count| count > 0)
    });
    if batch && finite {
        SafetyAssessment::read_only()
    } else {
        potentially_unbounded("top_requires_finite_batch_mode")
    }
}

fn assess_tail(arguments: &[String]) -> SafetyAssessment {
    if arguments.iter().any(|argument| {
        argument == "-f"
            || argument == "-F"
            || argument == "--follow"
            || argument.starts_with("--follow=")
    }) {
        potentially_unbounded("tail_follow")
    } else {
        SafetyAssessment::read_only()
    }
}

fn assess_journalctl(arguments: &[String]) -> SafetyAssessment {
    if arguments
        .iter()
        .any(|argument| argument == "-f" || argument == "--follow")
    {
        potentially_unbounded("journalctl_follow")
    } else {
        SafetyAssessment::read_only()
    }
}

fn assess_ping(arguments: &[String]) -> SafetyAssessment {
    let finite = arguments.iter().enumerate().any(|(index, argument)| {
        matches!(argument.as_str(), "-c" | "-w") && arguments.get(index + 1).is_some()
            || argument.starts_with("-c") && argument.len() > 2
            || argument.starts_with("-w") && argument.len() > 2
    });
    if finite {
        SafetyAssessment::read_only()
    } else {
        potentially_unbounded("ping_without_limit")
    }
}

fn has_unknown_systemctl_option(arguments: &[String]) -> bool {
    let mut index = 0;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            index += 1;
            continue;
        }
        let Some(arity) = systemctl_option_arity(argument) else {
            return true;
        };
        if arity == 1 && !argument.contains('=') && !short_option_has_attached_value(argument) {
            if arguments.get(index + 1).is_none() {
                return true;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    false
}

fn is_remote_systemctl_option(argument: &str) -> bool {
    argument == "-H"
        || argument.starts_with("-H") && argument.len() > 2
        || argument == "-M"
        || argument.starts_with("-M") && argument.len() > 2
        || argument == "--host"
        || argument.starts_with("--host=")
        || argument == "--machine"
        || argument.starts_with("--machine=")
}

fn short_option_has_attached_value(argument: &str) -> bool {
    matches!(
        argument.as_bytes().get(1),
        Some(b't' | b'p' | b'P' | b'n' | b'o' | b's')
    ) && argument.len() > 2
}

fn systemctl_option_arity(argument: &str) -> Option<u8> {
    if short_option_has_attached_value(argument) {
        return Some(1);
    }
    let option = argument.split_once('=').map_or(argument, |(name, _)| name);
    if matches!(
        option,
        "-q" | "--quiet"
            | "-h"
            | "--help"
            | "--version"
            | "-l"
            | "--full"
            | "-a"
            | "--all"
            | "--failed"
            | "--plain"
            | "--no-pager"
            | "--no-ask-password"
            | "--system"
            | "--user"
            | "--global"
            | "--runtime"
            | "-r"
            | "--recursive"
            | "--reverse"
            | "--with-dependencies"
            | "-T"
            | "--show-transaction"
            | "--show-types"
            | "--value"
            | "-i"
            | "--now"
            | "--dry-run"
            | "--no-warn"
            | "--wait"
            | "--no-block"
            | "--no-wall"
            | "--no-reload"
            | "-f"
            | "--force"
    ) {
        Some(0)
    } else if matches!(
        option,
        "-t" | "--type"
            | "-p"
            | "--property"
            | "-P"
            | "-n"
            | "--lines"
            | "-o"
            | "--output"
            | "--state"
            | "--job-mode"
            | "--check-inhibitors"
            | "--kill-whom"
            | "--kill-value"
            | "-s"
            | "--signal"
            | "--what"
            | "--legend"
            | "--preset-mode"
            | "--root"
            | "--image"
            | "--image-policy"
            | "--namespace"
    ) {
        Some(1)
    } else {
        None
    }
}

fn is_read_only_program(program: &str) -> bool {
    matches!(
        program,
        ":" | "true"
            | "false"
            | "test"
            | "["
            | "[["
            | "pwd"
            | "type"
            | "which"
            | "whereis"
            | "basename"
            | "dirname"
            | "realpath"
            | "readlink"
            | "stat"
            | "file"
            | "ls"
            | "dir"
            | "vdir"
            | "locate"
            | "cat"
            | "tac"
            | "head"
            | "grep"
            | "egrep"
            | "fgrep"
            | "wc"
            | "cut"
            | "paste"
            | "join"
            | "comm"
            | "tr"
            | "printf"
            | "echo"
            | "read"
            | "sleep"
            | "ps"
            | "pgrep"
            | "pidof"
            | "free"
            | "uptime"
            | "who"
            | "w"
            | "last"
            | "id"
            | "groups"
            | "whoami"
            | "uname"
            | "df"
            | "du"
            | "printenv"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn systemctl_uses_the_real_subcommand_position() {
        for query in [
            args(&["status", "sshd.service"]),
            args(&["--no-pager", "show", "sshd.service"]),
            args(&["--type", "service", "list-units"]),
            args(&["-tservice", "list-units"]),
        ] {
            assert_eq!(assess_systemctl(&query).level, SafetyLevel::ReadOnly);
        }

        for mutation in [
            args(&["restart", "status"]),
            args(&["--no-pager", "enable", "sshd.service"]),
            args(&["daemon-reload"]),
        ] {
            assert_ne!(assess_systemctl(&mutation).level, SafetyLevel::ReadOnly);
        }
    }

    #[test]
    fn systemctl_remote_and_unknown_options_force_confirmation() {
        for command in [
            args(&["--host", "server", "status", "sshd"]),
            args(&["-Mcontainer", "show", "sshd"]),
            args(&["status", "sshd", "--host=server"]),
            args(&["--future-option", "status"]),
            args(&["status", "--future-option"]),
        ] {
            assert!(assess_systemctl(&command).mandatory_confirmation);
        }
    }
}
