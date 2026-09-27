//! `unalias`：移除当前会话别名。
//!
//! # 用法
//! `unalias [--] 名称 [名称 ...]` 或 `unalias -a`。
//!
//! # 参数与选项
//! -a 必须单独使用，移除全部别名；按名称操作至少一个名称，支持开头 --。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! alias demo=pwd
//! unalias demo
//! ```
//! 移除刚定义的 demo 别名。
//!
//! # 输出与退出状态
//! 成功 0 且不输出；缺少参数或名称不存在为 1，诊断写 stderr。多个名称中成功项仍被移除。
//!
//! # 状态影响
//! 修改当前别名表。默认模式 Agent 不直接执行该状态操作；Native Agent 保留现有 Safety 与确认流程。 委托模式管道中由 Bash 子环境处理。

use super::super::SessionState;
use super::BuiltinResult;

/// 删除指定别名，或使用 `-a` 清空当前会话别名。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    if args == ["-a"] {
        shell.aliases.clear();
        return BuiltinResult::ok();
    }
    let operands = if args.first().is_some_and(|argument| argument == "--") {
        &args[1..]
    } else {
        args
    };
    if operands.is_empty() {
        return BuiltinResult::error("unalias: 用法: unalias [-a] 名称 [名称 ...]\n");
    }

    let mut errors = String::new();
    for name in operands {
        if shell.aliases.remove(name).is_none() {
            errors.push_str(&format!("unalias: {name}: 未找到\n"));
        }
    }
    if errors.is_empty() {
        BuiltinResult::ok()
    } else {
        BuiltinResult::error(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_multiple_aliases_and_reports_missing_names() {
        let mut shell = SessionState::test();
        shell.aliases.insert("a".into(), "true".into());
        shell.aliases.insert("b".into(), "false".into());
        assert_eq!(execute(&mut shell, &["a".into(), "b".into()]).code, 0);
        assert_eq!(execute(&mut shell, &["missing".into()]).code, 1);
    }
}
