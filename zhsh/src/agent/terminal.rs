//! Agent 运行期间的语义事件渲染、确认输入与终端模式守卫。

use super::safety::SafetyAssessment;
use super::{ClarificationQuestion, ClarificationReply};
use crate::common::CancellationToken;
use std::io::{self, IsTerminal, Write};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

/// 当前阶段的可观测进度。阶段轮次在每次 Request-Response 完成后递增。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskStatus {
    pub(crate) phase: u8,
    pub(crate) phase_turn: i32,
    pub(crate) total_turns: i32,
    pub(crate) clarifications: u8,
}

/// Agent 生命周期中需要稳定呈现的宿主语义事件。
pub(crate) enum AgentEvent<'a> {
    SafetyNotice {
        message: &'a str,
    },
    FormatRepair {
        status: TaskStatus,
    },
    EvidenceRepair {
        status: TaskStatus,
    },
    CommandProposed {
        command: &'a str,
    },
    CommandConfirmed {
        reason: &'a str,
    },
    CommandRejected {
        reason: &'a str,
    },
    PhaseEnded {
        phase: u8,
        phase_turns: i32,
        total_turns: i32,
        clarification: u8,
    },
    ClarificationSubmitted {
        phase: u8,
        questions: &'a [ClarificationQuestion],
        reply: &'a ClarificationReply,
    },
    ManualPaused {
        status: TaskStatus,
        clarification: u8,
        command_interrupted: bool,
    },
    ManualSubmitted {
        phase: u8,
        text: &'a str,
    },
    ManualResumed {
        timed_out: bool,
    },
    Answer {
        text: &'a str,
    },
    FinalStatus {
        outcome: FinalOutcome,
        reason: Option<&'a str>,
        total_turns: i32,
        clarifications: u8,
        elapsed_seconds: f64,
        state: Option<&'a str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalOutcome {
    Completed,
    Cancelled,
    Incomplete,
    Failed,
}

