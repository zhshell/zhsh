//! `popd`：弹出目录栈顶并切换到该目录。
//!
//! # 用法
//! `popd` 或 `popd --`。
//!
//! # 参数与选项
//! 不接受栈下标及其他参数；需要至少一个已保存目录。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! pushd /tmp
//! popd
//! ```
//! 在原目录仍可访问时返回原目录，并输出当前目录与剩余目录栈。
//!
//! # 输出与退出状态
//! 成功 0，stdout 显示目录栈；空栈、参数或切换失败为 1，诊断写 stderr。
//!
//! # 状态影响
//! 修改 cwd、PWD、OLDPWD 和目录栈；切换失败恢复弹出的记录。
//! 默认模式 Agent 不直接执行该状态操作；Native Agent 保留现有 Safety 与确认流程。 委托模式不能作为 builtin 管道源。

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
