//! 进程启动、输入模式选择和 REPL 生命周期。
//!
//! 非终端标准输入按行处理；交互终端由 rustyline 提供编辑、补全和历史。独立输入
//! 路由只检查去除边界空白后的首字符：ASCII 开头进入 Shell，非 ASCII 开头进入 Agent。
//! 两条路径都保持原文，历史始终只保存用户在主提示符提交的输入。

mod clarification;
mod codec_ui;
mod completion;
mod input;
mod llm_wizard;
mod manual_clarification;
mod prompt;
mod safety_ui;
mod user_home;

use crate::agent;
use crate::shell;
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::history::{History, SearchDirection};
use rustyline::{Cmd, CompletionType, Config, EditMode, Editor, KeyEvent};
use shell::Shell;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// zhsh 进程启动选项。
///
/// 所有诊断默认关闭；调用方必须通过显式 builder 方法开启。
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct RunOptions {
    trace_agent: bool,
}

impl RunOptions {
    /// 设置是否记录严格 Agent 协议无法解析的原始模型响应。
    ///
    /// 记录可能包含用户任务和 Provider 返回的内容，只应在当前进程排障时开启。
    #[must_use]
    pub fn with_agent_trace(mut self, enabled: bool) -> Self {
        self.trace_agent = enabled;
        self
    }
}

/// 启动 zhsh 并运行至输入结束或用户退出。
///
/// 该函数恢复持久化 LLM 配置、加载 `~/.zhshrc`，随后根据标准输入是否连接终端
/// 选择 REPL 或逐行管道模式。交互模式还负责 `~/.zh_history` 的加载、增量保存和
/// `0600` 权限维护。
///
/// # Returns
///
/// 返回 zhsh 的进程状态码：正常情况下是最后一次执行结果或 `exit N` 指定的状态码；
/// 初始化和输入读取失败返回 `1`，被取消的 Agent 请求将当前状态设置为 `130`。
///
/// # Side effects
///
/// 本函数读写用户历史与 zhsh 配置目录、安装 Ctrl-C 处理器，并可能启动 Bash 子进程
/// 和发起 LLM HTTP 请求。同一进程中不应重复调用，因为 Ctrl-C 全局处理器只能安装一次。
pub fn run() -> i32 {
    run_with_options(RunOptions::default())
}