/// 统一输出语义事件。顶层事件一律从第一列开始，只有选项等子项允许缩进。
pub(crate) fn present(event: AgentEvent<'_>) {
    let is_terminal = io::stderr().is_terminal();
    let dim_event = uses_dim_style(&event);
    let final_status = matches!(event, AgentEvent::FinalStatus { .. });
    for (index, line) in event_lines(event).into_iter().enumerate() {
        let dim = is_terminal && (dim_event || final_status && index > 0);
        if dim {
            eprintln!("\x1b[90m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
}

fn uses_dim_style(event: &AgentEvent<'_>) -> bool {
    matches!(
        event,
        AgentEvent::CommandProposed { .. }
            | AgentEvent::ClarificationSubmitted { .. }
            | AgentEvent::ManualPaused { .. }
            | AgentEvent::ManualSubmitted { .. }
            | AgentEvent::ManualResumed { .. }
            | AgentEvent::FinalStatus {
                outcome: FinalOutcome::Completed,
                ..
            }
    )
}

fn event_lines(event: AgentEvent<'_>) -> Vec<String> {
    match event {
        AgentEvent::SafetyNotice { message } => {
            vec![format!("! {}", sanitize_terminal_text(message))]
        }
        AgentEvent::FormatRepair { status } => vec![format!(
            "! 响应格式无效 · 阶段 {} · {}/6 轮 · 累计 {} 轮 · 正在请求修复",
            status.phase, status.phase_turn, status.total_turns
        )],
        AgentEvent::EvidenceRepair { status } => vec![format!(
            "! 系统状态回答缺少输出证据 · 阶段 {} · {}/6 轮 · 累计 {} 轮 · 正在请求修复",
            status.phase, status.phase_turn, status.total_turns
        )],
        AgentEvent::CommandProposed { command } => command
            .split('\n')
            .map(|line| format!("> {}", sanitize_terminal_text(line)))
            .collect(),
        AgentEvent::CommandConfirmed { reason } => vec![format!(
            "· 已确认 · {}",
            sanitize_terminal_text(reason)
        )],
        AgentEvent::CommandRejected { reason } => {
            vec![format!("! 命令未执行 · {}", sanitize_terminal_text(reason))]
        }
        AgentEvent::PhaseEnded {
            phase,
            phase_turns,
            total_turns,
            clarification,
        } => vec![format!(
            "→ 阶段 {phase} 结束 · {phase_turns}/6 轮 · 累计 {total_turns} 轮 · 发起澄清 {clarification}/3"
        )],
        AgentEvent::ClarificationSubmitted {
            phase,
            questions,
            reply,
        } => clarification_summary_lines(phase, questions, reply),
        AgentEvent::ManualPaused {
            status,
            clarification,
            command_interrupted,
        } => vec![format!(
            "→ 手动澄清 · 阶段 {} 已暂停 · {}/6 轮 · 累计 {} 轮 · 澄清待提交 {}/3{}",
            status.phase,
            status.phase_turn,
            status.total_turns,
            clarification,
            if command_interrupted {
                " · 已记录命令中断结果"
            } else {
                ""
            }
        )],
        AgentEvent::ManualSubmitted { phase, text } => vec![
            format!("→ 手动澄清已提交 · 进入阶段 {phase}"),
            format!("  补充: {}", sanitize_terminal_text(text)),
        ],
        AgentEvent::ManualResumed { timed_out } => vec![format!(
            "· 未提交补充 · 继续当前阶段{}",
            if timed_out { "（输入超时）" } else { "" }
        )],
        AgentEvent::Answer { text } => text.lines().map(sanitize_terminal_text).collect(),
        AgentEvent::FinalStatus {
            outcome,
            reason,
            total_turns,
            clarifications,
            elapsed_seconds,
            state,
        } => {
            let mut metadata = format!("{total_turns}轮");
            if clarifications > 0 {
                metadata.push_str(&format!(" 澄清 {clarifications}次"));
            }
            metadata.push_str(&format!(" {elapsed_seconds:.1}s"));
            if outcome == FinalOutcome::Completed {
                return vec![metadata];
            }
            let label = match outcome {
                FinalOutcome::Completed => unreachable!(),
                FinalOutcome::Cancelled => "已取消",
                FinalOutcome::Incomplete => "未完成",
                FinalOutcome::Failed => "失败",
            };
            let mut summary = format!("! {label}");
            if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
                summary.push_str(" · ");
                summary.push_str(&sanitize_terminal_text(reason));
            }
            if let Some(state) = state.filter(|state| !state.is_empty()) {
                summary.push_str(" · ");
                summary.push_str(&sanitize_terminal_text(state));
            }
            vec![summary, metadata]
        }
    }
}

fn clarification_summary_lines(
    phase: u8,
    questions: &[ClarificationQuestion],
    reply: &ClarificationReply,
) -> Vec<String> {
    let mut lines = vec![format!("→ 澄清已提交 · 进入阶段 {phase}")];
    for answer in &reply.answers {
        let Some(question) = questions
            .iter()
            .find(|question| question.id == answer.question_id)
        else {
            continue;
        };
        let prompt = question.prompt.trim_end_matches(['?', '？']);
        let selected = question
            .choices
            .iter()
            .filter(|choice| answer.selected_choice_ids.contains(&choice.id))
            .map(|choice| sanitize_terminal_text(&choice.label))
            .collect::<Vec<_>>();
        if selected.is_empty() {
            if !answer.free_text.is_empty() {
                lines.push(format!(
                    "  {}: {}",
                    sanitize_terminal_text(prompt),
                    sanitize_terminal_text(&answer.free_text)
                ));
            }
        } else {
            lines.push(format!(
                "  {}: {}",
                sanitize_terminal_text(prompt),
                selected.join("、")
            ));
            if !answer.free_text.is_empty() {
                lines.push(format!(
                    "  补充: {}",
                    sanitize_terminal_text(&answer.free_text)
                ));
            }
        }
    }
    lines
}

pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\t' => safe.push('\t'),
            '\u{001b}' => safe.push_str("\\x1b"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(safe, "\\u{{{:x}}}", character as u32);
            }
            _ => safe.push(character),
        }
    }
    safe
}

