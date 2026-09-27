//! `disown`：从当前作业管理中移除目标或设置免 HUP 标记。
//!
//! # 用法
//! `disown [-har] [--] [jobspec | pid ...]`。
//!
//! # 参数与选项
//! -h 标记不随 Shell 退出发送 HUP；无显式目标时 -a 选择全部、-r 选择运行作业，否则选择 %+。
//! 显式目标可为当前实例 jobspec 或成员 PID；短选项可组合。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。
//!
//! # 示例
//! 先通过 jobs 确认当前作业：
//! ```sh
//! disown -h %+
//! ```
//! 为该作业设置免 HUP 标记，保留作业记录。
//!
//! # 输出与退出状态
//! 成功 0 且不输出；选项错误为 2，目标或操作失败为 1，诊断写 stderr。多个目标可部分成功。
//!
//! # 状态影响
//! 不带 -h 时移除管理记录，移除不等于终止进程；带 -h 修改退出信号策略。
//! Agent 批量操作继续使用授权前冻结的目标集合。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::super::job::Status;
use super::{
    support::job_control::{error, flags, target},
    BuiltinResult,
};
pub(crate) fn select(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> Result<Vec<super::super::job::Binding>, BuiltinResult> {
    let (f, n) = flags(args, "har")?;
    let mut targets = Vec::new();
    if n < args.len() {
        for a in &args[n..] {
            match target(j, a) {
                Ok(b) => targets.push(b),
                Err(e) => return Err(error(1, e)),
            }
        }
    } else if f.contains('a') || f.contains('r') {
        targets = j
            .snapshots()
            .into_iter()
            .filter(|s| !f.contains('r') || s.status == Status::Running)
            .map(|s| s.binding)
            .collect();
    } else {
        match j.resolve("%+") {
            Ok(b) => targets.push(b),
            Err(e) => return Err(error(1, e)),
        }
    }
    Ok(targets)
}
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> BuiltinResult {
    let (f, n) = match flags(args, "har") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut result = BuiltinResult::ok();
    let targets = if let Some(selected) = j.selection {
        selected.to_vec()
    } else if n < args.len() {
        args[n..]
            .iter()
            .filter_map(|a| match target(j, a) {
                Ok(b) => Some(b),
                Err(e) => {
                    result.code = 1;
                    result.stderr.push_str(&format!("disown: {a}: {e}\n"));
                    None
                }
            })
            .collect()
    } else {
        match select(j, args) {
            Ok(v) => v,
            Err(e) => return e,
        }
    };
    for b in targets {
        if let Err(e) = j.disown(&b, f.contains('h')) {
            result.code = 1;
            result.stderr.push_str(&format!("disown: {e}\n"));
        }
    }
    result
}
