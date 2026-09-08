//! zhsh 的命令执行门面。
//!
//! `Shell` 组合会话状态、命令路由和 Bash 执行器，不再承载各层的具体实现。

mod agent_compound;
mod agent_plan;
mod builtin;
mod command;
mod executor;
mod safety_management;
mod session;
mod trust;

use crate::application::{CodecManagementUi, LlmConfigUi};
use crate::common::{AppError, AppResult, CancellationToken};
use crate::llm;
use command::{BuiltinPipelineSourceDisposition, Origin};
use executor::{BashExecutor, CommandExecutor};
use std::env;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

pub(crate) use agent_compound::{
    BoundCommand, BoundExpression, BoundPath, BoundRedirection, BoundTarget, OutputMode,
    QueryBuiltin, StandardStream,
};
pub(crate) use agent_plan::{
    AgentCommandPlan, AgentExecutionTarget, CommandTargetKind, ExecutableBinding,
    ResolvedInvocation, UnsupportedExecution,
};
pub(crate) use executor::{
    CapturedExecution, CommandTermination, OutputEvidence, AGENT_TASK_FEEDBACK_LIMIT,
};
pub(crate) use safety_management::{
    SafetyAssessmentView, SafetyCatalogView, SafetyInstallEntryView, SafetyInstallOutcomeView,
    SafetyInstallPlan, SafetyInstallReport, SafetyInstallRequest, SafetyInstallStateView,
    SafetyInstalledEntryView, SafetyManagementPort, SafetyManagementUi, SafetyOperationReport,
    SafetyOverwriteDecision, SafetyOverwritePrompt, SafetyRuleRowView, SafetyRuleSourceView,
    SafetyRuleStatusView, SafetySourceValidationView, SafetyTargetView,
};
pub(crate) use session::SessionState;
pub(crate) use trust::AgentTrust;

/// 面向 REPL 和 Agent 的会话执行门面。
pub(crate) struct Shell {
    state: SessionState,
    codec_runtime: Arc<llm::CodecRuntime>,
    executor: BashExecutor,
    startup_errors: Vec<AppError>,
    llm_config_ui: Option<Box<dyn LlmConfigUi>>,
    codec_management_ui: Option<Box<dyn CodecManagementUi>>,
    safety_management_ui: Option<Box<dyn SafetyManagementUi>>,
    safety_management: Option<Arc<dyn SafetyManagementPort>>,
}

impl Shell {
    /// 从进程环境、工作目录和已经验证的用户状态根创建执行门面。
    ///
    /// 活动配置恢复失败不会阻止构造；错误暂存并由 [`Self::take_startup_errors`] 交给
    /// 入口层展示。
    pub(crate) fn from_startup(
        user_home: Option<&Path>,
        codec_runtime: Arc<llm::CodecRuntime>,
    ) -> Self {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let (env_map, skipped_environment) = session::process_environment();
        let mut startup_errors = Vec::new();
        if skipped_environment != 0 {
            startup_errors.push(AppError::input(format!(
                "已跳过 {skipped_environment} 个非 UTF-8 环境变量"
            )));
        }
        let mut active_config_error = None;
        let mut active_config_name = None;
        let llm = match user_home {
            Some(home) => match llm::load_active_profile(home, &codec_runtime) {
                Ok(Some(profile)) => {
                    active_config_name = Some(profile.draft.name.clone());
                    match profile.readiness {
                        llm::LlmProfileReadiness::Ready(config) => Some(config),
                        llm::LlmProfileReadiness::Incomplete(issues) => {
                            let error = AppError::input(format!(
                                "配置 {} 不完整: {}",
                                profile.draft.name,
                                profile_issue_summary(&issues)
                            ));
                            active_config_error = Some(error.to_string());
                            startup_errors.push(error);
                            None
                        }
                    }
                }
                Ok(None) => None,
                Err(error) => {
                    active_config_error = Some(error.to_string());
                    startup_errors.push(error);
                    None
                }
            },
            None => None,
        };
        let mut state = SessionState::new(env_map, cwd, llm, user_home.map(Path::to_path_buf));
        if let Some(error) = active_config_error {
            state.commit_unavailable_llm(active_config_name, error);
        }
        Self {
            state,
            codec_runtime,
            executor: BashExecutor::default(),
            startup_errors,
            llm_config_ui: None,
            codec_management_ui: None,
            safety_management_ui: None,
            safety_management: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new() -> Self {
        let home = std::env::var_os("HOME")
            .filter(|value| value.to_str().is_some())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let codecs = Arc::new(llm::CodecRuntime::load(home.as_deref()));
        Self::from_startup(home.as_deref(), codecs)
    }

    /// 返回共享的 Codec generation 运行时 handle。
    pub(crate) fn codec_runtime(&self) -> &Arc<llm::CodecRuntime> {
        &self.codec_runtime
    }

    /// 在新 Agent 任务边界接纳其他 zhsh 进程已经提交的 Codec revision。
    pub(crate) fn refresh_codecs_for_agent(
        &mut self,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        if !self.codec_runtime.refresh_if_stale(cancellation)? {
            return Ok(());
        }
        let home = self
            .state
            .user_home()
            .ok_or_else(|| AppError::input("用户 HOME 不可用"))?;
        match llm::load_active_profile(home, &self.codec_runtime) {
            Ok(Some(profile)) => match profile.readiness {
                llm::LlmProfileReadiness::Ready(config) => {
                    self.state.commit_ready_llm(config);
                    Ok(())
                }
                llm::LlmProfileReadiness::Incomplete(issues) => {
                    let reason = format!(
                        "配置 {} 不完整: {}",
                        profile.draft.name,
                        profile_issue_summary(&issues)
                    );
                    self.state
                        .commit_unavailable_llm(Some(profile.draft.name), reason.clone());
                    Err(AppError::input(reason))
                }
            },
            Ok(None) => {
                self.state.clear_active_llm();
                Ok(())
            }
            Err(error) => {
                self.state.commit_unavailable_llm(None, error.to_string());
                Err(error)
            }
        }
    }

    /// 注入仅由交互入口实现的 LLM 配置界面端口。
    pub(crate) fn set_llm_config_ui(&mut self, ui: Box<dyn LlmConfigUi>) {
        self.llm_config_ui = Some(ui);
    }

    /// 注入只由交互入口实现的 Codec 首次发布者信任确认端口。
    pub(crate) fn set_codec_management_ui(&mut self, ui: Box<dyn CodecManagementUi>) {
        self.codec_management_ui = Some(ui);
    }

    /// 注入只由交互入口实现的 Safety 覆盖确认端口。
    pub(crate) fn set_safety_management_ui(&mut self, ui: Box<dyn SafetyManagementUi>) {
        self.safety_management_ui = Some(ui);
    }

    /// 注入只由组合根持有的进程级 Safety 管理端口。
    pub(crate) fn set_safety_management(&mut self, port: Arc<dyn SafetyManagementPort>) {
        self.safety_management = Some(port);
    }

    /// 返回指定 PATH 中稳定排序、去重的可执行文件名。
    pub(crate) fn executable_names(path_value: &str, cwd: &Path) -> Vec<String> {
        command::resolver::executable_names(path_value, cwd)
    }

    /// 按注册顺序返回所有内建命令名。
    pub(crate) fn builtin_names() -> impl Iterator<Item = &'static str> {
        command::names()
    }

    /// 判断一个内建命令的补全结果后是否应追加空格。
    pub(crate) fn builtin_takes_arguments(name: &str) -> bool {
        command::takes_arguments(name)
    }

    /// 用 Bash 只读语法检查判断当前交互输入是否确实需要 PS2 续行。
    pub(crate) fn input_needs_continuation(input: &str) -> bool {
        if !may_need_continuation(input) {
            return false;
        }
        let Ok(output) = Command::new("bash")
            .args(["--noprofile", "--norc", "-n", "-c", input])
            .env_clear()
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        else {
            return false;
        };
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        diagnostic.contains("unexpected end of file")
            || diagnostic.contains("unexpected EOF while looking for")
            || diagnostic.contains("delimited by end-of-file")
    }

    /// 返回 `zh trust` 使用的规范化授信等级名称。
    pub(crate) fn agent_trust_values() -> &'static [&'static str] {
        &AgentTrust::VALUES
    }