/// 仅保存终端显示位置的临时区域，不持有任何 Agent 业务状态。
///
/// 支持光标恢复的终端从区域起点整体重绘和清除，因此不依赖文本逻辑行数、Unicode
/// 显示宽度或终端自动换行数量。无法可靠恢复光标时退化为追加输出。
pub(crate) struct TransientRegion {
    rewrite: bool,
    active: bool,
    restore: TransientRestore,
}

#[derive(Clone, Copy)]
enum TransientRestore {
    SavedCursor,
    FixedLines(usize),
}

impl TransientRegion {
    pub(crate) fn new() -> Self {
        Self {
            rewrite: supports_transient_rewrite(),
            active: false,
            restore: TransientRestore::SavedCursor,
        }
    }

    fn fixed_lines(lines: usize) -> Self {
        Self {
            rewrite: supports_transient_rewrite(),
            active: false,
            restore: TransientRestore::FixedLines(lines.max(1)),
        }
    }

    pub(crate) fn render(&mut self, render: impl FnOnce()) {
        if self.rewrite {
            if self.active {
                self.erase();
            } else if matches!(self.restore, TransientRestore::SavedCursor) {
                // 同时设置 DEC 与 CSI 保存槽；部分终端只实现其中一种。
                eprint!("\x1b7\x1b[s");
            }
        } else if self.active {
            eprintln!();
        }
        render();
        self.active = true;
        let _ = io::stderr().flush();
    }

    pub(crate) fn clear(&mut self) {
        if !self.active {
            return;
        }
        if self.rewrite {
            self.erase();
        } else {
            eprintln!();
        }
        self.active = false;
        let _ = io::stderr().flush();
    }

    fn erase(&self) {
        match self.restore {
            TransientRestore::SavedCursor => eprint!("\x1b[u\x1b8\x1b[J"),
            TransientRestore::FixedLines(lines) => {
                for index in 0..lines {
                    eprint!("\r\x1b[2K");
                    if index + 1 < lines {
                        eprint!("\x1b[1A");
                    }
                }
                eprint!("\r");
            }
        }
    }
}

impl Drop for TransientRegion {
    fn drop(&mut self) {
        self.clear();
    }
}

/// 控制加载动画线程结束并等待终端清理完成的 RAII 句柄。
pub(super) struct LoadingGuard {
    stop: Option<mpsc::Sender<()>>,
    done: Option<mpsc::Receiver<()>>,
}

/// 等待 Provider 期间隐藏 TTY 光标，并保证线程正常退出或展开时恢复。
struct HiddenCursor;

impl HiddenCursor {
    fn enter() -> Self {
        eprint!("\x1b[?25l");
        let _ = io::stderr().flush();
        Self
    }
}

impl Drop for HiddenCursor {
    fn drop(&mut self) {
        eprint!("\x1b[?25h");
        let _ = io::stderr().flush();
    }
}

impl LoadingGuard {
    /// 停止动画、清除当前行并等待动画线程退出。
    pub(super) fn stop(self) {
        if let Some(stop) = self.stop {
            let _ = stop.send(());
        }
        if let Some(done) = self.done {
            let _ = done.recv();
        }
        let _ = io::stderr().flush();
    }
}

/// 启动观察同一取消令牌的非阻塞加载动画。
///
/// 非终端 stderr 不启动动画也不输出 ANSI 控制序列。返回句柄必须在打印其他事件前停止。
pub(super) fn start_loading(
    cancellation: Arc<CancellationToken>,
    status: TaskStatus,
) -> LoadingGuard {
    if !io::stderr().is_terminal() {
        return LoadingGuard {
            stop: None,
            done: None,
        };
    }
    let (stop_tx, stop_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        {
            let _hidden_cursor = HiddenCursor::enter();
            let dots = [".  ", ".. ", "..."];
            let mut frame = 0usize;
            let status_line = format!(
                "· 阶段 {} · {}/6 轮 · 累计 {} 轮 · 澄清 {}/3",
                status.phase, status.phase_turn, status.total_turns, status.clarifications
            );
            let mut rendered = false;
            loop {
                if cancellation.is_cancelled() {
                    break;
                }
                let activity = format!("分析中{}", dots[(frame / 2) % dots.len()]);
                if rendered {
                    // 隐藏的光标停在状态行行首；只回到上一行刷新动画，再返回状态行。
                    eprint!("\x1b[1A\r\x1b[2K\x1b[90m{activity}\x1b[0m\x1b[1B\r");
                } else {
                    eprint!("\r\x1b[2K\x1b[90m{activity}\x1b[0m\n\x1b[90m{status_line}\x1b[0m\r");
                    rendered = true;
                }
                let _ = io::stderr().flush();
                frame = frame.wrapping_add(1);
                match stop_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            if rendered {
                // 先清除状态行，再回到上一行清除动画；后续稳定事件从第一列输出。
                eprint!("\r\x1b[2K\x1b[1A\r\x1b[2K");
            }
            let _ = io::stderr().flush();
        }
        // HiddenCursor 已恢复光标后才通知调用方，保证后续确认框、回答或 PS1 可输入。
        let _ = done_tx.send(());
    });
    LoadingGuard {
        stop: Some(stop_tx),
        done: Some(done_rx),
    }
}

