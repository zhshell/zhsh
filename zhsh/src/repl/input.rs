//! Shell 输入与 Agent 输入的二元首字符分类。
//!
//! 去除边界空白后，ASCII 首字符进入 Shell，非 ASCII 首字符进入 Agent。本模块不按意图、
//! 命令名、PATH 或 Shell 语法做推断，因此 ASCII 开头的脚本和含 Unicode 参数的命令不会
//! 被 LLM 截获。

/// 一次输入应进入的顶层处理路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputKind {
    /// 用户命令，直接进入内建路由或 Bash。
    UserCommand,
    /// 非 ASCII 首字符输入，进入 LLM Agent 作意图分析。
    AgentInput,
}

/// 保留用户原文和实际执行文本的路由结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutedInput {
    /// 去除首尾空白后的用户原始输入；Agent 任务和历史语义使用此值。
    pub(crate) original: String,
    /// 命令路径实际执行的文本；二元路由不改写用户输入。
    pub(crate) executable: String,
    /// 选择 Shell 或 Agent 路径。
    pub(crate) kind: InputKind,
}

/// 仅根据首字符分类输入。
///
/// # Arguments
///
/// - `input`：用户在主提示符或管道模式提交的原始文本。
///
/// # Returns
///
/// 空白输入返回 [`None`]。去除边界空白后的首字符为 ASCII 时返回用户命令，否则返回
/// Agent 输入；两条路径都保留去除边界空白后的原文，不做任何规范化改写。
pub(crate) fn route(input: &str) -> Option<RoutedInput> {
    let original = input.trim();
    if original.is_empty() {
        return None;
    }

    Some(RoutedInput {
        original: original.to_string(),
        executable: original.to_string(),
        kind: if original
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii())
        {
            InputKind::UserCommand
        } else {
            InputKind::AgentInput
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_prefix_routes_to_shell_without_rewriting() {
        let input =
            r#"for f in 海贼王[0-9][0-9][0-9][0-9].{mkv,mp4}; do mv "$f" "${f#海贼王}"; done"#;
        for input in [
            input,
            "git中文参数",
            "./工具 测试",
            "command 工具",
            "check the current directory",
            "printf 结果 | sed 's/结/总/' > 输出.txt",
            "$HOME/工具",
        ] {
            let routed = route(input).unwrap();
            assert_eq!(routed.kind, InputKind::UserCommand, "{input}");
            assert_eq!(routed.original, input, "{input}");
            assert_eq!(routed.executable, input, "{input}");
        }
    }

    #[test]
    fn non_ascii_prefix_routes_to_agent_without_rewriting() {
        for input in [
            "检查系统状态",
            "𠮷字开头",
            "日本語の入力",
            "한국어 입력",
            "Проверить систему",
            "مرحبا بالعالم",
            "😊 检查状态",
            "ｌｓ",
            "？如何查看磁盘",
        ] {
            let routed = route(input).unwrap();
            assert_eq!(routed.kind, InputKind::AgentInput, "{input}");
            assert_eq!(routed.original, routed.executable);
        }
    }

    #[test]
    fn unicode_boundary_whitespace_is_trimmed_before_classification() {
        let shell = route(" \t\u{3000}for f in 工具; do :; done\u{3000} ").unwrap();
        assert_eq!(shell.kind, InputKind::UserCommand);
        assert_eq!(shell.original, "for f in 工具; do :; done");
        assert_eq!(shell.original, shell.executable);

        let agent = route(" \t\u{3000}检查状态\u{3000} ").unwrap();
        assert_eq!(agent.kind, InputKind::AgentInput);
        assert_eq!(agent.original, "检查状态");
        assert_eq!(agent.original, agent.executable);

        assert_eq!(route(" \t\n\u{3000}"), None);
    }
}