    /// 取走启动期间积累的非致命错误。
    pub(crate) fn take_startup_errors(&mut self) -> Vec<AppError> {
        std::mem::take(&mut self.startup_errors)
    }

    /// 执行不需要历史快照的用户命令。
    ///
    /// # Arguments
    ///
    /// - `input`：来自管道模式或内部启动流程的完整输入。
    ///
    /// # Returns
    ///
    /// 内建命令结果或 Bash 退出状态；执行基础设施错误映射为 `1`。
    pub(crate) fn run(&mut self, input: &str) -> i32 {
        self.run_with_history(input, &[])
    }

    /// 执行用户命令，并向 `history` 内建命令提供编辑器历史快照。
    ///
    /// # Arguments
    ///
    /// - `input`：未经执行的完整用户输入。
    /// - `history`：rustyline 当前历史的只读副本；其他命令忽略它。
    ///
    /// # Returns
    ///
    /// 命令退出状态，同时写入 [`SessionState::last_exit`]。
    pub(crate) fn run_with_history(&mut self, input: &str, history: &[String]) -> i32 {
        let input = input.trim();
        if input.is_empty() {
            return 0;
        }

        if let Some(result) = command::assign_prompt_variables(&mut self.state, input) {
            result.emit();
            self.state.last_exit = result.code;
            return result.code;
        }

        if let Some(result) = command::dispatch(
            &mut self.state,
            input,
            Origin::User,
            history,
            command::DispatchPorts {
                codec_runtime: Some(&self.codec_runtime),
                llm_config_ui: self.llm_config_ui.as_deref(),
                codec_management_ui: self.codec_management_ui.as_deref(),
                safety_management_ui: self.safety_management_ui.as_deref(),
                safety_management: self.safety_management.as_deref(),
                foreground_control: Some(&mut self.executor),
            },
        ) {
            result.emit();
            self.state.last_exit = result.code;
            return result.code;
        }

        let code = match command::pipeline_source::analyze(input) {
            command::pipeline_source::BuiltinPipelineSourceAnalysis::NotCandidate => {
                self.run_interactive_bash(input)
            }
            command::pipeline_source::BuiltinPipelineSourceAnalysis::Invalid(error) => {
                eprintln!("zhsh: {error}");
                2
            }
            command::pipeline_source::BuiltinPipelineSourceAnalysis::Candidate(plan) => {
                match plan.disposition {
                    BuiltinPipelineSourceDisposition::BashSubshell => {
                        self.run_interactive_bash(input)
                    }
                    BuiltinPipelineSourceDisposition::Reject(reason) => {
                        eprintln!("zhsh: {reason}");
                        2
                    }
                    BuiltinPipelineSourceDisposition::InternalOutput => {
                        let result = command::dispatch_pipeline_source_words(
                            &mut self.state,
                            &plan.name,
                            &plan.arguments,
                            history,
                            &self.codec_runtime,
                            self.llm_config_ui.as_deref(),
                            self.safety_management.as_deref(),
                        )
                        .expect("管道源计划中的 builtin 必须仍在注册表中");
                        if result.code != 0 {
                            result.emit();
                            result.code
                        } else {
                            eprint!("{}", result.stderr);
                            self.executor
                                .run_interactive_with_input(
                                    &self.state,
                                    &plan.tail,
                                    result.stdout.as_bytes(),
                                )
                                .unwrap_or_else(|error| {
                                    eprintln!("zhsh: {error}");
                                    1
                                })
                        }
                    }
                }
            }
        };
        self.state.last_exit = code;
        code
    }

