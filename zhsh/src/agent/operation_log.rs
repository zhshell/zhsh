//! Agent 任务内的只追加操作事件日志。
//!
//! [`super::AgentFlowState`] 仍是唯一当前控制状态；本模块只保存宿主已经确认发生的
//! 历史事实，并从事件投影操作是否已经处理。日志不落盘，随当前 Task 一起销毁。

use super::protocol::AgentResponseError;
use super::safety::{SafetyDecision, SafetyLevel};
use crate::shell::{
    AgentCommandPlan, AgentExecutionTarget, CommandTermination, OutputEvidence, ResolvedInvocation,
    UnsupportedExecution,
};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

const OPERATION_KEY_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct OperationId(pub(super) u32);

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "op-{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationKey {
    version: u8,
    executable: AgentExecutionTarget,
    cwd: PathBuf,
    path_snapshot: Option<OsString>,
    invocations: Vec<ResolvedInvocation>,
    dynamic_resolution: bool,
    unsupported_execution: Option<UnsupportedExecution>,
}

impl From<&AgentCommandPlan> for OperationKey {
    fn from(plan: &AgentCommandPlan) -> Self {
        Self {
            version: OPERATION_KEY_VERSION,
            executable: plan.executable.clone(),
            cwd: plan.cwd.clone(),
            path_snapshot: plan.path_snapshot.clone(),
            invocations: plan.invocations.clone(),
            dynamic_resolution: plan.dynamic_resolution,
            unsupported_execution: plan.unsupported_execution.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthorizationSource {
    Automatic,
    User,
}

impl AuthorizationSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::User => "user",
        }
    }
}

#[derive(Debug)]
enum TaskEventKind {
    RoundStarted,
    ResponseRejected,
    OperationPrepared(Box<OperationKey>),
    SafetyAssessed {
        level: SafetyLevel,
        decision: SafetyDecision,
    },
    AuthorizationGranted(AuthorizationSource),
    AuthorizationDenied(&'static str),
    ExecutionDispatched,
    ExecutionNotStarted,
    ExecutionStarted,
    StartUncertain,
    ExecutionFinished {
        termination: CommandTermination,
        exit_code: i32,
    },
    FeedbackRecorded {
        evidence: OutputEvidence,
        supports_observation: bool,
    },
    ReplayRejected,
    PhaseChanged {
        previous_phase: u8,
    },
}

#[derive(Debug)]
struct TaskEvent {
    phase: u8,
    turn: i32,
    operation_id: Option<OperationId>,
    kind: TaskEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionState {
    Prepared,
    Assessed,
    Authorized,
    NotStarted,
    Dispatching,
    Started,
    StartUncertain,
    Completed,
    Incomplete,
}

impl ExecutionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Assessed => "assessed",
            Self::Authorized => "authorized",
            Self::NotStarted => "not_started",
            Self::Dispatching => "dispatching",
            Self::Started => "started",
            Self::StartUncertain => "start_uncertain",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
        }
    }

    fn execution_started(self) -> &'static str {
        match self {
            Self::Dispatching | Self::StartUncertain => "uncertain",
            Self::Started | Self::Completed | Self::Incomplete => "true",
            Self::Prepared | Self::Assessed | Self::Authorized | Self::NotStarted => "false",
        }
    }

    fn execution_completed(self) -> bool {
        self == Self::Completed
    }
}

#[derive(Debug)]
struct OperationProjection {
    id: OperationId,
    phase: u8,
    turn: i32,
    state: ExecutionState,
    safety_level: Option<SafetyLevel>,
    safety_decision: Option<SafetyDecision>,
    authorization: Option<AuthorizationSource>,
    denial: Option<&'static str>,
    termination: Option<CommandTermination>,
    exit_code: Option<i32>,
    evidence: Option<OutputEvidence>,
    supports_observation: bool,
}

impl OperationProjection {
    fn new(id: OperationId, phase: u8, turn: i32) -> Self {
        Self {
            id,
            phase,
            turn,
            state: ExecutionState::Prepared,
            safety_level: None,
            safety_decision: None,
            authorization: None,
            denial: None,
            termination: None,
            exit_code: None,
            evidence: None,
            supports_observation: false,
        }
    }
}

#[derive(Debug)]
pub(super) struct ReplayRecord {
    projection: OperationProjection,
}

