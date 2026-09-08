//! Agent 澄清阶段的模态终端交互。

use crate::agent::{self, ClarificationAnswer, ClarificationQuestion, ClarificationReply};
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) enum Outcome {
    Submitted(ClarificationReply),
    TimedOut,
    Cancelled,
    InputClosed,
    TerminalError,
    Unavailable,
}

pub(super) fn collect(questions: &[ClarificationQuestion]) -> Outcome {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Outcome::Unavailable;
    }
    // 输入法可能分批提交同一段 UTF-8；整个模态交互期间只切换一次终端模式，避免
    // 批次之间短暂恢复 ECHO，造成内核回显与手动回显叠加。
    let Some(_raw_terminal) = RawTerminal::enter() else {
        return Outcome::TerminalError;
    };
    let mut pending_input = Vec::new();
    let mut answers = Vec::with_capacity(questions.len());
    for question in questions {
        let (selected_choice_ids, free_text) = match choose(question, &mut pending_input) {
            QuestionOutcome::Submitted(selected_choice_ids, free_text) => {
                (selected_choice_ids, free_text)
            }
            QuestionOutcome::TimedOut => return Outcome::TimedOut,
            QuestionOutcome::Cancelled => return Outcome::Cancelled,
            QuestionOutcome::InputClosed => return Outcome::InputClosed,
        };
        answers.push(ClarificationAnswer {
            question_id: question.id.clone(),
            selected_choice_ids,
            free_text,
        });
    }
    Outcome::Submitted(ClarificationReply { answers })
}

enum QuestionOutcome {
    Submitted(Vec<String>, String),
    TimedOut,
    Cancelled,
    InputClosed,
}

fn choose(question: &ClarificationQuestion, pending_input: &mut Vec<u8>) -> QuestionOutcome {
    let mut cursor = 0usize;
    let mut selected = vec![false; question.choices.len()];
    let mut free_text = Vec::new();
    let mut region = agent::TransientRegion::new();
    let mut deadline = Instant::now() + IDLE_TIMEOUT;
    region.render(|| render_question(question, cursor, &selected, &free_text));
    loop {
        let now = Instant::now();
        if now >= deadline {
            region.clear();
            return QuestionOutcome::TimedOut;
        }
        let Some(key) = read_key(pending_input, deadline.saturating_duration_since(now)) else {
            region.clear();
            return QuestionOutcome::TimedOut;
        };
        let mut accepted = false;
        match key.as_slice() {
            b"\x1b[A" if !question.choices.is_empty() => {
                cursor = cursor.saturating_sub(1);
                accepted = true;
                region.render(|| render_question(question, cursor, &selected, &free_text));
            }
            b"\x1b[B" if !question.choices.is_empty() => {
                cursor = (cursor + 1).min(question.choices.len() - 1);
                accepted = true;
                region.render(|| render_question(question, cursor, &selected, &free_text));
            }
            b" " if free_text.is_empty() && !question.choices.is_empty() => {
                if question.multiple {
                    selected[cursor] = !selected[cursor];
                } else {
                    selected.fill(false);
                    selected[cursor] = true;
                }
                accepted = true;
                region.render(|| render_question(question, cursor, &selected, &free_text));
            }
            b"\r" | b"\n" => {
                if !question.multiple
                    && free_text.is_empty()
                    && !question.choices.is_empty()
                    && !selected.iter().any(|selected| *selected)
                {
                    selected[cursor] = true;
                }
                region.clear();
                let selected_choice_ids = question
                    .choices
                    .iter()
                    .zip(selected)
                    .filter(|(_, selected)| *selected)
                    .map(|(choice, _)| choice.id.clone())
                    .collect();
                let free_text = String::from_utf8(free_text)
                    .ok()
                    .map(|value| value.trim().to_string());
                return free_text.map_or(QuestionOutcome::Cancelled, |free_text| {
                    QuestionOutcome::Submitted(selected_choice_ids, free_text)
                });
            }
            [8] | [127] => {
                if let Some(character) = pop_character(&mut free_text) {
                    accepted = true;
                    for _ in 0..UnicodeWidthChar::width(character).unwrap_or(0) {
                        eprint!("\x08 \x08");
                    }
                    let _ = io::stderr().flush();
                }
            }
            [3] => {
                region.clear();
                return QuestionOutcome::Cancelled;
            }
            [4] => {
                region.clear();
                return QuestionOutcome::InputClosed;
            }
            bytes if printable_text(bytes) => {
                free_text.extend_from_slice(bytes);
                accepted = true;
                eprint!("{}", String::from_utf8_lossy(bytes));
                let _ = io::stderr().flush();
            }
            _ => {}
        }
        if accepted {
            deadline = Instant::now() + IDLE_TIMEOUT;
        }
    }
}

