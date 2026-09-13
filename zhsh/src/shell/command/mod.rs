//! 内建命令目录、参数解析与路由。
//!
//! Shell 门面只依赖本路由，路由单向调用各内建命令实现。普通内建命令只接收
//! `SessionState`；`zh llm` 额外接收应用层 UI 端口，但不依赖 REPL 的具体实现。

use super::{
    builtin::{self, BuiltinResult},
    SafetyManagementPort, SafetyManagementUi, SessionState,
};
use crate::application::{CodecManagementUi, LlmConfigUi};
use crate::llm::CodecRuntime;
use std::sync::Arc;

pub(super) mod args;
pub(super) mod pipeline_source;
pub(crate) mod resolver;

type Handler = fn(&mut SessionState, &[String]) -> BuiltinResult;
type AgentPolicy = fn(&[String]) -> bool;
type BuiltinPipelineSourcePolicy = fn(&[String]) -> BuiltinPipelineSourceDisposition;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BuiltinPipelineSourceDisposition {
    InternalOutput,
    BashSubshell,
    Reject(&'static str),
}

#[derive(Clone, Copy)]
enum HandlerKind {
    Session(Handler),
    Foreground,
    Help,
    History,
    Type,
    Zh,
}

struct CommandSpec {
    name: &'static str,
    handler: HandlerKind,
    agent_policy: AgentPolicy,
    pipeline_source_policy: BuiltinPipelineSourcePolicy,
    takes_arguments: bool,
    usage: &'static str,
    summary: &'static str,
    details: &'static str,
}

#[derive(Default)]
pub(super) struct DispatchPorts<'ui, 'safety, 'foreground, 'codec> {
    pub(super) llm_config_ui: Option<&'ui dyn LlmConfigUi>,
    pub(super) codec_management_ui: Option<&'ui dyn CodecManagementUi>,
    pub(super) safety_management_ui: Option<&'ui dyn SafetyManagementUi>,
    pub(super) safety_management: Option<&'safety dyn SafetyManagementPort>,
    pub(super) foreground_control:
        Option<&'foreground mut dyn builtin::fg::ForegroundCommandControl>,
    pub(super) codec_runtime: Option<&'codec Arc<CodecRuntime>>,
}

fn agent_never(_: &[String]) -> bool {
    false
}

fn agent_always(_: &[String]) -> bool {
    true
}

fn agent_alias_read_only(args: &[String]) -> bool {
    args.iter().all(|argument| !argument.contains('='))
}

fn agent_export_read_only(args: &[String]) -> bool {
    args.is_empty() || args == ["-p"]
}

fn agent_dirs_read_only(args: &[String]) -> bool {
    args.iter().all(|argument| {
        argument == "--"
            || argument.strip_prefix('-').is_some_and(|options| {
                options
                    .chars()
                    .all(|option| matches!(option, 'l' | 'p' | 'v'))
            })
    })
}

fn agent_umask_read_only(args: &[String]) -> bool {
    args.is_empty() || args == ["-S"]
}

fn agent_zh_read_only(args: &[String]) -> bool {
    match args {
        [] => true,
        [command] => matches!(
            command.as_str(),
            "status" | "ls" | "trust" | "help" | "-h" | "--help"
        ),
        _ => false,
    }
}

fn pipeline_internal(_: &[String]) -> BuiltinPipelineSourceDisposition {
    BuiltinPipelineSourceDisposition::InternalOutput
}

fn pipeline_bash(_: &[String]) -> BuiltinPipelineSourceDisposition {
    BuiltinPipelineSourceDisposition::BashSubshell
}

fn pipeline_alias(args: &[String]) -> BuiltinPipelineSourceDisposition {
    if args.iter().all(|argument| !argument.contains('=')) {
        BuiltinPipelineSourceDisposition::InternalOutput
    } else {
        BuiltinPipelineSourceDisposition::BashSubshell
    }
}

fn pipeline_dirs(args: &[String]) -> BuiltinPipelineSourceDisposition {
    let clears_stack = args.iter().any(|argument| {
        argument
            .strip_prefix('-')
            .is_some_and(|options| !options.is_empty() && options.contains('c'))
    });
    if clears_stack {
        BuiltinPipelineSourceDisposition::Reject("dirs -c 会修改 zhsh 目录栈，不能用于管道")
    } else {
        BuiltinPipelineSourceDisposition::InternalOutput
    }
}

fn pipeline_exit(args: &[String]) -> BuiltinPipelineSourceDisposition {
    match args {
        [] => BuiltinPipelineSourceDisposition::Reject(
            "无参数 exit 依赖 zhsh 当前退出状态，不能用于管道",
        ),
        [_] => BuiltinPipelineSourceDisposition::BashSubshell,
        _ => BuiltinPipelineSourceDisposition::InternalOutput,
    }
}

fn pipeline_export(args: &[String]) -> BuiltinPipelineSourceDisposition {
    if args.is_empty() || args == ["-p"] {
        BuiltinPipelineSourceDisposition::InternalOutput
    } else {
        BuiltinPipelineSourceDisposition::BashSubshell
    }
}

fn pipeline_reject_directory_stack(_: &[String]) -> BuiltinPipelineSourceDisposition {
    BuiltinPipelineSourceDisposition::Reject("该命令会修改 zhsh 目录栈，不能用于管道")
}

fn pipeline_reject_foreground(_: &[String]) -> BuiltinPipelineSourceDisposition {
    BuiltinPipelineSourceDisposition::Reject("fg 需要直接控制 zhsh 前台终端，不能用于管道")
}

fn pipeline_umask(args: &[String]) -> BuiltinPipelineSourceDisposition {
    if args.is_empty() || args == ["-S"] {
        BuiltinPipelineSourceDisposition::InternalOutput
    } else {
        BuiltinPipelineSourceDisposition::BashSubshell
    }
}

fn pipeline_zh(args: &[String]) -> BuiltinPipelineSourceDisposition {
    use BuiltinPipelineSourceDisposition::{InternalOutput, Reject};

    match args {
        [] => InternalOutput,
        [command]
            if matches!(
                command.as_str(),
                "status" | "ls" | "trust" | "help" | "-h" | "--help"
            ) =>
        {
            InternalOutput
        }
        [command, option] if command == "llm" && matches!(option.as_str(), "-h" | "--help") => {
            InternalOutput
        }
        [command] if matches!(command.as_str(), "safety" | "codec") => InternalOutput,
        [command, option]
            if matches!(command.as_str(), "safety" | "codec")
                && matches!(option.as_str(), "help" | "-h" | "--help" | "-t") =>
        {
            InternalOutput
        }
        [command, ..] if matches!(command.as_str(), "use" | "llm" | "tier" | "trust") => {
            Reject("该 zh 子命令会修改状态或启动交互，不能用于管道")
        }
        [command, operation, ..]
            if command == "safety" && matches!(operation.as_str(), "reload" | "install") =>
        {
            Reject("该 zh safety 操作会修改规则状态，不能用于管道")
        }
        [command, operation, ..]
            if command == "codec"
                && matches!(operation.as_str(), "install" | "export" | "reload") =>
        {
            Reject("该 zh codec 操作会修改状态或文件，不能用于管道")
        }
        _ => InternalOutput,
    }
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: ".",
        handler: HandlerKind::Session(builtin::source::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_bash,
        takes_arguments: true,
        usage: ". 文件 [参数 ...]",
        summary: "在当前会话中读取并执行文件",
        details: "文件由 Bash 执行；成功取得状态快照后，目录、变量、函数、别名和 PS0–PS4 会同步回 zhsh。",
    },
    CommandSpec {
        name: "alias",
        handler: HandlerKind::Session(builtin::alias::execute),
        agent_policy: agent_alias_read_only,
        pipeline_source_policy: pipeline_alias,
        takes_arguments: true,
        usage: "alias [名称[=命令] ...]",
        summary: "定义或显示别名",
        details: "省略参数或使用 -p 时列出全部别名；名称=命令用于定义别名；仅给名称时查询该别名。",
    },
    CommandSpec {
        name: "cd",
        handler: HandlerKind::Session(builtin::cd::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_bash,
        takes_arguments: true,
        usage: "cd [-L|-P] [目录]",
        summary: "切换当前目录",
        details: "选项：\n  -L  保留逻辑路径（默认）\n  -P  解析符号链接后的物理路径\n\n省略目录时进入 HOME；目录为 - 时进入 OLDPWD。",
    },
    CommandSpec {
        name: "dirs",
        handler: HandlerKind::Session(builtin::dirs::execute),
        agent_policy: agent_dirs_read_only,
        pipeline_source_policy: pipeline_dirs,
        takes_arguments: true,
        usage: "dirs [-clpv]",
        summary: "显示或清空目录栈",
        details: "选项：\n  -c  清空目录栈\n  -l  不把 HOME 缩写为 ~\n  -p  每行显示一个目录\n  -v  每行显示序号和目录",
    },
    CommandSpec {
        name: "exit",
        handler: HandlerKind::Session(builtin::exit::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_exit,
        takes_arguments: true,
        usage: "exit [状态码]",
        summary: "退出 zhsh",
        details: "省略状态码时沿用上一条命令的退出状态；数字按 0–255 折算。",
    },
    CommandSpec {
        name: "export",
        handler: HandlerKind::Session(builtin::export::execute),
        agent_policy: agent_export_read_only,
        pipeline_source_policy: pipeline_export,
        takes_arguments: true,
        usage: "export [-n] [名称[=值] ...]",
        summary: "设置或显示导出的环境变量",
        details: "省略参数或使用 -p 时列出导出变量；-n 取消变量的导出属性，但保留其 Shell 值。",
    },
    CommandSpec {
        name: "fg",
        handler: HandlerKind::Foreground,
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_reject_foreground,
        takes_arguments: false,
        usage: "fg",
        summary: "恢复最近暂停的整行前台命令（有限支持）",
        details: "只恢复最近一个被 Ctrl-Z 暂停的整行前台命令；暂不支持作业编号、jobs、bg、wait 或 disown。",
    },
    CommandSpec {
        name: "help",
        handler: HandlerKind::Help,
        agent_policy: agent_always,
        pipeline_source_policy: pipeline_internal,
        takes_arguments: true,
        usage: "help [内建命令]",
        summary: "显示 zhsh 内建命令帮助",
        details: "省略名称时列出全部内建命令；指定名称时显示其用途、用法和关键边界。",
    },
    CommandSpec {
        name: "history",
        handler: HandlerKind::History,
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_internal,
        takes_arguments: true,
        usage: "history [数量]",
        summary: "显示用户输入历史",
        details: "数量表示仅显示最近若干条；内容只包含主提示符下的用户输入，不包含 Agent 响应和命令输出。",
    },
    CommandSpec {
        name: "popd",
        handler: HandlerKind::Session(builtin::popd::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_reject_directory_stack,
        takes_arguments: true,
        usage: "popd",
        summary: "从目录栈恢复目录",
        details: "弹出目录栈顶并切换到该目录；切换失败时不会丢失栈顶记录。",
    },
    CommandSpec {
        name: "pushd",
        handler: HandlerKind::Session(builtin::pushd::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_reject_directory_stack,
        takes_arguments: true,
        usage: "pushd [目录]",
        summary: "保存当前目录并切换",
        details: "指定目录时保存当前目录后切换；省略目录时与目录栈顶交换。",
    },
    CommandSpec {
        name: "pwd",
        handler: HandlerKind::Session(builtin::pwd::execute),
        agent_policy: agent_always,
        pipeline_source_policy: pipeline_internal,
        takes_arguments: true,
        usage: "pwd [-L|-P]",
        summary: "显示当前目录",
        details: "选项：\n  -L  显示逻辑路径（默认）\n  -P  显示解析符号链接后的物理路径",
    },
    CommandSpec {
        name: "source",
        handler: HandlerKind::Session(builtin::source::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_bash,
        takes_arguments: true,
        usage: "source 文件 [参数 ...]",
        summary: "在当前会话中读取并执行文件",
        details: "文件由 Bash 执行；成功取得状态快照后，目录、变量、函数、别名和 PS0–PS4 会同步回 zhsh。",
    },
    CommandSpec {
        name: "type",
        handler: HandlerKind::Type,
        agent_policy: agent_always,
        pipeline_source_policy: pipeline_internal,
        takes_arguments: true,
        usage: "type [-atpP] 名称 [名称 ...]",
        summary: "说明命令名称的解析方式",
        details: "选项：\n  -a  显示全部匹配\n  -t  只显示类型\n  -p  只搜索 PATH\n  -P  只搜索 PATH",
    },
    CommandSpec {
        name: "unalias",
        handler: HandlerKind::Session(builtin::unalias::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_bash,
        takes_arguments: true,
        usage: "unalias [-a] 名称 [名称 ...]",
        summary: "删除别名",
        details: "-a 删除当前会话中的全部别名；否则删除指定的一个或多个名称。",
    },
    CommandSpec {
        name: "umask",
        handler: HandlerKind::Session(builtin::umask::execute),
        agent_policy: agent_umask_read_only,
        pipeline_source_policy: pipeline_umask,
        takes_arguments: true,
        usage: "umask [-S] [八进制掩码]",
        summary: "显示或设置文件创建掩码",
        details: "省略参数时显示八进制掩码；-S 显示符号形式；提供八进制值时修改后续子进程继承的掩码。",
    },
    CommandSpec {
        name: "unset",
        handler: HandlerKind::Session(builtin::unset::execute),
        agent_policy: agent_never,
        pipeline_source_policy: pipeline_bash,
        takes_arguments: true,
        usage: "unset [-v] 名称 [名称 ...]",
        summary: "删除当前会话环境变量",
        details: "删除当前会话中的环境变量和同名 Shell 变量；-v 是兼容的显式变量选项。",
    },
    CommandSpec {
        name: "zh",
        handler: HandlerKind::Zh,
        agent_policy: agent_zh_read_only,
        pipeline_source_policy: pipeline_zh,
        takes_arguments: true,
        usage: "zh [status|ls|use|llm|tier|trust|safety|codec|help]",
        summary: "管理 zhsh 的 LLM、Codec、授信与 Safety 状态",
        details: "运行 `zh help` 查看管理子命令，或运行 `man zhsh` 查看完整手册。",
    },
];

/// 命令来源，用于在同一注册表上应用不同权限策略。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// 用户在主提示符或管道模式中直接提交。
    User,
    /// LLM Agent 在任务处理期间生成。
    Agent,
}

/// 按注册顺序迭代全部内建命令名。
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|command| command.name)
}

/// 判断名称是否属于 zhsh 内建命令。
///
/// # Arguments
///
/// - `name`：不含参数的命令名。
pub(crate) fn is_builtin(name: &str) -> bool {
    COMMANDS.iter().any(|command| command.name == name)
}

/// 判断命令补全后是否应追加空格以继续输入参数。
///
/// # Arguments
///
/// - `name`：不含参数的内建命令名；未知名称返回 `false`。
pub(crate) fn takes_arguments(name: &str) -> bool {
    COMMANDS
        .iter()
        .find(|command| command.name == name)
        .is_some_and(|command| command.takes_arguments)
}

/// 处理只包含 PS0–PS4 字面赋值的整行输入，使提示符变量像交互式 Bash 一样留在会话中。
pub(super) fn assign_prompt_variables(
    session: &mut SessionState,
    input: &str,
) -> Option<BuiltinResult> {
    let words = args::parse(input).ok()?;
    if words.is_empty() {
        return None;
    }
    let assignments: Option<Vec<_>> = words
        .iter()
        .map(|word| {
            let (name, value) = word.split_once('=')?;
            SessionState::is_prompt_variable(name).then_some((name, value))
        })
        .collect();
    let assignments = assignments?;
    for (name, value) in assignments {
        if let Err(error) = session.set_prompt_variable(name, value) {
            return Some(BuiltinResult::error(format!("zhsh: {error}\n")));
        }
    }
    Some(BuiltinResult::ok())
}

/// 迭代帮助列表使用的“命令名—摘要”对。
pub(crate) fn descriptions() -> impl Iterator<Item = (&'static str, &'static str)> {
    COMMANDS
        .iter()
        .map(|command| (command.name, command.summary))
}

/// 查询一个内建命令的用法和摘要。
///
/// # Arguments
///
/// - `name`：不含参数的命令名。
pub(crate) fn usage(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map(|command| (command.usage, command.summary, command.details))
}

/// 尝试通过唯一命令注册表路由输入。
///
/// # Arguments
///
/// - `session`：内建命令可读取或提交的当前会话状态。
/// - `input`：未经 Shell 展开的完整输入。
/// - `origin`：决定是否应用 Agent 只读权限策略的命令来源。
/// - `history`：仅供 `history` 内建命令读取的 rustyline 快照；不会写回。
///
/// # Returns
///
/// 返回 [`Some`] 表示输入已经由内建路由处理，包括语法错误和权限拒绝；返回 [`None`]
/// 表示首词不是内建命令，或输入包含应整体交给 Bash 的语法。Agent 对不允许的内建操作
/// 返回状态码 `126`，且不会调用处理函数。
pub(crate) fn dispatch(
    session: &mut SessionState,
    input: &str,
    origin: Origin,
    history: &[String],
    ports: DispatchPorts<'_, '_, '_, '_>,
) -> Option<BuiltinResult> {
    let words = match args::parse(input) {
        Ok(words) => words,
        Err(args::ParseError::NeedsBash) => return None,
        Err(args::ParseError::Syntax(message)) => {
            let name = input.split_whitespace().next().unwrap_or("");
            return is_builtin(name)
                .then(|| BuiltinResult::error(format!("zhsh: 语法错误: {message}\n")));
        }
    };
    let (name, arguments) = words.split_first()?;
    dispatch_words(session, name, arguments, origin, history, ports)
}

/// 返回某个已解析 builtin 是否允许由 Agent 直接调用。
pub(super) fn agent_allows(name: &str, arguments: &[String]) -> bool {
    COMMANDS
        .iter()
        .find(|command| command.name == name)
        .is_some_and(|command| (command.agent_policy)(arguments))
}

/// 返回已完成字面参数解析的 builtin 管道源策略。
pub(super) fn pipeline_source_disposition(
    name: &str,
    arguments: &[String],
) -> Option<BuiltinPipelineSourceDisposition> {
    COMMANDS
        .iter()
        .find(|command| command.name == name)
        .map(|command| (command.pipeline_source_policy)(arguments))
}

/// 执行准备阶段已经解析并冻结的 Agent 内建命令。
///
/// 该入口不再接收命令文本，避免安全判断后重新按另一份字符串解析参数。
pub(super) fn dispatch_agent_words(
    session: &mut SessionState,
    name: &str,
    arguments: &[String],
    codec_runtime: &Arc<CodecRuntime>,
) -> Option<BuiltinResult> {
    dispatch_words(
        session,
        name,
        arguments,
        Origin::Agent,
        &[],
        DispatchPorts {
            llm_config_ui: None,
            codec_management_ui: None,
            safety_management_ui: None,
            safety_management: None,
            foreground_control: None,
            codec_runtime: Some(codec_runtime),
        },
    )
}

/// 执行已经由管道源分析器解析并冻结的用户 builtin。
pub(super) fn dispatch_pipeline_source_words(
    session: &mut SessionState,
    name: &str,
    arguments: &[String],
    history: &[String],
    codec_runtime: &Arc<CodecRuntime>,
    llm_config_ui: Option<&dyn LlmConfigUi>,
    safety_management: Option<&dyn SafetyManagementPort>,
) -> Option<BuiltinResult> {
    dispatch_words(
        session,
        name,
        arguments,
        Origin::User,
        history,
        DispatchPorts {
            llm_config_ui,
            codec_management_ui: None,
            safety_management_ui: None,
            safety_management,
            foreground_control: None,
            codec_runtime: Some(codec_runtime),
        },
    )
}

/// Native 已授权/用户内建入口，复用注册表和处理器；不使用过渡模式的 Agent 白名单。
pub(super) fn dispatch_native_words(
    session: &mut SessionState,
    name: &str,
    arguments: &[String],
    history: &[String],
    ports: DispatchPorts<'_, '_, '_, '_>,
) -> Option<BuiltinResult> {
    match name {
        // source/. is implemented by the Native Shell caller; never invoke the Bash loader here.
        "source" | "." => Some(BuiltinResult::error(
            "source: 必须使用 Native 文件执行入口\n",
        )),
        "export" => Some(builtin::export::execute_native(session, arguments)),
        "type" => Some(builtin::r#type::execute_native(
            session,
            arguments,
            &names().collect::<Vec<_>>(),
        )),
        "help" => Some(builtin::help::execute(
            arguments,
            &descriptions().collect::<Vec<_>>(),
            native_usage,
        )),
        _ => dispatch_words(session, name, arguments, Origin::User, history, ports),
    }
}

fn native_usage(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    if matches!(name, "source" | ".") {
        Some(("source 文件 / . 文件", "在当前会话逐行执行 Native 命令", "支持当前 Native 字面命令与内建；不调用 Bash。不支持的语法会停止读取，此前的状态修改保留；位置参数尚未实现。"))
    } else {
        usage(name)
    }
}

fn dispatch_words(
    session: &mut SessionState,
    name: &str,
    arguments: &[String],
    origin: Origin,
    history: &[String],
    ports: DispatchPorts<'_, '_, '_, '_>,
) -> Option<BuiltinResult> {
    let command = COMMANDS.iter().find(|command| command.name == name)?;
    if origin == Origin::Agent && !(command.agent_policy)(arguments) {
        return Some(BuiltinResult {
            stdout: String::new(),
            stderr: format!("zhsh: Agent 不允许执行会话内建命令: {name}\n"),
            code: 126,
        });
    }

    let result = match command.handler {
        HandlerKind::Session(handler) => handler(session, arguments),
        HandlerKind::Foreground => builtin::fg::execute(ports.foreground_control, arguments),
        HandlerKind::Help => {
            let descriptions: Vec<_> = descriptions().collect();
            builtin::help::execute(arguments, &descriptions, usage)
        }
        HandlerKind::History if matches!(arguments, [argument] if matches!(argument.as_str(), "-h" | "--help")) =>
        {
            let (usage, summary, details) = usage("history").expect("history is registered");
            BuiltinResult::stdout(builtin::help::render_entry(
                "history", usage, summary, details,
            ))
        }
        HandlerKind::History => builtin::history::execute(history, arguments),
        HandlerKind::Type => {
            let builtin_names: Vec<_> = names().collect();
            builtin::r#type::execute(session, arguments, &builtin_names)
        }
        HandlerKind::Zh => builtin::zh::execute(
            session,
            arguments,
            ports.codec_runtime,
            ports.llm_config_ui,
            ports.codec_management_ui,
            ports.safety_management_ui,
            ports.safety_management,
        ),
    };
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_unique() {
        let mut names: Vec<_> = names().collect();
        let original_len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), original_len);
    }

    #[test]
    fn literal_ps_assignments_update_the_parent_session() {
        let mut shell = SessionState::test();
        let result = assign_prompt_variables(&mut shell, r"PS1='\u:\w\$ ' PS4='+ '").unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(shell.prompt_variable("PS1"), Some(r"\u:\w\$ "));
        assert_eq!(shell.prompt_variable("PS4"), Some("+ "));
        assert!(!shell.env.contains_key("PS1"));
        let bash_state = shell.prepare_bash_state();
        assert!(bash_state.contains("declare -- PS1='\\u:\\w\\$ '"));
        assert!(bash_state.contains("declare -- PS4='+ '"));
    }

    #[test]
    fn agent_cannot_mutate_session_or_read_history() {
        let mut shell = SessionState::test();
        for input in [
            "cd /",
            "dirs -c",
            "export A=1",
            "alias x=true",
            "history",
            "fg",
            "exit",
            "zh tier max",
            "zh trust trusted",
            "zh trust -w confirm",
            "zh safety",
            "zh safety -t",
            "zh safety reload",
            "zh safety install file.zhse.json",
            "zh codec",
            "zh codec help",
            "zh codec install file.zhcodec",
        ] {
            let result = dispatch(
                &mut shell,
                input,
                Origin::Agent,
                &[],
                DispatchPorts::default(),
            )
            .unwrap();
            assert_eq!(result.code, 126, "{input}");
        }
        assert!(!shell.should_exit);
        assert!(!shell.env.contains_key("A"));
        assert!(!shell.aliases.contains_key("x"));
    }

    #[test]
    fn agent_can_use_read_only_builtins() {
        let mut shell = SessionState::test();
        for input in [
            "pwd",
            "dirs",
            "type pwd",
            "help pwd",
            "zh status",
            "zh trust",
        ] {
            assert_ne!(
                dispatch(
                    &mut shell,
                    input,
                    Origin::Agent,
                    &[],
                    DispatchPorts::default(),
                )
                .unwrap()
                .code,
                126,
                "{input}"
            );
        }
    }

    #[test]
    fn every_registered_builtin_has_detailed_help_from_the_registry() {
        let mut shell = SessionState::test();
        for name in names() {
            let result = dispatch(
                &mut shell,
                &format!("help {name}"),
                Origin::User,
                &[],
                DispatchPorts::default(),
            )
            .unwrap();
            let (usage, summary, _) = usage(name).unwrap();
            assert_eq!(result.code, 0, "{name}");
            assert!(result.stdout.contains(usage), "{name}");
            assert!(result.stdout.contains(summary), "{name}");
        }

        let help_history = dispatch(
            &mut shell,
            "help history",
            Origin::User,
            &[],
            DispatchPorts::default(),
        )
        .unwrap();
        let history_help = dispatch(
            &mut shell,
            "history -h",
            Origin::User,
            &[],
            DispatchPorts::default(),
        )
        .unwrap();
        assert_eq!(history_help.stdout, help_history.stdout);
    }
}