    fn run_interactive_bash(&mut self, input: &str) -> i32 {
        self.executor
            .run_interactive(&self.state, input)
            .unwrap_or_else(|error| {
                eprintln!("zhsh: {error}");
                1
            })
    }

    /// 把模型命令解析为后续分类、确认和执行共享的不可变计划。
    pub(crate) fn prepare_agent_command(&self, input: &str) -> AgentCommandPlan {
        AgentCommandPlan::prepare(&self.state, input)
    }

    /// 前台交互程序独占键盘；Agent 不得在其运行期间监听 Esc。
    pub(crate) fn agent_plan_requires_terminal(&self, plan: &AgentCommandPlan) -> bool {
        executor::requires_foreground_terminal(&plan.original)
    }

    /// 执行已经批准的 Agent 命令计划。
    ///
    /// # Arguments
    ///
    /// - `plan`：由 [`Self::prepare_agent_command`] 生成并已经过安全判断的计划。
    /// - `cancellation`：覆盖路由、创建子进程和等待过程的任务令牌。
    ///
    /// # Returns
    ///
    /// 返回捕获输出和退出状态；启动前或执行中取消返回 [`None`]。计划的 cwd、PATH
    /// 或外部文件身份发生变化时失败关闭，绝不重新解析为另一个执行目标。
    pub(crate) fn execute_agent_plan(
        &mut self,
        plan: AgentCommandPlan,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled());
        }
        if let Some(unsupported) = &plan.unsupported_execution {
            return Err(AppError::input(unsupported.reason()));
        }
        if self.state.cwd != plan.cwd
            || self.state.env.get("PATH").map(std::ffi::OsString::from) != plan.path_snapshot
            || !plan.external_identity_is_current(&self.state.env)
        {
            return Err(AppError::input(
                "Agent 命令计划已失效（PlanStale）；目标、cwd 或 PATH 已改变，请重新准备并确认",
            ));
        }
        let result = match plan.executable {
            AgentExecutionTarget::ZhshBuiltin { name, arguments } => {
                let result = cancellation
                    .run_if_active(|| {
                        command::dispatch_agent_words(
                            &mut self.state,
                            &name,
                            &arguments,
                            &self.codec_runtime,
                        )
                    })
                    .ok_or_else(AppError::cancelled)?
                    .ok_or_else(|| AppError::internal("计划中的 zhsh builtin 已从注册表消失"))?;
                let output = result.combined();
                Some(CapturedExecution {
                    total_output_bytes: output.len(),
                    output,
                    exit_code: result.code,
                    termination: CommandTermination::Exited,
                    output_evidence: OutputEvidence::Complete,
                })
            }
            AgentExecutionTarget::External {
                path, arguments, ..
            } => self.executor.run_agent_external(
                &self.state,
                &path,
                &arguments,
                &plan.original,
                cancellation,
            )?,
            AgentExecutionTarget::BoundCompound { expression } => {
                let mode = executor::agent_stdio_mode(&plan.original);
                self.execute_bound_expression(&expression, mode, cancellation)?
            }
            AgentExecutionTarget::Bash { script } => {
                self.executor
                    .run_agent_prepared(&self.state, &script, cancellation)?
            }
        };
        if let Some(result) = &result {
            self.state.last_exit = result.exit_code;
        }
        Ok(result)
    }

    fn execute_bound_expression(
        &mut self,
        expression: &BoundExpression,
        mode: executor::AgentStdioMode,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        let mut remaining_output = executor::AGENT_COMMAND_OUTPUT_LIMIT;
        self.execute_bound_expression_with_budget(
            expression,
            &mut remaining_output,
            mode,
            cancellation,
        )
    }

    fn execute_bound_expression_with_budget(
        &mut self,
        expression: &BoundExpression,
        remaining_output: &mut usize,
        mode: executor::AgentStdioMode,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        if mode == executor::AgentStdioMode::Capture && *remaining_output == 0 {
            return Ok(Some(compound_output_limit()));
        }
        match expression {
            BoundExpression::Command(command) => {
                self.execute_bound_command(command, remaining_output, mode, cancellation)
            }
            BoundExpression::Pipeline(commands) => {
                let result = self.executor.run_agent_bound_pipeline(
                    &self.state,
                    commands,
                    *remaining_output,
                    mode,
                    cancellation,
                )?;
                Ok(charge_output_budget(result, remaining_output, mode))
            }
            BoundExpression::And(left, right) => {
                let Some(left_result) = self.execute_bound_expression_with_budget(
                    left,
                    remaining_output,
                    mode,
                    cancellation,
                )?
                else {
                    return Ok(None);
                };
                if left_result.termination != CommandTermination::Exited
                    || left_result.exit_code != 0
                {
                    return Ok(Some(left_result));
                }
                let Some(right_result) = self.execute_bound_expression_with_budget(
                    right,
                    remaining_output,
                    mode,
                    cancellation,
                )?
                else {
                    return Ok(None);
                };
                Ok(Some(combine_executions(left_result, right_result)))
            }
            BoundExpression::Or(left, right) => {
                let Some(left_result) = self.execute_bound_expression_with_budget(
                    left,
                    remaining_output,
                    mode,
                    cancellation,
                )?
                else {
                    return Ok(None);
                };
                if left_result.termination != CommandTermination::Exited
                    || left_result.exit_code == 0
                {
                    return Ok(Some(left_result));
                }
                let Some(right_result) = self.execute_bound_expression_with_budget(
                    right,
                    remaining_output,
                    mode,
                    cancellation,
                )?
                else {
                    return Ok(None);
                };
                Ok(Some(combine_executions(left_result, right_result)))
            }
            BoundExpression::Sequence(expressions) => {
                let mut combined = None;
                for expression in expressions {
                    let Some(result) = self.execute_bound_expression_with_budget(
                        expression,
                        remaining_output,
                        mode,
                        cancellation,
                    )?
                    else {
                        return Ok(None);
                    };
                    let finished_abnormally = result.termination != CommandTermination::Exited;
                    combined = Some(match combined {
                        Some(previous) => combine_executions(previous, result),
                        None => result,
                    });
                    if finished_abnormally {
                        break;
                    }
                }
                Ok(combined)
            }
        }
    }

    fn execute_bound_command(
        &mut self,
        command: &BoundCommand,
        remaining_output: &mut usize,
        mode: executor::AgentStdioMode,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<CapturedExecution>> {
        match command.target {
            BoundTarget::External { .. } => self
                .executor
                .run_agent_bound_pipeline(
                    &self.state,
                    std::slice::from_ref(command),
                    *remaining_output,
                    mode,
                    cancellation,
                )
                .map(|result| charge_output_budget(result, remaining_output, mode)),
            BoundTarget::ZhshQueryBuiltin(query) => {
                let arguments: Vec<_> = command
                    .arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect();
                let result = cancellation
                    .run_if_active(|| match query {
                        QueryBuiltin::Pwd => command::dispatch_agent_words(
                            &mut self.state,
                            "pwd",
                            &arguments,
                            &self.codec_runtime,
                        ),
                        QueryBuiltin::Type => command::dispatch_agent_words(
                            &mut self.state,
                            "type",
                            &arguments,
                            &self.codec_runtime,
                        ),
                        QueryBuiltin::CommandV => Some(self.query_command_v(&arguments)),
                        QueryBuiltin::LiteralEcho => Some(builtin::BuiltinResult {
                            stdout: format!("{}\n", arguments.join(" ")),
                            stderr: String::new(),
                            code: 0,
                        }),
                    })
                    .ok_or_else(AppError::cancelled)?
                    .ok_or_else(|| AppError::internal("绑定的查询 builtin 已从注册表消失"))?;
                if mode != executor::AgentStdioMode::Capture {
                    result.emit();
                }
                let captured = result.combined();
                let (output, total_output_bytes, output_evidence) =
                    if mode == executor::AgentStdioMode::Inherit {
                        (String::new(), 0, OutputEvidence::Unavailable)
                    } else {
                        let total = captured.len();
                        (captured, total, OutputEvidence::Complete)
                    };
                let result = Some(CapturedExecution {
                    total_output_bytes,
                    output,
                    exit_code: result.code,
                    termination: CommandTermination::Exited,
                    output_evidence,
                });
                Ok(charge_output_budget(result, remaining_output, mode))
            }
        }
    }

    fn query_command_v(&mut self, arguments: &[String]) -> builtin::BuiltinResult {
        let names = arguments
            .iter()
            .skip_while(|argument| argument.starts_with('-'));
        let mut stdout = String::new();
        let mut stderr = String::new();
        for name in names {
            if self.state.aliases.contains_key(name)
                || self.state.functions.contains_key(name)
                || command::is_builtin(name)
            {
                stdout.push_str(name);
                stdout.push('\n');
            } else if let Some(path) =
                command::resolver::executable_paths(&self.state, name).first()
            {
                stdout.push_str(&path.display().to_string());
                stdout.push('\n');
            } else {
                stderr.push_str(&format!("command: {name}: 未找到\n"));
            }
        }
        builtin::BuiltinResult {
            stdout,
            stderr: stderr.clone(),
            code: i32::from(!stderr.is_empty()),
        }
    }

    #[cfg(test)]
    pub(crate) fn run_agent_command(
        &mut self,
        input: &str,
        cancellation: &CancellationToken,
    ) -> Option<CapturedExecution> {
        let plan = self.prepare_agent_command(input);
        match self.execute_agent_plan(plan, cancellation) {
            Ok(result) => result,
            Err(error) if error.kind() == crate::common::ErrorKind::Cancelled => None,
            Err(error) => {
                let output = format!("zhsh: {error}\n");
                Some(CapturedExecution {
                    total_output_bytes: output.len(),
                    output,
                    exit_code: 1,
                    termination: CommandTermination::Exited,
                    output_evidence: OutputEvidence::Complete,
                })
            }
        }
    }

    /// 加载 Bash 启动状态和可选的 `~/.zhshrc`。
    ///
    /// 别名及 Bash 实际定义的 `command_not_found_handle` 导入属于尽力而为；启动文件
    /// 存在时通过 `source` 内建路径执行，使可持续状态经过完整快照协议提交到当前会话。
    pub(crate) fn load_rc(&mut self) {
        let Some(home) = self.state.user_home().map(Path::to_path_buf) else {
            return;
        };
        self.executor.import_bash_startup_state(&mut self.state);
        let zhshrc = home.join(".zhshrc");
        if zhshrc.exists() {
            let value = zhshrc.to_string_lossy().replace('\'', "'\\''");
            self.run(&format!("source '{value}'"));
        }
    }
}

