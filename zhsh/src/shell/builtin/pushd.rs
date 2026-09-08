//! `pushd`：保存当前目录并切换目录，省略参数时交换目录栈顶。
//!
//! 用法：`pushd [目录]`

use super::super::SessionState;
use super::{cd, dirs, BuiltinResult};

/// 保存当前目录并切换到目标，或在无参数时交换当前目录和栈顶。
///
/// 目录切换失败时不会提交新的栈状态。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    let operands = if args.first().is_some_and(|argument| argument == "--") {
        &args[1..]
    } else {
        args
    };
    if operands.len() > 1 {
        return BuiltinResult::error("pushd: 参数过多\n");
    }

    let old = shell.cwd.clone();
    let target = match operands.first() {
        Some(target) => target.clone(),
        None => match shell.directory_stack.pop() {
            Some(target) => target.to_string_lossy().into_owned(),
            None => return BuiltinResult::error("pushd: 目录栈为空\n"),
        },
    };
    let result = cd::change_directory(shell, &target, false);
    if result.code != 0 {
        if operands.is_empty() {
            shell.directory_stack.push(target.into());
        }
        return result;
    }
    shell.directory_stack.push(old);
    BuiltinResult::stdout(dirs::render(shell, false, false, false))
}
