//! `zh`：zhsh 自身状态的顶级管理命令。
//!
//! 用法：`zh [status|ls|use|llm|tier|trust|safety|codec|help]`

use super::super::{SafetyManagementPort, SafetyManagementUi, SessionState};
use super::BuiltinResult;
use crate::application::{CodecManagementUi, LlmConfigService, LlmConfigUi};
use crate::llm::{self, CodecRuntime, ModelTier};
use std::path::Path;
use std::sync::Arc;

const STATUS_HELP: &str = "zh status：显示当前 LLM、Codec、传输和 Agent 授信状态。\n\n用法：zh status\n\n该命令只读取当前会话状态。\n详细说明：man zhsh\n";
const LS_HELP: &str = "zh ls：列出已保存的 LLM 配置。\n\n用法：zh ls\n\n一行输出一个配置名；空输出表示没有持久化配置。\n详细说明：man zhsh\n";
const USE_HELP: &str = "zh use：选择一个已保存的 LLM 配置。\n\n用法：zh use <配置名>\n\n完整配置会立即启用；不完整配置可进入修复，或仅设为活动配置并保持 Agent 不可用。普通 Shell 不受影响。\n详细说明：man zhsh\n";
const TIER_HELP: &str = "zh tier：切换当前 LLM 配置的模型档位。\n\n用法：zh tier <flash|standard|max>\n\n成功后会写入当前配置文件，并切换当前会话使用的模型。\n详细说明：man zhsh\n";

fn no_extra(command: &str, args: &[String]) -> Option<BuiltinResult> {
    (!args.is_empty()).then(|| {
        BuiltinResult::error(format!(
            "zh {command}: 参数过多\n运行 `zh {command} -h` 查看用法。\n"
        ))
    })
}

fn help_requested(args: &[String]) -> bool {
    matches!(args, [value] if matches!(value.as_str(), "help" | "-h" | "--help"))
}

fn show(shell: &SessionState) -> BuiltinResult {
    let trust = shell.agent_trust().as_str();
    let Some(config) = shell.llm.as_ref() else {
        let profile = shell
            .active_llm_name()
            .map(|name| format!("配置: {name}"))
            .unwrap_or_else(|| "LLM: 未配置".into());
        return BuiltinResult::stdout(format!(
            "{profile}\nAgent: 不可用（{}）\nCodec: 无\n授信: {trust}\n",
            shell
                .agent_unavailable_reason()
                .unwrap_or("尚未配置 LLM；运行 `zh llm` 创建配置")
        ));
    };
    let agent = shell
        .agent_unavailable_reason()
        .map(|reason| format!("不可用（{reason}）"))
        .unwrap_or_else(|| "可用".into());
    let transport = llm::transport_status(&config.url).unwrap_or("无效");
    BuiltinResult::stdout(format!(
        "配置: {}\nAgent: {agent}\nCodec: {}\nJSON Schema: {}\nURL: {}\n传输: {transport}\n档位: {}\n当前模型: {}\nflash: {}\nstandard: {}\nmax: {}\naccess-token: {}\n授信: {trust}\n",
        config.name,
        config.request_format,
        config.json_schema.status(),
        config.url,
        config.tier.as_str(),
        config.model(),
        config.models.flash,
        config.models.standard,
        config.models.max,
        llm::mask_auth(Some(&config.access_token))
    ))
}

fn set_tier(
    shell: &mut SessionState,
    args: &[String],
    codecs: Option<&Arc<CodecRuntime>>,
) -> BuiltinResult {
    let [value] = args else {
        return BuiltinResult::error("zh tier: 需要一个模型档位\n运行 `zh tier -h` 查看用法。\n");
    };
    let Some(tier) = ModelTier::parse(value) else {
        return BuiltinResult::error(format!(
            "zh tier: 未知模型档位 {value}\n运行 `zh tier -h` 查看用法。\n"
        ));
    };
    let Some(codecs) = codecs.cloned() else {
        return BuiltinResult::error("zh: Codec 运行时未注入\n");
    };
    let service = LlmConfigService::new(shell.user_home().map(Path::to_path_buf), codecs);
    match service.set_tier(shell.llm.as_ref(), tier) {
        Ok(config) => {
            shell.commit_ready_llm(config);
            BuiltinResult::stdout(format!("tier: {}\n", tier.as_str()))
        }
        Err(error) => BuiltinResult::error(format!("zh: 保存档位失败: {error}\n")),
    }
}

