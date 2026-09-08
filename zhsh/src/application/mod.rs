//! 应用用例层。
//!
//! 本层组合领域校验和持久化顺序，并通过端口描述交互需求；不读取终端、不持有 Shell
//! 会话，也不实现文件格式或 HTTP。成功用例返回候选值，由 Shell 决定何时提交内存。

mod codec_plugin;
mod llm_config;

pub(crate) use codec_plugin::{
    ActiveLlmResolution, CodecInstallRequest, CodecLifecycleService, CodecManagementUi,
    CodecTrustView, CodecUninstallDecision, CodecUninstallPrompt, PluginInstallOutcome,
    PublisherTrustDecision, PublisherTrustPrompt,
};

pub(crate) use llm_config::{
    ConfigRecord, LlmConfigAction, LlmConfigDecision, LlmConfigService, LlmConfigUi,
    LlmConfigUiError, SaveMode,
};
