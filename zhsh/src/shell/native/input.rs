//! Native 命令输入准备：检查完整命令单元、展开别名，并生成供执行入口使用的绑定计划。
use super::super::language::{parse_single, InputError, SimpleUnit};
use super::*;
impl Shell {
    pub(crate) fn report_native_input_error(&mut self, error: &InputError) -> i32 {
        eprintln!("zhsh: {}", safe_diagnostic(&error.to_string()));
        self.state.last_exit = 2;
        2
    }
    pub(crate) fn prepare_native_agent_command_with_cancel(
        &self,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<AgentCommandPlan, NativePreparationError> {
        let unit = parse_single(text, Some(cancel))
            .map_err(|e| NativePreparationError::InvalidInput(e.to_string()))?;
        self.prepare_native_unit(unit, true, Some(cancel))?
            .ok_or(NativePreparationError::EmptyInput)
    }
    #[cfg(test)]
    pub(super) fn prepare_native_command(
        &self,
        text: &str,
        bind_jobs: bool,
    ) -> Result<AgentCommandPlan, NativePreparationError> {
        let unit = parse_single(text, None)
            .map_err(|e| NativePreparationError::InvalidInput(e.to_string()))?;
        self.prepare_native_unit(unit, bind_jobs, None)?
            .ok_or(NativePreparationError::EmptyInput)
    }
    pub(super) fn prepare_native_unit(
        &self,
        mut unit: SimpleUnit,
        bind_jobs: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<Option<AgentCommandPlan>, NativePreparationError> {
        if unit.words.is_empty() {
            return Ok(None);
        }
        let original = unit.original.trim().to_owned();
        let check = |u: &SimpleUnit| {
            let name = &u.words[0].value;
            u.validate(
                command::is_native_builtin(name),
                command::is_job_builtin(name),
            )
            .map_err(|e| NativePreparationError::InvalidInput(e.to_string()))
        };
        check(&unit)?;
        if !command::is_native_builtin(&unit.words[0].value) {
            let mut seen = std::collections::HashSet::new();
            let mut total = 0;
            for _ in 0..3 {
                let first = &unit.words[0];
                let raw = &unit.original[first.span.clone()];
                if !seen.insert(raw.to_owned()) {
                    break;
                }
                let Some(alias) = self.state.aliases.get(raw) else {
                    break;
                };
                total += alias.len() + unit.original.len();
                if total > 4 * 1024 * 1024 {
                    return Err(NativePreparationError::InvalidInput(
                        "Native alias 展开超过限制".into(),
                    ));
                }
                let replaced = format!(
                    "{}{}{}",
                    &unit.original[..first.span.start],
                    alias,
                    &unit.original[first.span.end..]
                );
                unit = parse_single(&replaced, cancel).map_err(|e| {
                    NativePreparationError::InvalidInput(format!("alias {raw}: {e}"))
                })?;
                if unit.words.is_empty() {
                    return Err(NativePreparationError::InvalidInput(
                        "Native alias 没有可执行命令".into(),
                    ));
                }
            }
        }
        check(&unit)?;
        let words = unit.values();
        if command::is_native_builtin(&words[0]) {
            self.prepare_native_words(&original, &words, 0, bind_jobs)
                .map(Some)
        } else {
            prepare_words(original, &words, &self.state.cwd, &self.state.env)
                .map(|p| p.map(AgentCommandPlan::from_native_external))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_preparation_and_aliases_never_execute_prefixes_or_rebind_authorized_words() {
        let mut shell = Shell::new();
        shell.state.last_exit = 17;
        let cancel = CancellationToken::default();
        let error = shell
            .prepare_native_agent_command_with_cancel("# no command\n", &cancel)
            .unwrap_err();
        shell.record_native_preparation_failure(&error);
        assert_eq!(shell.state.last_exit, 17);
        assert!(shell
            .prepare_native_agent_command_with_cancel("export S1_NO_EXEC=yes\npwd", &cancel)
            .is_err());
        assert!(!shell.state.env.contains_key("S1_NO_EXEC"));
        shell
            .state
            .aliases
            .insert("s1alias".into(), "export S1_FROZEN='old value'".into());
        let plan = shell
            .prepare_native_agent_command_with_cancel("  s1alias # comment", &cancel)
            .unwrap();
        shell
            .state
            .aliases
            .insert("s1alias".into(), "export S1_FROZEN=new".into());
        shell.execute_native_agent_plan(plan, &cancel).unwrap();
        assert_eq!(
            shell.state.env.get("S1_FROZEN").map(String::as_str),
            Some("old value")
        );
        shell
            .state
            .aliases
            .insert("bad".into(), "export S1_NO_EXEC=yes\npwd".into());
        assert!(shell
            .prepare_native_agent_command_with_cancel("bad", &cancel)
            .is_err());
        assert!(!shell.state.env.contains_key("S1_NO_EXEC"));
        assert_eq!(shell.run_native("export S1_USER='a\nb' # comment"), 0);
        let plan = shell
            .prepare_native_agent_command_with_cancel("export S1_AGENT='a\nb'", &cancel)
            .unwrap();
        shell.execute_native_agent_plan(plan, &cancel).unwrap();
        assert_eq!(
            shell.state.env.get("S1_USER"),
            shell.state.env.get("S1_AGENT")
        );
    }
}
