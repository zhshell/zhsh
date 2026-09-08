//! `zh` 的 LLM 配置命令适配器。
//!
//! 用法：`zh ls`、`zh use <配置名>`、`zh llm [-m|--modify] [配置名]`

use super::super::SessionState;
use super::BuiltinResult;
use crate::application::{
    LlmConfigAction, LlmConfigService, LlmConfigUi, LlmConfigUiError, SaveMode,
};
use crate::llm::{self, CodecRuntime, LlmConfig, LlmProfile, LlmProfileReadiness};
use std::path::Path;
use std::sync::Arc;

const LLM_HELP: &str = "LLM 配置：创建、暂存或修改 Provider、Codec 和模型档位配置。\n\n用法：\n  zh llm [<配置名>]\n  zh llm -m|--modify [<配置名>]\n\n选项：\n  -m, --modify  修改已有配置；省略配置名时默认选择当前配置\n\n表单中按 Esc 进入 Normal，方向键移动，i 恢复输入，:q 放弃，:wq 随时保存草稿。完整配置可在摘要后选择是否立即启用。\n详细说明：man zhsh\n";

fn argument_error(reason: impl std::fmt::Display) -> String {
    format!("zh llm: {reason}\n运行 `zh llm -h` 查看用法。\n")
}

enum ParsedLlmArgs {
    Help,
    Action { modify: bool, name: Option<String> },
}

fn parse_llm_args(args: &[String]) -> Result<ParsedLlmArgs, String> {
    let mut modify = false;
    let mut name = None;
    let mut parse_options = true;
    for argument in args {
        if parse_options && argument == "--" {
            parse_options = false;
        } else if parse_options && matches!(argument.as_str(), "-h" | "--help") {
            if args.len() == 1 {
                return Ok(ParsedLlmArgs::Help);
            }
            return Err(argument_error("help 选项不接受其他参数"));
        } else if parse_options && matches!(argument.as_str(), "-m" | "--modify") {
            if modify {
                return Err(argument_error("-m|--modify 只能指定一次"));
            }
            modify = true;
        } else if parse_options && argument.starts_with('-') {
            return Err(argument_error(format_args!("未知选项 {argument}")));
        } else if name.replace(argument.clone()).is_some() {
            return Err(argument_error("配置名只能指定一次"));
        }
    }
    Ok(ParsedLlmArgs::Action { modify, name })
}

fn service(
    session: &SessionState,
    codecs: Option<&Arc<CodecRuntime>>,
) -> Result<LlmConfigService, BuiltinResult> {
    let codecs = codecs
        .cloned()
        .ok_or_else(|| BuiltinResult::error("zh: Codec 运行时未注入\n"))?;
    Ok(LlmConfigService::new(
        session.user_home().map(Path::to_path_buf),
        codecs,
    ))
}

fn activated_result(stdout: String, config: &LlmConfig) -> BuiltinResult {
    let mut stderr = llm::plaintext_private_warning(&config.url, &config.access_token)
        .map(|warning| format!("{warning}\n"))
        .unwrap_or_default();
    if let Some(warning) = config.json_schema_downgrade_warning() {
        stderr.push_str(&warning);
        stderr.push('\n');
    }
    BuiltinResult {
        stdout,
        stderr,
        code: 0,
    }
}

/// 实现 `zh ls`，按名称列出持久化 LLM 配置。
pub(crate) fn list(session: &SessionState, codecs: Option<&Arc<CodecRuntime>>) -> BuiltinResult {
    let service = match service(session, codecs) {
        Ok(service) => service,
        Err(error) => return error,
    };
    match service.list() {
        Ok(names) if names.is_empty() => BuiltinResult::stdout(String::new()),
        Ok(names) => BuiltinResult::stdout(format!("{}\n", names.join("\n"))),
        Err(error) => BuiltinResult::error(format!("zh: {error}\n")),
    }
}

