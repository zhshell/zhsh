//! Native 外部调用准备和 Shell 适配；会话仍由 Shell 持有。

use super::builtin::BuiltinResult;
use super::command::{self, args, resolver};
use super::{CommandTermination, OutputEvidence};

mod source;
use super::executor::native::{self as execution, ExternalEnvironment};
use super::{AgentCommandPlan, AgentExecutionTarget, CapturedExecution, Shell};
use crate::common::{AppError, CancellationToken};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum NativePreparationError {
    InvalidInput(String),
    NotFound,
    UnsupportedFormat(String),
    ReadFailed(String),
}
impl NativePreparationError {
    pub(crate) fn code(&self) -> i32 {
        match self {
            Self::InvalidInput(_) => 2,
            Self::NotFound => 127,
            Self::UnsupportedFormat(_) => 126,
            Self::ReadFailed(_) => 1,
        }
    }
    pub(crate) fn is_failure(&self) -> bool {
        matches!(self, Self::ReadFailed(_))
    }
}
impl std::fmt::Display for NativePreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(s) | Self::UnsupportedFormat(s) | Self::ReadFailed(s) => {
                f.write_str(s)
            }
            Self::NotFound => f.write_str("Native PATH 中未找到可执行程序"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeNotStartedReason {
    PlanStale,
    InvalidRequest,
    UnsupportedFormat,
    SpawnFailed,
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
        if matches!(self, Self::NotStarted { .. }) {
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

fn prepare(
    input: &str,
    cwd: &Path,
    environment: &HashMap<String, String>,
) -> Result<Option<PreparedNativeExternal>, NativePreparationError> {
    let original = input.trim().to_owned();
    let words = args::parse_native_literal(&original).map_err(|e| {
        NativePreparationError::InvalidInput(match e {
            args::ParseError::NeedsBash => "Native 本期不支持该 Shell 语法或展开".into(),
            args::ParseError::Syntax(message) => message.into(),
        })
    })?;
    let Some((program, arguments)) = words.split_first() else {
        return Ok(None);
    };
    let path = environment.get("PATH").map(String::as_str);
    let (selected, _, _) = resolver::first_executable_path(cwd, path, program)
        .ok_or(NativePreparationError::NotFound)?;
    execution::validate_native_binary(&selected)?;
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
    #[cfg(test)]
    pub(crate) fn run_native(&mut self, input: &str) -> i32 {
        self.run_native_with_history(input, &[])
    }

    pub(crate) fn run_native_with_history(&mut self, input: &str, history: &[String]) -> i32 {
        if input.trim().is_empty() {
            return self.state.last_exit;
        }
        let plan = match self.prepare_native_agent_command(input) {
            Ok(plan) => plan,
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

    pub(crate) fn prepare_native_agent_command(
        &self,
        input: &str,
    ) -> Result<AgentCommandPlan, NativePreparationError> {
        let original = input.trim();
        let parse_builtin = |text: &str| {
            args::parse_native_builtin_literal(text).map_err(|error| {
                NativePreparationError::InvalidInput(match error {
                    args::ParseError::Syntax(message) => message.into(),
                    args::ParseError::NeedsBash => "Native 不支持该 Shell 语法或展开".into(),
                })
            })
        };
        let words = parse_builtin(original)?;
        // Preserve the existing builtin-before-alias ordering. Aliases only rewrite the command
        // before preparation, never after authorization, and their bodies obey Native syntax.
        let expanded = if words.first().is_some_and(|name| command::is_builtin(name)) {
            original.to_owned()
        } else {
            self.state.expand_alias(original)
        };
        let words = parse_builtin(&expanded)?;
        let Some((name, arguments)) = words.split_first() else {
            return Err(NativePreparationError::InvalidInput(
                "Native command 不能为空".into(),
            ));
        };
        if command::is_builtin(name) {
            return Ok(AgentCommandPlan::from_native_builtin(
                original.to_owned(),
                name.clone(),
                arguments.to_vec(),
                self.state.cwd.clone(),
                self.state.env.get("PATH").map(OsString::from),
            ));
        }
        let mut plan = prepare(&expanded, &self.state.cwd, &self.state.env)?
            .map(AgentCommandPlan::from_native_external)
            .ok_or_else(|| {
                NativePreparationError::InvalidInput("Native command 不能为空".into())
            })?;
        plan.original = original.to_owned();
        Ok(plan)
    }

    pub(crate) fn record_native_preparation_failure(&mut self, error: &NativePreparationError) {
        self.state.last_exit = error.code();
    }

    pub(crate) fn native_builtin_requires_confirmation(&self, plan: &AgentCommandPlan) -> bool {
        matches!(&plan.executable, AgentExecutionTarget::ZhshBuiltin { name, arguments }
            if !command::agent_allows(name, arguments))
    }

    pub(crate) fn native_agent_plan_requires_terminal(&self, plan: &AgentCommandPlan) -> bool {
        match (&plan.executable, plan.invocations.first()) {
            (AgentExecutionTarget::ZhshBuiltin { name, .. }, _) => {
                matches!(name.as_str(), "fg" | "zh" | "source" | ".")
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
        execution::validate_native_binary(path).map_err(|error| {
            NativeExecutionError::not_started(
                NativeNotStartedReason::UnsupportedFormat,
                error.to_string(),
            )
        })?;
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
        let result = (|| {
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
            let environment = ExternalEnvironment {
                cwd: &self.state.cwd,
                exported: &self.state.env,
            };
            if let Some(cancellation) = cancellation {
                self.executor
                    .run_native_agent(environment, path, program, arguments, cancellation)
            } else {
                self.executor
                    .run_native_user(environment, path, program, arguments)
                    .map(|code| {
                        Some(CapturedExecution {
                            output: String::new(),
                            total_output_bytes: 0,
                            exit_code: code,
                            termination: CommandTermination::Exited,
                            output_evidence: OutputEvidence::Unavailable,
                        })
                    })
            }
        })();
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
    fn native_resolves_session_path_and_rejects_stale_or_script_targets() {
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
        fs::write(
            root.join("a/probe"),
            "#!/bin/sh\nprintf unsafe > sentinel\n",
        )
        .unwrap();
        assert!(matches!(
            shell.prepare_native_agent_command("probe"),
            Err(NativePreparationError::UnsupportedFormat(_))
        ));
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
        assert_eq!(shell.run_native("probe"), 127);
        assert_eq!(shell.run_native(" \t "), 127);
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
        for name in command::names() {
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