impl ReplayRecord {
    pub(super) fn terminal_reason(&self) -> String {
        let when = format!(
            "第 {} 阶段第 {} 轮",
            self.projection.phase, self.projection.turn
        );
        let state = match self.projection.state {
            ExecutionState::Completed => "启动并完成",
            ExecutionState::Incomplete => "启动但未正常完成",
            ExecutionState::Started => "启动且尚无完整终止记录",
            ExecutionState::Dispatching | ExecutionState::StartUncertain => "可能已经启动",
            ExecutionState::NotStarted => "已处理但未启动",
            ExecutionState::Prepared | ExecutionState::Assessed | ExecutionState::Authorized => {
                "已经处理"
            }
        };
        format!("本任务中的相同操作已在{when}{state}")
    }

    pub(super) fn feedback(&self) -> String {
        format!(
            "result:duplicate_operation\noperation_id:{}\nprevious_phase:{}\nprevious_turn:{}\nexecution_started:{}\nexecution_completed:{}\ntermination:{}\nexit:{}\noutput_evidence:{}\ninstruction:使用任务历史中的既有 result；不得再次返回该操作。若仍缺少事实，只能提出不同的最小后置观测命令。",
            self.projection.id,
            self.projection.phase,
            self.projection.turn,
            self.projection.state.execution_started(),
            self.projection.state.execution_completed(),
            self.projection
                .termination
                .map(termination_name)
                .unwrap_or("none"),
            self.projection
                .exit_code
                .map_or_else(|| "none".into(), |code| code.to_string()),
            self.projection
                .evidence
                .map(output_evidence_name)
                .unwrap_or("none"),
        )
    }
}

pub(super) enum PrepareOutcome {
    New(OperationId),
    Duplicate(ReplayRecord),
}

#[derive(Debug, Default)]
pub(super) struct TaskEventLog {
    events: Vec<TaskEvent>,
    next_operation_id: u32,
}

impl TaskEventLog {
    pub(super) fn new() -> Self {
        Self {
            events: Vec::new(),
            next_operation_id: 1,
        }
    }

    pub(super) fn round_started(&mut self, phase: u8, turn: i32) {
        self.append(phase, turn, None, TaskEventKind::RoundStarted);
    }

    pub(super) fn response_rejected(&mut self, phase: u8, turn: i32) {
        self.append(phase, turn, None, TaskEventKind::ResponseRejected);
    }

    pub(super) fn prepare(
        &mut self,
        plan: &AgentCommandPlan,
        phase: u8,
        turn: i32,
    ) -> PrepareOutcome {
        let key = OperationKey::from(plan);
        if let Some(id) = self.operation_id_for_key(&key) {
            let projection = self
                .project(id)
                .expect("an operation key always belongs to a prepared operation");
            self.append(phase, turn, Some(id), TaskEventKind::ReplayRejected);
            return PrepareOutcome::Duplicate(ReplayRecord { projection });
        }
        let id = OperationId(self.next_operation_id);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        self.append(
            phase,
            turn,
            Some(id),
            TaskEventKind::OperationPrepared(Box::new(key)),
        );
        PrepareOutcome::New(id)
    }

    pub(super) fn safety_assessed(
        &mut self,
        id: OperationId,
        phase: u8,
        turn: i32,
        level: SafetyLevel,
        decision: SafetyDecision,
    ) {
        self.append(
            phase,
            turn,
            Some(id),
            TaskEventKind::SafetyAssessed { level, decision },
        );
    }

    pub(super) fn authorization_granted(
        &mut self,
        id: OperationId,
        phase: u8,
        turn: i32,
        source: AuthorizationSource,
    ) {
        self.append(
            phase,
            turn,
            Some(id),
            TaskEventKind::AuthorizationGranted(source),
        );
    }

    pub(super) fn authorization_denied(
        &mut self,
        id: OperationId,
        phase: u8,
        turn: i32,
        reason: &'static str,
    ) {
        self.append(
            phase,
            turn,
            Some(id),
            TaskEventKind::AuthorizationDenied(reason),
        );
    }

    pub(super) fn execution_dispatched(&mut self, id: OperationId, phase: u8, turn: i32) {
        self.append(phase, turn, Some(id), TaskEventKind::ExecutionDispatched);
    }

    pub(super) fn execution_not_started(&mut self, id: OperationId, phase: u8, turn: i32) {
        self.append(phase, turn, Some(id), TaskEventKind::ExecutionNotStarted);
    }

    pub(super) fn execution_started(&mut self, id: OperationId, phase: u8, turn: i32) {
        self.append(phase, turn, Some(id), TaskEventKind::ExecutionStarted);
    }

    pub(super) fn start_uncertain(&mut self, id: OperationId, phase: u8, turn: i32) {
        self.append(phase, turn, Some(id), TaskEventKind::StartUncertain);
    }

