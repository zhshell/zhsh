//! `fg`：恢复最近一次被终端暂停的整行前台命令。
//!
//! 这是 Bash 过渡执行器提供的单槽位恢复能力，不是完整 job control：没有作业编号、
//! `jobs`、`bg`、`wait` 或 `disown`，也不拆分一行 Bash 内部创建的多个作业。

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
