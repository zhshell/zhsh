//! `help`：列出当前模式的 zhsh 内建命令或查看单项帮助。
//!
//! # 用法
//! `help [内建命令]`；无参数列出命令名与摘要。
//!
//! # 参数与选项
//! 至多一个命令名；没有通用的 -h/--help 选项。`help .` 查询 source 的同义入口。
//!
//! # 模式与上下文
//! 默认模式从 descriptions/usage 读取过滤后的注册项；Native 使用全部注册项与 native_usage，保留 fg/source 的模式专属用法。
//!
//! # 示例
//! ```sh
//! help source
//! ```
//! 显示当前模式的 source 用法；Native 说明逐行执行，默认模式说明 Bash 状态同步。
//!
//! # 输出与退出状态
//! 列表或详细用法写 stdout，成功 0。未知名称或参数过多为 1，写 stderr。
//!
//! # 状态影响
//! 查询不修改会话；默认模式 Agent 可调用，Native Agent 仍经过既有 Safety/授权。 委托模式可作为 builtin 管道源。
//! 帮助文本仍来自既有注册表；本文件的源码说明不生成或改写运行时帮助。

use super::BuiltinResult;
use unicode_width::UnicodeWidthStr;

/// 根据唯一命令注册表生成内建帮助。
///
/// # Arguments
///
/// - `args`：零个参数列出全部命令，一个参数查询具体命令。
/// - `descriptions`：注册表提供的命令名和摘要。
/// - `usage`：按名称查询用法、摘要和详细说明的注册表访问器。
pub(crate) fn execute(
    args: &[String],
    descriptions: &[(&str, &str)],
    usage: impl Fn(&str) -> Option<(&'static str, &'static str, &'static str)>,
) -> BuiltinResult {
    if args.is_empty() {
        let width = descriptions
            .iter()
            .map(|(name, _)| UnicodeWidthStr::width(*name))
            .max()
            .unwrap_or(0);
        let mut output = String::from("zhsh 内建命令：\n");
        for (name, summary) in descriptions {
            output.push_str("  ");
            output.push_str(name);
            output.extend(std::iter::repeat_n(
                ' ',
                width.saturating_sub(UnicodeWidthStr::width(*name)) + 2,
            ));
            output.push_str(summary);
            output.push('\n');
        }
        output.push_str(
            "\n运行 `help <名称>` 查看具体用法。\n\
             运行 `zh help` 查看 zhsh 管理命令，或运行 `man zhsh` 查看完整手册。\nAgent 授信：balanced 自动执行可信只读命令；confirm 逐条确认；trusted 额外自动执行核心规则识别的任务根内普通修改。\ntrusted 仍确认会话修改、破坏性操作、网络/提权和敏感披露；不支持的执行形式仍拒绝。\n运行 `zh trust -h` 查看等级摘要与代表性案例。\n",
        );
        return BuiltinResult::stdout(output);
    }
    if args.len() != 1 {
        return BuiltinResult::error("help: 用法: help [内建命令]\n");
    }
    match usage(&args[0]) {
        Some((usage, summary, details)) => {
            BuiltinResult::stdout(render_entry(&args[0], usage, summary, details))
        }
        None => BuiltinResult::error(format!("help: {}: 不是 zhsh 内建命令\n", args[0])),
    }
}

/// 使用注册表事实渲染具体 builtin，供 `help history` 与 `history -h` 共享。
pub(crate) fn render_entry(name: &str, usage: &str, summary: &str, details: &str) -> String {
    let mut output = format!("{name}：{summary}。\n\n用法：{usage}\n");
    if !details.is_empty() {
        output.push('\n');
        output.push_str(details);
        if !details.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}
