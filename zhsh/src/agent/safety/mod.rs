//! Agent 生成命令的宿主侧风险分类。
//!
//! 本模块只为“是否要求用户确认”提供启发式副作用判断，不声称完整解析 Bash 或证明
//! 命令安全；用户直接输入的命令不经过本模块。

mod builtin;
mod external;
mod runtime;

use crate::shell::{
    AgentCommandPlan, AgentExecutionTarget, AgentTrust, BoundCommand, BoundExpression,
    BoundRedirection, BoundTarget, CommandTargetKind, ExecutableBinding, OutputMode,
    ResolvedInvocation,
};
use builtin::LinuxCoreAnalyzer;
use external::ExternalAnalyzer;
use runtime::SafetySnapshot;
use std::path::Path;
use std::sync::Arc;

pub(super) use runtime::SafetyRuntime;

/// 宿主、Linux 核心分析器和外部插件共享的安全等级。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum SafetyLevel {
    /// 高置信度地仅观察状态；不构成安全证明。
    ReadOnly,
    /// 无法可靠判断真实副作用。
    Unknown,
    /// 会创建或修改持久状态。
    StateChanging,
    /// 会删除、覆盖、截断或移动现有状态。
    Destructive,
    /// 当前 Agent 执行器不能监督或不允许执行。
    Unsupported,
}

impl SafetyLevel {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Unknown => "unknown",
            Self::StateChanging => "state_changing",
            Self::Destructive => "destructive",
            Self::Unsupported => "unsupported",
        }
    }
}

/// 命令输出可能向当前 LLM Provider 披露的数据范围。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum DisclosureClass {
    None,
    Metadata,
    WorkspaceContent,
    OperationalContent,
    SensitiveOrUnbounded,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum BindingClass {
    #[serde(alias = "static_trusted")]
    StaticSystemTrusted,
    StaticUserBound,
    StaticUntrusted,
    Dynamic,
}

impl BindingClass {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::StaticSystemTrusted => "static_system_trusted",
            Self::StaticUserBound => "static_user_bound",
            Self::StaticUntrusted => "static_untrusted",
            Self::Dynamic => "dynamic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SemanticSource {
    Builtin,
    LocalExplicit,
    LocalDefault,
    Unknown,
}

impl SemanticSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::LocalExplicit => "local_explicit",
            Self::LocalDefault => "local_default",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum StateScope {
    None,
    TaskRoot,
    OutsideOrUnknown,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub(super) enum SupervisionClass {
    FiniteForeground,
    PotentiallyUnbounded,
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SafetyDecision {
    AutoExecute,
    Confirm,
    Reject,
}

impl SafetyDecision {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::AutoExecute => "auto_execute",
            Self::Confirm => "confirm",
            Self::Reject => "reject",
        }
    }
}

/// 将授信快照与统一评估合并为自动、确认或拒绝三态决策。
pub(super) fn decide(trust: AgentTrust, assessment: &SafetyAssessment) -> SafetyDecision {
    if assessment.level == SafetyLevel::Unsupported
        || assessment.supervision != SupervisionClass::FiniteForeground
    {
        return SafetyDecision::Reject;
    }
    if trust == AgentTrust::Confirm
        || assessment.mandatory_confirmation
        || assessment.privilege_change
        || assessment.network
        || matches!(
            assessment.binding,
            BindingClass::StaticUntrusted | BindingClass::Dynamic
        )
        || assessment.disclosure >= DisclosureClass::OperationalContent
    {
        return SafetyDecision::Confirm;
    }
    let automatic = match trust {
        AgentTrust::Balanced => {
            assessment.level == SafetyLevel::ReadOnly
                && assessment.state_scope == StateScope::None
                && !assessment.session_mutation
                && !assessment.file_output
        }
        AgentTrust::Trusted => {
            matches!(
                (assessment.level, assessment.state_scope),
                (SafetyLevel::ReadOnly, StateScope::None)
                    | (SafetyLevel::StateChanging, StateScope::TaskRoot)
            ) && !assessment.session_mutation
        }
        AgentTrust::Confirm => false,
    };
    if automatic {
        SafetyDecision::AutoExecute
    } else {
        SafetyDecision::Confirm
    }
}

#[cfg(test)]
pub(super) fn requires_confirmation(trust: AgentTrust, assessment: &SafetyAssessment) -> bool {
    decide(trust, assessment) != SafetyDecision::AutoExecute
}

/// 一条命令的风险结论及触发原因。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SafetyAssessment {
    /// 风险等级。
    pub(super) level: SafetyLevel,
    /// 命令签名本身的副作用等级；执行目标身份只影响 `level` 和 `binding`。
    pub(super) semantic_level: SafetyLevel,
    /// 最终程序语义来自何处；只参与用户管理目标的自动执行门禁。
    pub(super) semantic_source: SemanticSource,
    pub(super) semantic_rule: Option<String>,
    /// 反馈给 Provider 时可能披露的数据范围。
    pub(super) disclosure: DisclosureClass,
    pub(super) binding: BindingClass,
    pub(super) state_scope: StateScope,
    pub(super) supervision: SupervisionClass,
    pub(super) network: bool,
    /// 是否包含提权或切换身份操作；所有策略均强制确认。
    pub(super) privilege_change: bool,
    /// 无论信任策略为何都必须确认，例如目标身份不明或动态 Bash 解析。
    pub(super) mandatory_confirmation: bool,
    pub(super) session_mutation: bool,
    pub(super) file_output: bool,
    /// 受限原因码；终端只按固定优先级生成简短摘要。
    pub(super) reasons: Vec<String>,
}

impl SafetyAssessment {
    fn neutral() -> Self {
        Self {
            level: SafetyLevel::ReadOnly,
            semantic_level: SafetyLevel::ReadOnly,
            disclosure: DisclosureClass::None,
            semantic_source: SemanticSource::Unknown,
            semantic_rule: None,
            binding: BindingClass::StaticSystemTrusted,
            state_scope: StateScope::None,
            supervision: SupervisionClass::FiniteForeground,
            network: false,
            privilege_change: false,
            mandatory_confirmation: false,
            session_mutation: false,
            file_output: false,
            reasons: Vec::new(),
        }
    }

    fn read_only() -> Self {
        Self::new(SafetyLevel::ReadOnly, "read_only")
    }

    pub(super) fn new(level: SafetyLevel, reason: impl Into<String>) -> Self {
        let mut assessment = Self::neutral();
        assessment.level = level;
        assessment.semantic_level = level;
        assessment.push_reason(reason);
        assessment
    }

    pub(super) fn privilege(summary: impl Into<String>) -> Self {
        Self {
            level: SafetyLevel::Unknown,
            semantic_level: SafetyLevel::Unknown,
            disclosure: DisclosureClass::None,
            semantic_source: SemanticSource::Unknown,
            semantic_rule: None,
            binding: BindingClass::StaticSystemTrusted,
            state_scope: StateScope::None,
            supervision: SupervisionClass::FiniteForeground,
            network: false,
            privilege_change: true,
            mandatory_confirmation: true,
            session_mutation: false,
            file_output: false,
            reasons: vec![summary.into()],
        }
    }

