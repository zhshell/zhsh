//! Native 作业内建的调用上下文和共享参数处理；不负责命令分派或模式选择。
//!
//! JobContext 借用现有作业运行时和冻结绑定，不持有新的作业资源；目标解析与信号发送
//! 保留原有效性检查。error、flags、target 供作业命令复用，不作为用户命令注册。

use super::super::super::job::{Binding, JobRuntime};
use super::super::BuiltinResult;
pub(crate) fn error(code: i32, msg: impl std::fmt::Display) -> BuiltinResult {
    BuiltinResult {
        stdout: String::new(),
        stderr: format!("{msg}\n"),
        code,
    }
}
pub(in super::super) fn target(j: &JobContext<'_>, arg: &str) -> Result<Binding, String> {
    if arg.starts_with('%') {
        j.resolve(arg)
    } else {
        let pid = arg.parse::<i32>().map_err(|_| "invalid pid")?;
        j.snapshots()
            .into_iter()
            .find(|s| s.pids.contains(&pid))
            .map(|s| s.binding)
            .ok_or("no such job".into())
    }
}
pub(in super::super) fn flags(
    args: &[String],
    allowed: &str,
) -> Result<(String, usize), BuiltinResult> {
    let mut flags = String::new();
    let mut index = 0;
    for a in args {
        if a == "--" {
            index += 1;
            break;
        }
        if !a.starts_with('-') || a == "-" {
            break;
        }
        for c in a[1..].chars() {
            if !allowed.contains(c) {
                return Err(error(2, format!("invalid option: -{c}")));
            }
            flags.push(c);
        }
        index += 1;
    }
    Ok((flags, index))
}

pub(crate) struct JobContext<'a> {
    pub runtime: &'a JobRuntime,
    pub selection: Option<&'a [Binding]>,
    pub bindings: &'a [(String, Binding)],
    pub pids: &'a [super::super::super::job::PidBinding],
}
impl std::ops::Deref for JobContext<'_> {
    type Target = JobRuntime;
    fn deref(&self) -> &JobRuntime {
        self.runtime
    }
}
impl JobContext<'_> {
    pub fn send_pid(&self, pid: i32, signal: i32) -> Result<(), String> {
        if let Some(b) = self.pids.iter().find(|b| b.pid == pid) {
            return b.send(signal).map_err(|e| e.to_string());
        }
        if unsafe { libc::kill(pid, signal) } < 0 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(())
        }
    }
    pub fn resolve(&self, spec: &str) -> Result<Binding, String> {
        if let Some((_, b)) = self.bindings.iter().find(|(s, _)| s == spec) {
            if self.runtime.valid(b) {
                return Ok(*b);
            }
            return Err("PlanStale: job no longer exists".into());
        }
        self.runtime.resolve(spec)
    }
}
