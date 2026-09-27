//! `history`：显示主提示符的用户输入历史。
//!
//! # 用法
//! `history [数量]`；`history -h`、`history --help` 由路由提供帮助。
//!
//! # 参数与选项
//! 数量为非负整数，省略显示传入的全部历史，0 不显示条目。只接受一个数量或单独帮助选项。
//!
//! # 模式与上下文
//! 默认模式和 Native 用户入口读取主 REPL 的历史快照；Native Agent 使用任务收到的历史快照。
//! 历史不包含模型响应、模型命令、向导字段和命令输出。
//!
//! # 示例
//! ```sh
//! history 5
//! ```
//! 最多显示最近五条用户输入，保留原序号。
//!
//! # 输出与退出状态
//! 编号和文本写 stdout；空历史成功且无条目。成功 0，参数错误 1 并写 stderr。
//!
//! # 状态影响
//! 不修改历史。默认 Agent 不允许读取；Native 保留 Safety/确认。委托模式可作为 builtin 管道源。

use super::BuiltinResult;

/// 历史文本的无状态渲染命名空间。
pub(crate) struct History;

impl History {
    fn render(entries: &[String], limit: usize) -> String {
        let start = entries.len().saturating_sub(limit);
        let width = entries.len().max(1).to_string().len();
        let mut output = String::new();
        for (index, entry) in entries.iter().enumerate().skip(start) {
            output.push_str(&format!("{:>width$}  {entry}\n", index + 1));
        }
        output
    }
}

/// 渲染用户主 REPL 的只读历史快照。
///
/// `entries` 不包含 Agent 响应、模型命令、向导字段或输出；可选 `args` 只接受最近条数。
pub(crate) fn execute(entries: &[String], args: &[String]) -> BuiltinResult {
    let limit = if args.is_empty() {
        entries.len()
    } else if args.len() != 1 {
        return BuiltinResult::error("history: 用法: history [数量]\n");
    } else {
        let Ok(limit) = args[0].parse::<usize>() else {
            return BuiltinResult::error("history: 用法: history [数量]\n");
        };
        limit
    };
    BuiltinResult::stdout(History::render(entries, limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    #[test]
    fn lists_only_recorded_user_inputs() {
        let entries = history(&["ls -la", "检查系统状态", "zh status"]);

        let result = execute(&entries, &[]);

        assert_eq!(result.stdout, "1  ls -la\n2  检查系统状态\n3  zh status\n");
        assert_eq!(result.code, 0);
    }

    #[test]
    fn limits_output_to_the_latest_inputs_without_renumbering() {
        let entries = history(&["first", "second", "third"]);

        let result = execute(&entries, &["2".into()]);

        assert_eq!(result.stdout, "2  second\n3  third\n");
    }

    #[test]
    fn rejects_non_numeric_arguments() {
        let entries = history(&[]);

        let result = execute(&entries, &["all".into()]);

        assert_eq!(result.code, 1);
        assert_eq!(result.stderr, "history: 用法: history [数量]\n");
    }
}