fn profile_issue_summary(issues: &[llm::LlmProfileIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("{}: {}", issue.field, issue.message))
        .collect::<Vec<_>>()
        .join("；")
}

fn may_need_continuation(input: &str) -> bool {
    input.ends_with('\\')
        || input.contains([
            '\'', '"', '`', '\n', '|', '&', '(', ')', '{', '}', '[', ']', '<', '>',
        ])
        || input
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|word| {
                matches!(
                    word,
                    "if" | "then"
                        | "elif"
                        | "else"
                        | "for"
                        | "while"
                        | "until"
                        | "do"
                        | "case"
                        | "select"
                        | "function"
                )
            })
}

fn combine_executions(mut left: CapturedExecution, right: CapturedExecution) -> CapturedExecution {
    let limit = executor::AGENT_COMMAND_FEEDBACK_LIMIT;
    const TRUNCATED: &str = "\n[zhsh: 组合命令输出已截断]\n";
    let mut combined_truncated = false;
    if left.output.len() < limit {
        let remaining = limit - left.output.len();
        if right.output.len() <= remaining {
            left.output.push_str(&right.output);
        } else {
            combined_truncated = true;
            let mut boundary = remaining.saturating_sub(TRUNCATED.len());
            while boundary > 0 && !right.output.is_char_boundary(boundary) {
                boundary -= 1;
            }
            left.output.push_str(&right.output[..boundary]);
            if TRUNCATED.len() <= limit - left.output.len() {
                left.output.push_str(TRUNCATED);
            }
        }
    } else if !right.output.is_empty() {
        combined_truncated = true;
    }
    left.total_output_bytes = left
        .total_output_bytes
        .saturating_add(right.total_output_bytes);
    left.exit_code = right.exit_code;
    if left.termination == CommandTermination::Exited {
        left.termination = right.termination;
    }
    left.output_evidence = left.output_evidence.merge(right.output_evidence);
    if combined_truncated && left.output_evidence.supports_observation() {
        left.output_evidence = OutputEvidence::Truncated;
    }
    left
}

