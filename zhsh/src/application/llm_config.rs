//! LLM 配置用例：读取目录、保存配置和切换当前配置。
//!
//! 服务只编排配置存储并返回可提交候选，不依赖 Shell 会话或终端。交互界面通过
//! [`LlmConfigUi`] 端口由 REPL 注入，调用方只在用例成功后提交内存状态。

use crate::common::{AppError, AppResult};
use crate::llm::{
    self, CodecRuntime, LlmConfig, LlmProfile, LlmProfileDraft, LlmProfileIssue,
    LlmProfileReadiness, ModelTier, PluginSummary,
};
use std::path::PathBuf;
use std::sync::Arc;

/// 配置目录中的一个名称及其独立加载结果。
///
/// 单个损坏文件不会阻止向导列出并选择其他配置。
pub(crate) struct ConfigRecord {
    /// 从文件名获得、已经通过名称校验的配置名。
    pub(crate) name: String,
    /// 可修复配置草稿及其当前完整性，或只属于该记录的结构/存储错误。
    pub(crate) profile: AppResult<LlmProfile>,
}

/// 一次 `zh llm` 调用已经由命令行确定的操作意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LlmConfigAction {
    /// 创建新配置；名称缺失时由向导询问。
    Create { name: Option<String> },
    /// 修改已有配置；名称缺失时由向导询问，并以当前配置作为默认值。
    Modify {
        name: Option<String>,
        current_name: Option<String>,
    },
}

/// 配置向导结束后允许的持久化方式。
#[derive(Clone, Copy)]
pub(crate) enum SaveMode {
    /// 只保存 `.llm` 文件，不改变活动标记或当前会话。
    Save,
    /// 保存配置、更新活动标记，然后提交当前会话配置。
    SaveAndActivate,
}

/// LLM 配置界面收集完成后的纯数据决策。
pub(crate) struct LlmConfigDecision {
    /// 用户当前编辑的配置文档；`:wq` 允许它仍不完整。
    pub(crate) draft: LlmProfileDraft,
    /// 用户选择的保存方式。
    pub(crate) mode: SaveMode,
}

/// 配置界面可能返回的与具体终端库无关的错误。
pub(crate) enum LlmConfigUiError {
    /// 用户主动取消配置。
    Cancelled,
    /// 终端输入或显示失败。
    Terminal(String),
}

/// Shell 调用、REPL 实现的 LLM 配置界面端口。
pub(crate) trait LlmConfigUi {
    /// 在命令行已经确定的创建/修改边界内，根据持久化清单收集一份保存决策。
    fn collect(
        &self,
        action: &LlmConfigAction,
        records: &[ConfigRecord],
        plugins: &[PluginSummary],
    ) -> Result<LlmConfigDecision, LlmConfigUiError>;

    /// 对 `zh use` 选中的不完整配置询问是否立即进入修复编辑器。
    fn confirm_repair(
        &self,
        name: &str,
        issues: &[LlmProfileIssue],
    ) -> Result<bool, LlmConfigUiError>;
}

/// 基于一个固定 HOME 目录执行 LLM 配置用例的应用服务。
pub(crate) struct LlmConfigService {
    home: Option<PathBuf>,
    codecs: Arc<CodecRuntime>,
}

impl LlmConfigService {
    /// 从已由调用方捕获的 HOME 创建一次用例服务。
    pub(crate) fn new(home: Option<PathBuf>, codecs: Arc<CodecRuntime>) -> Self {
        Self { home, codecs }
    }

