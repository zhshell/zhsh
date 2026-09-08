//! 当前 zhsh 会话的内存状态。
//!
//! 本模块不执行命令、不路由输入，也不直接输出终端内容。它还提供从会话状态生成
//! Bash 状态帧的纯投影，使执行器能够重放别名、函数和普通变量而不维护第二份状态。

use super::trust::AgentTrust;
use crate::llm::LlmConfig;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 把进程环境投影到当前字符串型会话，并报告被跳过的非 UTF-8 键值数量。
pub(super) fn process_environment() -> (HashMap<String, String>, usize) {
    let mut environment = HashMap::new();
    let mut skipped = 0;
    for (key, value) in std::env::vars_os() {
        let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
            skipped += 1;
            continue;
        };
        environment.insert(key.to_owned(), value.to_owned());
    }
    (environment, skipped)
}

/// 单一会话状态源。
pub(crate) struct SessionState {
    /// 传给后续 Bash 子进程的完整环境。
    pub(crate) env: HashMap<String, String>,
    /// 当前会话定义或从 Bash 导入的别名。
    pub(crate) aliases: HashMap<String, String>,
    /// 后续命令使用的逻辑工作目录。
    pub(crate) cwd: PathBuf,
    /// 最近一条已处理命令的退出状态。
    pub(crate) last_exit: i32,
    /// `exit` 内建命令设置的 REPL 终止标记。
    pub(crate) should_exit: bool,
    /// 当前启用的 LLM 配置；未配置时为 [`None`]。
    pub(crate) llm: Option<LlmConfig>,
    /// 活动标记指向的配置名；配置不完整或暂时不可用时仍保留。
    active_llm_name: Option<String>,
    /// LLM 客户端初始化失败时由组合根设置，配置切换不能清除。
    agent_runtime_error: Option<String>,
    /// 启动活动配置损坏或引用不可用 Codec 时的可恢复原因。
    active_config_error: Option<String>,
    /// 启动时固定的 zhsh 用户状态根；不随会话 `$HOME` 改变。
    user_home: Option<PathBuf>,
    /// `pushd`、`popd` 和 `dirs` 共享的目录栈，栈顶位于末尾。
    pub(crate) directory_stack: Vec<PathBuf>,
    /// 从启动文件或 `source` 快照恢复的 Bash 函数定义。
    pub(crate) functions: HashMap<String, String>,
    /// 函数可能依赖、需要在子 Bash 中重放的普通变量声明。
    pub(crate) variables: HashMap<String, String>,
    /// PS0–PS4 的当前字符串值；声明仍由 `variables`/`env` 保存，这里只供 REPL 渲染。
    pub(crate) prompt_variables: HashMap<String, String>,
}

impl SessionState {
    pub(crate) const DEFAULT_PS1: &'static str =
        r"\[\e[38;5;124m\]｢zh｣\[\e[0m\]\[\e[38;5;36m\]\u@\h\[\e[0m\]:\[\e[36m\]\w\[\e[0m\]\$ ";
    pub(crate) const DEFAULT_PS2: &'static str = "> ";
    pub(crate) const DEFAULT_PS3: &'static str = "#? ";
    pub(crate) const DEFAULT_PS4: &'static str = "+ ";

    /// 使用进程初始状态和可选 LLM 配置创建会话。
    ///
    /// # Arguments
    ///
    /// - `env`：作为后续子进程完整环境的键值集合。
    /// - `cwd`：会话的初始工作目录。
    /// - `llm`：启动时恢复的活动 LLM 配置。
    pub(crate) fn new(
        env: HashMap<String, String>,
        cwd: PathBuf,
        llm: Option<LlmConfig>,
        user_home: Option<PathBuf>,
    ) -> Self {
        let mut state = Self {
            env,
            aliases: HashMap::new(),
            cwd,
            last_exit: 0,
            should_exit: false,
            active_llm_name: llm.as_ref().map(|config| config.name.clone()),
            llm,
            agent_runtime_error: None,
            active_config_error: None,
            user_home,
            directory_stack: Vec::new(),
            functions: HashMap::new(),
            variables: HashMap::new(),
            prompt_variables: HashMap::new(),
        };
        state.normalize_prompt_variables();
        state
    }

