//! `wait`：等待当前实例管理的子进程或作业。
//!
//! # 用法
//! `wait [-fn] [-p 变量] [--] [id ...]`；无 id 使用原运行时的等待集合。
//!
//! # 参数与选项
//! -n 等待集合内一个完成目标；-f 等待结束而非停止状态；-p 接受 ASCII 变量名，也接受紧随选项的变量名。
//! id 为当前实例 jobspec 或受管理 PID；多个普通目标依次等待。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。
//! Shell 主入口使用 execute_with_state；不带会话上下文的 execute 会拒绝 -p，不可互换。
//!
//! # 示例
//! 已有后台子进程时：
//! ```sh
//! wait -n -p finished
//! ```
//! 等待一个完成目标，并将返回的 PID 写入 finished。
//!
//! # 输出与退出状态
//! 通常无 stdout。结果透传等待目标状态；无有效目标/目标错误为 127，取消等待为 130，参数错误为 2，只读变量错误为 1；诊断写 stderr。
//!
//! # 状态影响
//! -p 先清除指定变量；使用 -n 且取得完成 PID 时再写回，不保证失败后保留旧值。
//! 领取等待结果；取消观察不向后台作业发终止信号。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::{support::job_control::error, BuiltinResult};
use crate::common::CancellationToken;
struct Arguments {
    next: bool,
    force: bool,
    variable: Option<String>,
    ids: Vec<String>,
}
fn parse(args: &[String]) -> Result<Arguments, BuiltinResult> {
    let mut result = Arguments {
        next: false,
        force: false,
        variable: None,
        ids: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(options) = arg.strip_prefix('-').filter(|a| !a.is_empty()) else {
            break;
        };
        let mut chars = options.chars();
        while let Some(c) = chars.next() {
            match c {
                'n' => result.next = true,
                'f' => result.force = true,
                'p' => {
                    let rest = chars.as_str();
                    let value = if rest.is_empty() {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or_else(|| error(2, "wait: -p requires a variable"))?
                    } else {
                        rest.to_owned()
                    };
                    if !super::support::variable::valid_name(&value) {
                        return Err(error(2, "wait: invalid variable"));
                    }
                    result.variable = Some(value);
                    break;
                }
                _ => return Err(error(2, format!("wait: invalid option -{c}"))),
            }
        }
        i += 1;
    }
    result.ids = args[i..].to_vec();
    Ok(result)
}
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
    cancel: Option<&CancellationToken>,
) -> BuiltinResult {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    if a.variable.is_some() {
        return error(2, "wait -p requires the Shell variable context");
    }
    run(j, &a, cancel, j.bindings).0
}

pub(crate) fn execute_with_state(
    j: &super::super::job::JobRuntime,
    state: &mut super::super::SessionState,
    args: &[String],
    cancel: Option<&CancellationToken>,
    bindings: &[(String, super::super::job::Binding)],
) -> BuiltinResult {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    if let Some(name) = &a.variable {
        if state.variables.get(name).is_some_and(|d| {
            d.strip_prefix("declare -")
                .and_then(|s| s.split_whitespace().next())
                .is_some_and(|f| f.contains('r'))
        }) {
            return error(1, "wait: readonly variable");
        }
        super::unset::execute(state, std::slice::from_ref(name));
    }
    let (result, pid) = run(j, &a, cancel, bindings);
    if a.next {
        if let (Some(name), Some(pid)) = (a.variable, pid) {
            state
                .variables
                .insert(name.clone(), format!("declare -- {name}=\"{pid}\""));
        }
    }
    result
}
fn run(
    j: &super::super::job::JobRuntime,
    a: &Arguments,
    cancel: Option<&CancellationToken>,
    bindings: &[(String, super::super::job::Binding)],
) -> (BuiltinResult, Option<i32>) {
    let mut result = BuiltinResult::ok();
    let mut completed = None;
    let groups = if a.next && !a.ids.is_empty() {
        let mut valid = Vec::new();
        for id in &a.ids {
            match j.check_wait_target(id, bindings) {
                Ok(()) => valid.push(id.clone()),
                Err(e) => result.stderr.push_str(&format!("wait: {id}: {e}\n")),
            }
        }
        if valid.is_empty() {
            result.code = 127;
            return (result, None);
        }
        vec![valid]
    } else if a.ids.len() > 1 {
        a.ids.iter().map(|id| vec![id.clone()]).collect()
    } else {
        vec![a.ids.clone()]
    };
    for ids in groups {
        match j.wait_bound(&ids, a.next, a.force, cancel, bindings) {
            Ok((code, pid)) => {
                result.code = code;
                completed = pid;
                if code == 130 && pid.is_none() {
                    break;
                }
            }
            Err(e) => {
                result.code = 127;
                result.stderr.push_str(&format!("wait: {e}\n"));
            }
        }
    }
    (result, completed)
}
