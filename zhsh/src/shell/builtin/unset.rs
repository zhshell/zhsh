//! `unset`：删除当前会话变量。
//!
//! # 用法
//! `unset [-v|--] [名称 ...]`；无名称时成功且不修改。
//!
//! # 参数与选项
//! -v 显式选择变量，-- 结束选项；名称遵循 ASCII Shell 变量标识符规则，不支持 -f 删除函数。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! export EXAMPLE=value
//! unset EXAMPLE
//! ```
//! 删除当前会话中的 EXAMPLE。
//!
//! # 输出与退出状态
//! 成功 0 且不输出；不支持的选项或无效名称为 1，诊断写 stderr；不存在的合法名称仍成功。
//!
//! # 状态影响
//! 删除环境、同名普通变量及提示符变量，并按原规则恢复提示符默认值；多个名称逐项处理。
//! 默认模式 Agent 不直接执行该状态操作；Native Agent 保留现有 Safety 与确认流程。 委托模式管道中由 Bash 子环境处理。

use super::super::SessionState;
use super::{support::variable::valid_name, BuiltinResult};

/// 删除当前会话中的导出环境变量和同名可重放普通变量。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    let operands = match args.first().map(String::as_str) {
        Some("-v" | "--") => &args[1..],
        Some(option) if option.starts_with('-') => {
            return BuiltinResult::error("unset: 用法: unset [-v] 名称 [名称 ...]\n")
        }
        _ => args,
    };
    if operands.is_empty() {
        return BuiltinResult::ok();
    }

    let mut errors = String::new();
    for name in operands {
        if valid_name(name) {
            shell.remove_env(name);
            shell.variables.remove(name);
            shell.remove_prompt_variable(name);
        } else {
            errors.push_str(&format!("unset: `{name}`: 不是有效的标识符\n"));
        }
    }
    shell.normalize_prompt_variables();
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
    fn removes_multiple_environment_variables() {
        let mut shell = SessionState::test();
        shell.set_env("ZHSH_UNSET_A", "a").unwrap();
        shell.set_env("ZHSH_UNSET_B", "b").unwrap();
        assert_eq!(
            execute(&mut shell, &["ZHSH_UNSET_A".into(), "ZHSH_UNSET_B".into()]).code,
            0
        );
        assert!(!shell.env.contains_key("ZHSH_UNSET_A"));
        assert!(!shell.env.contains_key("ZHSH_UNSET_B"));
    }
}
