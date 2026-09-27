//! `fg`：恢复前台命令；默认模式与 Native 使用不同监督入口。
//!
//! # 用法
//! 默认模式：`fg`。Native：`fg [jobspec]`，省略选择 `%+`。
//!
//! # 参数与选项
//! 默认模式不接受参数，只恢复最近一个暂停的整行命令。Native 至多一个 jobspec，支持当前实例的编号、当前/前一、前缀和子串选择。
//!
//! # 模式与上下文
//! 默认 execute 使用 ForegroundCommandControl/BashExecutor 单槽位。Native Shell 在 execute_native_plan 的特殊分支中校验并调用 jobs.resume；本文件保留原 execute_native 适配，不能替换主路径。
//!
//! # 示例
//! 先在当前模式启动可暂停命令并按 Ctrl-Z：
//! ```sh
//! fg
//! ```
//! 恢复默认暂停槽位或 Native 当前作业的前台执行。
//!
//! # 输出与退出状态
//! 返回恢复执行的原状态，终端输出/捕获由监督器处理。默认无上下文、无暂停命令或参数错误为 1。
//! Native 多参数为 2；目标失败的用户错误结果与 Agent PlanStale 保持分离，监督失败按原执行层返回。
//!
//! # 状态影响
//! 交接前台终端并等待作业完成或再次停止。默认 Agent 不允许 fg；Native 保留确认和终端监督。
//! 委托模式不能作为 builtin 管道源；不把前台透传输出冒充完整捕获。

use super::BuiltinResult;
use crate::common::AppResult;

/// `fg` 对前台命令监督器所需的最小端口。
pub(crate) trait ForegroundCommandControl {
    /// 恢复最近暂停的整行命令；`None` 表示当前没有可恢复命令。
    fn resume_stopped_command(&mut self) -> AppResult<Option<i32>>;
}

/// 校验有限的 `fg` 语法并恢复单个暂停槽位。
pub(crate) fn execute(
    control: Option<&mut dyn ForegroundCommandControl>,
    args: &[String],
) -> BuiltinResult {
    if !args.is_empty() {
        return BuiltinResult::error("fg: 当前仅支持 `fg`；作业编号和完整 job control 尚未实现\n");
    }
    let Some(control) = control else {
        return BuiltinResult::error("fg: 当前执行上下文不能恢复前台命令\n");
    };
    match control.resume_stopped_command() {
        Ok(Some(code)) => BuiltinResult {
            stdout: String::new(),
            stderr: String::new(),
            code,
        },
        Ok(None) => BuiltinResult::error("fg: 没有暂停的前台命令\n"),
        Err(error) => BuiltinResult::error(format!("fg: {error}\n")),
    }
}

pub(crate) fn execute_native(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
    cancel: Option<&crate::common::CancellationToken>,
) -> BuiltinResult {
    if args.len() > 1 {
        return super::support::job_control::error(2, "fg: expected at most one jobspec");
    }
    let b = match j.resolve(args.first().map_or("%+", String::as_str)) {
        Ok(b) => b,
        Err(e) => return super::support::job_control::error(1, e),
    };
    match j.resume(&b, true, cancel) {
        Ok(Some(r)) => BuiltinResult {
            stdout: r.output,
            stderr: String::new(),
            code: r.exit_code,
        },
        Ok(None) => BuiltinResult::ok(),
        Err(e) => super::support::job_control::error(1, e),
    }
}