    pub(super) fn execution_finished(
        &mut self,
        id: OperationId,
        phase: u8,
        turn: i32,
        termination: CommandTermination,
        exit_code: i32,
    ) {
        self.append(
            phase,
            turn,
            Some(id),
            TaskEventKind::ExecutionFinished {
                termination,
                exit_code,
            },
        );
    }

    pub(super) fn feedback_recorded(
        &mut self,
        id: OperationId,
        phase: u8,
        turn: i32,
        evidence: OutputEvidence,
        supports_observation: bool,
    ) {
        self.append(
            phase,
            turn,
            Some(id),
            TaskEventKind::FeedbackRecorded {
                evidence,
                supports_observation,
            },
        );
    }

    pub(super) fn phase_changed(&mut self, previous_phase: u8, phase: u8) {
        self.append(
            phase,
            0,
            None,
            TaskEventKind::PhaseChanged { previous_phase },
        );
    }

    pub(super) fn format_repair_feedback(
        &self,
        phase: u8,
        turn: i32,
        error: &AgentResponseError,
    ) -> String {
        let last = self.latest_projection();
        let (id, state) = last.map_or_else(
            || ("none".into(), "none"),
            |operation| (operation.id.to_string(), operation.state.as_str()),
        );
        let error = error.repair_feedback();
        format!(
            "result:invalid_response\ninvalid_round:{phase}.{turn}\n{error}\nnew_operation_created:false\nprevious_operations_preserved:true\nlast_operation_id:{id}\nlast_operation_state:{state}\ninstruction:仅修复上一响应的协议编码；此前操作事实继续有效，不得重复任何已处理操作。保留上一响应的语义内容，只返回带 response 的完整 run、clarify 或 done JSON 对象。"
        )
    }

    pub(super) fn prompt_summary(&self) -> String {
        let mut summaries = Vec::new();
        for id in self.operation_ids() {
            let Some(operation) = self.project(id) else {
                continue;
            };
            summaries.push(format!(
                "operation:{} proposed={}.{} state={} safety={} decision={} authorization={} termination={} exit={} output_evidence={} observation={}",
                operation.id,
                operation.phase,
                operation.turn,
                operation.state.as_str(),
                operation
                    .safety_level
                    .map(SafetyLevel::as_str)
                    .unwrap_or("none"),
                operation
                    .safety_decision
                    .map(SafetyDecision::as_str)
                    .unwrap_or("none"),
                operation
                    .authorization
                    .map(AuthorizationSource::as_str)
                    .or(operation.denial)
                    .unwrap_or("none"),
                operation
                    .termination
                    .map(termination_name)
                    .unwrap_or("none"),
                operation
                    .exit_code
                    .map_or_else(|| "none".into(), |code| code.to_string()),
                operation
                    .evidence
                    .map(output_evidence_name)
                    .unwrap_or("none"),
                operation.supports_observation,
            ));
        }
        let invalid_responses = self
            .events
            .iter()
            .filter(|event| matches!(event.kind, TaskEventKind::ResponseRejected))
            .count();
        let replay_rejections = self
            .events
            .iter()
            .filter(|event| matches!(event.kind, TaskEventKind::ReplayRejected))
            .count();
        let phase_changes: Vec<_> = self
            .events
            .iter()
            .filter_map(|event| match event.kind {
                TaskEventKind::PhaseChanged { previous_phase } => {
                    Some(format!("{previous_phase}->{}", event.phase))
                }
                _ => None,
            })
            .collect();
        let started_rounds = self
            .events
            .iter()
            .filter(|event| matches!(event.kind, TaskEventKind::RoundStarted))
            .count();
        let mut summary = format!(
            "宿主操作日志摘要（只读权威事实；模型不得改写）：\nrounds_started:{started_rounds}\ninvalid_responses:{invalid_responses}\nreplay_rejections:{replay_rejections}\nphase_changes:{}",
            if phase_changes.is_empty() {
                "none".into()
            } else {
                phase_changes.join(",")
            }
        );
        if summaries.is_empty() {
            summary.push_str("\noperations:none");
        } else {
            summary.push('\n');
            summary.push_str(&summaries.join("\n"));
        }
        summary.push_str(
            "\n约束：相同冻结操作在本任务中不能再次执行；格式修复和澄清不会撤销上述事实。",
        );
        summary
    }

    fn append(
        &mut self,
        phase: u8,
        turn: i32,
        operation_id: Option<OperationId>,
        kind: TaskEventKind,
    ) {
        self.events.push(TaskEvent {
            phase,
            turn,
            operation_id,
            kind,
        });
    }

