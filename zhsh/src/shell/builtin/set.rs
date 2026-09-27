//! `set`：读取或修改 Native 作业相关选项。
//!
//! # 用法
//! `set [-m|+m|-b|+b ...]`、`set -o|+o [monitor|notify]`；无参数成功且不输出。
//!
//! # 参数与选项
//! -m/+m 启用/关闭 monitor；-b/+b 启用/关闭 notify，短选项可组合。
//! -o/+o 带选项名时启用/关闭；无名称时列出两项布尔值。不实现其他 Bash set 选项或位置参数。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。
//!
//! # 示例
//! ```sh
//! set -o
//! ```
//! 读取 monitor、notify 的当前状态。
//!
//! # 输出与退出状态
//! 查询写 stdout，设置成功不输出，状态为 0；未知选项/参数等错误为 2 并写 stderr。
//!
//! # 状态影响
//! 修改当前 Native 作业配置；多项顺序应用，后续失败不回滚先前选项。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::{support::job_control::error, BuiltinResult};
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> BuiltinResult {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-o" || a == "+o" {
            i += 1;
            if i == args.len() {
                let o = j.options();
                return BuiltinResult::stdout(format!(
                    "monitor\t{}\nnotify\t{}\n",
                    o.monitor, o.notify
                ));
            }
            if !matches!(args[i].as_str(), "monitor" | "notify") {
                return error(2, "set: unsupported option");
            }
            if let Err(e) = j.set_option(&args[i], a == "-o") {
                return error(2, e);
            }
        } else if a.starts_with('-') || a.starts_with('+') {
            for c in a[1..].chars() {
                let name = match c {
                    'm' => "monitor",
                    'b' => "notify",
                    _ => return error(2, "set: unsupported option"),
                };
                let _ = j.set_option(name, a.starts_with('-'));
            }
        } else {
            return error(2, "set: unsupported argument");
        }
        i += 1;
    }
    BuiltinResult::ok()
}
