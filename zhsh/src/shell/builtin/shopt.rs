//! `shopt`：查询或修改 Native 的 checkjobs/huponexit 选项。
//!
//! # 用法
//! `shopt [-suqp] [--] [checkjobs|huponexit ...]`。
//!
//! # 参数与选项
//! -s/-u 带名称时启用/关闭；无名称时筛选已启用/已关闭选项。-q 抑制查询输出，-p 输出可重放的 shopt 形式。
//! 无参数列出两项；-s 与 -u 冲突。不实现其他 Bash shopt 选项。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。
//!
//! # 示例
//! ```sh
//! shopt -p
//! ```
//! 输出两项当前选项对应的设置命令。
//!
//! # 输出与退出状态
//! 正常查询写 stdout，修改不输出。成功为 0；指定查询项存在关闭项或名称不支持为 1；无效/冲突标志为 2，错误诊断写 stderr。
//!
//! # 状态影响
//! 修改当前实例退出相关选项；逐项处理，后续错误不回滚先前修改。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::{
    support::job_control::{error, flags},
    BuiltinResult,
};
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> BuiltinResult {
    let (f, n) = match flags(args, "supq") {
        Ok(v) => v,
        Err(e) => return e,
    };
    if f.contains('s') && f.contains('u') {
        return error(2, "shopt: conflicting options");
    }
    let defaults = vec!["checkjobs".into(), "huponexit".into()];
    let names = if n == args.len() {
        &defaults
    } else {
        &args[n..]
    };
    let mut out = String::new();
    let mut code = 0;
    for name in names {
        if !matches!(name.as_str(), "checkjobs" | "huponexit") {
            return error(1, "shopt: unsupported option");
        }
        if n < args.len() && (f.contains('s') || f.contains('u')) {
            let _ = j.set_option(name, f.contains('s'));
        } else {
            let o = j.options();
            let value = if name == "checkjobs" {
                o.checkjobs
            } else {
                o.huponexit
            };
            if n == args.len() && ((f.contains('s') && !value) || (f.contains('u') && value)) {
                continue;
            }
            if !value && n < args.len() {
                code = 1;
            }
            if !f.contains('q') {
                if f.contains('p') {
                    out.push_str(&format!(
                        "shopt -{} {name}\n",
                        if value { 's' } else { 'u' }
                    ));
                } else {
                    out.push_str(&format!("{name}\t{}\n", if value { "on" } else { "off" }));
                }
            }
        }
    }
    BuiltinResult {
        stdout: out,
        stderr: String::new(),
        code,
    }
}
