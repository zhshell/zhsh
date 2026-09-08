//! `unalias`：删除一个、多个或全部当前会话别名。
//!
//! 用法：`unalias [-a] 名称 [名称 ...]`

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
