//! `popd`：弹出目录栈顶并切换到该目录。
//!
//! 用法：`popd`

use super::super::SessionState;
use super::{cd, dirs, BuiltinResult};

/// 弹出栈顶并切换目录；切换失败时把目标恢复到目录栈。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    if !args.is_empty() && args != ["--"] {
        return BuiltinResult::error("popd: 用法: popd\n");
    }
    let Some(target) = shell.directory_stack.pop() else {
        return BuiltinResult::error("popd: 目录栈为空\n");
    };
    let result = cd::change_directory(shell, &target.to_string_lossy(), false);
    if result.code != 0 {
        shell.directory_stack.push(target);
        return result;
    }
    BuiltinResult::stdout(dirs::render(shell, false, false, false))
}
