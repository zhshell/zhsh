//! `zh trust`：查看或设置 Agent 授信等级。
//!
//! # 用法
//! `zh trust [-w] [balanced|confirm|trusted]`；`zh trust help|-h|--help`。无参数查询当前等级。
//!
//! # 参数与选项
//! balanced 默认只自动执行静态可信、有限前台、披露受限且无会话修改或文件输出的只读命令。
//! confirm 确认所有可执行 Agent 命令。trusted 额外自动执行内置 Linux 核心规则判为
//! state_changing、所有写目标可确定且位于任务根内的操作，如满足条件的 touch/mkdir。
//! 自动执行仍要求目标身份可信、无网络或提权、无运维/敏感披露及其他强制确认信号。
//! 任务根是任务开始时固定的规范化工作目录，不随 cd 或任务文字中提到的路径扩大。
//! 未知行为、被判为破坏性的操作、范围外修改、应用规则报告的修改和会话修改仍需确认。
//! 不支持语法、脱离监督或没有静态终止条件的执行直接拒绝。
//! -w 最多一次，写入启动时确定的用户目录中的 .zhshrc；仅 -w 时持久化当前等级。
//! 等级只能给一个，帮助标志必须单独使用。
//!
//! # 模式与上下文
//! 默认委托模式与 Native 复用相同授信处理；持久化依赖可用的启动用户目录，不随后续 HOME 赋值切换。
//!
//! # 示例
//! ```sh
//! zh trust
//! ```
//! 显示当前会话的有效授信等级。
//!
//! # 输出与退出状态
//! 查询或修改回执写 stdout，成功 0；参数和持久化错误为 1，写 stderr。
//!
//! # 状态影响
//! 无 -w 的修改仅影响会话；-w 先完成原子持久化再提交会话，写入失败不切换等级。
//! 默认 Agent 仅允许无参数查询；Native 保留原授权。委托模式查询可作 builtin 管道源，授信修改不可。
//! 该策略针对 Agent，不限制用户直接输入的 Shell 命令。

use super::super::super::{trust, AgentTrust, SessionState};
use super::super::BuiltinResult;

const HELP: &str = "Agent 授信：设置 Agent 命令的确认策略。

用法：zh trust [-w] [balanced|confirm|trusted]

等级（从严格到宽松）：
  confirm   所有可执行命令逐条确认
  balanced  默认，符合条件的只读命令自动执行
  trusted   更宽松，额外允许核心规则识别的任务根内普通修改自动执行

典型案例（自动执行均须通过 Safety 评估）：
  pwd           balanced/trusted 自动执行；confirm 确认
  touch ./note  任务根内符合条件时 trusted 自动执行；其余等级确认
  cd /path（Native）、rm ./note  所有等级均需确认

任务根是任务开始时的工作目录；不会随 cd 或任务描述扩大。
trusted 仍保留网络、提权、敏感访问等确认；不支持的执行形式各等级均拒绝。

无参数查询；指定等级仅修改当前会话，-w 同时保存到 ~/.zhshrc。
示例：zh trust trusted；zh trust -w balanced。

仅作用于 Agent 命令。完整条件、模式限制和持久化细节：man zhsh
";

