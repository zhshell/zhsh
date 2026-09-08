//! `unset`：删除当前会话中的环境变量或已同步 Shell 变量。
//!
//! 用法：`unset [-v] 名称 [名称 ...]`

use super::super::SessionState;
use super::{export::valid_name, BuiltinResult};

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
