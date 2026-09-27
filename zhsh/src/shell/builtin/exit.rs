//! `exit`：以指定状态结束当前 zhsh 会话。
//!
//! # 用法
//! `exit [状态码]`；省略时沿用上一条命令状态。
//!
//! # 参数与选项
//! 只接受一个可解析为 i128 的整数，按模 256 转成 0–255；不接受其他选项。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//! Native Shell 在本处理器之后执行既有作业退出检查；被作业策略阻止时恢复未退出状态，保留再次退出的告警语义。
//!
//! # 示例
//! ```sh
//! exit 0
//! ```
//! 请求成功退出；Native 若存在受退出策略保护的作业，先按该策略报告并等待后续操作。
//!
//! # 输出与退出状态
//! 正常退出不输出，结果为选定状态码。参数过多为 1，非数字为 2，均写 stderr 且不设置退出标记。
//! Native 作业检查失败按执行层既有结果返回。
//!
//! # 状态影响
//! 设置会话退出标记；Native 的资源处理仍在 Shell 层。默认模式 Agent 不直接执行该状态操作；Native Agent 保留现有 Safety 与确认流程。
//! 委托模式无参数 exit 不能作管道源；带一个状态参数的形式在 Bash 子环境中执行。

use super::super::SessionState;
use super::BuiltinResult;

/// 验证退出状态并设置会话终止标记。
///
/// 无参数时沿用 `shell.last_exit`；数字按 Bash 习惯折算为无符号八位状态。参数错误不会
/// 设置 `should_exit`。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    if args.len() > 1 {
        return BuiltinResult::error("exit: 参数过多\n");
    }
    let status = match args.first() {
        None => shell.last_exit,
        Some(argument) => {
            let Ok(value) = argument.parse::<i128>() else {
                return BuiltinResult {
                    stdout: String::new(),
                    stderr: format!("exit: {argument}: 需要数字参数\n"),
                    code: 2,
                };
            };
            value.rem_euclid(256) as i32
        }
    };
    shell.should_exit = true;
    BuiltinResult {
        stdout: String::new(),
        stderr: String::new(),
        code: status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_last_status_and_normalizes_numeric_status() {
        let mut shell = SessionState::test();
        shell.last_exit = 7;
        assert_eq!(execute(&mut shell, &[]).code, 7);
        shell.should_exit = false;
        assert_eq!(execute(&mut shell, &["-1".into()]).code, 255);
    }

    #[test]
    fn invalid_arguments_do_not_exit() {
        let mut shell = SessionState::test();
        assert_eq!(execute(&mut shell, &["nope".into()]).code, 2);
        assert!(!shell.should_exit);
        assert_eq!(execute(&mut shell, &["1".into(), "2".into()]).code, 1);
        assert!(!shell.should_exit);
    }
}
