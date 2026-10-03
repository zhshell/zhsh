//! Native 外部调用准备和 Shell 适配；会话仍由 Shell 持有。

use super::builtin::BuiltinResult;
use super::command::{self, resolver};
use super::{CommandTermination, OutputEvidence};

mod input;
mod source;
use super::executor::native as execution;
use super::{AgentCommandPlan, AgentExecutionTarget, CapturedExecution, Shell};
use crate::common::{AppError, CancellationToken};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum NativePreparationError {
    EmptyInput,
    InvalidInput(String),
    NotFound,
    CannotExecute(String),
    ReadFailed(String),
    JobTarget(String),
}
impl NativePreparationError {
    pub(crate) fn code(&self) -> i32 {
        match self {
            Self::InvalidInput(_) | Self::EmptyInput => 2,
            Self::NotFound => 127,
            Self::CannotExecute(_) => 126,
            Self::ReadFailed(_) | Self::JobTarget(_) => 1,
        }
    }
    pub(crate) fn is_failure(&self) -> bool {
        matches!(self, Self::ReadFailed(_))
    }
}
impl std::fmt::Display for NativePreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(s)
            | Self::CannotExecute(s)
            | Self::ReadFailed(s)
            | Self::JobTarget(s) => f.write_str(s),
            Self::EmptyInput => f.write_str("Native run 没有可执行命令"),
            Self::NotFound => f.write_str("Native 未找到可执行程序"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeNotStartedReason {
    PlanStale,
    InvalidRequest,
    #[cfg(not(target_os = "linux"))]
    UnsupportedFormat,
    SpawnFailed,
    ExecutableNotFound,
}
#[derive(Debug)]
pub(crate) enum NativeExecutionError {
    NotStarted {
        reason: NativeNotStartedReason,
        error: AppError,
    },
    Execution(AppError),
}
impl NativeExecutionError {
    pub(super) fn not_started(reason: NativeNotStartedReason, message: impl Into<String>) -> Self {
        Self::NotStarted {
            reason,
            error: AppError::input(message),
        }
    }
    fn code(&self) -> i32 {
        if matches!(
            self,
            Self::NotStarted {
                reason: NativeNotStartedReason::ExecutableNotFound,
                ..
            }
        ) {
            127
        } else if matches!(self, Self::NotStarted { .. }) {
            126
        } else {
            1
        }
    }
}
impl std::fmt::Display for NativeExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted { error, .. } | Self::Execution(error) => error.fmt(f),
        }
    }
}

pub(super) struct PreparedNativeExternal {
    pub(super) original: String,
    pub(super) program: String,
    pub(super) arguments: Vec<OsString>,
    pub(super) target: resolver::AgentResolvedExecutable,
    pub(super) cwd: PathBuf,
    pub(super) path_snapshot: Option<OsString>,
}

fn prepare_words(
    original: String,
    words: &[String],
    cwd: &Path,
    environment: &HashMap<String, String>,
) -> Result<Option<PreparedNativeExternal>, NativePreparationError> {
    let Some((program, arguments)) = words.split_first() else {
        return Ok(None);
    };
    let path = environment.get("PATH").map(String::as_str);
    let (selected, _, _) =
        resolver::first_native_executable_path(cwd, path, program).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                NativePreparationError::NotFound
            } else {
                NativePreparationError::CannotExecute(format!("{program}: {error}"))
            }
        })?;
    let target = resolver::resolve_native_executable(cwd, path, program, environment)
        .ok_or_else(|| NativePreparationError::ReadFailed("无法绑定 Native 执行目标身份".into()))?;
    if target.resolved_path != selected {
        return Err(NativePreparationError::ReadFailed(
            "Native 目标在准备期间改变".into(),
        ));
    }
    Ok(Some(PreparedNativeExternal {
        original,
        program: program.clone(),
        arguments: arguments.iter().map(OsString::from).collect(),
        target,
        cwd: cwd.to_owned(),
        path_snapshot: path.map(OsString::from),
    }))
}