    fn operation_id_for_key(&self, expected: &OperationKey) -> Option<OperationId> {
        self.events.iter().find_map(|event| match &event.kind {
            TaskEventKind::OperationPrepared(key) if key.as_ref() == expected => event.operation_id,
            _ => None,
        })
    }

    fn operation_ids(&self) -> Vec<OperationId> {
        self.events
            .iter()
            .filter_map(|event| {
                matches!(event.kind, TaskEventKind::OperationPrepared(_))
                    .then_some(event.operation_id)
                    .flatten()
            })
            .collect()
    }

    fn latest_projection(&self) -> Option<OperationProjection> {
        self.operation_ids().last().and_then(|id| self.project(*id))
    }

    fn project(&self, id: OperationId) -> Option<OperationProjection> {
        let prepared = self.events.iter().find(|event| {
            event.operation_id == Some(id)
                && matches!(event.kind, TaskEventKind::OperationPrepared(_))
        })?;
        let mut projection = OperationProjection::new(id, prepared.phase, prepared.turn);
        for event in self
            .events
            .iter()
            .filter(|event| event.operation_id == Some(id))
        {
            match event.kind {
                TaskEventKind::OperationPrepared(_) | TaskEventKind::ReplayRejected => {}
                TaskEventKind::SafetyAssessed { level, decision } => {
                    projection.safety_level = Some(level);
                    projection.safety_decision = Some(decision);
                    projection.state = ExecutionState::Assessed;
                }
                TaskEventKind::AuthorizationGranted(source) => {
                    projection.authorization = Some(source);
                    projection.denial = None;
                    projection.state = ExecutionState::Authorized;
                }
                TaskEventKind::AuthorizationDenied(reason) => {
                    projection.denial = Some(reason);
                    projection.state = ExecutionState::NotStarted;
                }
                TaskEventKind::ExecutionDispatched => {
                    projection.state = ExecutionState::Dispatching;
                }
                TaskEventKind::ExecutionNotStarted => {
                    projection.state = ExecutionState::NotStarted;
                }
                TaskEventKind::ExecutionStarted => {
                    projection.state = ExecutionState::Started;
                }
                TaskEventKind::StartUncertain => {
                    projection.state = ExecutionState::StartUncertain;
                }
                TaskEventKind::ExecutionFinished {
                    termination,
                    exit_code,
                } => {
                    projection.termination = Some(termination);
                    projection.exit_code = Some(exit_code);
                    projection.state = if termination == CommandTermination::Exited {
                        ExecutionState::Completed
                    } else {
                        ExecutionState::Incomplete
                    };
                }
                TaskEventKind::FeedbackRecorded {
                    evidence,
                    supports_observation,
                } => {
                    projection.evidence = Some(evidence);
                    projection.supports_observation = supports_observation;
                }
                TaskEventKind::RoundStarted
                | TaskEventKind::ResponseRejected
                | TaskEventKind::PhaseChanged { .. } => {}
            }
        }
        Some(projection)
    }
}

fn termination_name(termination: CommandTermination) -> &'static str {
    match termination {
        CommandTermination::Exited => "exited",
        CommandTermination::OutputLimit => "output_limit",
        CommandTermination::BackgroundTerminated => "background_terminated",
        CommandTermination::SupervisionFailed => "supervision_failed",
        CommandTermination::StoppedTerminated => "stopped_terminated",
        CommandTermination::Interrupted => "interrupted",
    }
}

pub(super) fn output_evidence_name(evidence: OutputEvidence) -> &'static str {
    match evidence {
        OutputEvidence::Complete => "complete",
        OutputEvidence::Truncated => "truncated",
        OutputEvidence::Partial => "partial",
        OutputEvidence::Unavailable => "unavailable",
        OutputEvidence::CaptureFailed => "capture_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::CodecRuntime;
    use crate::shell::Shell;

    #[test]
    fn statically_bound_plan_identity_ignores_surface_whitespace() {
        let shell = Shell::from_startup(None, std::sync::Arc::new(CodecRuntime::load(None)));
        let first = shell.prepare_agent_command("pwd");
        let same = shell.prepare_agent_command("  pwd   ");
        let different = shell.prepare_agent_command("pwd extra");
        let mut log = TaskEventLog::new();

        assert_eq!(OperationKey::from(&first), OperationKey::from(&same));
        assert!(matches!(log.prepare(&first, 1, 1), PrepareOutcome::New(_)));
        assert!(matches!(
            log.prepare(&same, 1, 2),
            PrepareOutcome::Duplicate(_)
        ));
        assert!(matches!(
            log.prepare(&different, 1, 3),
            PrepareOutcome::New(_)
        ));
    }
}