/// 默认只修改当前会话；`-w` 先原子更新 `~/.zhshrc`，成功后再提交会话值。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    if matches!(args, [value] if matches!(value.as_str(), "help" | "-h" | "--help")) {
        return BuiltinResult::stdout(HELP);
    }
    let mut write = false;
    let mut requested = None;
    for argument in args {
        if argument == "-w" {
            if write {
                return BuiltinResult::error(
                    "zh trust: -w 只能指定一次\n运行 `zh trust -h` 查看用法。\n",
                );
            }
            write = true;
        } else if requested.is_none() {
            let Some(trust) = AgentTrust::parse(argument) else {
                return BuiltinResult::error(format!(
                    "zh trust: 未知授信等级 {argument}\n运行 `zh trust -h` 查看用法。\n"
                ));
            };
            requested = Some(trust);
        } else {
            return BuiltinResult::error(
                "zh trust: 授信等级只能指定一次\n运行 `zh trust -h` 查看用法。\n",
            );
        }
    }

    if requested.is_none() && !write {
        return BuiltinResult::stdout(format!("授信: {}\n", shell.agent_trust().as_str()));
    }

    let candidate = requested.unwrap_or_else(|| shell.agent_trust());
    if write {
        let Some(home) = shell.user_home() else {
            return BuiltinResult::error(
                "zh trust: 持久化失败: 用户状态不可用：HOME 必须是绝对路径\n",
            );
        };
        if let Err(error) = trust::persist(home, candidate) {
            return BuiltinResult::error(format!("zh trust: 持久化失败: {error}\n"));
        }
    }
    shell.set_agent_trust(candidate);

    let suffix = if write {
        "（已写入 ~/.zhshrc）"
    } else {
        ""
    };
    BuiltinResult::stdout(format!("授信: {}{suffix}\n", candidate.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_home(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zhsh-trust-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn no_arguments_show_the_effective_default() {
        let mut shell = SessionState::test();
        shell.env.remove(AgentTrust::ENVIRONMENT_KEY);

        let result = execute(&mut shell, &[]);

        assert_eq!(result.code, 0);
        assert_eq!(result.stdout, "授信: balanced\n");
    }

    #[test]
    fn setting_without_write_only_changes_the_session() {
        let home = temporary_home("session");
        let mut shell = SessionState::test();
        shell
            .env
            .insert("HOME".into(), home.to_string_lossy().into());
        shell.set_user_home_for_test(Some(home.clone()));

        let result = execute(&mut shell, &["trusted".into()]);

        assert_eq!(result.stdout, "授信: trusted\n");
        assert_eq!(shell.agent_trust(), AgentTrust::Trusted);
        assert!(!home.join(".zhshrc").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn write_persists_a_managed_block_and_then_commits_the_session() {
        let home = temporary_home("write");
        std::fs::write(home.join(".zhshrc"), "export EDITOR=vim\n").unwrap();
        let mut shell = SessionState::test();
        shell
            .env
            .insert("HOME".into(), home.to_string_lossy().into());
        shell.set_user_home_for_test(Some(home.clone()));

        let result = execute(&mut shell, &["-w".into(), "confirm".into()]);

        assert_eq!(result.code, 0);
        assert_eq!(shell.agent_trust(), AgentTrust::Confirm);
        let content = std::fs::read_to_string(home.join(".zhshrc")).unwrap();
        assert!(content.contains("export EDITOR=vim"));
        assert!(content.contains("export ZHSH_AGENT_TRUST=confirm"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn write_without_a_value_persists_the_effective_level() {
        let home = temporary_home("write-current");
        let mut shell = SessionState::test();
        shell
            .env
            .insert("HOME".into(), home.to_string_lossy().into());
        shell.set_user_home_for_test(Some(home.clone()));
        shell.set_agent_trust(AgentTrust::Trusted);

        let result = execute(&mut shell, &["-w".into()]);

        assert_eq!(result.code, 0);
        let content = std::fs::read_to_string(home.join(".zhshrc")).unwrap();
        assert!(content.contains("export ZHSH_AGENT_TRUST=trusted"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn persistence_failure_keeps_the_previous_session_value() {
        let home = temporary_home("failure");
        std::fs::create_dir(home.join(".zhshrc")).unwrap();
        let mut shell = SessionState::test();
        shell
            .env
            .insert("HOME".into(), home.to_string_lossy().into());
        shell.set_user_home_for_test(Some(home.clone()));
        shell.set_agent_trust(AgentTrust::Balanced);

        let result = execute(&mut shell, &["trusted".into(), "-w".into()]);

        assert_eq!(result.code, 1);
        assert_eq!(shell.agent_trust(), AgentTrust::Balanced);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn write_requires_the_fixed_startup_home() {
        let home = temporary_home("unavailable");
        let mut shell = SessionState::test();
        shell
            .env
            .insert("HOME".into(), home.to_string_lossy().into());
        shell.set_user_home_for_test(None);

        let result = execute(&mut shell, &["trusted".into(), "-w".into()]);

        assert_eq!(result.code, 1);
        assert_eq!(shell.agent_trust(), AgentTrust::Balanced);
        assert!(!home.join(".zhshrc").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn invalid_or_duplicate_arguments_do_not_change_the_session() {
        let mut shell = SessionState::test();
        shell.set_agent_trust(AgentTrust::Confirm);

        for args in [
            vec!["unsafe".into()],
            vec!["balanced".into(), "trusted".into()],
            vec!["-w".into(), "-w".into()],
        ] {
            assert_eq!(execute(&mut shell, &args).code, 1);
            assert_eq!(shell.agent_trust(), AgentTrust::Confirm);
        }
    }
}