    fn force_confirmation(mut self, reason: impl Into<String>) -> Self {
        self.mandatory_confirmation = true;
        self.push_reason(reason);
        self
    }

    fn with_disclosure(mut self, disclosure: DisclosureClass, reason: impl Into<String>) -> Self {
        self.disclosure = self.disclosure.max(disclosure);
        self.push_reason(reason);
        self
    }

    fn with_file_output(mut self) -> Self {
        self.file_output = true;
        self
    }

    fn merge(mut self, other: Self) -> Self {
        if other.semantic_source != SemanticSource::Unknown
            && (self.semantic_source == SemanticSource::Unknown
                || other.semantic_level >= self.semantic_level)
        {
            self.semantic_source = other.semantic_source;
            self.semantic_rule.clone_from(&other.semantic_rule);
        }
        self.level = self.level.max(other.level);
        self.semantic_level = self.semantic_level.max(other.semantic_level);
        self.disclosure = self.disclosure.max(other.disclosure);
        self.binding = self.binding.max(other.binding);
        self.state_scope = self.state_scope.max(other.state_scope);
        self.supervision = self.supervision.max(other.supervision);
        self.network |= other.network;
        self.privilege_change |= other.privilege_change;
        self.mandatory_confirmation |= other.mandatory_confirmation;
        self.session_mutation |= other.session_mutation;
        self.file_output |= other.file_output;
        for reason in other.reasons {
            self.push_reason(reason);
        }
        self
    }

    fn with_semantic_source(mut self, source: SemanticSource, rule: Option<String>) -> Self {
        self.semantic_source = source;
        self.semantic_rule = rule;
        self
    }

    fn push_reason(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if !reason.is_empty() && !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }

    pub(super) fn primary_reason(&self) -> &'static str {
        if self.privilege_change {
            "包含提权或身份切换"
        } else if self.supervision == SupervisionClass::Detached {
            "不支持脱离监督的后台执行"
        } else if self.supervision == SupervisionClass::PotentiallyUnbounded {
            "命令没有静态终止条件"
        } else if self.level == SafetyLevel::Unsupported {
            "不支持该执行形态"
        } else if self.level == SafetyLevel::Destructive {
            "将删除、覆盖或移动现有状态"
        } else if self.network {
            "可能访问网络"
        } else if self.disclosure >= DisclosureClass::OperationalContent {
            "可能读取敏感或任务范围外数据"
        } else if self.binding == BindingClass::Dynamic {
            "无法验证实际执行目标"
        } else if self.binding == BindingClass::StaticUntrusted {
            "执行目标不满足系统信任条件"
        } else if self.binding == BindingClass::StaticUserBound
            && self.semantic_source != SemanticSource::LocalExplicit
        {
            "用户管理目标缺少 local 显式只读规则"
        } else if self.level == SafetyLevel::Unknown {
            "无法静态验证命令行为"
        } else if self.level == SafetyLevel::StateChanging || self.file_output {
            "可能修改文件或系统状态"
        } else {
            "当前策略要求确认"
        }
    }
}

/// 已绑定命令提供给内置与外部分析器的统一输入。
#[derive(Debug)]
pub(super) struct SafetyCommand<'a> {
    pub(super) program: &'a str,
    pub(super) arguments: &'a [String],
    pub(super) nesting_depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RedirectionSummary {
    InputFile { fd: u32, path: String },
    OutputFile { fd: u32, path: String, mode: String },
    Duplicate { from: u32, to: u32 },
    Close { fd: u32 },
    Null { fd: u32 },
    StandardStream { fd: u32, target: String },
}

pub(super) enum AnalyzerDecision {
    Assessed(SafetyAssessment),
    Abstain,
}

/// Linux 核心规则和声明式 JSON 规则共享的唯一分析器接口。
pub(super) trait CommandSafetyAnalyzer: Send + Sync {
    fn analyze(&self, command: &SafetyCommand<'_>) -> Result<AnalyzerDecision, String>;
}

/// 汇总核心 Shell 语法、内置规则和用户插件目录中的声明式规则。
pub(super) struct SafetyEngine {
    linux_core: LinuxCoreAnalyzer,
    snapshot: Arc<SafetySnapshot>,
}

impl SafetyEngine {
    pub(super) fn from_runtime(runtime: &SafetyRuntime) -> Self {
        Self::from_snapshot(runtime.snapshot())
    }

    fn from_snapshot(snapshot: Arc<SafetySnapshot>) -> Self {
        Self {
            linux_core: LinuxCoreAnalyzer,
            snapshot,
        }
    }

    pub(super) fn builtin_only() -> Self {
        Self::from_snapshot(SafetySnapshot::builtin_only())
    }

    #[cfg(test)]
    fn with_external(rule_file: &Path) -> Self {
        Self::from_snapshot(SafetySnapshot::from_analyzers(
            Vec::new(),
            vec![ExternalAnalyzer::load(rule_file).unwrap()],
        ))
    }

    #[cfg(test)]
    pub(super) fn assess(&self, command: &str) -> SafetyAssessment {
        assess_with_depth(command, 0, self)
    }

    /// 测试辅助入口以计划 cwd 作为任务根；生产编排使用任务启动时冻结的根。
    #[cfg(test)]
    pub(super) fn assess_plan(&self, plan: &AgentCommandPlan) -> SafetyAssessment {
        let task_root = std::fs::canonicalize(&plan.cwd).unwrap_or_else(|_| plan.cwd.clone());
        self.assess_plan_for_task(plan, &task_root)
    }

    #[cfg(test)]
    pub(super) fn from_local_directory(directory: std::path::PathBuf) -> Self {
        Self::from_runtime(&SafetyRuntime::for_test(directory))
    }

    /// 评估 Shell 已绑定的命令计划；核心身份、动态标记和任务根不能被插件降低。
    pub(super) fn assess_plan_for_task(
        &self,
        plan: &AgentCommandPlan,
        task_root: &Path,
    ) -> SafetyAssessment {
        let mut assessment = match &plan.executable {
            AgentExecutionTarget::ZhshBuiltin { name, arguments } => {
                assess_words(name, arguments, 0, self)
            }
            AgentExecutionTarget::External {
                path, arguments, ..
            } => {
                let semantic_program = plan
                    .invocations
                    .first()
                    .map(ResolvedInvocation::semantic_name)
                    .unwrap_or_else(|| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                    });
                let arguments: Vec<_> = arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect();
                assess_words(semantic_program, &arguments, 0, self)
            }
            AgentExecutionTarget::BoundCompound { expression } => {
                self.assess_bound_expression(expression, &plan.cwd, task_root)
            }
            AgentExecutionTarget::Bash { script } => assess_with_depth(script, 0, self),
        };