/// 用户对一条需要确认的 Agent 命令的终端确认结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfirmationDecision {
    Approved,
    Rejected,
    TimedOut,
    InputClosed,
    Unavailable,
    Cancelled,
    TerminalError,
    ManualClarify,
}

const CONFIRMATION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 展示宿主侧风险判断，并要求用户明确确认即将执行的原始命令。
pub(super) fn confirm_command(
    assessment: &SafetyAssessment,
    cancellation: &CancellationToken,
    manual_clarification_available: bool,
) -> ConfirmationDecision {
    if cancellation.is_cancelled() {
        return ConfirmationDecision::Cancelled;
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return ConfirmationDecision::Unavailable;
    }

    // 确认控件固定为风险行和输入行。使用相对行清除，兼容不实现 CSI/DEC 光标保存的终端。
    let mut region = TransientRegion::fixed_lines(2);
    let mut answer = Vec::new();
    let mut deadline = Instant::now() + CONFIRMATION_IDLE_TIMEOUT;
    loop {
        region.render(|| {
            eprintln!("! {}", confirmation_summary(assessment));
            eprint!(
                "? 执行？[y/N]（30秒无输入取消） {}",
                sanitize_terminal_text(&String::from_utf8_lossy(&answer))
            );
        });
        let input = read_confirmation_line(cancellation, &mut deadline, &mut answer);
        region.clear();
        if cancellation.is_cancelled() {
            return ConfirmationDecision::Cancelled;
        }
        match input {
            Ok(ConfirmationInput::ManualClarify) if !manual_clarification_available => {
                present(AgentEvent::SafetyNotice {
                    message: "澄清额度已耗尽，无法手动澄清",
                });
            }
            Ok(ConfirmationInput::ManualClarify) => {
                return ConfirmationDecision::ManualClarify;
            }
            Ok(ConfirmationInput::Interrupted) => return ConfirmationDecision::Cancelled,
            Ok(ConfirmationInput::TimedOut) => return ConfirmationDecision::TimedOut,
            Ok(ConfirmationInput::InputClosed) => return ConfirmationDecision::InputClosed,
            Err(_) => return ConfirmationDecision::TerminalError,
            Ok(ConfirmationInput::Answer(value))
                if matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes") =>
            {
                return ConfirmationDecision::Approved;
            }
            Ok(ConfirmationInput::Answer(value)) if value.trim() == "是" => {
                return ConfirmationDecision::Approved;
            }
            Ok(ConfirmationInput::Answer(_)) => return ConfirmationDecision::Rejected,
        }
    }
}

fn confirmation_summary(assessment: &SafetyAssessment) -> String {
    assessment.primary_reason().into()
}

enum ConfirmationInput {
    Answer(String),
    ManualClarify,
    Interrupted,
    TimedOut,
    InputClosed,
}