    fn home(&self) -> AppResult<&std::path::Path> {
        let home = self
            .home
            .as_deref()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| AppError::input("用户状态不可用：HOME 必须是绝对路径"))?;
        Ok(home)
    }

    /// 返回按名称排序的配置名。
    ///
    /// # Errors
    ///
    /// 配置目录无法安全读取时返回 store 错误。
    pub(crate) fn list(&self) -> AppResult<Vec<String>> {
        llm::list_configs(self.home()?)
    }

    /// 读取配置清单，同时保留每个文件各自的加载结果。
    ///
    /// # Errors
    ///
    /// 只有目录枚举失败会使整个调用失败；单个损坏配置保存在 [`ConfigRecord::profile`]。
    pub(crate) fn inventory(&self) -> AppResult<Vec<ConfigRecord>> {
        let home = self.home()?;
        Ok(self
            .list()?
            .into_iter()
            .map(|name| ConfigRecord {
                profile: llm::load_config_profile(home, &name, &self.codecs),
                name,
            })
            .collect())
    }

    /// 返回已通过发布签名、包 schema 和目录策略验证的 Codec 清单。
    pub(crate) fn plugins(&self) -> AppResult<Vec<PluginSummary>> {
        self.home()?;
        Ok(self.codecs.summaries())
    }

    /// 读取一个可修复配置文档并按当前 Codec generation 重新计算完整性。
    pub(crate) fn profile(&self, name: &str) -> AppResult<LlmProfile> {
        llm::load_config_profile(self.home()?, name, &self.codecs)
    }

    /// 只更新活动标记；调用方负责把 Ready/Incomplete 状态提交到当前会话。
    pub(crate) fn select(&self, name: &str) -> AppResult<()> {
        llm::set_active(self.home()?, name)
    }

    /// 保存向导生成的配置文档，并按模式决定是否启用。
    ///
    /// # Arguments
    ///
    /// - `draft`：待保存的完整或不完整配置草稿。
    /// - `mode`：只保存，或保存并启用。
    ///
    /// # Errors
    ///
    /// 配置文件写入失败时不执行后续步骤。配置文件写入成功而活动标记失败时，文件会保留，
    /// 但不返回可提交候选；这是可恢复的部分提交，而不是内存半更新。
    pub(crate) fn save(&self, draft: LlmProfileDraft, mode: SaveMode) -> AppResult<LlmProfile> {
        let home = self.home()?;
        let profile = llm::assess_profile(draft, &self.codecs);
        if matches!(mode, SaveMode::SaveAndActivate)
            && matches!(profile.readiness, LlmProfileReadiness::Incomplete(_))
        {
            return Err(AppError::input("配置不完整，不能保存并启用"));
        }
        llm::save_config_profile(home, &profile.draft)?;
        if matches!(mode, SaveMode::SaveAndActivate) {
            llm::set_active(home, &profile.draft.name)?;
        }
        Ok(profile)
    }

    /// 持久化修改当前配置的模型档位。
    ///
    /// # Errors
    ///
    /// 当前配置缺失由调用方拒绝；配置文件写入失败时不返回可提交候选。
    pub(crate) fn set_tier(
        &self,
        current: Option<&LlmConfig>,
        tier: ModelTier,
    ) -> AppResult<LlmConfig> {
        let mut candidate = current
            .cloned()
            .ok_or_else(|| AppError::input("未配置 LLM；运行 zh llm"))?;
        candidate.tier = tier;
        llm::save_config(self.home()?, &candidate, &self.codecs)?;
        Ok(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ModelTier, ModelTiers};

    #[test]
    fn unavailable_home_rejects_persistent_operations() {
        let service = LlmConfigService::new(None, Arc::new(CodecRuntime::load(None)));
        assert!(service.list().is_err());
        assert!(service.plugins().is_err());
        assert!(service.profile("missing").is_err());
    }

    #[test]
    fn failed_activation_does_not_return_a_commit_candidate() {
        let home = std::env::temp_dir().join(format!(
            "zhsh-service-transaction-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        crate::llm::install_test_openai_codec(&home);
        let codecs = Arc::new(CodecRuntime::load(Some(&home)));
        let service = LlmConfigService::new(Some(home.clone()), codecs);
        let config = LlmConfig {
            name: "candidate".into(),
            url: "https://example.com".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: "token".into(),
            models: ModelTiers {
                flash: "fast".into(),
                standard: "standard".into(),
                max: "max".into(),
            },
            tier: ModelTier::Flash,
        };
        service
            .save(LlmProfileDraft::from_config(&config), SaveMode::Save)
            .unwrap();
        std::fs::create_dir(home.join(".zhsh/active-llm")).unwrap();

        assert!(service.select("candidate").is_err());
        let _ = std::fs::remove_dir_all(home);
    }
}