        if plan.dynamic_resolution {
            assessment.level = assessment.level.max(SafetyLevel::Unknown);
            assessment.semantic_level = assessment.semantic_level.max(SafetyLevel::Unknown);
            assessment.binding = BindingClass::Dynamic;
            assessment = assessment
                .force_confirmation("命令需要 Bash 动态解析；确认前无法绑定全部实际执行目标");
        }
        for invocation in &plan.invocations {
            assessment = apply_invocation_identity(assessment, invocation);
        }
        if let Some(unsupported) = &plan.unsupported_execution {
            assessment.level = SafetyLevel::Unsupported;
            assessment.semantic_level = SafetyLevel::Unsupported;
            assessment.supervision = SupervisionClass::Detached;
            assessment.push_reason(unsupported.reason());
        }
        assessment = builtin::apply_disclosure(assessment, plan, task_root);
        apply_state_scope(assessment, plan, task_root)
    }

    fn assess_bound_expression(
        &self,
        expression: &BoundExpression,
        cwd: &Path,
        task_root: &Path,
    ) -> SafetyAssessment {
        match expression {
            BoundExpression::Command(command) => self.assess_bound_command(command, cwd, task_root),
            BoundExpression::Pipeline(commands) => {
                commands
                    .iter()
                    .fold(SafetyAssessment::neutral(), |assessment, command| {
                        assessment.merge(self.assess_bound_command(command, cwd, task_root))
                    })
            }
            BoundExpression::And(left, right) | BoundExpression::Or(left, right) => self
                .assess_bound_expression(left, cwd, task_root)
                .merge(self.assess_bound_expression(right, cwd, task_root)),
            BoundExpression::Sequence(expressions) => {
                expressions
                    .iter()
                    .fold(SafetyAssessment::neutral(), |assessment, expression| {
                        assessment.merge(self.assess_bound_expression(expression, cwd, task_root))
                    })
            }
        }
    }

    fn assess_bound_command(
        &self,
        command: &BoundCommand,
        cwd: &Path,
        task_root: &Path,
    ) -> SafetyAssessment {
        let arguments: Vec<_> = command
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let semantic_program = match &command.target {
            BoundTarget::ZhshQueryBuiltin(query) => match query {
                crate::shell::QueryBuiltin::Pwd => "pwd",
                crate::shell::QueryBuiltin::Type => "type",
                crate::shell::QueryBuiltin::CommandV => "zh-query-command-v",
                crate::shell::QueryBuiltin::LiteralEcho => "echo",
            },
            BoundTarget::External { .. } => command.invocation.semantic_name(),
        };
        if let Err(reason) = summarize_redirections(&command.redirections) {
            return SafetyAssessment::new(SafetyLevel::Unknown, reason);
        }
        let mut assessment = if matches!(command.target, BoundTarget::ZhshQueryBuiltin(_)) {
            SafetyAssessment::read_only()
                .with_semantic_source(SemanticSource::Builtin, Some("builtin/zhsh-query".into()))
        } else {
            assess_words(semantic_program, &arguments, 0, self)
        };
        assessment = apply_invocation_identity(assessment, &command.invocation);

        for redirection in &command.redirections {
            match redirection {
                BoundRedirection::OutputFile { path, mode, .. } => {
                    let level = match mode {
                        OutputMode::Overwrite => SafetyLevel::Destructive,
                        OutputMode::Append => SafetyLevel::StateChanging,
                    };
                    let mut output = SafetyAssessment::new(level, "bound_file_output");
                    output.file_output = true;
                    output.state_scope = if bound_path_is_within(path, task_root) {
                        StateScope::TaskRoot
                    } else {
                        StateScope::OutsideOrUnknown
                    };
                    assessment = assessment.merge(output);
                }
                BoundRedirection::InputFile { path, .. } => {
                    assessment = assessment.with_disclosure(
                        if bound_path_is_within(path, task_root) {
                            DisclosureClass::WorkspaceContent
                        } else {
                            DisclosureClass::SensitiveOrUnbounded
                        },
                        "bound_input_file",
                    );
                }
                BoundRedirection::Duplicate { .. }
                | BoundRedirection::Close { .. }
                | BoundRedirection::Null { .. }
                | BoundRedirection::StandardStream { .. } => {}
            }
        }

        if assessment.level >= SafetyLevel::StateChanging
            && assessment.state_scope == StateScope::None
        {
            let targets = builtin::known_write_targets(semantic_program, &arguments);
            assessment.state_scope = if !targets.is_empty()
                && targets
                    .iter()
                    .all(|target| path_is_within(target, cwd, task_root))
            {
                StateScope::TaskRoot
            } else {
                StateScope::OutsideOrUnknown
            };
        }
        assessment
    }

    fn analyze_command(&self, command: &SafetyCommand<'_>) -> SafetyAssessment {
        let Some(route) = self.snapshot.routes.get(command.program) else {
            return SafetyAssessment::new(SafetyLevel::Unknown, "no_safety_analyzer");
        };
        let mut result = if route.local.is_empty() {
            if route.linux_core {
                match self.linux_core.analyze(command) {
                    Ok(AnalyzerDecision::Assessed(assessment)) => {
                        let rule = assessment
                            .reasons
                            .first()
                            .map(String::as_str)
                            .unwrap_or("-")
                            .to_owned();
                        Some(assessment.with_semantic_source(
                            SemanticSource::Builtin,
                            Some(format!("builtin/linux-core:{rule}")),
                        ))
                    }
                    Ok(AnalyzerDecision::Abstain) => None,
                    Err(error) => Some(SafetyAssessment::new(SafetyLevel::Unknown, error)),
                }
            } else {
                None
            }
        } else {
            // 文件和文件内规则均按“最后匹配覆盖”求值。default 只有在整条
            // local 链没有显式匹配时才参与，避免宽泛默认值遮蔽旧的精确规则。
            let explicit = route.local.iter().rev().find_map(|analyzer| {
                analyzer.explicit_match(command).map(|assessment| {
                    let rule = assessment
                        .reasons
                        .first()
                        .map(String::as_str)
                        .unwrap_or("-")
                        .to_owned();
                    assessment.with_semantic_source(
                        SemanticSource::LocalExplicit,
                        Some(format!("local/{}:{rule}", analyzer.name())),
                    )
                })
            });
            explicit.or_else(|| {
                route.local.last().map(|analyzer| {
                    let assessment = analyzer.default_assessment();
                    let rule = assessment
                        .reasons
                        .first()
                        .map(String::as_str)
                        .unwrap_or("-")
                        .to_owned();
                    assessment.with_semantic_source(
                        SemanticSource::LocalDefault,
                        Some(format!("local/{}:{rule}", analyzer.name())),
                    )
                })
            })
        };
        if let Some(assessment) = result.as_mut() {
            if !route.local.is_empty() && assessment.level == SafetyLevel::StateChanging {
                assessment.mandatory_confirmation = true;
                assessment.push_reason("application_state_change");
            }
        }
        result.unwrap_or_else(|| SafetyAssessment::new(SafetyLevel::Unknown, "no_safety_analyzer"))
    }
}