/// 使用显式进程选项启动 zhsh，并运行至输入结束或用户退出。
///
/// 除 [`RunOptions`] 明确启用的诊断外，其启动、持久化和退出语义与 [`run`] 相同。
pub fn run_with_options(options: RunOptions) -> i32 {
    let user_home = user_home::UserHomeState::from_process();
    if let Some(reason) = user_home.reason() {
        eprintln!("zhsh: 用户状态已禁用: {reason}");
    }
    let codec_runtime = Arc::new(crate::llm::CodecRuntime::load(user_home.path()));
    let codec_issues = codec_runtime.issue_messages();
    if !codec_issues.is_empty() {
        eprintln!(
            "zhsh: Codec 加载诊断（{} 项）：\n  {}",
            codec_issues.len(),
            codec_issues.join("\n  ")
        );
    }
    let mut shell = Shell::from_startup(user_home.path(), Arc::clone(&codec_runtime));
    shell.set_llm_config_ui(Box::new(llm_wizard::TerminalLlmConfigUi));
    shell.set_codec_management_ui(Box::new(codec_ui::TerminalCodecManagementUi));
    shell.set_safety_management_ui(Box::new(safety_ui::TerminalSafetyManagementUi));
    for error in shell.take_startup_errors() {
        eprintln!("zhsh: 启动警告: {error}");
    }
    if let Some(config) = shell.llm.as_ref() {
        if let Some(warning) =
            crate::llm::plaintext_private_warning(&config.url, &config.access_token)
        {
            eprintln!("{warning}");
        }
    }
    shell.load_rc();
    let agent = agent::new_agent(user_home.path(), codec_runtime, options.trace_agent);
    if options.trace_agent {
        if let Some(path) = agent.invalid_response_diagnostics_path() {
            eprintln!(
                "zhsh: Agent 原始响应诊断已启用（日志可能包含任务内容）: {}",
                crate::common::terminal_safe_path(&path)
            );
        } else {
            eprintln!("zhsh: Agent 原始响应诊断无法启用: 用户状态目录不可用");
        }
    }
    shell.set_safety_management(agent.safety_management_port());
    for notice in agent.safety_startup_notices() {
        agent::present(agent::AgentEvent::SafetyNotice { message: notice });
    }
    shell.set_agent_runtime_error(agent.unavailable_reason().map(str::to_owned));
    if let Some(reason) = agent.unavailable_reason() {
        eprintln!("zhsh: Agent 已禁用: {reason}");
    }
    if let Some(warning) = shell
        .llm
        .as_ref()
        .and_then(crate::llm::LlmConfig::json_schema_downgrade_warning)
    {
        eprintln!("{warning}");
    }

    // 管道模式
    if !io::stdin().is_terminal() {
        let mut input = String::new();
        if let Err(error) = io::stdin().read_to_string(&mut input) {
            eprintln!("zhsh: 无法读取标准输入: {error}");
            return 1;
        }
        for line in input.lines() {
            process(&mut shell, &agent, line, &[]);
            if shell.should_exit {
                break;
            }
        }
        return shell.last_exit;
    }

    // 信号处理
    if let Err(error) = ctrlc::set_handler(agent::request_cancel) {
        eprintln!("zhsh: 无法安装 Ctrl-C 处理器: {error}");
        return 1;
    }
    #[cfg(unix)]
    if let Err(error) = install_suspend_guard() {
        eprintln!("zhsh: 无法安装终端暂停防护: {error}");
        return 1;
    }

    eprintln!("zh [{}]", agent::model(&shell));

    // REPL
    let history_path = user_home.path().and_then(|home| {
        let path = home.join(".zh_history");
        match prepare_history(&path) {
            Ok(()) => Some(path),
            Err(error) => {
                eprintln!("zhsh: 历史持久化已禁用: {error}");
                None
            }
        }
    });
    let config = Config::builder()
        .history_ignore_space(true)
        .completion_type(CompletionType::List)
        .edit_mode(EditMode::Emacs)
        .build();
    let mut rl = match Editor::with_config(config) {
        Ok(editor) => editor,
        Err(error) => {
            eprintln!("zhsh: 无法初始化行编辑器: {error}");
            return 1;
        }
    };
    // zhsh 尚未实现完整 job control。提示符处的 Ctrl-Z（部分终端的 Pause 键也会
    // 发送同一个 VSUSP 字符）必须只是无操作，不能让 rustyline 暂停整个 zhsh 进程组。
    // 前台外部命令的停止与恢复由 Shell 执行器单独监督。
    rl.bind_sequence(KeyEvent::ctrl('Z'), Cmd::Noop);
    let completion_cache = Arc::new(completion::CompletionCache::new());
    rl.set_helper(Some(completion::ShellCompleter::new(
        &shell,
        Arc::clone(&completion_cache),
    )));
    let prompt_renderer = prompt::PromptRenderer::new();
    if let Err(error) = rl.set_max_history_size(10000) {
        eprintln!("zhsh: 无法设置历史容量: {error}");
    }
    if let Some(path) = history_path.as_deref().filter(|path| path.exists()) {
        if let Err(error) = rl.load_history(path) {
            eprintln!("zhsh: 无法加载历史文件 {}: {error}", path.display());
        }
    }
    let mut command_number = 1u64;
    'repl: loop {
        let context = prompt::PromptContext {
            history_number: rl.history().len() + 1,
            command_number,
        };
        match rl.readline(&prompt_renderer.prompt(&shell, context)) {
            Ok(mut line) => {
                while Shell::input_needs_continuation(&line) {
                    match rl.readline(&prompt_renderer.secondary_prompt(&shell, context)) {
                        Ok(continuation) => {
                            line.push('\n');
                            line.push_str(&continuation);
                        }
                        Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                            eprintln!();
                            continue 'repl;
                        }
                        Err(error) => {
                            eprintln!("Error: {error}");
                            break 'repl;
                        }
                    }
                }
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(ps0) = prompt_renderer.pre_command_prompt(&shell, context) {
                    eprint!("{ps0}");
                    let _ = io::stderr().flush();
                }
                match rl.add_history_entry(&line) {
                    Ok(true) => {
                        if let Some(path) = history_path.as_deref() {
                            if let Err(error) = rl.append_history(path) {
                                eprintln!("zhsh: 无法增量保存历史文件 {}: {error}", path.display());
                            } else if let Err(error) = secure_history(path) {
                                eprintln!("zhsh: 无法设置历史文件权限: {error}");
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(error) => eprintln!("zhsh: 无法记录历史: {error}"),
                }
                let history = history_entries(&rl);
                process(&mut shell, &agent, &line, &history);
                command_number = command_number.saturating_add(1);
                // 内建命令可能改变 cwd、PATH、alias 或 LLM 配置；下一次读取前刷新补全器。
                rl.set_helper(Some(completion::ShellCompleter::new(
                    &shell,
                    Arc::clone(&completion_cache),
                )));
                if shell.should_exit {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => {
                eprintln!("bye");
                break;
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    if let Some(path) = history_path.as_deref() {
        if let Err(error) = rl.save_history(path) {
            eprintln!("zhsh: 无法保存历史文件 {}: {error}", path.display());
        } else if let Err(error) = secure_history(path) {
            eprintln!("zhsh: 无法设置历史文件权限: {error}");
        }
    }
    shell.last_exit
}

#[cfg(unix)]
extern "C" fn ignore_terminal_suspend(_: libc::c_int) {}

/// 保证只有被显式交出终端的子进程组会响应 Ctrl-Z；zhsh 自身始终保持可调度。
#[cfg(unix)]
fn install_suspend_guard() -> std::io::Result<()> {
    // 使用“捕获后无操作”而不是 SIG_IGN：POSIX exec 会把捕获信号恢复为默认处置，
    // 因而前台外部程序仍可正常被 SIGTSTP 暂停。
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = ignore_terminal_suspend as *const () as usize;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: action 是完整初始化的 sigaction，handler 只返回且不访问共享状态。
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTSTP, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn secure_history(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "历史路径必须是非符号链接普通文件",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "历史文件所有者不是当前用户",
            ));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let mode = std::fs::symlink_metadata(path)?.mode() & 0o777;
        if mode != 0o600 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "历史文件权限必须是 0600",
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn prepare_history(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return secure_history(path);
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let _file = options.open(path)?;
    secure_history(path)
}

fn history_entries<H: rustyline::Helper, I: History>(editor: &Editor<H, I>) -> Vec<String> {
    let history = editor.history();
    (0..history.len())
        .filter_map(|index| {
            history
                .get(index, SearchDirection::Forward)
                .ok()
                .flatten()
                .map(|result| result.entry.into_owned())
        })
        .collect()
}

fn process(shell: &mut Shell, agent: &agent::AgentRuntime, input: &str, history: &[String]) -> i32 {
    let Some(routed) = input::route(input) else {
        return 0;
    };

    if routed.kind == input::InputKind::AgentInput {
        let freshness = Arc::new(crate::common::CancellationToken::default());
        let _freshness_active = crate::common::ActiveCancellation::register(Arc::clone(&freshness));
        if let Err(error) = shell.refresh_codecs_for_agent(&freshness) {
            eprintln!(
                "! Codec 状态刷新失败：{}",
                agent::sanitize_terminal_text(&error.to_string())
            );
            shell.last_exit = 1;
            return 1;
        }
        drop(_freshness_active);
        let client = match agent.client() {
            Ok(client) => client,
            Err(reason) => {
                eprintln!(
                    "! Agent 不可用：{}；运行 `zh status` 查看状态并修复 LLM/Codec 配置",
                    agent::sanitize_terminal_text(reason)
                );
                shell.last_exit = 1;
                return 1;
            }
        };
        if let Some(reason) = shell.agent_unavailable_reason() {
            eprintln!(
                "! Agent 不可用：{}；运行 `zh status` 查看状态并使用 `zh llm` 或 `zh use` 修复",
                agent::sanitize_terminal_text(reason)
            );
            shell.last_exit = 1;
            return 1;
        }
        let t = Instant::now();
        let mut task = agent::Task::new_with_runtime(shell, &routed.original, agent);
        let result = loop {
            match agent::run_phase(client, shell, &mut task) {
                agent::PhaseResult::Finished(result) => break result,
                agent::PhaseResult::Clarify {
                    questions,
                    phase,
                    phase_turns,
                    total_turns,
                    clarification,
                } => {
                    agent::present(agent::AgentEvent::PhaseEnded {
                        phase,
                        phase_turns,
                        total_turns,
                        clarification,
                    });
                    match clarification::collect(&questions) {
                        clarification::Outcome::Submitted(reply) => {
                            let summary = reply.clone();
                            let cwd = shell.cwd.clone();
                            task.resume(&questions, reply, &cwd);
                            agent::present(agent::AgentEvent::ClarificationSubmitted {
                                phase: task.phase(),
                                questions: &questions,
                                reply: &summary,
                            });
                        }
                        clarification::Outcome::TimedOut => {
                            break agent::RunResult::cancelled(
                                agent::CancelCause::ClarificationTimedOut,
                                total_turns,
                            )
                        }
                        clarification::Outcome::Cancelled => {
                            break agent::RunResult::cancelled(
                                agent::CancelCause::UserInterrupted,
                                total_turns,
                            )
                        }
                        clarification::Outcome::InputClosed => {
                            break agent::RunResult::cancelled(
                                agent::CancelCause::ClarificationClosed,
                                total_turns,
                            )
                        }
                        clarification::Outcome::TerminalError => {
                            break agent::RunResult::failed("无法安全读取澄清输入", total_turns)
                        }
                        clarification::Outcome::Unavailable => {
                            break agent::RunResult::incomplete(
                                "当前终端无法进行澄清输入",
                                total_turns,
                            )
                        }
                    }
                }
                agent::PhaseResult::ManualClarify {
                    phase,
                    phase_turns,
                    total_turns,
                    clarification,
                    command_interrupted,
                } => {
                    agent::present(agent::AgentEvent::ManualPaused {
                        status: agent::TaskStatus {
                            phase,
                            phase_turn: phase_turns,
                            total_turns,
                            clarifications: task.clarifications(),
                        },
                        clarification,
                        command_interrupted,
                    });
                    match manual_clarification::collect() {
                        manual_clarification::Outcome::Submitted(text) => {
                            let cwd = shell.cwd.clone();
                            if !task.submit_manual(&text, &cwd) {
                                task.cancel_manual();
                                break agent::RunResult::failed(
                                    "手动澄清状态无效，任务已关闭。",
                                    total_turns,
                                );
                            }
                            agent::present(agent::AgentEvent::ManualSubmitted {
                                phase: task.phase(),
                                text: &text,
                            });
                        }
                        manual_clarification::Outcome::Blank => {
                            if !task.abandon_manual() {
                                break agent::RunResult::failed(
                                    "手动澄清恢复点无效，任务已关闭。",
                                    total_turns,
                                );
                            }
                            agent::present(agent::AgentEvent::ManualResumed { timed_out: false });
                        }
                        manual_clarification::Outcome::TimedOut => {
                            if !task.abandon_manual() {
                                break agent::RunResult::failed(
                                    "手动澄清恢复点无效，任务已关闭。",
                                    total_turns,
                                );
                            }
                            agent::present(agent::AgentEvent::ManualResumed { timed_out: true });
                        }
                        manual_clarification::Outcome::Cancelled => {
                            task.cancel_manual();
                            break agent::RunResult::cancelled(
                                agent::CancelCause::UserInterrupted,
                                total_turns,
                            );
                        }
                        manual_clarification::Outcome::InputClosed => {
                            task.cancel_manual();
                            break agent::RunResult::cancelled(
                                agent::CancelCause::ClarificationClosed,
                                total_turns,
                            );
                        }
                        manual_clarification::Outcome::TerminalError => {
                            task.cancel_manual();
                            break agent::RunResult::failed(
                                "无法安全读取手动澄清输入",
                                total_turns,
                            );
                        }
                        manual_clarification::Outcome::Unavailable => {
                            task.cancel_manual();
                            break agent::RunResult::failed(
                                "无法安全进入手动澄清输入界面。",
                                total_turns,
                            );
                        }
                    }
                }
            }
        };
        let elapsed = t.elapsed().as_secs_f64();
        if let Some(answer) = result.answer() {
            agent::present(agent::AgentEvent::Answer { text: answer });
        }
        let outcome = result.final_outcome();
        agent::present(agent::AgentEvent::FinalStatus {
            outcome,
            reason: result.reason(),
            total_turns: result.turns,
            clarifications: task.clarifications(),
            elapsed_seconds: elapsed,
            state: if outcome == agent::FinalOutcome::Completed {
                None
            } else {
                Some(task.execution_state_note())
            },
        });
        let code = result.exit_code();
        shell.last_exit = code;
        return code;
    }

    shell.run_with_history(&routed.executable, history)
}
