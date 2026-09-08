//! 供应商中立的 LLM 配置、请求、响应和错误类型。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonSchemaMode {
    Off,
    On,
}

impl JsonSchemaMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// 用户请求的 JSON Schema 模式与当前 Codec 实际能力的一次性解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonSchemaResolution {
    Off,
    On,
    Downgraded,
}

impl JsonSchemaResolution {
    pub fn requested(self) -> JsonSchemaMode {
        match self {
            Self::Off => JsonSchemaMode::Off,
            Self::On | Self::Downgraded => JsonSchemaMode::On,
        }
    }

    pub fn is_degraded(self) -> bool {
        matches!(self, Self::Downgraded)
    }

    pub fn status(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Downgraded => "on（实际 off）",
        }
    }

    pub(crate) fn effective_profile(self) -> EncodeProfile {
        match self {
            Self::On => EncodeProfile::JsonSchema,
            Self::Off | Self::Downgraded => EncodeProfile::Default,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncodeProfile {
    Default,
    JsonSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTier {
    Flash,
    Standard,
    Max,
}

impl ModelTier {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "flash" => Some(Self::Flash),
            "standard" => Some(Self::Standard),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flash => "flash",
            Self::Standard => "standard",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTiers {
    pub flash: String,
    pub standard: String,
    pub max: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmConfig {
    pub name: String,
    /// 可包含路径前缀、但不含凭据、query 或 fragment 的规范 Base URL。
    pub url: String,
    /// 精确 Codec 标识，采用 `id@exact-semver`。
    pub request_format: String,
    /// 用户请求与当前 Codec 能力解析后的单一 JSON Schema 状态。
    pub json_schema: JsonSchemaResolution,
    pub access_token: String,
    pub models: ModelTiers,
    pub tier: ModelTier,
}

/// 可持久化但不保证能启动 Agent 的 LLM 配置文档。
///
/// 草稿保留用户当前填写的字段；只有 [`super::store`] 重新评估为完整后，才会转换成
/// [`LlmConfig`] 并进入 HTTP/Codec 运行时。`ACCESS_TOKEN` 允许为空，其必要性和有效性由
/// Provider 决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LlmProfileDraft {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) request_format: String,
    pub(crate) json_schema: String,
    pub(crate) access_token: String,
    pub(crate) flash: String,
    pub(crate) standard: String,
    pub(crate) max: String,
    pub(crate) tier: String,
}

impl LlmProfileDraft {
    pub(crate) fn empty(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: String::new(),
            request_format: String::new(),
            json_schema: JsonSchemaMode::Off.as_str().into(),
            access_token: String::new(),
            flash: String::new(),
            standard: String::new(),
            max: String::new(),
            tier: ModelTier::Flash.as_str().into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_config(config: &LlmConfig) -> Self {
        Self {
            name: config.name.clone(),
            url: config.url.clone(),
            request_format: config.request_format.clone(),
            json_schema: config.json_schema.requested().as_str().into(),
            access_token: config.access_token.clone(),
            flash: config.models.flash.clone(),
            standard: config.models.standard.clone(),
            max: config.models.max.clone(),
            tier: config.tier.as_str().into(),
        }
    }
}

/// 一个字段阻止草稿成为运行时配置的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LlmProfileIssue {
    pub(crate) field: &'static str,
    pub(crate) message: String,
}

/// 每次读取、保存或 Codec generation 变化后重新计算的配置状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LlmProfileReadiness {
    Ready(LlmConfig),
    Incomplete(Vec<LlmProfileIssue>),
}

/// 磁盘配置文档及其当前运行时完整性；不在文件中持久化完成标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LlmProfile {
    pub(crate) draft: LlmProfileDraft,
    pub(crate) readiness: LlmProfileReadiness,
}

impl LlmProfile {
    pub(crate) fn config(&self) -> Option<&LlmConfig> {
        match &self.readiness {
            LlmProfileReadiness::Ready(config) => Some(config),
            LlmProfileReadiness::Incomplete(_) => None,
        }
    }

    pub(crate) fn issues(&self) -> &[LlmProfileIssue] {
        match &self.readiness {
            LlmProfileReadiness::Ready(_) => &[],
            LlmProfileReadiness::Incomplete(issues) => issues,
        }
    }
}

impl LlmConfig {
    pub fn model(&self) -> &str {
        match self.tier {
            ModelTier::Flash => &self.models.flash,
            ModelTier::Standard => &self.models.standard,
            ModelTier::Max => &self.models.max,
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.model())
    }

    pub(crate) fn json_schema_downgrade_warning(&self) -> Option<String> {
        self.json_schema.is_degraded().then(|| {
            format!(
                "! Codec {} 未声明可验证的 JSON Schema 映射；已保留 JSON_SCHEMA=on，实际使用无 Schema 模式",
                self.request_format
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

impl LlmMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompletionRequest {
    pub system: String,
    pub messages: Vec<LlmMessage>,
    pub model: String,
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Completed,
    OutputLimit,
    ContentFiltered,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResponse {
    pub text: String,
    pub finish_reason: FinishReason,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    Permission,
    RateLimited,
    QuotaExhausted,
    InvalidRequest,
    ModelNotFound,
    ContextLimit,
    ContentRejected,
    ServiceUnavailable,
    ProviderInternal,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub provider_code: Option<String>,
    pub request_id: Option<String>,
    pub retryable_hint: bool,
    pub retry_after_ms: Option<u64>,
}
