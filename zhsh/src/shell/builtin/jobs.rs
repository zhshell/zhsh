//! `jobs`：查看当前实例作业，或在准备阶段替换 jobspec 后执行命令。
//!
//! # 用法
//! `jobs [-lnprs] [--] [jobspec ...]`；`jobs -x 命令 [参数 ...]`。无参数列出全部作业。
//!
//! # 参数与选项
//! -l 显示成员 PID；-n 只显示有变化者；-p 只显示 PGID；-r/-s 仅运行/停止作业，可组合。
//! -x 必须位于首个参数，由 Native 计划准备阶段替换后续参数中的 jobspec 为 PGID，然后准备内层命令。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。
//!
//! # 示例
//! ```sh
//! jobs -l
//! ```
//! 显示当前 Native 会话的作业及成员 PID，没有作业时不输出。
//!
//! # 输出与退出状态
//! 列表写 stdout，正常为 0；未知选项为 2，目标不存在为 1，诊断写 stderr。
//! -x 返回实际内层命令的结果，准备失败保持原 Native 错误状态。
//!
//! # 状态影响
//! 列表会确认已显示作业的变化标记；-x 具有实际内层命令的效果，并保留冻结绑定。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::super::job::Status;
use super::{
    support::job_control::{error, flags},
    BuiltinResult,
};
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> BuiltinResult {
    let (f, n) = match flags(args, "lnprs") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let all = j.snapshots();
    let mut selected = Vec::new();
    if n == args.len() {
        selected = all;
    } else {
        for a in &args[n..] {
            let b = match j.resolve(a) {
                Ok(b) => b,
                Err(e) => return error(1, e),
            };
            selected.extend(all.iter().filter(|s| s.binding == b).cloned());
        }
    }
    let mut out = String::new();
    let mut keys = Vec::new();
    for s in selected {
        if f.contains('n') && !s.changed
            || f.contains('r') && s.status != Status::Running
            || f.contains('s') && !matches!(s.status, Status::Stopped(_))
        {
            continue;
        }
        keys.push(s.binding.key);
        if f.contains('p') {
            out.push_str(&format!("{}\n", s.binding.pgid));
            continue;
        }
        let status = s.status.label();
        let pids = if f.contains('l') {
            format!(
                "{} ",
                s.pids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "[{}]{} {pids}{status:<23} {}\n",
            s.binding.id, s.mark, s.command
        ));
    }
    j.acknowledge(&keys);
    BuiltinResult::stdout(out)
}