    #[cfg(test)]
    pub(crate) fn test() -> Self {
        let (environment, _) = process_environment();
        let user_home = std::env::var_os("HOME")
            .filter(|value| value.to_str().is_some())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        Self::new(
            environment,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            None,
            user_home,
        )
    }

    /// 返回启动时固定的用户状态根。
    pub(crate) fn user_home(&self) -> Option<&Path> {
        self.user_home.as_deref()
    }

    /// 设置 LLM 客户端级不可用原因；`None` 表示运行时本身可用。
    pub(crate) fn set_agent_runtime_error(&mut self, reason: Option<String>) {
        self.agent_runtime_error = reason;
    }

    /// 记录启动活动配置的可恢复加载失败。
    pub(crate) fn set_active_config_error(&mut self, reason: Option<String>) {
        self.active_config_error = reason;
    }

    /// 成功启用配置后清除旧的活动配置错误。
    pub(crate) fn clear_active_config_error(&mut self) {
        self.active_config_error = None;
    }

    pub(crate) fn active_llm_name(&self) -> Option<&str> {
        self.llm
            .as_ref()
            .map(|config| config.name.as_str())
            .or(self.active_llm_name.as_deref())
    }

    pub(crate) fn commit_ready_llm(&mut self, config: LlmConfig) {
        self.active_llm_name = Some(config.name.clone());
        self.llm = Some(config);
        self.clear_active_config_error();
    }

    pub(crate) fn commit_unavailable_llm(&mut self, name: Option<String>, reason: String) {
        self.active_llm_name = name;
        self.llm = None;
        self.set_active_config_error(Some(reason));
    }

    pub(crate) fn clear_active_llm(&mut self) {
        self.active_llm_name = None;
        self.llm = None;
        self.clear_active_config_error();
    }

    /// 返回当前 Agent 不可用原因；没有配置也属于可恢复的不可用状态。
    pub(crate) fn agent_unavailable_reason(&self) -> Option<&str> {
        self.agent_runtime_error
            .as_deref()
            .or(self.active_config_error.as_deref())
            .or_else(|| {
                self.llm
                    .is_none()
                    .then_some("尚未配置 LLM；运行 `zh llm` 创建配置")
            })
    }

    #[cfg(test)]
    pub(crate) fn set_user_home_for_test(&mut self, home: Option<PathBuf>) {
        self.user_home = home;
    }

    /// 设置一个环境变量。
    ///
    /// # Arguments
    ///
    /// - `key`：不含 `=` 或 NUL 的非空变量名。
    /// - `value`：不含 NUL 的变量值。
    ///
    /// # Errors
    ///
    /// 名称或值不能传给操作系统环境时返回面向用户的校验错误。
    pub(crate) fn set_env(&mut self, key: &str, value: &str) -> Result<(), String> {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err("环境变量名称或值包含无效字符".into());
        }
        self.env.insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// 删除环境变量；变量不存在时不执行任何操作。
    ///
    /// # Arguments
    ///
    /// - `key`：需要删除的变量名。
    pub(crate) fn remove_env(&mut self, key: &str) {
        self.env.remove(key);
    }

    pub(crate) fn is_prompt_variable(name: &str) -> bool {
        matches!(name, "PS0" | "PS1" | "PS2" | "PS3" | "PS4")
    }

    /// 返回 PS 变量当前值；普通 Shell 变量优先于同名导出环境。
    pub(crate) fn prompt_variable(&self, name: &str) -> Option<&str> {
        self.prompt_variables
            .get(name)
            .or_else(|| self.env.get(name))
            .map(String::as_str)
    }

    /// 提交一条仅含 PS0–PS4 的交互式赋值，并保留原有 export 属性。
    pub(crate) fn set_prompt_variable(&mut self, name: &str, value: &str) -> Result<(), String> {
        if !Self::is_prompt_variable(name) || value.contains('\0') {
            return Err("不是有效的 PS 变量赋值".into());
        }
        self.prompt_variables
            .insert(name.to_string(), value.to_string());
        if self.env.contains_key(name) {
            self.set_env(name, value)?;
            self.variables.insert(
                name.to_string(),
                Self::prompt_declaration(name, value, true),
            );
        } else {
            self.variables.insert(
                name.to_string(),
                Self::prompt_declaration(name, value, false),
            );
        }
        Ok(())
    }

    pub(crate) fn remove_prompt_variable(&mut self, name: &str) {
        self.prompt_variables.remove(name);
    }