fn apply_state_scope(
    mut assessment: SafetyAssessment,
    plan: &AgentCommandPlan,
    task_root: &Path,
) -> SafetyAssessment {
    if assessment.level < SafetyLevel::StateChanging {
        return assessment;
    }
    let (program, arguments): (&str, Vec<String>) = match &plan.executable {
        AgentExecutionTarget::External {
            path, arguments, ..
        } => (
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
            arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
        ),
        AgentExecutionTarget::ZhshBuiltin { name, arguments } => (name.as_str(), arguments.clone()),
        AgentExecutionTarget::BoundCompound { .. } => return assessment,
        AgentExecutionTarget::Bash { .. } => {
            assessment.state_scope = StateScope::OutsideOrUnknown;
            return assessment;
        }
    };
    let targets = builtin::known_write_targets(program, &arguments);
    if targets.is_empty() {
        assessment.state_scope = StateScope::OutsideOrUnknown;
        return assessment;
    }
    assessment.state_scope = if targets
        .iter()
        .all(|target| path_is_within(target, &plan.cwd, task_root))
    {
        StateScope::TaskRoot
    } else {
        StateScope::OutsideOrUnknown
    };
    assessment
}

fn bound_path_is_within(path: &crate::shell::BoundPath, task_root: &Path) -> bool {
    path.canonical
        .as_deref()
        .unwrap_or(&path.absolute)
        .starts_with(task_root)
        || path
            .absolute
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .is_some_and(|parent| parent.starts_with(task_root))
}

fn path_is_within(value: &str, cwd: &Path, task_root: &Path) -> bool {
    if value.is_empty() || value == "-" || value.contains(['$', '`', '*', '?', '{']) {
        return false;
    }
    let path = Path::new(value);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if let Ok(canonical) = std::fs::canonicalize(&candidate) {
        return canonical.starts_with(task_root);
    }
    let Some(parent) = candidate.parent() else {
        return false;
    };
    std::fs::canonicalize(parent)
        .map(|parent| parent.starts_with(task_root))
        .unwrap_or(false)
}

fn assess_words(
    program: &str,
    arguments: &[String],
    depth: usize,
    engine: &SafetyEngine,
) -> SafetyAssessment {
    let mut words = Vec::with_capacity(arguments.len() + 1);
    words.push(program.to_owned());
    words.extend_from_slice(arguments);
    assess_segment(&words, depth, engine)
}

fn summarize_redirections(
    redirections: &[BoundRedirection],
) -> Result<Vec<RedirectionSummary>, &'static str> {
    redirections
        .iter()
        .map(|redirection| {
            Ok(match redirection {
                BoundRedirection::InputFile { fd, path } => RedirectionSummary::InputFile {
                    fd: *fd,
                    path: path
                        .absolute
                        .to_str()
                        .ok_or("redirection_path_is_not_utf8")?
                        .into(),
                },
                BoundRedirection::OutputFile { fd, path, mode } => RedirectionSummary::OutputFile {
                    fd: *fd,
                    path: path
                        .absolute
                        .to_str()
                        .ok_or("redirection_path_is_not_utf8")?
                        .into(),
                    mode: match mode {
                        OutputMode::Overwrite => "overwrite",
                        OutputMode::Append => "append",
                    }
                    .into(),
                },
                BoundRedirection::Duplicate { from, to } => RedirectionSummary::Duplicate {
                    from: *from,
                    to: *to,
                },
                BoundRedirection::Close { fd } => RedirectionSummary::Close { fd: *fd },
                BoundRedirection::Null { fd } => RedirectionSummary::Null { fd: *fd },
                BoundRedirection::StandardStream { fd, target } => {
                    RedirectionSummary::StandardStream {
                        fd: *fd,
                        target: format!("{target:?}").to_ascii_lowercase(),
                    }
                }
            })
        })
        .collect()
}

fn apply_invocation_identity(
    mut assessment: SafetyAssessment,
    invocation: &ResolvedInvocation,
) -> SafetyAssessment {
    let semantic_target_unknown = matches!(
        invocation.kind,
        CommandTargetKind::Alias
            | CommandTargetKind::Function
            | CommandTargetKind::DynamicOrUnresolved
    );
    let reason = invocation
        .binding_reason
        .as_deref()
        .unwrap_or("实际执行目标身份不可信或无法静态确定");
    let binding = match invocation.binding {
        ExecutableBinding::SystemTrusted => BindingClass::StaticSystemTrusted,
        ExecutableBinding::UserBound => BindingClass::StaticUserBound,
        ExecutableBinding::Untrusted => BindingClass::StaticUntrusted,
        ExecutableBinding::Dynamic => BindingClass::Dynamic,
    };
    assessment.binding = assessment.binding.max(binding);
    if semantic_target_unknown || binding == BindingClass::Dynamic {
        assessment.level = assessment.level.max(SafetyLevel::Unknown);
        assessment.semantic_level = assessment.semantic_level.max(SafetyLevel::Unknown);
        assessment.mandatory_confirmation = true;
        assessment.push_reason(format!("{}: {reason}", invocation.original));
    } else if binding == BindingClass::StaticUntrusted {
        assessment.level = assessment.level.max(SafetyLevel::Unknown);
        assessment.mandatory_confirmation = true;
        assessment.push_reason(format!("{}: {reason}", invocation.original));
    } else if binding == BindingClass::StaticUserBound
        && (assessment.semantic_level != SafetyLevel::ReadOnly
            || assessment.semantic_source != SemanticSource::LocalExplicit)
    {
        assessment.mandatory_confirmation = true;
        assessment.push_reason(format!(
            "{}: 用户管理目标只有命中 local 显式只读规则才能自动执行",
            invocation.original
        ));
    }
    assessment
}