#[cfg(unix)]
fn read_confirmation_line(
    cancellation: &CancellationToken,
    deadline: &mut Instant,
    answer: &mut Vec<u8>,
) -> io::Result<ConfirmationInput> {
    let _raw =
        RawInputGuard::enter(false).ok_or_else(|| io::Error::other("无法进入命令确认输入模式"))?;
    loop {
        if cancellation.is_cancelled() {
            return Ok(ConfirmationInput::Interrupted);
        }
        let now = Instant::now();
        if now >= *deadline {
            return Ok(ConfirmationInput::TimedOut);
        }
        let wait = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(50));
        let Some(byte) = read_byte(wait)? else {
            continue;
        };
        match byte {
            b'\r' | b'\n' => {
                return Ok(ConfirmationInput::Answer(
                    String::from_utf8_lossy(answer).into_owned(),
                ));
            }
            3 => return Ok(ConfirmationInput::Interrupted),
            4 => return Ok(ConfirmationInput::InputClosed),
            8 | 127 => {
                if let Some(character) = pop_character(answer) {
                    for _ in 0..UnicodeWidthChar::width(character).unwrap_or(0) {
                        eprint!("\x08 \x08");
                    }
                    let _ = io::stderr().flush();
                    *deadline = Instant::now() + CONFIRMATION_IDLE_TIMEOUT;
                }
            }
            b'\x1b' => {
                let next = read_byte(Duration::from_millis(25))?;
                if next.is_none() {
                    return Ok(ConfirmationInput::ManualClarify);
                }
                if matches!(next, Some(b'[' | b'O')) {
                    while let Some(byte) = read_byte(Duration::from_millis(2))? {
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
            }
            byte if !byte.is_ascii_control() => {
                answer.push(byte);
                if byte.is_ascii() {
                    eprint!("{}", byte as char);
                    let _ = io::stderr().flush();
                } else if let Ok(text) = std::str::from_utf8(answer) {
                    if let Some(character) = text.chars().next_back() {
                        eprint!("{character}");
                    }
                    let _ = io::stderr().flush();
                }
                *deadline = Instant::now() + CONFIRMATION_IDLE_TIMEOUT;
            }
            _ => {}
        }
    }
}

#[cfg(not(unix))]
fn read_confirmation_line(
    _: &CancellationToken,
    _: &mut Instant,
    _: &mut Vec<u8>,
) -> io::Result<ConfirmationInput> {
    let mut answer = String::new();
    let read = io::stdin().read_line(&mut answer)?;
    Ok(if read > 0 {
        ConfirmationInput::Answer(answer)
    } else {
        ConfirmationInput::InputClosed
    })
}

fn supports_transient_rewrite() -> bool {
    io::stderr().is_terminal()
        && std::env::var_os("TERM").is_none_or(|term| term != std::ffi::OsStr::new("dumb"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManualControlEvent {
    Requested,
    QuotaExhausted,
}

/// Agent 拥有终端时侦听独立 Esc。事件通道只负责唤醒主状态机，不保存业务状态。
pub(super) struct ManualControlGuard {
    event: mpsc::Receiver<ManualControlEvent>,
    stop: Option<mpsc::Sender<()>>,
    done: Option<mpsc::Receiver<()>>,
    #[cfg(unix)]
    original: Option<libc::termios>,
}

impl ManualControlGuard {
    pub(super) fn take_event(&self) -> Option<ManualControlEvent> {
        self.event.try_recv().ok()
    }

    pub(super) fn stop(mut self) -> Option<ManualControlEvent> {
        self.stop_inner();
        self.take_event()
    }

    fn stop_inner(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(done) = self.done.take() {
            let _ = done.recv();
        }
        #[cfg(unix)]
        if let Some(original) = self.original.take() {
            // SAFETY: original 是启动监听器前成功取得的 termios 快照。
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
            }
        }
    }
}

impl Drop for ManualControlGuard {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

pub(super) fn start_manual_control(
    cancellation: Arc<CancellationToken>,
    quota_available: bool,
    interrupt_operation: bool,
) -> ManualControlGuard {
    let (event_tx, event_rx) = mpsc::channel();
    #[cfg(unix)]
    {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return ManualControlGuard {
                event: event_rx,
                stop: None,
                done: None,
                original: None,
            };
        }
        let Some(raw) = RawInputGuard::enter(true) else {
            return ManualControlGuard {
                event: event_rx,
                stop: None,
                done: None,
                original: None,
            };
        };
        let original = raw.into_original();
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            loop {
                if stop_rx.try_recv().is_ok() || cancellation.is_hard_cancelled() {
                    break;
                }
                match read_byte(Duration::from_millis(25)) {
                    Ok(Some(4)) => {
                        cancellation.cancel();
                        break;
                    }
                    Ok(Some(b'\x1b')) => {
                        if read_byte(Duration::from_millis(25))
                            .ok()
                            .flatten()
                            .is_some()
                        {
                            continue;
                        }
                        let event = if quota_available {
                            ManualControlEvent::Requested
                        } else {
                            ManualControlEvent::QuotaExhausted
                        };
                        let _ = event_tx.send(event);
                        if quota_available && interrupt_operation {
                            cancellation.interrupt();
                        }
                        break;
                    }
                    Ok(Some(_)) | Ok(None) => {}
                    Err(_) => break,
                }
            }
            let _ = done_tx.send(());
        });
        ManualControlGuard {
            event: event_rx,
            stop: Some(stop_tx),
            done: Some(done_rx),
            original: Some(original),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (cancellation, quota_available, interrupt_operation, event_tx);
        ManualControlGuard {
            event: event_rx,
            stop: None,
            done: None,
        }
    }
}

#[cfg(unix)]
struct RawInputGuard {
    original: Option<libc::termios>,
}

#[cfg(unix)]
impl RawInputGuard {
    fn enter(keep_signals: bool) -> Option<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return None;
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        if !keep_signals {
            raw.c_lflag &= !libc::ISIG;
        }
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self {
            original: Some(original),
        })
    }

    fn into_original(mut self) -> libc::termios {
        self.original.take().expect("raw terminal owns a snapshot")
    }
}

