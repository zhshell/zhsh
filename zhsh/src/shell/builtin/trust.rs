//! `zh trust`：查询或修改 Agent 授信等级。
//!
//! 用法：`zh trust [-w] [balanced|confirm|trusted]`

use super::super::{trust, AgentTrust, SessionState};
use super::BuiltinResult;

const HELP: &str = "Agent 授信：查看或修改 Agent 命令的确认策略。\n\n用法：zh trust [-w] [balanced|confirm|trusted]\n\n等级：\n  balanced  默认策略，只自动执行满足可信只读条件的命令\n  confirm   所有可执行 Agent 命令都需要确认\n  trusted   额外放行受限的任务范围内普通修改，不绕过强制确认边界\n\n选项：\n  -w  同时写入 ~/.zhshrc；省略时只影响当前会话\n\nSafety 分类不是安全证明，用户直接输入的 Shell 命令不经过该策略。\n详细说明：man zhsh\n";

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
