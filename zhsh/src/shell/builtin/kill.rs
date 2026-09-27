//! `kill`：列出信号，或向 PID/当前实例作业发送信号。
//!
//! # 用法
//! `kill [-s 信号|-n 信号|-信号] [--] pid|jobspec ...`；`kill -l|-L [信号或状态 ...]`。
//!
//! # 参数与选项
//! 默认 TERM；信号可用名称（可带 SIG）、编号和受支持的 RTMIN/RTMAX 偏移。
//! -l/-L 无参数列出信号，有参数在名称/编号间转换，超过 128 的数字按退出状态减 128。
//! PID 数值交原信号接口处理；jobspec 选择当前实例的作业进程组。
//!
//! # 模式与上下文
//! 仅 Native 路由使用本文件的作业实现，操作当前 zhsh 实例；默认委托模式保留原路由，不调用此实现。
//!
//! # 示例
//! ```sh
//! kill -l TERM
//! ```
//! 查询 TERM 的编号，不发送信号。
//!
//! # 输出与退出状态
//! 列表/转换写 stdout，发送成功通常无输出。成功 0；发送或列表转换失败为 1，缺少目标/信号或发送选项错误为 2，诊断写 stderr。
//!
//! # 状态影响
//! 发送信号可改变目标进程状态；列表无进程副作用。保留既有 PID 身份绑定与 jobspec 检查，不导入其他实例作业。Native Agent 保留现有 Safety、确认及目标绑定；本命令不接入默认模式的 builtin 管道源。

use super::{support::job_control::error, BuiltinResult};
const SIGNALS: &[(&str, i32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("STKFLT", 16),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
];
fn signal_name(n: i32) -> Option<String> {
    if let Some((name, _)) = SIGNALS.iter().find(|(_, v)| *v == n) {
        return Some((*name).into());
    }
    let min = libc::SIGRTMIN();
    let max = libc::SIGRTMAX();
    if n == min {
        Some("RTMIN".into())
    } else if n == max {
        Some("RTMAX".into())
    } else if (min..=max).contains(&n) {
        Some(format!("RTMIN+{}", n - min))
    } else {
        None
    }
}
fn number(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return (0..=libc::SIGRTMAX()).contains(&n).then_some(n);
    }
    let name = s.to_uppercase();
    let name = name.strip_prefix("SIG").unwrap_or(&name);
    let rt = if name == "RTMIN" {
        Some(libc::SIGRTMIN())
    } else if name == "RTMAX" {
        Some(libc::SIGRTMAX())
    } else if let Some(n) = name.strip_prefix("RTMIN+") {
        n.parse::<i32>()
            .ok()
            .and_then(|n| libc::SIGRTMIN().checked_add(n))
    } else if let Some(n) = name.strip_prefix("RTMAX-") {
        n.parse::<i32>()
            .ok()
            .and_then(|n| libc::SIGRTMAX().checked_sub(n))
    } else {
        None
    };
    rt.filter(|n| (libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(n))
        .or_else(|| SIGNALS.iter().find(|(n, _)| *n == name).map(|(_, n)| *n))
}
pub(crate) fn execute(
    j: &super::support::job_control::JobContext<'_>,
    args: &[String],
) -> BuiltinResult {
    if args.first().is_some_and(|s| s == "-l" || s == "-L") {
        if args.len() == 1 {
            return BuiltinResult::stdout(format!(
                "{}\n",
                (1..=libc::SIGRTMAX())
                    .filter_map(signal_name)
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        let mut out = String::new();
        for a in &args[1..] {
            if let Ok(n) = a.parse::<i32>() {
                let n = if n > 128 { n - 128 } else { n };
                if let Some(name) = signal_name(n) {
                    out.push_str(&format!("{name}\n"));
                } else {
                    return error(1, "invalid signal");
                }
            } else if let Some(n) = number(a) {
                out.push_str(&format!("{n}\n"));
            } else {
                return error(1, "invalid signal");
            }
        }
        return BuiltinResult::stdout(out);
    }
    let mut sig = libc::SIGTERM;
    let mut i = 0;
    if let Some(a) = args.first() {
        if a == "-s" || a == "-n" {
            let Some(n) = args.get(1).and_then(|a| number(a)) else {
                return error(2, "kill: signal required");
            };
            sig = n;
            i = 2;
        } else if let Some(a) = a.strip_prefix('-').filter(|a| *a != "-") {
            let Some(n) = number(a) else {
                return error(2, "invalid signal");
            };
            sig = n;
            i = 1;
        }
    }
    if args.get(i).is_some_and(|a| a == "--") {
        i += 1;
    }
    if i == args.len() {
        return error(2, "kill: target required");
    }
    let mut result = BuiltinResult::ok();
    for a in &args[i..] {
        let r = if a.starts_with('%') {
            j.resolve(a)
                .and_then(|b| j.send(&b, sig).map_err(|e| e.to_string()))
        } else {
            a.parse::<i32>()
                .map_err(|_| "invalid pid".into())
                .and_then(|pid| j.send_pid(pid, sig))
        };
        if let Err(e) = r {
            result.code = 1;
            result.stderr.push_str(&format!("kill: {a}: {e}\n"));
        }
    }
    result
}

pub(crate) fn pid_operands(args: &[String]) -> Vec<i32> {
    if args.first().is_some_and(|a| a == "-l" || a == "-L") {
        return Vec::new();
    }
    let mut i = match args.first().map(String::as_str) {
        Some("-s" | "-n") => 2,
        Some(a) if a.starts_with('-') && a != "--" => 1,
        _ => 0,
    };
    if args.get(i).is_some_and(|a| a == "--") {
        i += 1;
    }
    args.get(i..)
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a.parse::<i32>().ok().filter(|p| *p > 0))
        .collect()
}