#[cfg(unix)]
impl Drop for RawInputGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
            }
        }
    }
}

#[cfg(unix)]
fn read_byte(timeout: Duration) -> io::Result<Option<u8>> {
    let mut descriptor = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    if ready == 0 {
        return Ok(None);
    }
    let mut byte = 0u8;
    let read = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            (&mut byte as *mut u8).cast::<libc::c_void>(),
            1,
        )
    };
    if read == 1 {
        Ok(Some(byte))
    } else if read == 0 {
        Ok(Some(4))
    } else {
        Err(io::Error::last_os_error())
    }
}

fn pop_character(value: &mut Vec<u8>) -> Option<char> {
    let start = value
        .iter()
        .rposition(|byte| byte & 0b1100_0000 != 0b1000_0000)?;
    let character = std::str::from_utf8(&value[start..]).ok()?.chars().next()?;
    value.truncate(start);
    Some(character)
}

/// 临时关闭终端的 ECHOCTL，避免 Ctrl-C 留下字面 ^C。
#[cfg(unix)]
pub(super) struct InterruptEchoGuard(Option<libc::termios>);

#[cfg(unix)]
impl InterruptEchoGuard {
    pub(super) fn new() -> Self {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: 仅在 tcgetattr 成功后读取初始化后的 termios。
        let original = unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) == 0 {
                Some(original.assume_init())
            } else {
                None
            }
        };
        if let Some(mut quiet) = original {
            quiet.c_lflag &= !libc::ECHOCTL;
            // SAFETY: quiet 来自成功的 tcgetattr，且只在本次调用期间借用。
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet);
            }
        }
        Self(original)
    }
}

#[cfg(unix)]
impl Drop for InterruptEchoGuard {
    fn drop(&mut self) {
        if let Some(original) = &self.0 {
            // SAFETY: original 是守卫持有的有效 termios 快照。
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original);
            }
        }
    }
}

#[cfg(not(unix))]
pub(super) struct InterruptEchoGuard;

#[cfg(not(unix))]
impl InterruptEchoGuard {
    pub(super) fn new() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_block_only_shows_prefixed_command_lines() {
        assert_eq!(
            event_lines(AgentEvent::CommandProposed {
                command: "printf one\nprintf two",
            }),
            ["> printf one", "> printf two"]
        );
    }