fn render_question(
    question: &ClarificationQuestion,
    cursor: usize,
    selected: &[bool],
    free_text: &[u8],
) {
    let guidance = if question.choices.is_empty() {
        "（直接输入，回车确认；30秒无输入取消）"
    } else if question.multiple {
        "（↑↓选择，空格多选，回车确认；直接输入可补充或替代选项；30秒无输入取消）"
    } else {
        "（↑↓选择，空格选择，回车确认；直接输入可替代选项；30秒无输入取消）"
    };
    eprintln!(
        "? {}{guidance}",
        agent::sanitize_terminal_text(&question.prompt)
    );
    for (index, choice) in question.choices.iter().enumerate() {
        eprintln!(
            "  {} {} {}",
            if index == cursor { "›" } else { " " },
            if selected[index] { "[x]" } else { "[ ]" },
            agent::sanitize_terminal_text(&choice.label)
        );
    }
    eprint!(
        "  自由输入（可替代选项）: {}",
        String::from_utf8_lossy(free_text)
    );
    let _ = io::stderr().flush();
}

fn printable_text(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .is_ok_and(|text| text.chars().all(|character| !character.is_control()))
}

#[cfg(unix)]
struct RawTerminal {
    original: libc::termios,
}

#[cfg(unix)]
impl RawTerminal {
    fn enter() -> Option<Self> {
        let fd = libc::STDIN_FILENO;
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return None;
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        // 澄清 UI 自己消费 Ctrl-C/Ctrl-D；关闭 ISIG 才能收到对应字节并可靠取消。
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { original })
    }
}

#[cfg(unix)]
impl Drop for RawTerminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(not(unix))]
struct RawTerminal;

#[cfg(not(unix))]
impl RawTerminal {
    fn enter() -> Option<Self> {
        None
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

#[cfg(unix)]
fn read_key(pending: &mut Vec<u8>, timeout: Duration) -> Option<Vec<u8>> {
    if pending.is_empty() && !read_pending_input(pending, timeout) {
        return None;
    }

    let first = pending[0];
    let key_len = if first == b'\x1b' {
        if pending.len() < 3 {
            let _ = read_pending_input(pending, Duration::from_millis(10));
        }
        if pending.starts_with(b"\x1b[") && pending.len() >= 3 {
            3
        } else {
            1
        }
    } else if first.is_ascii() {
        1
    } else {
        // read(2) 不保证停在 UTF-8 字符边界；等齐当前标量后再交给渲染逻辑。
        let key_len = utf8_sequence_len(first);
        while pending.len() < key_len {
            if !read_pending_input(pending, timeout) {
                return None;
            }
        }
        if std::str::from_utf8(&pending[..key_len]).is_ok() {
            key_len
        } else {
            1
        }
    };
    Some(pending.drain(..key_len).collect())
}

#[cfg(unix)]
fn read_pending_input(pending: &mut Vec<u8>, timeout: Duration) -> bool {
    let fd = libc::STDIN_FILENO;
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe {
        libc::poll(
            &mut pollfd,
            1,
            timeout.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if ready <= 0 {
        return false;
    }
    let mut bytes = [0u8; 256];
    let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast::<libc::c_void>(), bytes.len()) };
    if count <= 0 {
        false
    } else {
        pending.extend_from_slice(&bytes[..count as usize]);
        true
    }
}

#[cfg(not(unix))]
fn read_key(_: &mut Vec<u8>, _: Duration) -> Option<Vec<u8>> {
    None
}

fn utf8_sequence_len(first: u8) -> usize {
    match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}
