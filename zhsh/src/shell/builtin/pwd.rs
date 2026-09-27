//! `pwd`：显示当前会话目录。
//!
//! # 用法
//! `pwd [-L|-P]`；无参数显示逻辑路径，也接受单独的 `--`。
//!
//! # 参数与选项
//! `-L` 为逻辑路径，`-P` 为解析符号链接后的物理路径；只接受零个或一个参数。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! pwd -P
//! ```
//! 输出当前会话目录的物理路径。
//!
//! # 输出与退出状态
//! 路径与换行写 stdout。成功 0，参数或物理路径解析错误为 1 并写 stderr。
//!
//! # 状态影响
//! 查询不修改会话；默认模式 Agent 可调用，Native Agent 仍经过既有 Safety/授权。 委托模式可作为 builtin 管道源。

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
