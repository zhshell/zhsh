//! Codec 生命周期用例：检查、首次信任、安装、卸载、列表、导出与重载。

use crate::common::{AppError, AppResult, CancellationToken, PersistOutcome};
use crate::llm::{self, CodecRuntime, CodecTrustState, LlmConfig};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PluginInstallOutcome {
    Created,
    Replaced,
    Identical,
}

#[derive(Debug)]
pub(crate) struct CodecInstallRequest {
    pub(crate) source: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodecTrustView {
    Official,
    Trusted,
    Untrusted,
}

#[derive(Debug)]
pub(crate) struct CodecInspectionView {
    pub(crate) format: String,
    pub(crate) publisher: String,
    pub(crate) codec_sha256: String,
    pub(crate) key_fingerprint: String,
    pub(crate) trust: CodecTrustView,
    pub(crate) supports_json_schema: bool,
    pub(crate) query_secret_warning: bool,
    pub(crate) generation: u64,
}

#[derive(Debug)]
pub(crate) struct PublisherTrustPrompt {
    pub(crate) format: String,
    pub(crate) publisher: String,
    pub(crate) codec_sha256: String,
    pub(crate) key_fingerprint: String,
    pub(crate) query_secret_warning: bool,
}

#[derive(Debug)]
pub(crate) struct CodecUninstallPrompt {
    pub(crate) format: String,
    pub(crate) artifact_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublisherTrustDecision {
    Authorize,
    Decline,
    Cancelled,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodecUninstallDecision {
    Confirm,
    Decline,
    Cancelled,
    TimedOut,
    Unavailable,
}

pub(crate) trait CodecManagementUi {
    fn confirm_publisher(
        &self,
        prompt: &PublisherTrustPrompt,
        cancellation: &CancellationToken,
    ) -> PublisherTrustDecision;

    fn confirm_active_uninstall(
        &self,
        prompt: &CodecUninstallPrompt,
        cancellation: &CancellationToken,
    ) -> CodecUninstallDecision;
}

#[derive(Debug)]
pub(crate) struct CodecInstallReport {
    pub(crate) format: String,
    pub(crate) artifact_path: PathBuf,
    pub(crate) artifact_outcome: PluginInstallOutcome,
    pub(crate) publisher: &'static str,
    pub(crate) source: &'static str,
    pub(crate) trusted_key_path: Option<PathBuf>,
    pub(crate) trusted_key_outcome: Option<PluginInstallOutcome>,
    pub(crate) key_fingerprint: String,
    pub(crate) generation: u64,
    pub(crate) disk_revision: u64,
    pub(crate) active: ActiveLlmResolution,
}

#[derive(Debug)]
pub(crate) struct CodecUninstallReport {
    pub(crate) format: String,
    pub(crate) artifact_path: PathBuf,
    pub(crate) publisher: &'static str,
    pub(crate) key_fingerprint: String,
    pub(crate) generation: u64,
    pub(crate) disk_revision: u64,
    pub(crate) active: ActiveLlmResolution,
}

#[derive(Debug)]
pub(crate) enum ActiveLlmResolution {
    NotConfigured,
    Available(LlmConfig),
    Unavailable(String),
}

#[derive(Debug)]
pub(crate) struct CodecReloadReport {
    pub(crate) generation: u64,
    pub(crate) disk_revision: u64,
    pub(crate) active: ActiveLlmResolution,
    pub(crate) issues: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CodecListRow {
    pub(crate) format: String,
    pub(crate) publisher: &'static str,
    pub(crate) source: &'static str,
    pub(crate) supports_json_schema: bool,
    pub(crate) signer: String,
}

#[derive(Debug)]
pub(crate) struct CodecListReport {
    pub(crate) rows: Vec<CodecListRow>,
    pub(crate) generation: u64,
    pub(crate) disk_revision: u64,
    pub(crate) stale: bool,
}

#[derive(Debug)]
pub(crate) struct CodecExportView {
    pub(crate) format: String,
    pub(crate) destination: PathBuf,
    pub(crate) identical: bool,
}

pub(crate) struct CodecLifecycleService {
    user_home: Option<PathBuf>,
    codecs: Arc<CodecRuntime>,
}

impl CodecLifecycleService {
    pub(crate) fn new(user_home: Option<PathBuf>, codecs: Arc<CodecRuntime>) -> Self {
        Self { user_home, codecs }
    }

    fn home(&self) -> AppResult<&std::path::Path> {
        self.user_home
            .as_deref()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| AppError::input("用户 HOME 不可用，无法管理 Codec"))
    }

    pub(crate) fn inspect(&self, source: PathBuf) -> AppResult<CodecInspectionView> {
        let inspected = llm::inspect_codec(&self.codecs, &source)?;
        Ok(CodecInspectionView {
            format: inspected.format,
            publisher: inspected.publisher,
            codec_sha256: inspected.codec_sha256,
            key_fingerprint: inspected.key_fingerprint,
            trust: trust_view(inspected.trust),
            supports_json_schema: inspected.supports_json_schema,
            query_secret_warning: inspected.query_secret_warning,
            generation: self.codecs.generation_number(),
        })
    }

    pub(crate) fn install(
        &self,
        request: CodecInstallRequest,
        ui: Option<&dyn CodecManagementUi>,
        cancellation: &CancellationToken,
    ) -> AppResult<CodecInstallReport> {
        let home = self.home()?;
        let inspected = llm::inspect_codec(&self.codecs, &request.source)?;
        self.codecs.refresh_if_stale(cancellation)?;
        let trusted = inspected.is_currently_trusted(&self.codecs);
        let authorize = if trusted {
            false
        } else {
            let ui = ui.ok_or_else(|| AppError::input("当前终端不能确认未知 Codec 发布者"))?;
            let prompt = PublisherTrustPrompt {
                format: inspected.format.clone(),
                publisher: inspected.publisher.clone(),
                codec_sha256: inspected.codec_sha256.clone(),
                key_fingerprint: inspected.key_fingerprint.clone(),
                query_secret_warning: inspected.query_secret_warning,
            };
            match ui.confirm_publisher(&prompt, cancellation) {
                PublisherTrustDecision::Authorize => true,
                PublisherTrustDecision::Decline => {
                    return Err(AppError::input("用户未授权 Codec 发布者"))
                }
                PublisherTrustDecision::Cancelled => return Err(AppError::cancelled()),
                PublisherTrustDecision::Unavailable => {
                    return Err(AppError::input("非交互输入不能授权未知 Codec 发布者"))
                }
            }
        };
        let installed =
            llm::install_user_codec(&self.codecs, home, inspected, authorize, cancellation)?;
        Ok(CodecInstallReport {
            format: installed.format,
            artifact_path: installed.artifact.path,
            artifact_outcome: outcome(installed.artifact.outcome),
            publisher: if installed.official {
                "official"
            } else {
                "trusted"
            },
            source: installed.source.as_str(),
            trusted_key_path: installed
                .trusted_key
                .as_ref()
                .map(|receipt| receipt.path.clone()),
            trusted_key_outcome: installed
                .trusted_key
                .as_ref()
                .map(|receipt| outcome(receipt.outcome)),
            key_fingerprint: installed.key_fingerprint,
            generation: installed.generation,
            disk_revision: installed.disk_revision,
            active: self.active_resolution(),
        })
    }

    pub(crate) fn list(&self) -> AppResult<CodecListReport> {
        self.home()?;
        let rows = self
            .codecs
            .summaries()
            .into_iter()
            .map(|summary| CodecListRow {
                format: summary.label(),
                publisher: if summary.official {
                    "official"
                } else {
                    "trusted"
                },
                source: summary.source.as_str(),
                supports_json_schema: summary.supports_json_schema,
                signer: summary.key_fingerprint.chars().take(12).collect(),
            })
            .collect();
        Ok(CodecListReport {
            rows,
            generation: self.codecs.generation_number(),
            disk_revision: self.codecs.disk_revision(),
            stale: self.codecs.is_stale()?,
        })
    }

    pub(crate) fn uninstall(
        &self,
        format: &str,
        ui: Option<&dyn CodecManagementUi>,
        cancellation: &CancellationToken,
    ) -> AppResult<CodecUninstallReport> {
        llm::validate_format(format).map_err(AppError::input)?;
        let home = self.home()?;
        self.codecs.refresh_if_stale(cancellation)?;
        let active = llm::load_active_profile(home, &self.codecs)?
            .is_some_and(|profile| profile.draft.request_format.trim() == format);
        if active {
            let ui = ui.ok_or_else(|| AppError::input("非交互终端不能确认卸载活动 Codec"))?;
            let prompt = CodecUninstallPrompt {
                format: format.to_owned(),
                artifact_path: home
                    .join(".zhsh/plugins/llm")
                    .join(format!("{format}.zhcodec")),
            };
            match ui.confirm_active_uninstall(&prompt, cancellation) {
                CodecUninstallDecision::Confirm => {}
                CodecUninstallDecision::Decline => {
                    return Err(AppError::cancelled_with("用户取消卸载"))
                }
                CodecUninstallDecision::Cancelled => {
                    return Err(AppError::cancelled_with("用户中断卸载"))
                }
                CodecUninstallDecision::TimedOut => {
                    return Err(AppError::cancelled_with("卸载确认超时"))
                }
                CodecUninstallDecision::Unavailable => {
                    return Err(AppError::input("当前终端不能确认卸载活动 Codec"))
                }
            }
        }
        let removed = llm::uninstall_user_codec(&self.codecs, home, format, cancellation)?;
        Ok(CodecUninstallReport {
            format: removed.format,
            artifact_path: removed.artifact_path,
            publisher: if removed.official {
                "official"
            } else {
                "trusted"
            },
            key_fingerprint: removed.key_fingerprint,
            generation: removed.generation,
            disk_revision: removed.disk_revision,
            active: self.active_resolution(),
        })
    }

    pub(crate) fn reload(&self, cancellation: &CancellationToken) -> AppResult<CodecReloadReport> {
        self.home()?;
        let generation = self.codecs.reload(cancellation)?;
        Ok(CodecReloadReport {
            generation,
            disk_revision: self.codecs.disk_revision(),
            active: self.active_resolution(),
            issues: self.codecs.issue_messages(),
        })
    }

    pub(crate) fn export(
        &self,
        format: &str,
        output_dir: PathBuf,
        require_owned_output: bool,
        cancellation: &CancellationToken,
    ) -> AppResult<CodecExportView> {
        llm::validate_format(format).map_err(AppError::input)?;
        let exported = llm::export_user_codec(
            &self.codecs,
            self.home()?,
            format,
            &output_dir,
            require_owned_output,
            cancellation,
        )?;
        Ok(CodecExportView {
            format: exported.format,
            destination: exported.destination,
            identical: exported.identical,
        })
    }

    /// 依据当前 generation 重新解释活动配置，供 Shell 在成功或中途刷新后统一提交会话状态。
    pub(crate) fn active_resolution(&self) -> ActiveLlmResolution {
        let Some(home) = self.user_home.as_deref() else {
            return ActiveLlmResolution::Unavailable("用户 HOME 不可用".into());
        };
        match llm::load_active(home, &self.codecs) {
            Ok(Some(config)) => ActiveLlmResolution::Available(config),
            Ok(None) => ActiveLlmResolution::NotConfigured,
            Err(error) => ActiveLlmResolution::Unavailable(error.to_string()),
        }
    }
}

fn trust_view(value: CodecTrustState) -> CodecTrustView {
    match value {
        CodecTrustState::Official => CodecTrustView::Official,
        CodecTrustState::Trusted => CodecTrustView::Trusted,
        CodecTrustState::Untrusted => CodecTrustView::Untrusted,
    }
}

fn outcome(value: PersistOutcome) -> PluginInstallOutcome {
    match value {
        PersistOutcome::Created => PluginInstallOutcome::Created,
        PersistOutcome::Replaced => PluginInstallOutcome::Replaced,
        PersistOutcome::Identical => PluginInstallOutcome::Identical,
    }
}