/// 实现 `zh use`，在持久化活动标记成功后切换当前会话。
pub(crate) fn use_config(
    session: &mut SessionState,
    name: &str,
    codecs: Option<&Arc<CodecRuntime>>,
    ui: Option<&dyn LlmConfigUi>,
) -> BuiltinResult {
    let service = match service(session, codecs) {
        Ok(service) => service,
        Err(error) => return error,
    };
    let profile = match service.profile(name) {
        Ok(profile) => profile,
        Err(error) => return BuiltinResult::error(format!("zh: {error}\n")),
    };
    if let LlmProfileReadiness::Incomplete(issues) = &profile.readiness {
        let Some(ui) = ui else {
            return BuiltinResult::error(format!(
                "zh: 配置 {name} 不完整，当前入口不能启动交互修复\n"
            ));
        };
        let repair = match ui.confirm_repair(name, issues) {
            Ok(repair) => repair,
            Err(LlmConfigUiError::Cancelled) => return BuiltinResult::stdout("\n- 已取消\n"),
            Err(LlmConfigUiError::Terminal(error)) => {
                return BuiltinResult::error(format!("× 配置修复失败: {error}\n"));
            }
        };
        if repair {
            let records = match service.inventory() {
                Ok(records) => records,
                Err(error) => return BuiltinResult::error(format!("× 配置修复失败: {error}\n")),
            };
            let plugins = match service.plugins() {
                Ok(plugins) => plugins,
                Err(error) => return BuiltinResult::error(format!("× 配置修复失败: {error}\n")),
            };
            let action = LlmConfigAction::Modify {
                name: Some(name.to_string()),
                current_name: session.active_llm_name().map(str::to_string),
            };
            let decision = match ui.collect(&action, &records, &plugins) {
                Ok(decision) => decision,
                Err(LlmConfigUiError::Cancelled) => {
                    return BuiltinResult::stdout("\n- 已取消\n");
                }
                Err(LlmConfigUiError::Terminal(error)) => {
                    return BuiltinResult::error(format!("× 配置修复失败: {error}\n"));
                }
            };
            let repaired = match service.save(decision.draft, SaveMode::Save) {
                Ok(profile) => profile,
                Err(error) => return BuiltinResult::error(format!("× 配置修复失败: {error}\n")),
            };
            return select_profile(session, &service, repaired);
        }
    }
    select_profile(session, &service, profile)
}

fn select_profile(
    session: &mut SessionState,
    service: &LlmConfigService,
    profile: LlmProfile,
) -> BuiltinResult {
    let name = profile.draft.name.clone();
    if let Err(error) = service.select(&name) {
        return BuiltinResult::error(format!("zh: {error}\n"));
    }
    match profile.readiness {
        LlmProfileReadiness::Ready(config) => {
            let result = activated_result(format!("配置: {name}\n"), &config);
            session.commit_ready_llm(config);
            result
        }
        LlmProfileReadiness::Incomplete(issues) => {
            let reason = format!("配置 {name} 不完整: {}", issue_summary(&issues));
            session.commit_unavailable_llm(Some(name.clone()), reason);
            BuiltinResult {
                stdout: format!("配置: {name}\nAgent: unavailable（配置不完整）\n"),
                stderr: format!("! 配置不完整: {}\n", issue_summary(&issues)),
                code: 0,
            }
        }
    }
}

fn issue_summary(issues: &[crate::llm::LlmProfileIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("{}: {}", issue.field, issue.message))
        .collect::<Vec<_>>()
        .join("；")
}

