//! Agent 命令在分类、确认和执行之间共享的不可变计划。
//!
//! 这里有意只实现可静态绑定的单一简单命令。需要完整 Bash 语义的输入仍由 Bash
//! 执行，但计划明确标记为动态，安全策略不能把它自动放行。

use super::agent_compound::{self, BoundExpression};
use super::command::{self, resolver};
use super::SessionState;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub(crate) use resolver::{ExecutableBinding, FileIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentCommandPlan {
    pub(crate) original: String,
    pub(crate) executable: AgentExecutionTarget,
    pub(crate) cwd: PathBuf,
    pub(crate) path_snapshot: Option<OsString>,
    pub(crate) invocations: Vec<ResolvedInvocation>,
    pub(crate) dynamic_resolution: bool,
    pub(crate) unsupported_execution: Option<UnsupportedExecution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentExecutionTarget {
    ZhshBuiltin {
        name: String,
        arguments: Vec<String>,
    },
    External {
        path: PathBuf,
        arguments: Vec<OsString>,
        identity: FileIdentity,
    },
    BoundCompound {
        expression: BoundExpression,
    },
    Bash {
        script: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandTargetKind {
    ZhshBuiltin,
    BashBuiltin,
    Alias,
    Function,
    External,
    DynamicOrUnresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedInvocation {
    pub(crate) original: String,
    pub(crate) kind: CommandTargetKind,
    resolution: Option<resolver::AgentResolvedExecutable>,
    pub(crate) binding: ExecutableBinding,
    pub(crate) binding_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnsupportedExecution {
    BackgroundOperator,
    DetachProgram {
        name: String,
    },
    ProtectedBuiltin {
        name: String,
    },
    #[allow(dead_code)]
    UnsupportedShellForm,
}

impl UnsupportedExecution {
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::BackgroundOperator => "Agent 不支持后台执行运算符 &".into(),
            Self::DetachProgram { name } => format!("Agent 不支持脱离监督的程序: {name}"),
            Self::ProtectedBuiltin { name } => {
                format!("Agent 不允许执行会话状态命令: {name}")
            }
            Self::UnsupportedShellForm => "Agent 不支持该 Shell 执行形态".into(),
        }
    }
}

impl AgentCommandPlan {
    /// Native 内建计划不应用 Bash 过渡模式的只读内建白名单；执行仍须经过 Agent Safety/授权。
    pub(super) fn from_native_builtin(
        original: String,
        name: String,
        arguments: Vec<String>,
        cwd: PathBuf,
        path_snapshot: Option<OsString>,
    ) -> Self {
        Self {
            original,
            executable: AgentExecutionTarget::ZhshBuiltin {
                name: name.clone(),
                arguments,
            },
            cwd,
            path_snapshot,
            invocations: vec![ResolvedInvocation::named(
                name,
                CommandTargetKind::ZhshBuiltin,
                ExecutableBinding::SystemTrusted,
                None,
            )],
            dynamic_resolution: false,
            unsupported_execution: None,
        }
    }

    /// 授权视图只包装已准备的 Native 外部调用，不重新解释原文。
    pub(super) fn from_native_external(prepared: super::native::PreparedNativeExternal) -> Self {
        let arguments: Vec<String> = prepared
            .arguments
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut words = vec![prepared.program.clone()];
        words.extend(arguments.clone());
        let mut invocations = vec![ResolvedInvocation::external(
            prepared.program.clone(),
            &prepared.target,
        )];
        if is_wrapper(&prepared.program) || has_nested_execution(&prepared.program, &arguments) {
            // Native only binds the direct target. Existing Safety still analyzes argv, while
            // downstream executable identities remain conservative (including script contents).
            invocations.push(ResolvedInvocation::named(
                prepared.program.clone(),
                CommandTargetKind::DynamicOrUnresolved,
                ExecutableBinding::Dynamic,
                Some("外部程序的下游执行域未静态绑定".into()),
            ));
        }
        Self {
            original: prepared.original,
            executable: AgentExecutionTarget::External {
                path: prepared.target.resolved_path,
                arguments: prepared.arguments,
                identity: prepared.target.identity,
            },
            cwd: prepared.cwd,
            path_snapshot: prepared.path_snapshot,
            invocations,
            dynamic_resolution: false,
            unsupported_execution: unsupported_execution_in_words(&words, 0),
        }
    }

    pub(super) fn prepare(session: &SessionState, input: &str) -> Self {
        let original = input.trim().to_string();
        let path_snapshot = session.env.get("PATH").map(OsString::from);
        let cwd = session.cwd.clone();
        if let Ok(words) = command::args::parse(&original) {
            if let Some((name, arguments)) = words.split_first() {
                if command::is_builtin(name) {
                    let unsupported_execution = unsupported_execution(&original).or_else(|| {
                        (!command::agent_allows(name, arguments))
                            .then(|| UnsupportedExecution::ProtectedBuiltin { name: name.clone() })
                    });
                    return Self {
                        original: original.clone(),
                        executable: AgentExecutionTarget::ZhshBuiltin {
                            name: name.clone(),
                            arguments: arguments.to_vec(),
                        },
                        cwd,
                        path_snapshot,
                        invocations: vec![ResolvedInvocation::named(
                            name.clone(),
                            CommandTargetKind::ZhshBuiltin,
                            ExecutableBinding::SystemTrusted,
                            None,
                        )],
                        dynamic_resolution: false,
                        unsupported_execution,
                    };
                }
            }
        }
        let script = session.expand_alias(&original);
        let unsupported_execution = unsupported_execution(&script);

        let Ok(words) = command::args::parse(&script) else {
            return Self::compound_or_dynamic(
                session,
                original,
                script,
                cwd,
                path_snapshot,
                unsupported_execution,
            );
        };
        let Some((name, arguments)) = words.split_first() else {
            return Self {
                original,
                executable: AgentExecutionTarget::Bash { script },
                cwd,
                path_snapshot,
                invocations: Vec::new(),
                dynamic_resolution: true,
                unsupported_execution,
            };
        };

        if words.iter().any(|word| word.starts_with('~')) {
            let mut invocations = Vec::new();
            append_resolved_invocation(session, name, &mut invocations);
            return Self::dynamic_bash(
                original,
                script,
                cwd,
                path_snapshot,
                invocations,
                unsupported_execution,
            );
        }

        if is_assignment(name) {
            let mut invocations = Vec::new();
            if let Some(next) = words.iter().find(|word| !is_assignment(word)) {
                append_resolved_invocation(session, next, &mut invocations);
            }
            return Self::dynamic_bash(
                original,
                script,
                cwd,
                path_snapshot,
                invocations,
                unsupported_execution,
            );
        }

        if session.functions.contains_key(name) {
            return Self::dynamic_bash(
                original,
                script,
                cwd,
                path_snapshot,
                vec![ResolvedInvocation::named(
                    name.clone(),
                    CommandTargetKind::Function,
                    ExecutableBinding::Dynamic,
                    Some("命令首词命中会话 Bash function".into()),
                )],
                unsupported_execution,
            );
        }

        let semantic_name = semantic_name(name);
        let bash_builtin = !name.contains('/') && is_bash_builtin(semantic_name);
        if bash_builtin
            || is_wrapper(semantic_name)
            || has_nested_execution(semantic_name, arguments)
        {
            if let Ok((expression, invocations)) = agent_compound::bind(session, &script) {
                return Self {
                    original,
                    executable: AgentExecutionTarget::BoundCompound { expression },
                    cwd,
                    path_snapshot,
                    invocations,
                    dynamic_resolution: false,
                    unsupported_execution,
                };
            }
            let mut invocations = Vec::new();
            append_resolved_invocation(session, name, &mut invocations);
            append_nested_invocations(session, semantic_name, arguments, &mut invocations);
            return Self::dynamic_bash(
                original,
                script,
                cwd,
                path_snapshot,
                invocations,
                unsupported_execution,
            );
        }

        let Some(external) = resolver::resolve_agent_executable(session, name) else {
            return Self::dynamic_bash(
                original,
                script,
                cwd,
                path_snapshot,
                vec![ResolvedInvocation::named(
                    name.clone(),
                    CommandTargetKind::DynamicOrUnresolved,
                    ExecutableBinding::Dynamic,
                    Some("PATH 中没有可绑定的执行目标".into()),
                )],
                unsupported_execution,
            );
        };
        let invocation = ResolvedInvocation::external(name.clone(), &external);
        Self {
            original,
            executable: AgentExecutionTarget::External {
                path: external.canonical_path,
                arguments: arguments.iter().map(OsString::from).collect(),
                identity: external.identity,
            },
            cwd,
            path_snapshot,
            invocations: vec![invocation],
            dynamic_resolution: false,
            unsupported_execution,
        }
    }

    fn dynamic_bash(
        original: String,
        script: String,
        cwd: PathBuf,
        path_snapshot: Option<OsString>,
        invocations: Vec<ResolvedInvocation>,
        unsupported_execution: Option<UnsupportedExecution>,
    ) -> Self {
        Self {
            original,
            executable: AgentExecutionTarget::Bash { script },
            cwd,
            path_snapshot,
            invocations,
            dynamic_resolution: true,
            unsupported_execution,
        }
    }

    fn compound_or_dynamic(
        session: &SessionState,
        original: String,
        script: String,
        cwd: PathBuf,
        path_snapshot: Option<OsString>,
        unsupported_execution: Option<UnsupportedExecution>,
    ) -> Self {
        match agent_compound::bind(session, &script) {
            Ok((expression, invocations)) => Self {
                original,
                executable: AgentExecutionTarget::BoundCompound { expression },
                cwd,
                path_snapshot,
                invocations,
                dynamic_resolution: false,
                unsupported_execution,
            },
            Err(mut invocations) => {
                if invocations.is_empty() {
                    append_first_resolved_invocation(session, &script, &mut invocations);
                }
                Self::dynamic_bash(
                    original,
                    script,
                    cwd,
                    path_snapshot,
                    invocations,
                    unsupported_execution,
                )
            }
        }
    }

    pub(super) fn external_identity_is_current(
        &self,
        environment: &HashMap<String, String>,
    ) -> bool {
        let invocations_current = self.invocations.iter().all(|invocation| {
            invocation.resolution_is_current(&self.cwd, self.path_snapshot.as_deref(), environment)
        });
        invocations_current
            && match &self.executable {
                AgentExecutionTarget::External { path, identity, .. } => {
                    resolver::identity_matches(path, *identity)
                }
                AgentExecutionTarget::BoundCompound { expression } => {
                    expression.external_identities_are_current()
                }
                _ => true,
            }
    }
}

impl ResolvedInvocation {
    pub(crate) fn named(
        original: String,
        kind: CommandTargetKind,
        binding: ExecutableBinding,
        binding_reason: Option<String>,
    ) -> Self {
        Self {
            original,
            kind,
            resolution: None,
            binding,
            binding_reason,
        }
    }

    pub(super) fn external(original: String, target: &resolver::AgentResolvedExecutable) -> Self {
        Self {
            original,
            kind: CommandTargetKind::External,
            resolution: Some(target.clone()),
            binding: target.binding,
            binding_reason: target.binding_reason.clone(),
        }
    }

    /// 返回用户命令中声明的程序名；规范路径只用于身份校验，不能改变 Safety 路由语义。
    pub(crate) fn semantic_name(&self) -> &str {
        semantic_name(&self.original)
    }

    pub(crate) fn target_path(&self) -> Option<&Path> {
        self.resolution
            .as_ref()
            .map(|resolution| resolution.canonical_path.as_path())
    }

    fn resolution_is_current(
        &self,
        cwd: &Path,
        path_value: Option<&OsStr>,
        environment: &HashMap<String, String>,
    ) -> bool {
        self.resolution.as_ref().is_none_or(|expected| {
            resolver::resolution_matches(cwd, path_value, &self.original, expected, environment)
        })
    }
}

fn append_first_resolved_invocation(
    session: &SessionState,
    script: &str,
    invocations: &mut Vec<ResolvedInvocation>,
) {
    if let Some(name) = first_literal_word(script) {
        append_resolved_invocation(session, name, invocations);
    }
}

fn append_resolved_invocation(
    session: &SessionState,
    name: &str,
    invocations: &mut Vec<ResolvedInvocation>,
) {
    if session.aliases.contains_key(name) {
        invocations.push(ResolvedInvocation::named(
            name.into(),
            CommandTargetKind::Alias,
            ExecutableBinding::Dynamic,
            Some("命中会话 alias".into()),
        ));
    } else if session.functions.contains_key(name) {
        invocations.push(ResolvedInvocation::named(
            name.into(),
            CommandTargetKind::Function,
            ExecutableBinding::Dynamic,
            Some("命中会话 Bash function".into()),
        ));
    } else if command::is_builtin(name) {
        invocations.push(ResolvedInvocation::named(
            name.into(),
            CommandTargetKind::ZhshBuiltin,
            ExecutableBinding::SystemTrusted,
            None,
        ));
    } else if is_bash_builtin(name) {
        invocations.push(ResolvedInvocation::named(
            name.into(),
            CommandTargetKind::BashBuiltin,
            ExecutableBinding::Dynamic,
            Some("目标由 Bash builtin 解释".into()),
        ));
    } else if let Some(external) = resolver::resolve_agent_executable(session, name) {
        invocations.push(ResolvedInvocation::external(name.into(), &external));
    } else {
        invocations.push(ResolvedInvocation::named(
            name.into(),
            CommandTargetKind::DynamicOrUnresolved,
            ExecutableBinding::Dynamic,
            Some("无法静态解析实际执行目标".into()),
        ));
    }
}

fn append_nested_invocations(
    session: &SessionState,
    wrapper: &str,
    arguments: &[String],
    invocations: &mut Vec<ResolvedInvocation>,
) {
    append_nested_invocations_at_depth(session, wrapper, arguments, invocations, 0);
}

fn append_nested_invocations_at_depth(
    session: &SessionState,
    wrapper: &str,
    arguments: &[String],
    invocations: &mut Vec<ResolvedInvocation>,
    depth: usize,
) {
    if depth >= 8 {
        invocations.push(ResolvedInvocation::named(
            wrapper.into(),
            CommandTargetKind::DynamicOrUnresolved,
            ExecutableBinding::Dynamic,
            Some("嵌套命令超过静态解析深度".into()),
        ));
        return;
    }

    let nested_words = match wrapper {
        "command" | "exec" | "time" | "nohup" | "env" | "xargs" => {
            wrapper_command_index(wrapper, arguments).map(|index| arguments[index..].to_vec())
        }
        "bash" | "sh" | "dash" | "zsh" | "fish" => arguments
            .iter()
            .position(|argument| shell_option_executes_string(argument))
            .and_then(|index| arguments.get(index + 1))
            .and_then(|script| command::args::parse(script).ok()),
        "eval" => command::args::parse(&arguments.join(" ")).ok(),
        "find" => arguments
            .iter()
            .position(|argument| {
                matches!(argument.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
            })
            .map(|index| {
                arguments[index + 1..]
                    .iter()
                    .take_while(|argument| !matches!(argument.as_str(), ";" | "+"))
                    .cloned()
                    .collect()
            }),
        "sort" => arguments.iter().find_map(|argument| {
            argument
                .strip_prefix("--compress-program=")
                .filter(|program| !program.is_empty())
                .map(|program| vec![program.to_owned()])
        }),
        _ => None,
    };
    let Some(words) = nested_words else {
        return;
    };
    append_nested_words(session, &words, invocations, depth + 1);
}

fn append_nested_words(
    session: &SessionState,
    words: &[String],
    invocations: &mut Vec<ResolvedInvocation>,
    depth: usize,
) {
    let Some(index) = words.iter().position(|word| !is_assignment(word)) else {
        return;
    };
    let name = &words[index];
    append_resolved_invocation(session, name, invocations);
    let semantic = semantic_name(name);
    let arguments = &words[index + 1..];
    if is_wrapper(semantic) || has_nested_execution(semantic, arguments) {
        append_nested_invocations_at_depth(session, semantic, arguments, invocations, depth);
    }
}

fn is_wrapper(name: &str) -> bool {
    matches!(
        name,
        "command"
            | "exec"
            | "time"
            | "nohup"
            | "env"
            | "xargs"
            | "bash"
            | "sh"
            | "dash"
            | "zsh"
            | "fish"
            | "eval"
    )
}

fn has_nested_execution(name: &str, arguments: &[String]) -> bool {
    name == "find"
        && arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
        || name == "sort"
            && arguments
                .iter()
                .any(|argument| argument.starts_with("--compress-program"))
}

fn semantic_name(name: &str) -> &str {
    std::path::Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name)
}

fn is_bash_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "true"
            | "false"
            | "test"
            | "["
            | "[["
            | "printf"
            | "echo"
            | "read"
            | "mapfile"
            | "readarray"
            | "set"
            | "shopt"
            | "declare"
            | "typeset"
            | "local"
            | "caller"
            | "getopts"
            | "hash"
            | "jobs"
            | "wait"
            | "builtin"
            | "enable"
            | "eval"
            | "compgen"
            | "complete"
    )
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn unsupported_execution(script: &str) -> Option<UnsupportedExecution> {
    unsupported_execution_at_depth(script, 0)
}

fn unsupported_execution_at_depth(script: &str, depth: usize) -> Option<UnsupportedExecution> {
    if depth > 8 {
        return Some(UnsupportedExecution::UnsupportedShellForm);
    }
    if contains_background_operator(script) {
        return Some(UnsupportedExecution::BackgroundOperator);
    }
    if let Ok(words) = command::args::parse(script) {
        if let Some(unsupported) = unsupported_execution_in_words(&words, depth) {
            return Some(unsupported);
        }
    }
    command_heads(script)
        .into_iter()
        .find(|name| is_detach_program(name))
        .map(|name| UnsupportedExecution::DetachProgram { name })
}

fn unsupported_execution_in_words(words: &[String], depth: usize) -> Option<UnsupportedExecution> {
    if depth > 8 {
        return Some(UnsupportedExecution::UnsupportedShellForm);
    }
    let mut index = 0;
    while words
        .get(index)
        .is_some_and(|word| is_assignment(word) || word == "!")
    {
        index += 1;
    }
    let name = words.get(index).map(String::as_str)?;
    let semantic = semantic_name(name);
    if is_detach_program(semantic) {
        return Some(UnsupportedExecution::DetachProgram {
            name: semantic.into(),
        });
    }
    let arguments = &words[index + 1..];
    match semantic {
        "command" | "exec" | "time" | "env" | "xargs" => {
            let nested = wrapper_command_index(semantic, arguments)?;
            unsupported_execution_in_words(&arguments[nested..], depth + 1)
        }
        "bash" | "sh" | "dash" | "zsh" | "fish" => {
            let position = arguments
                .iter()
                .position(|argument| shell_option_executes_string(argument))?;
            unsupported_execution_at_depth(arguments.get(position + 1)?, depth + 1)
        }
        "eval" => {
            let nested = arguments.join(" ");
            unsupported_execution_at_depth(&nested, depth + 1)
        }
        "find" => {
            let position = arguments.iter().position(|argument| {
                matches!(argument.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
            })?;
            unsupported_execution_in_words(&arguments[position + 1..], depth + 1)
        }
        _ => None,
    }
}

fn shell_option_executes_string(argument: &str) -> bool {
    argument == "-c"
        || argument
            .strip_prefix('-')
            .is_some_and(|options| !options.starts_with('-') && options.contains('c'))
}

fn wrapper_command_index(wrapper: &str, arguments: &[String]) -> Option<usize> {
    if wrapper == "command"
        && arguments
            .iter()
            .take_while(|argument| argument.starts_with('-'))
            .any(|argument| {
                argument
                    .strip_prefix('-')
                    .is_some_and(|options| options.contains('v') || options.contains('V'))
            })
    {
        return None;
    }
    let mut index = 0;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if argument == "--" {
            return (index + 1 < arguments.len()).then_some(index + 1);
        }
        if wrapper == "env" && is_assignment(argument) {
            index += 1;
            continue;
        }
        if !argument.starts_with('-') || argument == "-" {
            return Some(index);
        }
        let consumes_next = match wrapper {
            "env" => matches!(
                argument,
                "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
            ),
            "exec" => argument == "-a",
            "time" => matches!(argument, "-f" | "--format" | "-o" | "--output"),
            "xargs" => matches!(
                argument,
                "-a" | "--arg-file"
                    | "-E"
                    | "--eof"
                    | "-I"
                    | "--replace"
                    | "-L"
                    | "--max-lines"
                    | "-n"
                    | "--max-args"
                    | "-P"
                    | "--max-procs"
                    | "-s"
                    | "--max-chars"
            ),
            _ => false,
        };
        index += 1 + usize::from(consumes_next);
    }
    None
}

fn is_detach_program(name: &str) -> bool {
    matches!(
        name,
        "nohup" | "disown" | "setsid" | "coproc" | "daemonize" | "start-stop-daemon"
    )
}

fn command_heads(input: &str) -> Vec<String> {
    let mut heads = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut expect_head = true;
    let characters: Vec<_> = input.chars().collect();

    fn finish_word(word: &mut String, expect_head: &mut bool, heads: &mut Vec<String>) {
        if word.is_empty() {
            return;
        }
        let finished = std::mem::take(word);
        if *expect_head && !is_assignment(&finished) && finished != "!" {
            heads.push(semantic_name(&finished).to_owned());
            *expect_head = false;
        }
    }

    for (index, character) in characters.iter().copied().enumerate() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        match (quote, character) {
            (Some(current), value) if current == value => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, value) if value.is_whitespace() => {
                finish_word(&mut word, &mut expect_head, &mut heads);
                if value == '\n' || value == '\r' {
                    expect_head = true;
                }
            }
            (None, '&') if characters.get(index + 1) == Some(&'>') => {
                finish_word(&mut word, &mut expect_head, &mut heads);
            }
            (None, ';' | '|' | '&' | '(' | ')') => {
                finish_word(&mut word, &mut expect_head, &mut heads);
                expect_head = true;
            }
            _ => word.push(character),
        }
    }
    finish_word(&mut word, &mut expect_head, &mut heads);
    heads
}

fn first_literal_word(input: &str) -> Option<&str> {
    let trimmed = input.trim_start();
    let end = trimmed
        .find(|character: char| character.is_whitespace() || ";&|<>()".contains(character))
        .unwrap_or(trimmed.len());
    (end > 0).then_some(&trimmed[..end])
}

fn contains_background_operator(input: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let characters: Vec<_> = input.chars().collect();
    for (index, character) in characters.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        match (quote, character) {
            (Some(current), value) if current == value => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '&')
                if characters.get(index.wrapping_sub(1)) != Some(&'&')
                    && characters.get(index.wrapping_sub(1)) != Some(&'>')
                    && characters.get(index.wrapping_sub(1)) != Some(&'<')
                    && characters.get(index.wrapping_sub(1)) != Some(&'|')
                    && characters.get(index + 1) != Some(&'&')
                    && characters.get(index + 1) != Some(&'>') =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn session() -> SessionState {
        let mut session = SessionState::test();
        session.env.insert("PATH".into(), "/usr/bin:/bin".into());
        session
    }

    #[test]
    fn static_external_is_bound_to_an_absolute_identity() {
        let plan = AgentCommandPlan::prepare(&session(), "ls -la");
        let AgentExecutionTarget::External { path, .. } = &plan.executable else {
            panic!("expected bound external: {plan:?}");
        };
        assert!(path.is_absolute());
        assert!(!plan.dynamic_resolution);
        assert_eq!(plan.invocations[0].kind, CommandTargetKind::External);
        assert!(plan.external_identity_is_current(&session().env));
    }

    #[test]
    fn ssh_plan_binds_only_the_local_launcher() {
        let root = std::env::temp_dir().join(format!("zhsh-agent-plan-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let fake = root.join("ssh");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut state = session();
        state.env.insert("PATH".into(), root.display().to_string());

        let plan = AgentCommandPlan::prepare(&state, "ssh server java -version");
        let AgentExecutionTarget::External {
            path, arguments, ..
        } = &plan.executable
        else {
            panic!("expected bound local ssh executable: {plan:?}");
        };

        assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("ssh"));
        assert_eq!(arguments, &["server", "java", "-version"]);
        assert_eq!(plan.invocations.len(), 1);
        assert_eq!(plan.invocations[0].semantic_name(), "ssh");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn literal_alias_is_frozen_while_function_remains_dynamic() {
        let mut state = session();
        state.aliases.insert("ls".into(), "rm -- sentinel".into());
        let alias = AgentCommandPlan::prepare(&state, "ls");
        assert!(!alias.dynamic_resolution);
        let AgentExecutionTarget::External {
            path, arguments, ..
        } = alias.executable
        else {
            panic!("literal alias was not bound");
        };
        assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("rm"));
        assert_eq!(arguments, ["--", "sentinel"]);

        state
            .functions
            .insert("inspect".into(), "inspect () { rm -- sentinel; }".into());
        let function = AgentCommandPlan::prepare(&state, "inspect");
        assert!(function.dynamic_resolution);
        assert_eq!(function.invocations[0].kind, CommandTargetKind::Function);
    }

    #[test]
    fn literal_alias_inside_compound_is_frozen() {
        let mut state = session();
        state.aliases.insert("ls".into(), "ls --color=auto".into());

        let plan = AgentCommandPlan::prepare(&state, "uptime && ls -1");

        assert!(!plan.dynamic_resolution, "{plan:?}");
        assert!(matches!(
            plan.executable,
            AgentExecutionTarget::BoundCompound { .. }
        ));
        assert!(plan
            .invocations
            .iter()
            .all(|invocation| invocation.kind == CommandTargetKind::External));
    }

    #[test]
    fn zhsh_builtin_priority_matches_real_dispatcher() {
        let mut state = session();
        state.aliases.insert("pwd".into(), "rm -- sentinel".into());

        let plan = AgentCommandPlan::prepare(&state, "pwd");

        assert!(matches!(
            plan.executable,
            AgentExecutionTarget::ZhshBuiltin { ref name, .. } if name == "pwd"
        ));
        assert_eq!(plan.invocations[0].kind, CommandTargetKind::ZhshBuiltin);
        assert!(!plan.dynamic_resolution);
    }

    #[test]
    fn writable_path_shadow_is_resolved_but_not_trusted() {
        let root =
            std::env::temp_dir().join(format!("zhsh-agent-plan-shadow-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let fake = root.join("ls");
        std::fs::write(&fake, "#!/bin/sh\nprintf shadow\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut state = session();
        state
            .env
            .insert("PATH".into(), format!("{}:/usr/bin", root.display()));

        let plan = AgentCommandPlan::prepare(&state, "ls");
        assert_eq!(plan.invocations[0].target_path(), Some(fake.as_path()));
        assert_eq!(plan.invocations[0].binding, ExecutableBinding::Untrusted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dynamic_and_background_forms_are_marked() {
        let compound = AgentCommandPlan::prepare(&session(), "ls | sort");
        assert!(!compound.dynamic_resolution);
        assert!(matches!(
            compound.executable,
            AgentExecutionTarget::BoundCompound { .. }
        ));
        assert_eq!(
            AgentCommandPlan::prepare(&session(), "sleep 1 &").unsupported_execution,
            Some(UnsupportedExecution::BackgroundOperator)
        );
    }

    #[test]
    fn background_lexer_distinguishes_control_and_redirection_forms() {
        for supported in [
            "true && true",
            "printf '&'",
            "printf \"&\"",
            "printf x &>output",
            "printf x &>>output",
            "java -version 2>&1 | head -3",
            "printf error 1>&2",
            "printf x |& head -1",
        ] {
            assert_eq!(
                AgentCommandPlan::prepare(&session(), supported).unsupported_execution,
                None,
                "{supported}"
            );
        }
        for detached in [
            "disown",
            "coproc task { true; }",
            "command nohup sleep 1",
            "bash -c 'setsid sleep 1'",
            "env command setsid sleep 1",
            "xargs -n 1 setsid",
            "find . -exec env setsid sleep 1 \\;",
            "eval 'env command setsid sleep 1'",
            "bash -lc 'sleep 1 &'",
        ] {
            assert!(
                matches!(
                    AgentCommandPlan::prepare(&session(), detached).unsupported_execution,
                    Some(
                        UnsupportedExecution::DetachProgram { .. }
                            | UnsupportedExecution::BackgroundOperator
                    )
                ),
                "{detached}"
            );
        }
        assert_eq!(
            AgentCommandPlan::prepare(&session(), "bash -lc 'sleep 1 &'").unsupported_execution,
            Some(UnsupportedExecution::BackgroundOperator)
        );
        assert_eq!(
            AgentCommandPlan::prepare(&session(), "command -v setsid").unsupported_execution,
            None,
            "查询命令类型不能被误认为启动 setsid"
        );
    }
}