impl Shell {
    pub(crate) fn native_interrupt() {
        super::job::interrupt();
    }
    pub(crate) fn enable_native_jobs(&mut self, interactive: bool) {
        self.native_mode = true;
        self.native_jobs
            .get_or_init(|| super::job::JobRuntime::new(interactive));
    }
    fn jobs(&self) -> &super::job::JobRuntime {
        self.native_jobs
            .get_or_init(|| super::job::JobRuntime::new(false))
    }
    pub(crate) fn install_native_job_signals(&self) -> std::io::Result<()> {
        self.jobs().install_signals()
    }
    /// 使用进程启动时固定的用户状态根加载 Native 启动文件。
    pub(crate) fn load_native_startup_rc(&mut self) {
        let Some(home) = self.state.user_home().map(Path::to_path_buf) else {
            return;
        };
        let path = home.join(".zhshrc");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                let diagnostic = source::NativeStartupDiagnostic {
                    line: None,
                    reason: safe_diagnostic(&error.to_string()),
                };
                self.state.last_exit = 1;
                self.report_native_startup_rc_failure(&path, 1, Some(&diagnostic));
                return;
            }
            Ok(_) => {}
        }

        match self.run_native_source_path(&path, &[], None, 0, source::SourceOrigin::NativeStartup)
        {
            Ok(run) => {
                self.state.last_exit = run.execution.exit_code;
                self.report_native_startup_rc_failure(
                    &path,
                    run.execution.exit_code,
                    run.startup_diagnostic.as_ref(),
                );
            }
            Err(error) => {
                self.state.last_exit = error.code();
                let diagnostic = source::NativeStartupDiagnostic {
                    line: None,
                    reason: safe_diagnostic(&error.to_string()),
                };
                self.report_native_startup_rc_failure(&path, error.code(), Some(&diagnostic));
            }
        }
    }

    fn report_native_startup_rc_failure(
        &self,
        path: &Path,
        status: i32,
        diagnostic: Option<&source::NativeStartupDiagnostic>,
    ) {
        if self.state.should_exit || status == 0 {
            return;
        }

        let path = crate::common::terminal_safe_path(path);
        if let Some(diagnostic) = diagnostic {
            let location = diagnostic
                .line
                .map(|line| format!("{path}:{line}"))
                .unwrap_or(path);
            eprintln!(
                "zhsh: Native 启动配置未完整加载\n  位置：{location}\n  原因：{}\n  影响：错误后的配置行未执行；失败前已生效的配置效果不会回滚。Shell 将继续启动。\n  处理：修复配置后重启，或用 `zhsh --native --norc` 跳过自动加载。",
                safe_diagnostic(&diagnostic.reason)
            );
        } else {
            eprintln!(
                "zhsh: Native 启动配置执行结束，返回状态 {status}\n  文件：{path}\n  说明：配置执行效果会保留，Shell 将继续启动。\n  处理：检查配置命令，或用 `zhsh --native --norc` 跳过自动加载。"
            );
        }
    }
    pub(crate) fn set_native_notice_sink(
        &self,
        sink: std::sync::Arc<dyn Fn(String) + Send + Sync>,
    ) {
        self.jobs().set_notice_sink(sink);
    }
    pub(crate) fn native_job_failure(&self) -> Option<String> {
        self.native_jobs.get().and_then(|j| j.failure())
    }
    pub(crate) fn native_notifications(&self) -> Vec<String> {
        self.native_jobs
            .get()
            .map_or_else(Vec::new, |j| j.notifications())
    }
    pub(crate) fn native_eof(&mut self) -> bool {
        match self.jobs().exit(!self.native_exit_warned, false) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("zhsh: {e}");
                self.native_exit_warned = true;
                false
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn run_native(&mut self, input: &str) -> i32 {
        self.run_native_with_history(input, &[])
    }

    pub(crate) fn run_native_with_history(&mut self, input: &str, history: &[String]) -> i32 {
        let unit = match super::language::parse_single(input, None) {
            Ok(unit) => unit,
            Err(error) => return self.report_native_input_error(&error),
        };
        self.run_native_unit(unit, history)
    }

    pub(crate) fn run_native_unit(
        &mut self,
        unit: super::language::SimpleUnit,
        history: &[String],
    ) -> i32 {
        let plan = match self.prepare_native_unit(unit, false, None) {
            Ok(Some(plan)) => plan,
            Ok(None) => return self.state.last_exit,
            Err(error) => {
                eprintln!("zhsh: {}", safe_diagnostic(&error.to_string()));
                self.state.last_exit = error.code();
                return self.state.last_exit;
            }
        };
        let result = self.execute_native_plan(plan, history, None, 0);
        self.state.last_exit = match result {
            Ok(Some(result)) => result.exit_code,
            Ok(None) => self.state.last_exit,
            Err(error) => {
                eprintln!("zhsh: {}", safe_diagnostic(&error.to_string()));
                error.code()
            }
        };
        self.state.last_exit
    }

    #[cfg(test)]
    pub(crate) fn prepare_native_agent_command(
        &self,
        input: &str,
    ) -> Result<AgentCommandPlan, NativePreparationError> {
        self.prepare_native_command(input, true)
    }
    fn prepare_native_words(
        &self,
        original: &str,
        words: &[String],
        depth: usize,
        bind_jobs: bool,
    ) -> Result<AgentCommandPlan, NativePreparationError> {
        if depth > 16 {
            return Err(NativePreparationError::InvalidInput(
                "jobs -x: nesting limit".into(),
            ));
        }
        let Some((name, arguments)) = words.split_first() else {
            return Err(NativePreparationError::InvalidInput(
                "Native command 不能为空".into(),
            ));
        };
        if name == "jobs" && arguments.first().is_some_and(|a| a == "-x") {
            if arguments.len() < 2 {
                return Err(NativePreparationError::InvalidInput(
                    "jobs -x: command required".into(),
                ));
            }
            let mut inner = arguments[1..].to_vec();
            let mut bindings = Vec::new();
            for word in inner.iter_mut().skip(1) {
                if word.starts_with('%') {
                    let b = self
                        .jobs()
                        .resolve(word)
                        .map_err(NativePreparationError::InvalidInput)?;
                    bindings.push((word.clone(), b));
                    *word = b.pgid.to_string();
                }
            }
            let mut plan = self.prepare_native_words(original, &inner, depth + 1, bind_jobs)?;
            plan.job_bindings.extend(bindings);
            return Ok(plan);
        }
        if command::is_native_builtin(name) {
            let mut plan = AgentCommandPlan::from_native_builtin(
                original.to_owned(),
                name.clone(),
                arguments.to_vec(),
                self.state.cwd.clone(),
                self.state.env.get("PATH").map(OsString::from),
            );
            if bind_jobs && command::is_job_builtin(name) && name != "jobs" && name != "disown" {
                for a in arguments.iter().filter(|a| a.starts_with('%')) {
                    let b = self
                        .jobs()
                        .resolve(a)
                        .map_err(NativePreparationError::JobTarget)?;
                    plan.job_bindings.push((a.clone(), b));
                }
                if matches!(name.as_str(), "fg" | "bg" | "disown") && arguments.is_empty() {
                    let b = self
                        .jobs()
                        .resolve("%+")
                        .map_err(NativePreparationError::JobTarget)?;
                    plan.job_bindings.push(("%+".into(), b));
                }
            }
            if bind_jobs && name == "disown" {
                let context = super::builtin::JobContext {
                    runtime: self.jobs(),
                    bindings: &[],
                    pids: &[],
                    selection: None,
                };
                let selection = super::builtin::disown::select(&context, arguments)
                    .map_err(|e| NativePreparationError::JobTarget(e.stderr.trim().to_owned()))?;
                plan.job_bindings
                    .extend(selection.iter().map(|b| (format!("%{}", b.id), *b)));
                plan.job_selection = Some(selection);
            }
            if bind_jobs && name == "kill" {
                for pid in super::builtin::kill::pid_operands(arguments) {
                    plan.pid_bindings.push(
                        super::job::PidBinding::open(pid).map_err(|e| {
                            NativePreparationError::InvalidInput(format!("kill: {e}"))
                        })?,
                    );
                }
            }
            return Ok(plan);
        }
        let mut plan = prepare_words(original.to_owned(), words, &self.state.cwd, &self.state.env)?
            .map(AgentCommandPlan::from_native_external)
            .ok_or_else(|| {
                NativePreparationError::InvalidInput("Native command 不能为空".into())
            })?;
        plan.original = original.to_owned();
        Ok(plan)
    }

    pub(crate) fn record_native_preparation_failure(&mut self, error: &NativePreparationError) {
        if !matches!(error, NativePreparationError::EmptyInput) {
            self.state.last_exit = error.code();
        }
    }

    pub(crate) fn native_builtin_requires_confirmation(&self, plan: &AgentCommandPlan) -> bool {
        matches!(&plan.executable, AgentExecutionTarget::ZhshBuiltin { name, arguments }
            if !command::agent_allows(name, arguments))
    }

    pub(crate) fn native_agent_plan_requires_terminal(&self, plan: &AgentCommandPlan) -> bool {
        match (&plan.executable, plan.invocations.first()) {
            (AgentExecutionTarget::ZhshBuiltin { name, .. }, _) => {
                matches!(name.as_str(), "fg" | "suspend" | "zh" | "source" | ".")
            }
            (AgentExecutionTarget::External { arguments, .. }, Some(invocation)) => {
                execution::requires_terminal(&invocation.original, arguments)
            }
            _ => false,
        }
    }

    fn validate_native_plan<'a>(
        &self,
        plan: &'a AgentCommandPlan,
    ) -> Result<(&'a Path, &'a str, &'a [OsString]), NativeExecutionError> {
        let (
            AgentExecutionTarget::External {
                path, arguments, ..
            },
            Some(invocation),
        ) = (&plan.executable, plan.invocations.first())
        else {
            return Err(NativeExecutionError::not_started(
                NativeNotStartedReason::InvalidRequest,
                "Native 计划不是外部程序",
            ));
        };
        if self.state.cwd != plan.cwd
            || self.state.env.get("PATH").map(OsString::from) != plan.path_snapshot
            || !plan.external_identity_is_current(&self.state.env)
        {
            return Err(NativeExecutionError::not_started(
                NativeNotStartedReason::PlanStale,
                "Native 计划已失效（PlanStale），请重新准备并确认",
            ));
        }
        Ok((path, &invocation.original, arguments))
    }

    #[cfg(test)]
    pub(crate) fn execute_native_agent_plan(
        &mut self,
        plan: AgentCommandPlan,
        cancellation: &CancellationToken,
    ) -> Result<Option<CapturedExecution>, NativeExecutionError> {
        self.execute_native_agent_plan_with_history(plan, cancellation, &[])
    }

    pub(crate) fn execute_native_agent_plan_with_history(
        &mut self,
        plan: AgentCommandPlan,
        cancellation: &CancellationToken,
        history: &[String],
    ) -> Result<Option<CapturedExecution>, NativeExecutionError> {
        self.execute_native_plan(plan, history, Some(cancellation), 0)
    }

    fn execute_native_plan(
        &mut self,
        plan: AgentCommandPlan,
        history: &[String],
        cancellation: Option<&CancellationToken>,
        depth: usize,
    ) -> Result<Option<CapturedExecution>, NativeExecutionError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Ok(None);
        }
        if !matches!(&plan.executable, AgentExecutionTarget::ZhshBuiltin { name, .. } if name == "jobs" || name == "exit")
        {
            self.native_exit_warned = false;
        }
        let result = (|| {
            if plan.job_bindings.iter().any(|(_, b)| {
                if matches!(&plan.executable,AgentExecutionTarget::ZhshBuiltin{name,..} if name=="wait") { !self.jobs().waitable(b) } else { !self.jobs().valid(b) }
            }) {
                return Err(NativeExecutionError::not_started(
                    NativeNotStartedReason::PlanStale,
                    "Native 作业目标已失效（PlanStale）",
                ));
            }
            if cancellation.is_some() {
                if let Some(unsupported) = &plan.unsupported_execution {
                    return Err(NativeExecutionError::not_started(
                        NativeNotStartedReason::InvalidRequest,
                        unsupported.reason(),
                    ));
                }
            }
            if let AgentExecutionTarget::ZhshBuiltin { name, arguments } = &plan.executable {
                if self.state.cwd != plan.cwd
                    || self.state.env.get("PATH").map(OsString::from) != plan.path_snapshot
                {
                    return Err(NativeExecutionError::not_started(
                        NativeNotStartedReason::PlanStale,
                        "Native 内建计划上下文已改变（PlanStale）",
                    ));
                }
                if command::is_job_builtin(name) {
                    self.jobs();
                    let jobs = self.native_jobs.get().expect("initialized job runtime");
                    if name == "wait" {
                        let result = super::builtin::wait::execute_with_state(
                            jobs,
                            &mut self.state,
                            arguments,
                            cancellation,
                            &plan.job_bindings,
                        );
                        return Ok(Some(builtin_execution(
                            result,
                            cancellation.is_some(),
                            false,
                        )));
                    }
                    if plan.job_bindings.iter().any(|(_, b)| !jobs.valid(b)) {
                        return Err(NativeExecutionError::not_started(
                            NativeNotStartedReason::PlanStale,
                            "Native 作业目标已失效（PlanStale）",
                        ));
                    }
                    let context = super::builtin::JobContext {
                        runtime: jobs,
                        selection: plan.job_selection.as_deref(),
                        bindings: &plan.job_bindings,
                        pids: &plan.pid_bindings,
                    };
                    if name == "fg" {
                        if arguments.len() > 1 {
                            return Ok(Some(builtin_execution(
                                super::builtin::job_error(2, "fg: expected one jobspec"),
                                cancellation.is_some(),
                                false,
                            )));
                        }
                        let b =
                            match context.resolve(arguments.first().map_or("%+", String::as_str)) {
                                Ok(b) => b,
                                Err(e) if cancellation.is_none() => {
                                    return Ok(Some(builtin_execution(
                                        super::builtin::job_error(1, e),
                                        false,
                                        false,
                                    )))
                                }
                                Err(e) => {
                                    return Err(NativeExecutionError::not_started(
                                        NativeNotStartedReason::PlanStale,
                                        e,
                                    ))
                                }
                            };
                        return jobs.resume(&b, true, cancellation).map_err(|e| {
                            NativeExecutionError::Execution(AppError::io(format!("fg: {e}")))
                        });
                    }
                    let result =
                        command::dispatch_native_job(&context, name, arguments, cancellation);
                    return Ok(Some(builtin_execution(
                        result,
                        cancellation.is_some(),
                        name == "fg" || name == "suspend",
                    )));
                }
                if name == "exit" {
                    let result = super::builtin::exit::execute(&mut self.state, arguments);
                    if self.state.should_exit {
                        if let Err(e) = self.jobs().exit(!self.native_exit_warned, false) {
                            self.state.should_exit = false;
                            self.native_exit_warned = true;
                            return Ok(Some(builtin_execution(
                                BuiltinResult::error(format!("exit: {e}\n")),
                                cancellation.is_some(),
                                false,
                            )));
                        }
                    }
                    return Ok(Some(builtin_execution(
                        result,
                        cancellation.is_some(),
                        false,
                    )));
                }
                if matches!(name.as_str(), "source" | ".") {
                    return self
                        .run_native_source(arguments, history, cancellation, depth)
                        .map(Some);
                }
                // Agent reaches this dispatch only after its normal Safety/authorization gate.
                // Origin::User selects the existing builtin semantics, not the old Agent whitelist.
                let result = command::dispatch_native_words(
                    &mut self.state,
                    name,
                    arguments,
                    history,
                    command::DispatchPorts {
                        codec_runtime: Some(&self.codec_runtime),
                        llm_config_ui: self.llm_config_ui.as_deref(),
                        codec_management_ui: self.codec_management_ui.as_deref(),
                        safety_management_ui: self.safety_management_ui.as_deref(),
                        safety_management: self.safety_management.as_deref(),
                        foreground_control: Some(&mut self.executor),
                    },
                )
                .ok_or_else(|| {
                    NativeExecutionError::not_started(
                        NativeNotStartedReason::InvalidRequest,
                        "Native 内建未注册",
                    )
                })?;
                let opaque = matches!(name.as_str(), "fg" | "zh");
                return Ok(Some(builtin_execution(
                    result,
                    cancellation.is_some(),
                    opaque,
                )));
            }
            let (path, program, arguments) = self.validate_native_plan(&plan)?;
            let mode = execution::requires_terminal(program, arguments);
            let jobs = self.jobs();
            let launch = || {
                jobs.launch(
                    path,
                    program,
                    arguments,
                    &self.state.cwd,
                    &self.state.env,
                    &plan.original,
                    cancellation.is_some() && execution::captures_output(program, arguments),
                    cancellation.is_none() || mode,
                    false,
                    cancellation,
                )
            };
            let binding = launch().map_err(execution::spawn_error)?;
            jobs.wait_foreground(&binding, cancellation)
                .map(Some)
                .map_err(|e| NativeExecutionError::Execution(AppError::io(e.to_string())))
        })();
        if let Some(jobs) = self.native_jobs.get() {
            self.state.last_async_pid = jobs.last_async_pid();
        }
        match &result {
            Ok(Some(result)) => self.state.last_exit = result.exit_code,
            Err(error) => self.state.last_exit = error.code(),
            Ok(None) => {}
        }
        result
    }
}

