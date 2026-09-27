//! `pushd`：保存当前目录并切换，或与目录栈顶交换。
//!
//! # 用法
//! `pushd [--] [目录]`。
//!
//! # 参数与选项
//! 指定目录时将原 cwd 入栈；无参数时交换当前目录与已有栈顶。
//! 最多一个目录，使用 cd 的逻辑路径及 ~ 处理；不提供栈下标选项。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! pushd /tmp
//! popd
//! ```
//! 在目录可访问时保存原目录，随后恢复。
//!
//! # 输出与退出状态
//! 成功 0，stdout 显示新目录栈。空栈交换、参数过多或目录错误为 1，诊断写 stderr。
//!
//! # 状态影响
//! 修改 cwd、PWD、OLDPWD 及目录栈；切换失败不提交新栈状态。
//! 默认模式 Agent 不直接执行该状态操作；Native Agent 保留现有 Safety 与确认流程。 委托模式不能作为 builtin 管道源。

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