    #[test]
    fn confirmation_decision_replaces_the_transient_prompt_with_one_record() {
        assert_eq!(
            event_lines(AgentEvent::CommandConfirmed {
                reason: "可能修改文件或系统状态",
            }),
            ["· 已确认 · 可能修改文件或系统状态"]
        );
    }

    #[test]
    fn cancelled_status_is_not_reported_as_failure() {
        assert_eq!(
            event_lines(AgentEvent::FinalStatus {
                outcome: FinalOutcome::Cancelled,
                reason: Some("用户拒绝执行命令"),
                total_turns: 1,
                clarifications: 0,
                elapsed_seconds: 6.7,
                state: Some("未执行命令"),
            }),
            ["! 已取消 · 用户拒绝执行命令 · 未执行命令", "1轮 6.7s"]
        );
    }

    #[test]
    fn transition_and_final_status_expose_task_progress() {
        assert_eq!(
            event_lines(AgentEvent::EvidenceRepair {
                status: TaskStatus {
                    phase: 1,
                    phase_turn: 2,
                    total_turns: 2,
                    clarifications: 0,
                },
            }),
            ["! 系统状态回答缺少输出证据 · 阶段 1 · 2/6 轮 · 累计 2 轮 · 正在请求修复"]
        );
        assert_eq!(
            event_lines(AgentEvent::PhaseEnded {
                phase: 1,
                phase_turns: 3,
                total_turns: 3,
                clarification: 1,
            }),
            ["→ 阶段 1 结束 · 3/6 轮 · 累计 3 轮 · 发起澄清 1/3"]
        );
        assert_eq!(
            event_lines(AgentEvent::FinalStatus {
                outcome: FinalOutcome::Failed,
                reason: Some("Provider 响应无效"),
                total_turns: 9,
                clarifications: 1,
                elapsed_seconds: 45.4,
                state: Some("未执行本轮命令"),
            }),
            [
                "! 失败 · Provider 响应无效 · 未执行本轮命令",
                "9轮 澄清 1次 45.4s"
            ]
        );
    }

    #[test]
    fn completed_status_is_compact_and_only_mentions_used_clarifications() {
        assert_eq!(
            event_lines(AgentEvent::FinalStatus {
                outcome: FinalOutcome::Completed,
                reason: None,
                total_turns: 2,
                clarifications: 0,
                elapsed_seconds: 4.74,
                state: None,
            }),
            ["2轮 4.7s"]
        );
        assert_eq!(
            event_lines(AgentEvent::FinalStatus {
                outcome: FinalOutcome::Completed,
                reason: None,
                total_turns: 4,
                clarifications: 1,
                elapsed_seconds: 12.26,
                state: None,
            }),
            ["4轮 澄清 1次 12.3s"]
        );
    }

    #[test]
    fn commands_and_completed_status_use_the_dim_non_answer_style() {
        assert!(uses_dim_style(&AgentEvent::CommandProposed {
            command: "pwd",
        }));
        assert!(!uses_dim_style(&AgentEvent::CommandConfirmed {
            reason: "无法静态验证命令行为",
        }));
        assert!(uses_dim_style(&AgentEvent::FinalStatus {
            outcome: FinalOutcome::Completed,
            reason: None,
            total_turns: 2,
            clarifications: 0,
            elapsed_seconds: 1.0,
            state: None,
        }));
        assert!(!uses_dim_style(&AgentEvent::FinalStatus {
            outcome: FinalOutcome::Failed,
            reason: Some("内部错误"),
            total_turns: 2,
            clarifications: 0,
            elapsed_seconds: 1.0,
            state: Some("未执行命令"),
        }));
    }

    #[test]
    fn model_text_cannot_inject_terminal_control_sequences() {
        assert_eq!(
            event_lines(AgentEvent::CommandProposed {
                command: "printf '\u{001b}[2J'",
            }),
            ["> printf '\\x1b[2J'"]
        );
    }

    #[test]
    fn answer_preserves_literal_backslash_sequences_and_real_lines() {
        assert_eq!(
            event_lines(AgentEvent::Answer {
                text: "printf 'a\\nb'\n第二行",
            }),
            ["printf 'a\\nb'", "第二行"]
        );
    }
}
