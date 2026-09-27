//! Native 作业内建的既有分派；调用前的模式选择、授权和特殊分支仍由 Shell 负责。

use super::super::builtin::{self, job_error as error, BuiltinResult, JobContext};
use crate::common::CancellationToken;

pub(crate) fn execute(
    j: &JobContext<'_>,
    name: &str,
    args: &[String],
    cancel: Option<&CancellationToken>,
) -> BuiltinResult {
    match name {
        "jobs" => builtin::jobs::execute(j, args),
        "fg" => builtin::fg::execute_native(j, args, cancel),
        "bg" => builtin::bg::execute(j, args),
        "wait" => builtin::wait::execute(j, args, cancel),
        "kill" => builtin::kill::execute(j, args),
        "disown" => builtin::disown::execute(j, args),
        "suspend" => builtin::suspend::execute(j, args),
        "set" => builtin::set::execute(j, args),
        "shopt" => builtin::shopt::execute(j, args),
        _ => error(2, "unknown job builtin"),
    }
}