#[derive(Default)]
struct LexedCommand {
    segments: Vec<Vec<String>>,
    redirection: RedirectionEffect,
    dynamic_execution: bool,
    dynamic_expansion: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RedirectionEffect {
    #[default]
    None,
    Unknown,
    Append,
    Overwrite,
}

#[derive(Clone, Copy)]
enum RedirectionKind {
    Overwrite,
    Append,
    Duplicate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// 启发式评估一条 Agent Bash 命令；结果不构成安全证明。
#[cfg(test)]
pub(super) fn assess(command: &str) -> SafetyAssessment {
    SafetyEngine::builtin_only().assess(command)
}

fn assess_with_depth(command: &str, depth: usize, engine: &SafetyEngine) -> SafetyAssessment {
    if depth > 4 {
        return SafetyAssessment::new(SafetyLevel::Unknown, "嵌套 Shell 超过安全分析深度");
    }
    let lexed = lex(command);
    let mut assessment = SafetyAssessment::read_only();
    assessment = assessment.merge(match lexed.redirection {
        RedirectionEffect::None => SafetyAssessment::read_only(),
        RedirectionEffect::Unknown => {
            SafetyAssessment::new(SafetyLevel::Unknown, "输出重定向目标无法可靠判断")
        }
        RedirectionEffect::Append => {
            let mut result =
                SafetyAssessment::new(SafetyLevel::StateChanging, "输出重定向会追加或创建文件");
            result.file_output = true;
            result
        }
        RedirectionEffect::Overwrite => {
            let mut result = SafetyAssessment::new(
                SafetyLevel::Destructive,
                "输出重定向可能创建、覆盖或截断文件",
            );
            result.file_output = true;
            result
        }
    });
    if lexed.dynamic_execution {
        let mut dynamic =
            SafetyAssessment::new(SafetyLevel::Unknown, "包含命令替换或反引号动态执行");
        dynamic.binding = BindingClass::Dynamic;
        assessment = assessment.merge(dynamic);
    }
    if lexed.dynamic_expansion {
        let mut dynamic = SafetyAssessment::new(
            SafetyLevel::Unknown,
            "包含可能改变命令参数的变量、通配或花括号展开",
        );
        dynamic.binding = BindingClass::Dynamic;
        assessment = assessment.merge(dynamic);
    }
    if lexed.segments.is_empty() {
        return assessment.merge(SafetyAssessment::new(
            SafetyLevel::Unknown,
            "没有识别到可验证的只读命令",
        ));
    }
    for segment in &lexed.segments {
        assessment = assessment.merge(assess_segment(segment, depth, engine));
    }
    assessment
}

fn assess_segment(words: &[String], depth: usize, engine: &SafetyEngine) -> SafetyAssessment {
    let mut index = 0;
    while words.get(index).is_some_and(|word| {
        is_assignment(word)
            || matches!(
                word.as_str(),
                "!" | "if" | "then" | "elif" | "else" | "while" | "until" | "do"
            )
    }) {
        index += 1;
    }
    if words
        .get(index)
        .is_some_and(|word| matches!(word.as_str(), "fi" | "done" | "esac"))
    {
        return SafetyAssessment::read_only();
    }

    let Some(program) = words.get(index).map(String::as_str) else {
        return SafetyAssessment::read_only();
    };
    let command = SafetyCommand {
        program,
        arguments: &words[index + 1..],
        nesting_depth: depth,
    };
    engine.analyze_command(&command)
}

fn assess_builtin_script(script: &str, depth: usize) -> SafetyAssessment {
    let engine = SafetyEngine::builtin_only();
    assess_with_depth(script, depth, &engine)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn lex(command: &str) -> LexedCommand {
    let mut result = LexedCommand::default();
    let mut segment = Vec::new();
    let mut word = String::new();
    let mut quote = Quote::None;
    let mut pending_redirection = None;
    let mut chars = command.chars().peekable();

    fn classify_redirection(kind: RedirectionKind, target: &str) -> RedirectionEffect {
        let dynamic = target.contains('$') || target.contains('`') || target.contains('*');
        if target.is_empty() || dynamic {
            return RedirectionEffect::Unknown;
        }
        match kind {
            RedirectionKind::Duplicate
                if target == "-" || target.chars().all(|c| c.is_ascii_digit()) =>
            {
                RedirectionEffect::None
            }
            RedirectionKind::Duplicate => RedirectionEffect::Overwrite,
            _ if matches!(target, "/dev/null" | "/dev/stdout" | "/dev/stderr") => {
                RedirectionEffect::None
            }
            _ if target
                .strip_prefix("/dev/fd/")
                .is_some_and(|fd| !fd.is_empty() && fd.chars().all(|c| c.is_ascii_digit())) =>
            {
                RedirectionEffect::None
            }
            RedirectionKind::Append => RedirectionEffect::Append,
            RedirectionKind::Overwrite => RedirectionEffect::Overwrite,
        }
    }

    fn finish_word(
        result: &mut LexedCommand,
        segment: &mut Vec<String>,
        word: &mut String,
        pending: &mut Option<RedirectionKind>,
    ) {
        if let Some(kind) = pending.take() {
            result.redirection = result.redirection.max(classify_redirection(kind, word));
            word.clear();
        } else if !word.is_empty() {
            segment.push(std::mem::take(word));
        }
    }

    fn finish_segment(
        result: &mut LexedCommand,
        segment: &mut Vec<String>,
        word: &mut String,
        pending: &mut Option<RedirectionKind>,
    ) {
        finish_word(result, segment, word, pending);
        if !segment.is_empty() {
            result.segments.push(std::mem::take(segment));
        }
    }

    fn begin_redirection(
        result: &mut LexedCommand,
        segment: &mut Vec<String>,
        word: &mut String,
        pending: &mut Option<RedirectionKind>,
        kind: RedirectionKind,
    ) {
        if pending.is_some() {
            finish_word(result, segment, word, pending);
        }
        // 紧邻操作符的纯数字是源 FD，不是命令参数。
        if word.chars().all(|c| c.is_ascii_digit()) {
            word.clear();
        } else {
            finish_word(result, segment, word, pending);
        }
        *pending = Some(kind);
    }

    while let Some(character) = chars.next() {
        match quote {
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(character);
                }
            }
            Quote::Double => match character {
                '"' => quote = Quote::None,
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                '$' if chars.peek() == Some(&'(') => {
                    result.dynamic_execution = true;
                    word.push(character);
                }
                '$' => {
                    result.dynamic_expansion = true;
                    word.push(character);
                }
                '`' => {
                    result.dynamic_execution = true;
                    word.push(character);
                }
                _ => word.push(character),
            },
            Quote::None => match character {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                '`' => {
                    result.dynamic_execution = true;
                    word.push(character);
                }
                '$' if chars.peek() == Some(&'(') => {
                    result.dynamic_execution = true;
                    word.push(character);
                }
                '$' => {
                    result.dynamic_expansion = true;
                    word.push(character);
                }
                '*' | '?' | '{' => {
                    result.dynamic_expansion = true;
                    word.push(character);
                }
                '>' => {
                    let kind = match chars.peek() {
                        Some('>') => {
                            chars.next();
                            RedirectionKind::Append
                        }
                        Some('|') => {
                            chars.next();
                            RedirectionKind::Overwrite
                        }
                        Some('&') => {
                            chars.next();
                            RedirectionKind::Duplicate
                        }
                        _ => RedirectionKind::Overwrite,
                    };
                    begin_redirection(
                        &mut result,
                        &mut segment,
                        &mut word,
                        &mut pending_redirection,
                        kind,
                    );
                }
                '&' if chars.peek() == Some(&'>') => {
                    chars.next();
                    let kind = if chars.peek() == Some(&'>') {
                        chars.next();
                        RedirectionKind::Append
                    } else {
                        RedirectionKind::Overwrite
                    };
                    begin_redirection(
                        &mut result,
                        &mut segment,
                        &mut word,
                        &mut pending_redirection,
                        kind,
                    );
                }
                ';' | '|' | '&' | '(' | ')' | '\n' => {
                    finish_segment(
                        &mut result,
                        &mut segment,
                        &mut word,
                        &mut pending_redirection,
                    );
                }
                '<' => finish_word(
                    &mut result,
                    &mut segment,
                    &mut word,
                    &mut pending_redirection,
                ),
                character if character.is_whitespace() => {
                    if pending_redirection.is_none() || !word.is_empty() {
                        finish_word(
                            &mut result,
                            &mut segment,
                            &mut word,
                            &mut pending_redirection,
                        );
                    }
                }
                _ => word.push(character),
            },
        }
    }
    finish_segment(
        &mut result,
        &mut segment,
        &mut word,
        &mut pending_redirection,
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn external_rule(contents: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static NEXT_PLUGIN: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_PLUGIN.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zhsh-safety-plugin-{}-{unique}-{sequence}.zhse.json",
            std::process::id(),
        ));
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[cfg(unix)]
    fn assessed_rule_contents(programs: &[&str], level: &str, reason: &str) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": 1,
            "name": "test-plugin",
            "programs": programs,
            "rules": [],
            "default": {
                "id": reason,
                "assessment": {"level": level}
            }
        }))
        .unwrap()
    }

    #[cfg(unix)]
    fn assessed_rule_for(programs: &[&str], level: &str, reason: &str) -> std::path::PathBuf {
        external_rule(&assessed_rule_contents(programs, level, reason))
    }

    #[cfg(unix)]
    fn assessed_rule(level: &str, reason: &str) -> std::path::PathBuf {
        assessed_rule_for(&["custom-tool"], level, reason)
    }

    #[test]
    fn user_bound_target_requires_an_explicit_local_read_only_rule() {
        let user_bound = ResolvedInvocation::named(
            "java".into(),
            CommandTargetKind::External,
            ExecutableBinding::UserBound,
            Some("current user managed target".into()),
        );
        let explicit = apply_invocation_identity(
            SafetyAssessment::read_only().with_semantic_source(
                SemanticSource::LocalExplicit,
                Some("local/java:version".into()),
            ),
            &user_bound,
        );
        assert_eq!(explicit.semantic_level, SafetyLevel::ReadOnly);
        assert_eq!(explicit.binding, BindingClass::StaticUserBound);
        assert_eq!(
            decide(AgentTrust::Balanced, &explicit),
            SafetyDecision::AutoExecute
        );

        let default = apply_invocation_identity(
            SafetyAssessment::read_only().with_semantic_source(
                SemanticSource::LocalDefault,
                Some("local/java:default".into()),
            ),
            &user_bound,
        );
        assert_eq!(
            decide(AgentTrust::Balanced, &default),
            SafetyDecision::Confirm
        );
    }

    #[cfg(unix)]
    #[test]
    fn reported_java_observation_plan_keeps_read_only_semantics() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let directory = std::env::temp_dir().join(format!(
            "zhsh-java-observation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.join("tool.real");
        std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["java", "javac", "mvn", "gradle", "head", "which"] {
            symlink(&target, directory.join(name)).unwrap();
        }
        let mut shell = crate::shell::Shell::new();
        shell
            .env
            .insert("PATH".into(), directory.to_string_lossy().into_owned());
        let rule = directory.join("toolchain.zhse.json");
        std::fs::write(
            &rule,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": 1,
                "name": "toolchain-test",
                "programs": ["java", "javac", "mvn", "gradle"],
                "rules": [{
                    "id": "version",
                    "match": {"any_arguments": ["-version", "--version"]},
                    "assessment": {"level": "read_only"}
                }],
                "default": {
                    "id": "unknown",
                    "assessment": {"level": "unknown"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&rule, std::fs::Permissions::from_mode(0o600)).unwrap();
        let runtime = SafetyRuntime::for_test(directory.clone());
        let engine = SafetyEngine::from_runtime(&runtime);
        for command in [
            "java -version 2>&1; which java javac mvn gradle 2>&1",
            "java -version 2>&1; echo '---'; javac -version 2>&1; echo '---'; mvn -version 2>&1 | head -3; echo '---'; gradle -version 2>&1 | head -3; echo '---'; which java javac mvn gradle 2>&1",
        ] {
            let plan = shell.prepare_agent_command(command);
            let assessment = engine.assess_plan(&plan);
            assert_eq!(
                assessment.semantic_level,
                SafetyLevel::ReadOnly,
                "{command}\nplan={plan:#?}\nassessment={assessment:#?}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn classifies_the_reported_destructive_commands() {
        for command in [
            r#"find "/srv/media/library" -maxdepth 2 -type f -links +1 -delete"#,
            r#"find . -type f -name '*.mkv.bak' -delete"#,
            r#"find . -exec sh -c 'mv "$1" "${1%.bak}"' _ {} \;"#,
        ] {
            assert_eq!(assess(command).level, SafetyLevel::Destructive, "{command}");
        }
    }

    #[test]
    fn prose_redirection_cannot_be_considered_read_only() {
        let assessment = assess("1 > 1）。执行删除。");
        assert_eq!(assessment.level, SafetyLevel::Destructive);
        assert!(assessment
            .reasons
            .iter()
            .any(|reason| reason.contains("重定向")));
    }

    #[test]
    fn distinguishes_fd_redirection_from_persistent_file_writes() {
        for command in [
            "which java javac 2>/dev/null",
            "printf error 1>&2",
            "printf ignored >/dev/null",
            "printf visible >/dev/stdout",
            "printf 'a > b'",
        ] {
            assert_eq!(assess(command).level, SafetyLevel::ReadOnly, "{command}");
        }
        assert_eq!(
            assess("java -version 2>&1").level,
            SafetyLevel::Unknown,
            "应用级命令没有插件时不得由核心规则放行"
        );
        assert_eq!(
            assess("printf data > output").level,
            SafetyLevel::Destructive
        );
        assert_eq!(
            assess("printf data >| output").level,
            SafetyLevel::Destructive
        );
        assert_eq!(
            assess("printf data >> output").level,
            SafetyLevel::StateChanging
        );
        assert_eq!(
            assess("printf data > \"$OUTPUT\"").level,
            SafetyLevel::Unknown
        );
        let reported = "which java javac mvn gradle 2>/dev/null; echo '---'; java -version 2>&1 | head -n 3; echo '---'; ls /usr/lib/jvm 2>/dev/null; echo '---'; ls ~/.sdkman/candidates 2>/dev/null";
        assert_eq!(assess(reported).level, SafetyLevel::Unknown);
    }

    #[test]
    fn java_observation_signature_does_not_allow_code_execution() {
        for command in [
            "java Example",
            "java -jar app.jar",
            "java -javaagent:a.jar --version",
        ] {
            assert_eq!(assess(command).level, SafetyLevel::Unknown, "{command}");
        }
    }

    #[test]
    fn trust_policies_follow_the_decision_matrix() {
        let read = SafetyAssessment::read_only();
        let unknown = SafetyAssessment::new(SafetyLevel::Unknown, "test");
        let mut mutate = SafetyAssessment::new(SafetyLevel::StateChanging, "test");
        mutate.state_scope = StateScope::TaskRoot;
        let destroy = SafetyAssessment::new(SafetyLevel::Destructive, "test");
        let privilege = SafetyAssessment::privilege("test");
        let mut continuous = SafetyAssessment::read_only();
        continuous.supervision = SupervisionClass::PotentiallyUnbounded;

        assert!(!requires_confirmation(AgentTrust::Balanced, &read));
        assert!(requires_confirmation(AgentTrust::Balanced, &unknown));
        assert!(requires_confirmation(AgentTrust::Balanced, &mutate));
        assert!(requires_confirmation(AgentTrust::Confirm, &read));
        assert!(!requires_confirmation(AgentTrust::Trusted, &read));
        assert!(requires_confirmation(AgentTrust::Trusted, &unknown));
        assert!(!requires_confirmation(AgentTrust::Trusted, &mutate));
        for policy in [
            AgentTrust::Balanced,
            AgentTrust::Confirm,
            AgentTrust::Trusted,
        ] {
            assert!(requires_confirmation(policy, &destroy));
            assert!(requires_confirmation(policy, &privilege));
            assert_eq!(decide(policy, &continuous), SafetyDecision::Reject);
        }
        assert_eq!(continuous.primary_reason(), "命令没有静态终止条件");

        let mut network = SafetyAssessment::read_only();
        network.network = true;
        assert_eq!(network.primary_reason(), "可能访问网络");

        let sensitive = SafetyAssessment::read_only()
            .with_disclosure(DisclosureClass::SensitiveOrUnbounded, "sensitive test data");
        assert!(requires_confirmation(AgentTrust::Balanced, &sensitive));
        assert!(requires_confirmation(AgentTrust::Trusted, &sensitive));
    }

    #[cfg(unix)]
    #[test]
    fn command_plan_identity_and_dynamic_signals_cannot_be_cleared_by_trust() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("zhsh-safety-shadow-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let shadow = root.join("ls");
        std::fs::write(&shadow, "#!/bin/sh\nprintf shadow\n").unwrap();
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut shell = crate::shell::Shell::new();
        shell
            .env
            .insert("PATH".into(), format!("{}:/usr/bin", root.display()));
        let shadow_plan = shell.prepare_agent_command("ls");
        let shadow_assessment = SafetyEngine::builtin_only().assess_plan(&shadow_plan);
        assert_eq!(shadow_assessment.level, SafetyLevel::Unknown);
        assert!(shadow_assessment.mandatory_confirmation);
        assert!(requires_confirmation(
            AgentTrust::Trusted,
            &shadow_assessment
        ));

        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        let static_plan = shell.prepare_agent_command("ls | sort");
        let static_assessment = SafetyEngine::builtin_only().assess_plan(&static_plan);
        if static_plan
            .invocations
            .iter()
            .all(|invocation| invocation.binding == ExecutableBinding::SystemTrusted)
        {
            assert_eq!(static_assessment.level, SafetyLevel::ReadOnly);
            assert!(!static_assessment.mandatory_confirmation);
            assert!(!requires_confirmation(
                AgentTrust::Balanced,
                &static_assessment
            ));
        } else {
            assert_eq!(static_assessment.level, SafetyLevel::Unknown);
            assert!(static_assessment.mandatory_confirmation);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn aliases_are_frozen_while_functions_and_path_overrides_require_confirmation() {
        let mut shell = crate::shell::Shell::new();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        shell.aliases.insert("ls".into(), "rm -- sentinel".into());
        let engine = SafetyEngine::builtin_only();

        let alias = engine.assess_plan(&shell.prepare_agent_command("ls"));
        assert_eq!(alias.level, SafetyLevel::Destructive);
        assert!(requires_confirmation(AgentTrust::Balanced, &alias));

        shell
            .functions
            .insert("inspect".into(), "inspect () { printf arbitrary; }".into());
        let function = engine.assess_plan(&shell.prepare_agent_command("inspect"));
        assert!(function.mandatory_confirmation);

        for command in [
            "PATH=/tmp/bin:$PATH ls",
            "env PATH=/tmp/bin ls",
            "command ls",
            "xargs ls",
            "find . -exec ls {} ;",
            "bash -c 'ls'",
        ] {
            let assessment = engine.assess_plan(&shell.prepare_agent_command(command));
            assert!(assessment.mandatory_confirmation, "{command}");
            assert!(requires_confirmation(AgentTrust::Trusted, &assessment));
        }
    }

    #[cfg(unix)]
    #[test]
    fn external_plugin_cannot_clear_an_untrusted_executable_identity() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("zhsh-plugin-identity-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("custom-tool");
        std::fs::write(&executable, "#!/bin/sh\nprintf unsafe\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plugin = assessed_rule("read_only", "plugin_permits");
        let engine = SafetyEngine::with_external(&plugin);
        let mut shell = crate::shell::Shell::new();
        shell
            .env
            .insert("PATH".into(), root.to_string_lossy().into_owned());

        let assessment = engine.assess_plan(&shell.prepare_agent_command("custom-tool"));
        assert_eq!(assessment.level, SafetyLevel::Unknown);
        assert!(assessment.mandatory_confirmation);
        assert!(requires_confirmation(AgentTrust::Trusted, &assessment));

        std::fs::remove_file(plugin).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disclosure_is_orthogonal_to_command_level() {
        let mut shell = crate::shell::Shell::new();
        let root =
            std::env::temp_dir().join(format!("zhsh-disclosure-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notes.txt"), "workspace").unwrap();
        shell.cwd = root.clone();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        let engine = SafetyEngine::builtin_only();

        let workspace_plan = shell.prepare_agent_command("cat notes.txt");
        let target_is_trusted = workspace_plan
            .invocations
            .first()
            .is_some_and(|invocation| invocation.binding == ExecutableBinding::SystemTrusted);
        let workspace = engine.assess_plan(&workspace_plan);
        assert_eq!(
            workspace.level,
            if target_is_trusted {
                SafetyLevel::ReadOnly
            } else {
                SafetyLevel::Unknown
            }
        );
        assert_eq!(workspace.disclosure, DisclosureClass::WorkspaceContent);

        let outside = engine.assess_plan(&shell.prepare_agent_command("cat /etc/passwd"));
        assert_eq!(outside.level, workspace.level);
        assert_eq!(outside.disclosure, DisclosureClass::SensitiveOrUnbounded);
        assert!(requires_confirmation(AgentTrust::Balanced, &outside));
        assert!(requires_confirmation(AgentTrust::Trusted, &outside));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_plugin_can_classify_an_unknown_program() {
        let plugin = assessed_rule("read_only", "third_party_signature");
        let engine = SafetyEngine::with_external(&plugin);

        let assessment = engine.assess("custom-tool inspect");

        assert_eq!(
            assessment.level,
            SafetyLevel::ReadOnly,
            "{:?}",
            assessment.reasons
        );
        assert!(assessment.reasons.contains(&"third_party_signature".into()));
        assert_eq!(
            engine.assess("rm victim").level,
            SafetyLevel::Destructive,
            "第三方插件不得降低内置插件的更高分类"
        );
        std::fs::remove_file(plugin).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn later_matching_local_rule_replaces_the_earlier_assessment() {
        let read_plugin = assessed_rule_for(&["java"], "read_only", "language_observation");
        let veto_plugin = assessed_rule_for(&["java"], "destructive", "third_party_veto");
        let engine = SafetyEngine::from_snapshot(SafetySnapshot::from_analyzers(
            Vec::new(),
            vec![
                ExternalAnalyzer::load(&read_plugin).unwrap(),
                ExternalAnalyzer::load(&veto_plugin).unwrap(),
            ],
        ));

        let assessment = engine.assess("java -version");

        assert_eq!(assessment.level, SafetyLevel::Destructive);
        assert!(!assessment.reasons.contains(&"language_observation".into()));
        assert!(assessment.reasons.contains(&"third_party_veto".into()));
        std::fs::remove_file(read_plugin).unwrap();
        std::fs::remove_file(veto_plugin).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn later_local_rule_replaces_an_earlier_local_rule() {
        let earlier = assessed_rule_for(&["custom-tool"], "destructive", "earlier_policy");
        let later = assessed_rule_for(&["custom-tool"], "read_only", "later_policy");
        let engine = SafetyEngine::from_snapshot(SafetySnapshot::from_analyzers(
            vec![ExternalAnalyzer::load(&earlier).unwrap()],
            vec![ExternalAnalyzer::load(&later).unwrap()],
        ));

        let assessment = engine.assess("custom-tool inspect");

        assert_eq!(assessment.level, SafetyLevel::ReadOnly);
        assert!(assessment.reasons.contains(&"later_policy".into()));
        assert!(!assessment.reasons.contains(&"earlier_policy".into()));
        std::fs::remove_file(earlier).unwrap();
        std::fs::remove_file(later).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn user_rule_overrides_linux_core_command_semantics() {
        let plugin = assessed_rule_for(&["rm"], "read_only", "local_rm_policy");
        let engine = SafetyEngine::from_snapshot(SafetySnapshot::from_analyzers(
            Vec::new(),
            vec![ExternalAnalyzer::load(&plugin).unwrap()],
        ));

        let assessment = engine.assess("rm victim");

        assert_eq!(assessment.level, SafetyLevel::ReadOnly);
        assert!(assessment.reasons.contains(&"local_rm_policy".into()));
        std::fs::remove_file(plugin).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn invalid_json_rule_is_rejected() {
        let plugin = external_rule("{not-json");
        let error = ExternalAnalyzer::load(&plugin).err().unwrap();
        assert!(error.contains("JSON 格式无效"));
        std::fs::remove_file(plugin).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn discovers_only_trusted_routed_plugins_in_the_user_plugin_directory() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!(
            "zhsh-safety-plugin-home-{}-{unique}",
            std::process::id()
        ));
        let directory = home.join(".zhsh/plugins/safety");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();

        let enabled = directory.join("company-policy.zhse.json");
        std::fs::write(
            &enabled,
            assessed_rule_contents(&["custom-tool", "ls"], "read_only", "discovered_plugin"),
        )
        .unwrap();
        std::fs::set_permissions(&enabled, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ignored = directory.join("10-not-rule.json");
        std::fs::write(
            &ignored,
            assessed_rule_contents(&["custom-tool"], "destructive", "must_be_ignored"),
        )
        .unwrap();
        std::fs::set_permissions(&ignored, std::fs::Permissions::from_mode(0o600)).unwrap();
        let valid_runtime = SafetyRuntime::for_test(directory.clone());
        let valid_engine = SafetyEngine::from_snapshot(valid_runtime.snapshot());
        let assessment = valid_engine.assess("custom-tool inspect");
        assert_eq!(assessment.level, SafetyLevel::ReadOnly);
        assert!(assessment.reasons.contains(&"discovered_plugin".into()));

        symlink(&enabled, directory.join("30-symlink.zhse.json")).unwrap();

        let writable = directory.join("40-group-writable.zhse.json");
        std::fs::write(
            &writable,
            assessed_rule_contents(&["custom-tool"], "destructive", "must_also_be_ignored"),
        )
        .unwrap();
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o620)).unwrap();
        let malformed = directory.join("50-malformed.zhse.json");
        std::fs::write(&malformed, "{not-json").unwrap();
        std::fs::set_permissions(&malformed, std::fs::Permissions::from_mode(0o600)).unwrap();
        let runtime = SafetyRuntime::for_test(directory.clone());
        let engine = SafetyEngine::from_snapshot(runtime.snapshot());
        let assessment = engine.assess("custom-tool inspect");
        assert_eq!(assessment.level, SafetyLevel::Unknown);
        assert_eq!(
            engine.assess("unrelated-tool inspect").level,
            SafetyLevel::Unknown,
            "未声明该二进制名的插件不得收到命令"
        );
        assert!(runtime
            .startup_notices()
            .iter()
            .any(|notice| notice.contains("50-malformed.zhse.json")
                && notice.contains("JSON 格式无效")));
        assert!(runtime.startup_notices().iter().any(|notice| {
            notice.contains("30-symlink.zhse.json") && notice.contains("非符号链接普通文件")
        }));

        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(
            SafetyEngine::from_snapshot(SafetyRuntime::for_test(directory.clone()).snapshot())
                .assess("custom-tool inspect")
                .level,
            SafetyLevel::Unknown,
            "组可写插件目录必须整体拒绝"
        );

        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn allows_bounded_read_only_inspection() {
        for command in [
            "ls -la .",
            "find . -maxdepth 2 -type f -links +1 -printf '%p\\n' | head -50",
            "stat -c '%i %h' file",
            "read -rsp 'Secret: ' secret; printf '\\naccepted\\n'",
        ] {
            assert_eq!(assess(command).level, SafetyLevel::ReadOnly, "{command}");
        }
        assert_eq!(
            assess("git status --short").level,
            SafetyLevel::Unknown,
            "Git 语义必须由外部插件提供"
        );
    }

    #[test]
    fn static_linux_query_combinations_are_balanced_auto_execute_candidates() {
        let mut shell = crate::shell::Shell::new();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        for command in [
            "uptime && top -b -n 1 | head -5",
            "find . -maxdepth 1 -type f | sed 's/.*\\.//' | sort | uniq -c | sort -rn",
        ] {
            let plan = shell.prepare_agent_command(command);
            assert!(matches!(
                plan.executable,
                AgentExecutionTarget::BoundCompound { .. }
            ));
            let semantic_assessment = assess(command);
            assert_eq!(
                semantic_assessment.level,
                SafetyLevel::ReadOnly,
                "{command}"
            );
            let assessment = SafetyEngine::builtin_only().assess_plan(&plan);
            let expected = if plan
                .invocations
                .iter()
                .all(|invocation| invocation.binding == ExecutableBinding::SystemTrusted)
            {
                SafetyDecision::AutoExecute
            } else {
                SafetyDecision::Confirm
            };
            assert_eq!(
                decide(AgentTrust::Balanced, &assessment),
                expected,
                "{command}: {assessment:?}"
            );
        }
    }

    #[test]
    fn unknown_and_dynamic_commands_require_confirmation() {
        for command in [
            "custom-tool arg",
            "printf '%s' \"$(custom-tool)\"",
            "find $OPTIONS",
            "find *",
            "find . {-print,-delete}",
        ] {
            assert_ne!(assess(command).level, SafetyLevel::ReadOnly, "{command}");
        }
    }

    #[test]
    fn catches_write_options_on_otherwise_read_only_tools() {
        for command in [
            "find . -fprint0 output",
            "sort -o output input",
            "sort --output=output input",
            "uniq input output",
            "date -s2026-08-22",
        ] {
            assert_ne!(assess(command).level, SafetyLevel::ReadOnly, "{command}");
        }
        assert_eq!(assess("tee output").level, SafetyLevel::Destructive);
        assert_eq!(assess("tee -a output").level, SafetyLevel::StateChanging);
        assert_eq!(
            assess("truncate -s 0 output").level,
            SafetyLevel::Destructive
        );
    }
}
