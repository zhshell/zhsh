//! `bg`：恢复当前实例作业并让其在后台运行。
//!
//! # 用法
//! `bg [jobspec ...]`；无参数选择当前作业 `%+`。
//!
//! # 参数与选项
//! jobspec 按原作业运行时解析，可使用 %编号、%+、%-、命令前缀或 %?子串；多个目标依次处理。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。
//!
//! # 示例
//! 先在 Native 中启动命令并用 Ctrl-Z 停止，再执行：
//! ```sh
//! bg %+
//! ```
//! 后台恢复当前作业，输出其编号、标记和命令。
//!
//! # 输出与退出状态
//! 成功项目写 stdout；目标或恢复错误写 stderr。全部成功为 0，任一目标失败为 1，已恢复项目不回滚。
//!
//! # 状态影响
//! 改变作业运行/前后台状态，不重新启动原命令。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::{support::job_control::error, BuiltinResult};
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> BuiltinResult {
    let fallback = vec!["%+".into()];
    let args = if args.is_empty() { &fallback } else { args };
    let mut result = BuiltinResult::ok();
    for a in args {
        match j.resolve(a).and_then(|b| {
            let snapshot = j
                .snapshots()
                .into_iter()
                .find(|s| s.binding == b)
                .ok_or_else(|| "no such job".to_string())?;
            j.resume(&b, false, None)
                .map(|_| {
                    j.snapshots()
                        .into_iter()
                        .find(|s| s.binding == b)
                        .unwrap_or(snapshot)
                })
                .map_err(|e| e.to_string())
        }) {
            Ok(s) => result
                .stdout
                .push_str(&format!("[{}]{} {}\n", s.binding.id, s.mark, s.command)),
            Err(e) => {
                let e = error(1, e);
                result.stderr.push_str(&e.stderr);
                result.code = 1;
            }
        }
    }
    result
}
