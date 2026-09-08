//! `pwd`：显示当前 zhsh 会话的逻辑或物理工作目录。
//!
//! 用法：`pwd [-L|-P]`

use super::super::SessionState;
use super::BuiltinResult;

/// 显示会话逻辑 cwd，或在 `-P` 时显示解析符号链接后的物理路径。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    let physical = match args {
        [] => false,
        [option] if option == "-L" || option == "--" => false,
        [option] if option == "-P" => true,
        _ => return BuiltinResult::error("pwd: 用法: pwd [-LP]\n"),
    };
    let path = if physical {
        match std::fs::canonicalize(&shell.cwd) {
            Ok(path) => path,
            Err(error) => return BuiltinResult::error(format!("pwd: {error}\n")),
        }
    } else {
        shell.cwd.clone()
    };
    BuiltinResult::stdout(format!("{}\n", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_or_extra_options() {
        let mut shell = SessionState::test();
        assert_eq!(execute(&mut shell, &["extra".into()]).code, 1);
        assert_eq!(execute(&mut shell, &["-P".into(), "extra".into()]).code, 1);
    }
}
