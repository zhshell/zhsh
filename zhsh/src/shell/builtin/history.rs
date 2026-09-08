//! `history`：显示用户在主 REPL 中真正输入过的内容。
//!
//! 用法：`history [数量]`

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