fn charge_output_budget(
    result: Option<CapturedExecution>,
    remaining: &mut usize,
    mode: executor::AgentStdioMode,
) -> Option<CapturedExecution> {
    let mut result = result?;
    if mode != executor::AgentStdioMode::Capture {
        return Some(result);
    }
    let available = *remaining;
    *remaining = available.saturating_sub(result.total_output_bytes);
    if result.total_output_bytes > available && result.termination == CommandTermination::Exited {
        result.termination = CommandTermination::OutputLimit;
        result.exit_code = 125;
        result.output_evidence = OutputEvidence::Partial;
    }
    Some(result)
}

fn compound_output_limit() -> CapturedExecution {
    CapturedExecution {
        output: "[zhsh: 组合命令累计输出已达到 1 MiB 上限]\n".into(),
        total_output_bytes: 0,
        exit_code: 125,
        termination: CommandTermination::OutputLimit,
        output_evidence: OutputEvidence::Partial,
    }
}

impl Deref for Shell {
    type Target = SessionState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for Shell {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{
        ConfigRecord, LlmConfigAction, LlmConfigDecision, LlmConfigUiError, SaveMode,
    };
    use crate::llm::{LlmConfig, ModelTier, ModelTiers, PluginSummary};

    struct FakeLlmConfigUi;

    struct FakeSafetyManagement;

    impl SafetyManagementPort for FakeSafetyManagement {
        fn catalog(&self) -> SafetyCatalogView {
            SafetyCatalogView {
                generation: 7,
                rows: vec![SafetyRuleRowView {
                    order: 1,
                    source: SafetyRuleSourceView::Local,
                    name: "java-tools".into(),
                    program: "java".into(),
                    rule_count: Some(2),
                    builtin: false,
                    status: SafetyRuleStatusView::Loaded,
                    modified: None,
                    location_or_diagnostic: "/usr/share/zhsh/java.zhse.json".into(),
                }],
            }
        }

        fn assess(&self, _: AgentCommandPlan, _: AgentTrust) -> SafetyAssessmentView {
            SafetyAssessmentView {
                targets: vec![SafetyTargetView {
                    program: "java".into(),
                    target: Some(PathBuf::from("/usr/bin/java")),
                }],
                semantic: "read_only",
                rule: "local/java:version".into(),
                binding: "static_system_trusted",
                decision: "auto_execute",
                reason: "当前策略要求确认".into(),
            }
        }

        fn test_candidate(&self) -> SafetyOperationReport {
            fake_safety_operation()
        }

        fn test_source(&self, _: PathBuf) -> SafetyOperationReport {
            fake_safety_operation()
        }

        fn reload(&self) -> SafetyOperationReport {
            fake_safety_operation()
        }

        fn plan_install(&self, _: Vec<PathBuf>) -> SafetyInstallPlan {
            unreachable!("测试不会安装 Safety 规则")
        }

        fn install(&self, _: SafetyInstallRequest) -> SafetyInstallReport {
            unreachable!("测试不会安装 Safety 规则")
        }
    }

    fn fake_safety_operation() -> SafetyOperationReport {
        SafetyOperationReport {
            success: true,
            generation: 7,
            local_rules: 1,
            shadowed_builtin_programs: 0,
            warnings: Vec::new(),
            errors: Vec::new(),
            source: None,
        }
    }

    impl LlmConfigUi for FakeLlmConfigUi {
        fn collect(
            &self,
            action: &LlmConfigAction,
            _: &[ConfigRecord],
            _: &[PluginSummary],
        ) -> Result<LlmConfigDecision, LlmConfigUiError> {
            assert_eq!(action, &LlmConfigAction::Create { name: None });
            Ok(LlmConfigDecision {
                draft: crate::llm::LlmProfileDraft::from_config(&LlmConfig {
                    name: "injected-ui".into(),
                    url: "https://example.com".into(),
                    request_format: "openai@0.3.0".into(),
                    json_schema: crate::llm::JsonSchemaResolution::Off,
                    access_token: "test-token".into(),
                    models: ModelTiers {
                        flash: "fast".into(),
                        standard: "standard".into(),
                        max: "max".into(),
                    },
                    tier: ModelTier::Flash,
                }),
                mode: SaveMode::SaveAndActivate,
            })
        }

        fn confirm_repair(
            &self,
            _: &str,
            _: &[crate::llm::LlmProfileIssue],
        ) -> Result<bool, LlmConfigUiError> {
            Ok(false)
        }
    }

    #[test]
    fn ps2_is_requested_only_for_incomplete_bash_input() {
        assert!(Shell::input_needs_continuation("if true; then"));
        assert!(Shell::input_needs_continuation("echo 'unfinished"));
        assert!(Shell::input_needs_continuation("printf ok |"));
        assert!(Shell::input_needs_continuation("[[ -f Cargo.toml"));
        assert!(!Shell::input_needs_continuation("echo )"));
        assert!(!Shell::input_needs_continuation(
            "for value in one; do printf '%s\\n' \"$value\"; done"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn builtin_pipeline_sources_use_current_history_and_safety_state() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-builtin-pipeline-source-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let history_output = root.join("history");
        let safety_output = root.join("safety");
        let mut shell = Shell::new();
        shell.cwd = root.clone();
        shell.env.remove("BASH_ENV");
        shell.set_safety_management(Arc::new(FakeSafetyManagement));

        assert_eq!(
            shell.run_with_history(
                "history | grep cargo > history",
                &["pwd".into(), "cargo test".into(), "zh status".into()],
            ),
            0
        );
        assert_eq!(
            shell.run_with_history("zh safety | grep java > safety", &[]),
            0
        );

        assert_eq!(
            std::fs::read_to_string(history_output).unwrap(),
            "2  cargo test\n"
        );
        assert!(std::fs::read_to_string(safety_output)
            .unwrap()
            .contains("java-tools"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pipeline_state_policies_preserve_the_parent_session() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-builtin-pipeline-policy-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let bash_cwd = root.join("bash-cwd");
        let rejected_tail = root.join("rejected-tail");
        let mut shell = Shell::new();
        shell.cwd = root.clone();
        shell.env.remove("BASH_ENV");
        shell.directory_stack.push(PathBuf::from("/stack-entry"));

        assert_eq!(
            shell.run(&format!("cd / | pwd > '{}'", bash_cwd.display())),
            0
        );
        assert_eq!(shell.cwd, root);
        assert_eq!(
            std::fs::read_to_string(&bash_cwd).unwrap().trim_end(),
            root.to_string_lossy()
        );

        assert_eq!(
            shell.run(&format!("dirs -c | cat > '{}'", rejected_tail.display())),
            2
        );
        assert_eq!(shell.directory_stack, [PathBuf::from("/stack-entry")]);
        assert!(!rejected_tail.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn injected_llm_config_ui_drives_builtin_without_a_repl_dependency() {
        let home =
            std::env::temp_dir().join(format!("zhsh-injected-ui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        crate::llm::install_test_openai_codec(&home);
        let codecs = Arc::new(crate::llm::CodecRuntime::load(Some(&home)));
        let mut shell = Shell::from_startup(Some(&home), Arc::clone(&codecs));
        shell.set_llm_config_ui(Box::new(FakeLlmConfigUi));

        assert_eq!(shell.run("zh llm"), 0);
        assert_eq!(
            shell.llm.as_ref().map(|config| config.name.as_str()),
            Some("injected-ui")
        );
        assert_eq!(
            crate::llm::load_active(&home, &codecs)
                .unwrap()
                .map(|config| config.name),
            Some("injected-ui".into())
        );

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn zh_tier_updates_active_configuration() {
        let test_home = format!("/tmp/zhsh-tier-test-{}", std::process::id());
        let _ = std::fs::remove_dir_all(&test_home);
        let test_home_path = PathBuf::from(&test_home);
        crate::llm::install_test_openai_codec(&test_home_path);
        let codecs = Arc::new(crate::llm::CodecRuntime::load(Some(&test_home_path)));
        let mut shell = Shell::from_startup(Some(&test_home_path), Arc::clone(&codecs));
        shell.commit_ready_llm(LlmConfig {
            name: "test".into(),
            url: "https://example.com".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: "token".into(),
            models: ModelTiers {
                flash: "f".into(),
                standard: "s".into(),
                max: "m".into(),
            },
            tier: ModelTier::Flash,
        });
        let result = command::dispatch(
            &mut shell,
            "zh tier max",
            Origin::User,
            &[],
            command::DispatchPorts {
                codec_runtime: Some(&codecs),
                ..command::DispatchPorts::default()
            },
        )
        .unwrap();

        assert_eq!(result.code, 0);
        assert_eq!(result.stdout, "tier: max\n");
        assert_eq!(shell.llm.as_ref().unwrap().tier, ModelTier::Max);
        let _ = std::fs::remove_dir_all(test_home);
    }

    #[test]
    fn zh_trust_updates_the_session_and_status_without_persisting() {
        let mut shell = Shell::new();
        let test_home =
            std::env::temp_dir().join(format!("zhsh-trust-session-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&test_home);
        std::fs::create_dir_all(&test_home).unwrap();
        shell
            .env
            .insert("HOME".into(), test_home.to_string_lossy().into_owned());
        shell.state.set_user_home_for_test(Some(test_home.clone()));
        shell.env.remove(AgentTrust::ENVIRONMENT_KEY);

        let initial = command::dispatch(
            &mut shell,
            "zh trust",
            Origin::User,
            &[],
            command::DispatchPorts::default(),
        )
        .unwrap();
        assert_eq!(initial.stdout, "授信: balanced\n");

        let changed = command::dispatch(
            &mut shell,
            "zh trust trusted",
            Origin::User,
            &[],
            command::DispatchPorts::default(),
        )
        .unwrap();
        assert_eq!(changed.code, 0);
        assert_eq!(shell.agent_trust(), AgentTrust::Trusted);

        let status = command::dispatch(
            &mut shell,
            "zh status",
            Origin::User,
            &[],
            command::DispatchPorts::default(),
        )
        .unwrap();
        assert!(status.stdout.contains("授信: trusted\n"));
        assert!(!test_home.join(".zhshrc").exists());
        std::fs::remove_dir_all(test_home).unwrap();
    }

    #[test]
    fn agent_command_path_rejects_session_mutating_builtins() {
        let mut shell = Shell::new();
        let cancellation = CancellationToken::default();
        let original = shell.cwd.clone();

        let export_code = shell
            .run_agent_command("export UNIT_MODEL=unit-model", &cancellation)
            .unwrap()
            .exit_code;
        let alias_code = shell
            .run_agent_command("alias ll='ls -l'", &cancellation)
            .unwrap()
            .exit_code;
        let cd_code = shell
            .run_agent_command("cd .", &cancellation)
            .unwrap()
            .exit_code;

        assert_eq!(export_code, 1);
        assert_eq!(alias_code, 1);
        assert_eq!(cd_code, 1);
        assert!(!shell.env.contains_key("UNIT_MODEL"));
        assert!(!shell.aliases.contains_key("ll"));
        assert_eq!(shell.cwd, original);
    }

    #[test]
    fn zh_top_level_commands_are_handled_as_builtins() {
        let mut shell = Shell::new();
        shell.commit_ready_llm(LlmConfig {
            name: "test".into(),
            url: "https://example.com".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: "token".into(),
            models: ModelTiers {
                flash: "f".into(),
                standard: "s".into(),
                max: "m".into(),
            },
            tier: ModelTier::Flash,
        });

        let run = |shell: &mut Shell, input| {
            let result = command::dispatch(
                shell,
                input,
                Origin::User,
                &[],
                command::DispatchPorts::default(),
            )
            .unwrap();
            (result.combined(), result.code)
        };
        let (status, status_code) = run(&mut shell, "zh status");
        let (_, show_code) = run(&mut shell, "zh show");
        let (_, provider_code) = run(&mut shell, "zh provider");
        let (_, model_code) = run(&mut shell, "zh model");
        let (legacy, legacy_code) = run(&mut shell, "zh config show");
        let (_, legacy_llm_code) = run(&mut shell, "zh config.llm");
        let (_, legacy_list_code) = run(&mut shell, "zh list");

        assert_eq!(status_code, 0);
        assert_eq!(show_code, 1);
        assert_eq!(provider_code, 1);
        assert_eq!(model_code, 1);
        assert_eq!(legacy_code, 1);
        assert_eq!(legacy_llm_code, 1);
        assert_eq!(legacy_list_code, 1);
        assert!(status.contains("Codec: openai@0.3.0"));
        assert!(!status.contains("FORMAT:"));
        assert!(status.contains("URL:"));
        assert!(status.contains("授信:"));
        assert!(legacy.contains("未知命令"));
    }

    #[test]
    fn cancelled_agent_command_cannot_start_a_builtin_or_bash() {
        let mut shell = Shell::new();
        let original = shell.cwd.clone();
        let cancellation = CancellationToken::default();
        cancellation.cancel();

        assert!(shell.run_agent_command("cd /", &cancellation).is_none());
        assert!(shell
            .run_agent_command("printf should-not-run", &cancellation)
            .is_none());
        assert_eq!(shell.cwd, original);
    }

    #[cfg(unix)]
    #[test]
    fn agent_plan_rejects_an_external_target_replaced_after_preparation() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("zhsh-agent-plan-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("probe");
        std::fs::write(&executable, "#!/bin/sh\nprintf old\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut shell = Shell::new();
        shell
            .env
            .insert("PATH".into(), root.to_string_lossy().into_owned());
        let plan = shell.prepare_agent_command("probe");
        std::fs::rename(&executable, root.join("probe.old")).unwrap();
        std::fs::write(&executable, "#!/bin/sh\nprintf replacement\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

        let error = shell
            .execute_agent_plan(plan, &CancellationToken::default())
            .unwrap_err();
        assert!(error.to_string().contains("PlanStale"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn approved_alias_plan_executes_the_frozen_expansion_once() {
        let mut shell = Shell::new();
        shell.aliases.insert("probe".into(), "printf first".into());
        let plan = shell.prepare_agent_command("probe");
        shell.aliases.insert("probe".into(), "printf second".into());

        let result = shell
            .execute_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();

        assert_eq!(result.output, "first");
        assert_eq!(result.exit_code, 0);
    }

    #[cfg(unix)]
    #[test]
    fn bound_external_target_cannot_be_shadowed_by_exec_alias() {
        let mut shell = Shell::new();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        shell.aliases.insert("exec".into(), "printf alias".into());
        let plan = shell.prepare_agent_command("/usr/bin/printf fixed");

        let result = shell
            .execute_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();

        assert_eq!(result.output, "fixed");
        assert_eq!(result.exit_code, 0);
    }

    #[cfg(unix)]
    #[test]
    fn bound_external_target_is_started_without_a_second_bash_interpreter() {
        let root =
            std::env::temp_dir().join(format!("zhsh-agent-direct-external-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let sentinel = root.join("bash-env-was-loaded");
        let bash_env = root.join("bash-env");
        std::fs::write(&bash_env, format!("touch '{}'\n", sentinel.display())).unwrap();

        let mut shell = Shell::new();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        let plan = shell.prepare_agent_command("/usr/bin/printf direct");
        shell
            .env
            .insert("BASH_ENV".into(), bash_env.to_string_lossy().into_owned());

        let result = shell
            .execute_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();

        assert_eq!(result.output, "direct");
        assert!(!sentinel.exists(), "静态外部计划仍绕回 Bash 执行");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn bound_compound_executes_query_sequence_and_pipeline_without_bash() {
        let mut shell = Shell::new();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        let plan = shell.prepare_agent_command(
            "command -v sh; /usr/bin/printf 'hello\\n' | /usr/bin/tr a-z A-Z",
        );
        assert!(matches!(
            plan.executable,
            AgentExecutionTarget::BoundCompound { .. }
        ));

        let result = shell
            .execute_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();

        assert!(result.output.contains("/usr/bin/sh") || result.output.contains("/bin/sh"));
        assert!(result.output.ends_with("HELLO\n"), "{}", result.output);
        assert_eq!(result.exit_code, 0);

        let merged = shell
            .run_agent_command(
                "/usr/bin/ls /zhsh-definitely-missing 2>&1 | /usr/bin/grep zhsh-definitely-missing",
                &CancellationToken::default(),
            )
            .unwrap();
        assert_eq!(merged.exit_code, 0, "{}", merged.output);
        assert!(merged.output.contains("zhsh-definitely-missing"));

        let output_path =
            std::env::temp_dir().join(format!("zhsh-bound-output-{}", std::process::id()));
        let _ = std::fs::remove_file(&output_path);
        let plan = shell.prepare_agent_command(&format!(
            "/usr/bin/printf written > '{}'",
            output_path.display()
        ));
        shell
            .execute_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_to_string(&output_path).unwrap(), "written");
        std::fs::remove_file(output_path).unwrap();

        let stale_path =
            std::env::temp_dir().join(format!("zhsh-bound-output-stale-{}", std::process::id()));
        let _ = std::fs::remove_file(&stale_path);
        let stale_plan = shell.prepare_agent_command(&format!(
            "/usr/bin/printf replaced > '{}'",
            stale_path.display()
        ));
        std::fs::write(&stale_path, "appeared-after-approval").unwrap();
        let error = shell
            .execute_agent_plan(stale_plan, &CancellationToken::default())
            .unwrap_err();
        assert!(error.to_string().contains("PlanStale"));
        assert_eq!(
            std::fs::read_to_string(&stale_path).unwrap(),
            "appeared-after-approval"
        );
        std::fs::remove_file(stale_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_an_active_agent_command_interrupts_its_process_group() {
        use std::sync::{mpsc, Arc};
        use std::time::{Duration, Instant};

        let cancellation = Arc::new(CancellationToken::default());
        let runner_cancellation = Arc::clone(&cancellation);
        let (sender, receiver) = mpsc::channel();
        let runner = std::thread::spawn(move || {
            let mut shell = Shell::new();
            let result = shell.run_agent_command("sleep 30", &runner_cancellation);
            let _ = sender.send(result);
        });

        let spawn_deadline = Instant::now() + Duration::from_secs(2);
        while cancellation.active_process_group().is_none() {
            assert!(Instant::now() < spawn_deadline, "Agent 命令未能及时启动");
            std::thread::sleep(Duration::from_millis(5));
        }

        let cancelled_at = Instant::now();
        cancellation.cancel();
        let result = match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(_) => {
                if let Some(process_group) = cancellation.active_process_group() {
                    // SAFETY: 测试令牌只登记自己启动的独立进程组；负 PID 按 POSIX
                    // 向该组发送 SIGKILL，用于测试失败后的有界清理，不访问内存。
                    unsafe {
                        libc::kill(-(process_group as i32), libc::SIGKILL);
                    }
                }
                let _ = runner.join();
                panic!("Ctrl-C 未能及时终止 Agent 进程组");
            }
        };
        runner.join().unwrap();

        assert!(result.is_some());
        assert!(cancellation.is_cancelled());
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
    }
}
