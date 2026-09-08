//! Safety 规则管理命令消费的端口和只读展示类型。

use super::{AgentCommandPlan, AgentTrust};
use crate::common::CancellationToken;
use std::path::PathBuf;
use std::time::SystemTime;

/// Safety 规则来源；除二进制内置语义外，磁盘规则统一属于 local。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafetyRuleSourceView {
    Builtin,
    Local,
}

impl SafetyRuleSourceView {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Local => "local",
        }
    }
}

/// 当前快照目录中的规则状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafetyRuleStatusView {
    Active,
    Loaded,
    Shadowed,
}

impl SafetyRuleStatusView {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Loaded => "loaded",
            Self::Shadowed => "shadowed",
        }
    }
}

/// `zh safety` 表格中的一个“规则集 × program”条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyRuleRowView {
    /// 文件加载序号；builtin 固定为 0。
    pub(crate) order: usize,
    pub(crate) source: SafetyRuleSourceView,
    pub(crate) name: String,
    pub(crate) program: String,
    pub(crate) rule_count: Option<usize>,
    pub(crate) builtin: bool,
    pub(crate) status: SafetyRuleStatusView,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) location_or_diagnostic: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyCatalogView {
    pub(crate) generation: u64,
    pub(crate) rows: Vec<SafetyRuleRowView>,
}

/// `zh safety assess` 中的一个实际执行目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyTargetView {
    pub(crate) program: String,
    pub(crate) target: Option<PathBuf>,
}

/// `zh safety assess` 的纯诊断结果；只描述当前快照，不执行命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyAssessmentView {
    pub(crate) targets: Vec<SafetyTargetView>,
    pub(crate) semantic: &'static str,
    pub(crate) rule: String,
    pub(crate) binding: &'static str,
    pub(crate) decision: &'static str,
    pub(crate) reason: String,
}

/// `-t` 或 reload 的结构化结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyOperationReport {
    pub(crate) success: bool,
    pub(crate) generation: u64,
    pub(crate) local_rules: usize,
    pub(crate) shadowed_builtin_programs: usize,
    pub(crate) warnings: Vec<String>,
    pub(crate) errors: Vec<String>,
    pub(crate) source: Option<SafetySourceValidationView>,
}

/// 带 SOURCE 的 `zh safety -t` 所展示的纯完整性信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetySourceValidationView {
    pub(crate) kind: String,
    pub(crate) version: String,
    pub(crate) rules: usize,
    pub(crate) programs: usize,
    pub(crate) sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafetyInstallStateView {
    New,
    Unchanged,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyInstallEntryView {
    pub(crate) name: String,
    pub(crate) programs: Vec<String>,
    pub(crate) state: SafetyInstallStateView,
    pub(crate) destination: PathBuf,
}

/// 安装预检结果；`plan_id` 绑定源字节和当前目标状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyInstallPlan {
    pub(crate) success: bool,
    pub(crate) plan_id: Option<String>,
    pub(crate) generation: u64,
    pub(crate) entries: Vec<SafetyInstallEntryView>,
    pub(crate) warnings: Vec<String>,
    pub(crate) errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyInstallRequest {
    pub(crate) sources: Vec<PathBuf>,
    pub(crate) overwrite: bool,
    pub(crate) expected_plan_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafetyInstallOutcomeView {
    Created,
    Replaced,
    Identical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyInstalledEntryView {
    pub(crate) name: String,
    pub(crate) outcome: SafetyInstallOutcomeView,
    pub(crate) destination: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafetyInstallReport {
    pub(crate) success: bool,
    pub(crate) entries: Vec<SafetyInstalledEntryView>,
    pub(crate) generation: u64,
    pub(crate) candidate_reloadable: bool,
    pub(crate) warnings: Vec<String>,
    pub(crate) errors: Vec<String>,
}

pub(crate) struct SafetyOverwritePrompt {
    pub(crate) new_rules: usize,
    pub(crate) conflicts: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafetyOverwriteDecision {
    Confirm,
    Decline,
    Cancelled,
    TimedOut,
    Unavailable,
}

/// 只有 REPL 实现的覆盖确认端口。
pub(crate) trait SafetyManagementUi: Send + Sync {
    fn confirm_overwrite(
        &self,
        prompt: &SafetyOverwritePrompt,
        cancellation: &CancellationToken,
    ) -> SafetyOverwriteDecision;
}

/// 由 Agent SafetyRuntime 实现、供 `zh safety` 使用的管理端口。
pub(crate) trait SafetyManagementPort: Send + Sync {
    fn catalog(&self) -> SafetyCatalogView;
    fn assess(&self, plan: AgentCommandPlan, trust: AgentTrust) -> SafetyAssessmentView;
    fn test_candidate(&self) -> SafetyOperationReport;
    fn test_source(&self, source: PathBuf) -> SafetyOperationReport;
    fn reload(&self) -> SafetyOperationReport;
    fn plan_install(&self, sources: Vec<PathBuf>) -> SafetyInstallPlan;
    fn install(&self, request: SafetyInstallRequest) -> SafetyInstallReport;
}