fn builtin_execution(result: BuiltinResult, capture: bool, opaque: bool) -> CapturedExecution {
    let output = if capture {
        result.combined()
    } else {
        result.emit();
        String::new()
    };
    CapturedExecution {
        job: None,
        total_output_bytes: output.len(),
        output,
        exit_code: result.code,
        termination: CommandTermination::Exited,
        output_evidence: if opaque {
            OutputEvidence::Unavailable
        } else {
            OutputEvidence::Complete
        },
    }
}

fn safe_diagnostic(message: &str) -> String {
    message
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn native_resolves_session_path_and_rejects_stale_or_invalid_targets() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-native-bind-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        fs::copy("/usr/bin/true", root.join("b/probe")).unwrap();
        let mut shell = Shell::new();
        shell.cwd = root.clone();
        shell.env.insert("PATH".into(), "a:b".into());
        let plan = shell.prepare_native_agent_command("probe").unwrap();
        let AgentExecutionTarget::External { path, .. } = &plan.executable else {
            panic!()
        };
        assert_eq!(path, &root.join("b/probe"));
        // Creating a preceding candidate invalidates the approval; never execute the replacement.
        fs::copy("/usr/bin/false", root.join("a/probe")).unwrap();
        assert!(matches!(
            shell.execute_native_agent_plan(plan, &CancellationToken::default()),
            Err(NativeExecutionError::NotStarted {
                reason: NativeNotStartedReason::PlanStale,
                ..
            })
        ));
        fs::write(root.join("a/probe"), "printf unsafe > sentinel\n").unwrap();
        assert!(shell.prepare_native_agent_command("probe").is_ok());
        assert_eq!(shell.run_native("probe"), 126);
        assert!(!root.join("sentinel").exists());
        fs::remove_file(root.join("a/probe")).unwrap();
        symlink(root.join("b/probe"), root.join("a/probe")).unwrap();
        assert_eq!(shell.run_native("probe"), 0);
        let plan = shell.prepare_native_agent_command("probe").unwrap();
        fs::remove_file(root.join("a/probe")).unwrap();
        symlink("/usr/bin/false", root.join("a/probe")).unwrap();
        assert!(matches!(
            shell.execute_native_agent_plan(plan, &CancellationToken::default()),
            Err(NativeExecutionError::NotStarted { .. })
        ));
        shell.env.remove("PATH");
        fs::copy("/usr/bin/true", root.join("probe")).unwrap();
        assert_eq!(shell.run_native("probe"), 0);
        fs::set_permissions(root.join("probe"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(shell.run_native("probe"), 126);
        assert_eq!(shell.run_native(" \t "), 126);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_explicit_paths_keep_identity_and_downstream_checks() {
        let root = std::env::temp_dir().join(format!("zhsh-native-paths-{}", std::process::id()));
        fs::create_dir_all(root.join("child")).unwrap();
        let target = root.join("my app");
        fs::copy("/usr/bin/true", &target).unwrap();
        let mut shell = Shell::new();
        shell.cwd = root.join("child");
        shell.env.insert("PATH".into(), "/nonexistent".into());
        assert_eq!(shell.run_native("'../my app'"), 0);
        let plan = shell.prepare_native_agent_command("'../my app'").unwrap();
        fs::copy("/usr/bin/false", &target).unwrap();
        assert!(matches!(
            shell.execute_native_agent_plan(plan, &CancellationToken::default()),
            Err(NativeExecutionError::NotStarted {
                reason: NativeNotStartedReason::PlanStale,
                ..
            })
        ));
        fs::write(&target, "touch sentinel\n").unwrap();
        assert_eq!(shell.run_native("'../my app'"), 126);
        assert!(!shell.cwd.join("sentinel").exists());
        for input in ["/usr/bin/env true", "/usr/bin/find . -exec true ';'"] {
            let plan = shell.prepare_native_agent_command(input).unwrap();
            assert!(plan.invocations.iter().any(|invocation| {
                invocation.kind == super::super::CommandTargetKind::DynamicOrUnresolved
            }));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_agent_and_user_effects_share_the_session_directory() {
        let root = std::env::temp_dir().join(format!("zhsh-native-effects-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mut shell = Shell::new();
        shell.cwd = root.clone();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        let plan = shell
            .prepare_native_agent_command("touch from-agent")
            .unwrap();
        let result = shell
            .execute_native_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(root.join("from-agent").exists());
        assert_eq!(shell.run_native("touch from-user"), 0);
        let plan = shell.prepare_native_agent_command("ls from-user").unwrap();
        let result = shell
            .execute_native_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert!(result.output.contains("from-user"));
        assert_eq!(shell.cwd, root);
        for text in ["cd /", "export PATH=/", "source script"] {
            assert!(matches!(
                shell.prepare_native_agent_command(text).unwrap().executable,
                AgentExecutionTarget::ZhshBuiltin { .. }
            ));
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod builtin_tests {
    use super::*;
    use std::fs;

    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zhsh-native-builtins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("child")).unwrap();
        root
    }

    #[test]
    fn native_builtins_share_directory_environment_alias_and_history_state() {
        let root = root();
        let mut shell = Shell::new();
        shell.cwd = root.clone();
        shell
            .env
            .insert("HOME".into(), root.to_string_lossy().into_owned());
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        assert_eq!(shell.run_native("cd child"), 0);
        assert_eq!(shell.cwd, root.join("child"));
        let plan = shell.prepare_native_agent_command("cd ~").unwrap();
        assert_eq!(
            shell
                .execute_native_agent_plan(plan, &CancellationToken::default())
                .unwrap()
                .unwrap()
                .exit_code,
            0
        );
        assert_eq!(shell.cwd, root);
        assert_eq!(shell.run_native("pushd child"), 0);
        assert_eq!(shell.run_native("popd"), 0);
        assert_eq!(shell.cwd, root);
        assert_eq!(shell.run_native("export NATIVE_VALUE='a b'"), 0);
        assert_eq!(shell.run_native("export -n NATIVE_VALUE"), 0);
        assert!(!shell.env.contains_key("NATIVE_VALUE"));
        assert_eq!(shell.run_native("export NATIVE_VALUE"), 0);
        assert_eq!(
            shell.env.get("NATIVE_VALUE").map(String::as_str),
            Some("a b")
        );
        let plan = shell
            .prepare_native_agent_command("printenv NATIVE_VALUE")
            .unwrap();
        assert_eq!(
            shell
                .execute_native_agent_plan(plan, &CancellationToken::default())
                .unwrap()
                .unwrap()
                .output,
            "a b\n"
        );
        assert_eq!(shell.run_native("unset NATIVE_VALUE"), 0);
        assert!(!shell.env.contains_key("NATIVE_VALUE"));
        assert_eq!(shell.run_native("alias jump='cd child'"), 0);
        assert_eq!(shell.run_native("jump"), 0);
        assert_eq!(shell.cwd, root.join("child"));
        assert_eq!(shell.run_native("unalias jump"), 0);
        assert!(!shell.aliases.contains_key("jump"));
        let plan = shell.prepare_native_agent_command("history").unwrap();
        let history = vec!["cd child".into(), "export NATIVE_VALUE='a b'".into()];
        let result = shell
            .execute_native_agent_plan_with_history(plan, &CancellationToken::default(), &history)
            .unwrap()
            .unwrap();
        assert!(result.output.contains("cd child"));
        assert_eq!(shell.run_native("exit 17"), 17);
        assert!(shell.should_exit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_registered_builtin_has_a_native_plan_without_path_lookup() {
        let mut shell = Shell::new();
        shell.env.insert("PATH".into(), "/not-a-native-bin".into());
        for name in command::native_names() {
            if matches!(name, "fg" | "bg" | "disown") {
                assert!(
                    matches!(
                        shell.prepare_native_agent_command(name),
                        Err(NativePreparationError::JobTarget(_))
                    ),
                    "{name}: missing job is not a PATH failure"
                );
                continue;
            }
            assert!(
                matches!(
                    shell.prepare_native_agent_command(name).unwrap().executable,
                    AgentExecutionTarget::ZhshBuiltin { .. }
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn native_source_runs_in_the_current_session_and_stops_on_unsupported_input() {
        let root = root();
        fs::write(
            root.join("changes"),
            "# comment\nexport FROM_SOURCE=yes\ncd child\nalias here='pwd'\n",
        )
        .unwrap();
        let mut shell = Shell::new();
        shell.cwd = root.clone();
        shell.env.insert("PATH".into(), "/usr/bin:/bin".into());
        assert_eq!(shell.run_native(". changes"), 0);
        assert_eq!(shell.cwd, root.join("child"));
        assert_eq!(
            shell.env.get("FROM_SOURCE").map(String::as_str),
            Some("yes")
        );
        assert_eq!(shell.run_native("here"), 0);
        fs::write(
            root.join("child/partial"),
            "export FIRST=yes\nprintf x > forbidden\nexport NEVER=yes\n",
        )
        .unwrap();
        let plan = shell
            .prepare_native_agent_command("source partial")
            .unwrap();
        let result = shell
            .execute_native_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert_eq!(result.exit_code, 2);
        assert!(result.output.contains("第 2 行"));
        assert_eq!(shell.env.get("FIRST").map(String::as_str), Some("yes"));
        assert!(!shell.env.contains_key("NEVER"));
        assert!(!root.join("child/forbidden").exists());
        fs::write(
            root.join("child/output"),
            "printf source-output\nexport AGENT_SOURCE=yes\n",
        )
        .unwrap();
        let plan = shell.prepare_native_agent_command("source output").unwrap();
        let result = shell
            .execute_native_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert_eq!(result.output, "source-output");
        assert_eq!(
            shell.env.get("AGENT_SOURCE").map(String::as_str),
            Some("yes")
        );
        fs::write(root.join("child/cycle"), "source cycle\n").unwrap();
        let plan = shell.prepare_native_agent_command("source cycle").unwrap();
        let result = shell
            .execute_native_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert_eq!(result.exit_code, 1);
        assert!(result.output.contains("16 层"));
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod job_tests {
    use super::*;
    #[test]
    fn stopped_agent_job_is_retained_and_old_cancellation_cannot_kill_it() {
        let mut shell = Shell::new();
        shell.jobs().set_option("monitor", true).unwrap();
        let token = CancellationToken::default();
        let plan = shell
            .prepare_native_agent_command("sh -c 'kill -STOP $$; printf continued'")
            .unwrap();
        let stopped = shell
            .execute_native_agent_plan(plan, &token)
            .unwrap()
            .unwrap();
        assert_eq!(stopped.termination, CommandTermination::StoppedRetained);
        let binding = stopped.job.unwrap();
        assert!(shell.jobs().valid(&binding));
        token.cancel();
        assert!(shell.jobs().valid(&binding));
        let plan = shell.prepare_native_agent_command("fg %1").unwrap();
        let resumed = shell
            .execute_native_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert_eq!(resumed.termination, CommandTermination::Exited);
        assert_eq!(resumed.exit_code, 0);
        assert!(resumed.output.contains("continued"));
    }
    #[test]
    fn jobs_x_freezes_argv_and_stale_job_authorization_is_rejected() {
        let mut shell = Shell::new();
        shell.jobs().set_option("monitor", true).unwrap();
        shell.run_native("sh -c 'kill -STOP $$; exit 7'");
        let plan = shell
            .prepare_native_agent_command("jobs -x /usr/bin/printf '[%s]' %1 'a b' '$HOME'")
            .unwrap();
        let output = shell
            .execute_native_agent_plan(plan, &CancellationToken::default())
            .unwrap()
            .unwrap();
        assert!(output.output.ends_with("[a b][$HOME]"), "{}", output.output);
        let stale = shell.prepare_native_agent_command("bg %+").unwrap();
        shell.run_native("kill -KILL %1");
        shell.jobs().wait(&[], false, true, None).unwrap();
        shell.run_native("sh -c 'kill -STOP $$; exit 0'");
        assert!(matches!(
            shell.execute_native_agent_plan(stale, &CancellationToken::default()),
            Err(NativeExecutionError::NotStarted {
                reason: NativeNotStartedReason::PlanStale,
                ..
            })
        ));
        shell.run_native("kill -KILL %1");
        shell.jobs().wait(&[], false, true, None).unwrap();
    }
    #[test]
    fn agent_disown_bulk_selection_does_not_expand_after_authorization() {
        let mut shell = Shell::new();
        shell.jobs().set_option("monitor", true).unwrap();
        let empty = shell.prepare_native_agent_command("disown -a").unwrap();
        shell.run_native("sh -c 'kill -STOP $$; exit 0'");
        let first = shell.jobs().resolve("%1").unwrap();
        shell
            .execute_native_agent_plan(empty, &CancellationToken::default())
            .unwrap();
        assert!(shell.jobs().valid(&first));
        let frozen = shell.prepare_native_agent_command("disown -ah").unwrap();
        shell.run_native("sh -c 'kill -STOP $$; exit 0'");
        shell
            .execute_native_agent_plan(frozen, &CancellationToken::default())
            .unwrap();
        // A separate query resolves both jobs; only the prepared selection was changed.
        assert_eq!(shell.jobs().snapshots().len(), 2);
        let remove = shell.prepare_native_agent_command("disown -a").unwrap();
        shell.run_native("sh -c 'kill -STOP $$; exit 0'");
        let pids = shell
            .jobs()
            .snapshots()
            .iter()
            .flat_map(|j| j.pids.clone())
            .collect::<Vec<_>>();
        shell
            .execute_native_agent_plan(remove, &CancellationToken::default())
            .unwrap();
        assert_eq!(shell.jobs().snapshots().len(), 1);
        assert_eq!(shell.jobs().snapshots()[0].binding.id, 3);
        for pid in pids {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        shell.jobs().wait(&[], false, true, None).unwrap();
    }
    #[test]
    fn wait_p_changes_current_shell_variables_and_mode_help_is_isolated() {
        let mut shell = Shell::new();
        shell.jobs().set_option("monitor", true).unwrap();
        shell.run_native("sh -c 'kill -STOP $$; exit 7'");
        shell.run_native("bg %1");
        assert_eq!(shell.run_native("wait -np finished"), 7);
        assert!(shell
            .variables
            .get("finished")
            .unwrap()
            .contains("declare -- finished="));
        assert!(command::is_native_builtin("jobs"));
        assert!(!command::is_builtin("jobs"));
        assert!(command::usage("jobs").is_none());
        assert!(command::native_usage("jobs").is_some());
    }
}
