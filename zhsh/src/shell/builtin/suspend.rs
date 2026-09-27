//! `suspend`：暂停当前 Native Shell 并等待恢复前台。
//!
//! # 用法
//! `suspend [-f]`。
//!
//! # 参数与选项
//! 无参数要求 monitor 启用且不是登录 Shell；-f 只跳过这些前置检查，不保证环境一定允许成功暂停/恢复。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。 需要能够恢复该 Shell 的父级监督者和适用的终端环境。
//!
//! # 示例
//! 在支持作业控制的父 Shell 中启动嵌套 zhsh --native 后执行：
//! ```sh
//! suspend
//! ```
//! 父 Shell 重新获得控制，随后可从父 Shell 将嵌套 Shell 恢复到前台。
//!
//! # 输出与退出状态
//! 成功恢复后返回 0 且不输出；参数错误 2，暂停条件/信号/恢复失败为 1，诊断写 stderr。
//!
//! # 状态影响
//! 向本 Shell 发送 SIGTSTP，恢复原信号处理并等待前台控制；不以退出替代暂停。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::{support::job_control::error, BuiltinResult};
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> BuiltinResult {
    if !(args.is_empty() || args == ["-f"]) {
        return error(2, "suspend: usage: suspend [-f]");
    }
    if args.is_empty() && (!j.options().monitor || j.login()) {
        return error(1, "suspend: no job control");
    }
    // SAFETY: suspend this shell, restoring its original disposition after SIGCONT.
    unsafe {
        let old = libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        let rc = libc::raise(libc::SIGTSTP);
        libc::signal(libc::SIGTSTP, old);
        if rc != 0 {
            return error(1, std::io::Error::last_os_error());
        }
    }
    if let Err(e) = j.await_shell_foreground() {
        return error(1, e);
    }
    BuiltinResult::ok()
}