fn help() -> BuiltinResult {
    BuiltinResult::stdout(
        "zh：管理 zhsh 自身状态。\n\n用法：zh [COMMAND]\n\n命令：\n  status  显示当前 LLM、Codec、传输和 Agent 授信状态\n  ls      列出已保存的 LLM 配置\n  use     启用一个 LLM 配置\n  llm     创建或修改 LLM 配置\n  tier    切换当前模型档位\n  trust   查看或修改 Agent 授信等级\n  safety  查看、校验、安装或重载 Safety 规则\n  codec   查看、校验、安装、卸载、导出或重载 Codec\n  help    显示本帮助\n\n运行 `zh <COMMAND> -h` 查看具体说明，或运行 `man zhsh` 查看完整手册。\n",
    )
}

/// 路由受支持的 zhsh 状态与 LLM 管理子命令。
///
/// 无参数等价于 `zh status`。旧的 `show`、`provider`、`model`、`list` 和 `config.*`
/// 形式不会兼容，统一作为未知命令返回。
pub(crate) fn execute(
    shell: &mut SessionState,
    args: &[String],
    codec_runtime: Option<&Arc<CodecRuntime>>,
    llm_config_ui: Option<&dyn LlmConfigUi>,
    codec_management_ui: Option<&dyn CodecManagementUi>,
    safety_management_ui: Option<&dyn SafetyManagementUi>,
    safety_management: Option<&dyn SafetyManagementPort>,
) -> BuiltinResult {
    let Some((command, rest)) = args.split_first() else {
        return show(shell);
    };
    match command.as_str() {
        "status" => {
            if help_requested(rest) {
                BuiltinResult::stdout(STATUS_HELP)
            } else {
                no_extra("status", rest).unwrap_or_else(|| show(shell))
            }
        }
        "ls" => {
            if help_requested(rest) {
                BuiltinResult::stdout(LS_HELP)
            } else {
                no_extra("ls", rest)
                    .unwrap_or_else(|| super::llm_config::list(shell, codec_runtime))
            }
        }
        "use" => {
            if help_requested(rest) {
                return BuiltinResult::stdout(USE_HELP);
            }
            let [name] = rest else {
                return BuiltinResult::error(
                    "zh use: 需要一个配置名\n运行 `zh use -h` 查看用法。\n",
                );
            };
            super::llm_config::use_config(shell, name, codec_runtime, llm_config_ui)
        }
        "llm" => super::llm_config::interactive(shell, rest, codec_runtime, llm_config_ui),
        "tier" => {
            if help_requested(rest) {
                BuiltinResult::stdout(TIER_HELP)
            } else {
                set_tier(shell, rest, codec_runtime)
            }
        }
        "trust" => super::trust::execute(shell, rest),
        "safety" => super::safety::execute(shell, rest, safety_management_ui, safety_management),
        "codec" => super::codec::execute(shell, rest, codec_runtime, codec_management_ui),
        "help" | "--help" | "-h" => no_extra("help", rest).unwrap_or_else(help),
        _ => BuiltinResult::error("zh: 未知命令；运行 `zh help`\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_always_reports_the_effective_trust_level() {
        let mut shell = SessionState::test();
        shell.clear_active_llm();
        shell.env.insert("ZHSH_AGENT_TRUST".into(), String::new());
        let default = execute(&mut shell, &[], None, None, None, None, None);
        assert!(default.stdout.contains("授信: balanced\n"));

        shell
            .env
            .insert("ZHSH_AGENT_TRUST".into(), "trusted".into());
        let trusted = execute(&mut shell, &["status".into()], None, None, None, None, None);
        assert!(trusted.stdout.contains("授信: trusted\n"));
    }

    #[test]
    fn rejects_extra_arguments_on_read_only_commands() {
        let mut shell = SessionState::test();
        for args in [
            vec!["status".into(), "extra".into()],
            vec!["ls".into(), "extra".into()],
            vec!["help".into(), "extra".into()],
        ] {
            assert_eq!(
                execute(&mut shell, &args, None, None, None, None, None).code,
                1
            );
        }
    }

    #[test]
    fn llm_subcommand_help_does_not_require_an_interactive_ui() {
        let mut shell = SessionState::test();
        let result = execute(
            &mut shell,
            &["llm".into(), "--help".into()],
            None,
            None,
            None,
            None,
            None,
        );

        assert_eq!(result.code, 0);
        assert!(result.stdout.contains("LLM 配置："));
        assert!(result.stdout.contains("zh llm -m|--modify [<配置名>]"));
    }

    #[test]
    fn every_management_command_exposes_help_without_runtime_dependencies() {
        let mut shell = SessionState::test();
        for args in [
            vec!["--help".into()],
            vec!["status".into(), "-h".into()],
            vec!["ls".into(), "-h".into()],
            vec!["use".into(), "-h".into()],
            vec!["llm".into(), "-h".into()],
            vec!["tier".into(), "-h".into()],
            vec!["trust".into(), "-h".into()],
            vec!["safety".into(), "-h".into()],
            vec!["codec".into(), "-h".into()],
        ] {
            let result = execute(&mut shell, &args, None, None, None, None, None);
            assert_eq!(result.code, 0, "{args:?}: {}", result.stderr);
            assert!(!result.stdout.trim().is_empty(), "{args:?}");
            assert!(result.stdout.contains("用法"), "{args:?}");
        }
    }

    #[test]
    fn failed_tier_save_keeps_the_previous_in_memory_tier() {
        let mut shell = SessionState::test();
        let home = std::env::temp_dir().join(format!("zhsh-tier-failure-{}", std::process::id()));
        let _ = std::fs::remove_file(&home);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::write(&home, "not a directory").unwrap();
        shell
            .env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        shell.set_user_home_for_test(Some(home.clone()));
        shell.llm = Some(crate::llm::LlmConfig {
            name: "test".into(),
            url: "https://example.com".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: "secret".into(),
            models: crate::llm::ModelTiers {
                flash: "fast".into(),
                standard: "standard".into(),
                max: "max".into(),
            },
            tier: crate::llm::ModelTier::Flash,
        });

        let result = execute(
            &mut shell,
            &["tier".into(), "max".into()],
            None,
            None,
            None,
            None,
            None,
        );

        assert_eq!(result.code, 1);
        assert_eq!(
            shell.llm.as_ref().unwrap().tier,
            crate::llm::ModelTier::Flash
        );
        let _ = std::fs::remove_file(home);
    }

    #[test]
    fn status_shows_private_plaintext_transport_and_an_unset_token() {
        let mut shell = SessionState::test();
        shell.llm = Some(crate::llm::LlmConfig {
            name: "local".into(),
            url: "http://192.168.10.123:11434".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: String::new(),
            models: crate::llm::ModelTiers {
                flash: "local-model".into(),
                standard: "local-model".into(),
                max: "local-model".into(),
            },
            tier: crate::llm::ModelTier::Flash,
        });

        let result = execute(&mut shell, &["status".into()], None, None, None, None, None);

        assert_eq!(result.code, 0);
        assert!(result.stdout.contains("传输: HTTP（私网明文）\n"));
        assert!(result.stdout.contains("access-token: (unset)\n"));
    }
}