    /// 在构造和 source 提交后恢复 Bash/zhsh 的默认 PS 值，并生成可重放声明。
    pub(crate) fn normalize_prompt_variables(&mut self) {
        for (name, default) in [
            ("PS1", Self::DEFAULT_PS1),
            ("PS2", Self::DEFAULT_PS2),
            ("PS3", Self::DEFAULT_PS3),
            ("PS4", Self::DEFAULT_PS4),
        ] {
            let value = self
                .prompt_variables
                .get(name)
                .cloned()
                .or_else(|| self.env.get(name).cloned())
                .unwrap_or_else(|| default.to_string());
            self.prompt_variables
                .insert(name.to_string(), value.clone());
            self.variables.insert(
                name.to_string(),
                Self::prompt_declaration(name, &value, self.env.contains_key(name)),
            );
        }
        if let Some(value) = self
            .prompt_variables
            .get("PS0")
            .cloned()
            .or_else(|| self.env.get("PS0").cloned())
        {
            self.prompt_variables.insert("PS0".into(), value.clone());
            self.variables.insert(
                "PS0".into(),
                Self::prompt_declaration("PS0", &value, self.env.contains_key("PS0")),
            );
        }
    }

    fn prompt_declaration(name: &str, value: &str, exported: bool) -> String {
        let attributes = if exported { "-x" } else { "--" };
        format!(
            "declare {attributes} {name}='{}'",
            value.replace('\'', "'\\''")
        )
    }

    /// 返回当前环境变量对应的有效 Agent 授信等级。
    pub(crate) fn agent_trust(&self) -> AgentTrust {
        AgentTrust::from_environment(
            self.env
                .get(AgentTrust::ENVIRONMENT_KEY)
                .map(String::as_str),
        )
    }

    /// 将规范化后的 Agent 授信等级提交到当前会话环境。
    pub(crate) fn set_agent_trust(&mut self, trust: AgentTrust) {
        self.env
            .insert(AgentTrust::ENVIRONMENT_KEY.into(), trust.as_str().into());
    }

    /// 在完整验证候选集合后原子替换会话环境。
    ///
    /// # Arguments
    ///
    /// - `replacement`：`source` 快照产生的新环境。
    ///
    /// # Errors
    ///
    /// 任一键值不能表示为操作系统环境时返回错误，原环境保持不变。
    pub(crate) fn replace_environment(
        &mut self,
        replacement: HashMap<String, String>,
    ) -> Result<(), String> {
        if replacement
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
        {
            return Err("环境变量名称或值包含无效字符".into());
        }
        self.env = replacement;
        Ok(())
    }

    /// 展开输入首词对应的别名。
    ///
    /// 为避免循环和无界递归，最多展开三层，并在再次遇到同一首词时停止。
    ///
    /// # Arguments
    ///
    /// - `input`：尚未传给 Bash 的完整命令文本。
    pub(crate) fn expand_alias(&self, input: &str) -> String {
        let mut result = input.to_string();
        let mut seen = HashSet::new();
        for _ in 0..3 {
            let first = result.split_whitespace().next().unwrap_or("");
            if !seen.insert(first.to_string()) {
                break;
            }
            if let Some(alias) = self.aliases.get(first) {
                result = format!("{}{}", alias, &result[first.len()..]);
            } else {
                break;
            }
        }
        result
    }

    /// 将会话内别名、函数和普通变量投影为独立的 Bash 状态帧。
    ///
    /// # Returns
    ///
    /// 没有需要重放的状态时返回空字符串。用户命令不属于该帧，由执行器通过独立帧
    /// 传输，避免状态前导改变用户命令的诊断来源和行号。该函数不修改会话。
    pub(crate) fn prepare_bash_state(&self) -> String {
        if self.aliases.is_empty() && self.functions.is_empty() && self.variables.is_empty() {
            return String::new();
        }

        fn quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', "'\\''"))
        }

        let mut prepared = String::from("shopt -s expand_aliases\n");
        for declaration in self.variables.values() {
            prepared.push_str(declaration);
            prepared.push('\n');
        }
        for (name, value) in &self.aliases {
            prepared.push_str(&format!("alias -- {}\n", quote(&format!("{name}={value}"))));
        }
        for definition in self.functions.values() {
            prepared.push_str(definition);
            prepared.push('\n');
        }
        prepared
    }
}
