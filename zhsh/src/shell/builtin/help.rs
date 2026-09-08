//! `help`：列出 zhsh 内建命令或显示指定命令的简要用法。
//!
//! 用法：`help [内建命令]`

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
             运行 `zh help` 查看 zhsh 管理命令，或运行 `man zhsh` 查看完整手册。\n",
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
