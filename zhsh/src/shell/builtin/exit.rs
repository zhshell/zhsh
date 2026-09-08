//! `exit`：使用指定状态码退出 zhsh，省略时沿用上一条命令的状态。
//!
//! 用法：`exit [状态码]`

use super::super::SessionState;
use super::BuiltinResult;

/// 验证退出状态并设置会话终止标记。
///
/// 无参数时沿用 `shell.last_exit`；数字按 Bash 习惯折算为无符号八位状态。参数错误不会
/// 设置 `should_exit`。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    if args.len() > 1 {
        return BuiltinResult::error("exit: 参数过多\n");
    }
    let status = match args.first() {
        None => shell.last_exit,
        Some(argument) => {
            let Ok(value) = argument.parse::<i128>() else {
                return BuiltinResult {
                    stdout: String::new(),
                    stderr: format!("exit: {argument}: 需要数字参数\n"),
                    code: 2,
                };
            };
            value.rem_euclid(256) as i32
        }
    };
    shell.should_exit = true;
    BuiltinResult {
        stdout: String::new(),
        stderr: String::new(),
        code: status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_last_status_and_normalizes_numeric_status() {
        let mut shell = SessionState::test();
        shell.last_exit = 7;
        assert_eq!(execute(&mut shell, &[]).code, 7);
        shell.should_exit = false;
        assert_eq!(execute(&mut shell, &["-1".into()]).code, 255);
    }

    #[test]
    fn invalid_arguments_do_not_exit() {
        let mut shell = SessionState::test();
        assert_eq!(execute(&mut shell, &["nope".into()]).code, 2);
        assert!(!shell.should_exit);
        assert_eq!(execute(&mut shell, &["1".into(), "2".into()]).code, 1);
        assert!(!shell.should_exit);
    }
}