/// 实现 `zh llm`，组合清单、交互向导和保存/启用应用服务。
///
/// 取消是成功的用户决策；终端、读取或持久化失败返回非零结果。
pub(crate) fn interactive(
    session: &mut SessionState,
    args: &[String],
    codecs: Option<&Arc<CodecRuntime>>,
    ui: Option<&dyn LlmConfigUi>,
) -> BuiltinResult {
    let action = match parse_llm_args(args) {
        Ok(ParsedLlmArgs::Help) => return BuiltinResult::stdout(LLM_HELP),
        Ok(ParsedLlmArgs::Action { modify, name }) if modify => LlmConfigAction::Modify {
            name,
            current_name: session.active_llm_name().map(str::to_string),
        },
        Ok(ParsedLlmArgs::Action { name, .. }) => LlmConfigAction::Create { name },
        Err(error) => return BuiltinResult::error(error),
    };
    let Some(ui) = ui else {
        return BuiltinResult::error("zh: 当前入口不支持交互式 LLM 配置\n");
    };
    let service = match service(session, codecs) {
        Ok(service) => service,
        Err(error) => return error,
    };
    let records = match service.inventory() {
        Ok(records) => records,
        Err(error) => return BuiltinResult::error(format!("× 配置向导失败: {error}\n")),
    };
    let plugins = match service.plugins() {
        Ok(plugins) => plugins,
        Err(error) => return BuiltinResult::error(format!("× 插件发现失败: {error}\n")),
    };
    let decision = match ui.collect(&action, &records, &plugins) {
        Ok(decision) => decision,
        Err(LlmConfigUiError::Cancelled) => return BuiltinResult::stdout("\n- 已取消\n"),
        Err(LlmConfigUiError::Terminal(error)) => {
            return BuiltinResult::error(format!("× 配置向导失败: {error}\n"))
        }
    };
    let name = decision.draft.name.clone();
    let mode = decision.mode;
    let was_current = session.active_llm_name() == Some(name.as_str());
    match service.save(decision.draft, mode) {
        Ok(profile) => match profile.readiness {
            LlmProfileReadiness::Ready(config) => {
                let activated = matches!(mode, SaveMode::SaveAndActivate);
                let schema_warning = config.json_schema_downgrade_warning();
                if activated || was_current {
                    session.commit_ready_llm(config.clone());
                }
                let stdout = if activated {
                    format!("✓ 配置已保存并启用: {name}\n")
                } else {
                    format!("✓ 配置已保存: {name}\n")
                };
                BuiltinResult {
                    stdout,
                    stderr: schema_warning
                        .map(|warning| format!("{warning}\n"))
                        .unwrap_or_default(),
                    code: 0,
                }
            }
            LlmProfileReadiness::Incomplete(issues) => {
                if was_current {
                    session.commit_unavailable_llm(
                        Some(name.clone()),
                        format!("配置 {name} 不完整: {}", issue_summary(&issues)),
                    );
                }
                BuiltinResult {
                    stdout: format!("✓ 配置草稿已保存: {name}\n"),
                    stderr: format!(
                        "! 配置不完整，尚不能用于 Agent: {}\n",
                        issue_summary(&issues)
                    ),
                    code: 0,
                }
            }
        },
        Err(error) => BuiltinResult::error(format!("× 配置向导失败: {error}\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ModelTier, ModelTiers};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_create_and_modify_forms_without_guessing() {
        assert!(matches!(
            parse_llm_args(&[]).unwrap(),
            ParsedLlmArgs::Action {
                modify: false,
                name: None
            }
        ));
        assert!(matches!(
            parse_llm_args(&args(&["deepseek"])).unwrap(),
            ParsedLlmArgs::Action {
                modify: false,
                name: Some(name)
            } if name == "deepseek"
        ));
        assert!(matches!(
            parse_llm_args(&args(&["-m", "deepseek"])).unwrap(),
            ParsedLlmArgs::Action {
                modify: true,
                name: Some(name)
            } if name == "deepseek"
        ));
        assert!(matches!(
            parse_llm_args(&args(&["--", "-m"])).unwrap(),
            ParsedLlmArgs::Action {
                modify: false,
                name: Some(name)
            } if name == "-m"
        ));
    }

    #[test]
    fn private_http_activation_warns_without_claiming_an_empty_token_is_sent() {
        let config = LlmConfig {
            name: "local".into(),
            url: "http://192.168.10.123:11434".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: String::new(),
            models: ModelTiers {
                flash: "local".into(),
                standard: "local".into(),
                max: "local".into(),
            },
            tier: ModelTier::Flash,
        };

        let result = activated_result("配置: local\n".into(), &config);

        assert!(result.stderr.contains("私网 HTTP 为明文传输"));
        assert!(!result.stderr.contains("access-token 也将明文发送"));
    }

    #[test]
    fn schema_downgrade_warning_is_the_last_activation_diagnostic() {
        let mut config = LlmConfig {
            name: "custom".into(),
            url: "https://example.com".into(),
            request_format: "com.example.codec@1.0.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Downgraded,
            access_token: String::new(),
            models: ModelTiers {
                flash: "model".into(),
                standard: "model".into(),
                max: "model".into(),
            },
            tier: ModelTier::Flash,
        };

        let result = activated_result("配置: custom\n".into(), &config);
        assert!(result.stderr.ends_with("实际使用无 Schema 模式\n"));
        config.json_schema = crate::llm::JsonSchemaResolution::Off;
        assert!(activated_result(String::new(), &config).stderr.is_empty());
    }

    #[test]
    fn rejects_ambiguous_or_unknown_arguments() {
        for values in [
            &["-m", "--modify"][..],
            &["first", "second"],
            &["--unknown"],
            &["--help", "extra"],
        ] {
            assert!(
                parse_llm_args(&args(values)).is_err(),
                "accepted {values:?}"
            );
        }
    }
}
