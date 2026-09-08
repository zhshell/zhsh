//! Agent 手动澄清的单行模态编辑器。

use crate::agent;
use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) enum Outcome {
    Submitted(String),
    Blank,
    TimedOut,
    Cancelled,
    InputClosed,
    TerminalError,
    Unavailable,
}

pub(super) fn collect() -> Outcome {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Outcome::Unavailable;
    }
    let Some(_raw) = RawTerminal::enter() else {
        return Outcome::TerminalError;
    };
    let mut text = Vec::<char>::new();
    let mut cursor = 0usize;
    let mut pending = Vec::new();
    let mut deadline = Instant::now() + IDLE_TIMEOUT;
    let mut region = agent::TransientRegion::new();
    region.render(|| render(&text, cursor));
    loop {
        let now = Instant::now();
        if now >= deadline {
            region.clear();
            discard_ready_input();
            return Outcome::TimedOut;
        }
        let Some(key) = read_key(&mut pending, deadline.saturating_duration_since(now)) else {
            region.clear();
            discard_ready_input();
            return Outcome::TimedOut;
        };
        let mut edited = false;
        match key.as_slice() {
            b"\r" | b"\n" => {
                region.clear();
                let value: String = text.iter().collect();
                return if value.trim().is_empty() {
                    Outcome::Blank
                } else {
                    Outcome::Submitted(value)
                };
            }
            [3] => {
                region.clear();
                return Outcome::Cancelled;
            }
            [4] => {
                region.clear();
                return Outcome::InputClosed;
            }
            b"\x1b[D" => {
                let next = cursor.saturating_sub(1);
                edited = next != cursor;
                cursor = next;
            }
            b"\x1b[C" => {
                let next = (cursor + 1).min(text.len());
                edited = next != cursor;
                cursor = next;
            }
            b"\x1b[H" | b"\x1b[1~" => {
                edited = cursor != 0;
                cursor = 0;
            }
            b"\x1b[F" | b"\x1b[4~" => {
                edited = cursor != text.len();
                cursor = text.len();
            }
            [8] | [127] if cursor > 0 => {
                cursor -= 1;
                text.remove(cursor);
                edited = true;
            }
            b"\x1b[3~" if cursor < text.len() => {
                text.remove(cursor);
                edited = true;
            }
            b"\x1b" => {}
            bytes => {
                if let Ok(value) = std::str::from_utf8(bytes) {
                    if let Some(character) = value
                        .chars()
                        .next()
                        .filter(|character| !character.is_control())
                    {
                        text.insert(cursor, character);
                        cursor += 1;
                        edited = true;
                    }
                }
            }
        }
        if edited {
            deadline = Instant::now() + IDLE_TIMEOUT;
            region.render(|| render(&text, cursor));
        }
    }
}

fn render(text: &[char], cursor: usize) {
    let value: String = text.iter().collect();
    eprint!(
        "? 补充说明（30秒无输入恢复）: {}",
        agent::sanitize_terminal_text(&value)
    );
    let trailing_width: usize = text[cursor..]
        .iter()
        .map(|character| UnicodeWidthChar::width(*character).unwrap_or(0))
        .sum();
    if trailing_width > 0 {
        eprint!("\x1b[{trailing_width}D");
    }
}

#[cfg(unix)]
struct RawTerminal {
    original: libc::termios,
}

#[cfg(unix)]
impl RawTerminal {
    fn enter() -> Option<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return None;
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
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

#[cfg(unix)]
fn read_key(pending: &mut Vec<u8>, timeout: Duration) -> Option<Vec<u8>> {
    if pending.is_empty() && !read_pending_input(pending, timeout) {
        return None;
    }
    let first = pending[0];
    let key_len = if first == b'\x1b' {
        if pending.len() < 2 {
            let _ = read_pending_input(pending, Duration::from_millis(25));
        }
        if pending
            .get(1)
            .is_some_and(|byte| matches!(byte, b'[' | b'O'))
        {
            let deadline = Instant::now() + Duration::from_millis(25);
            while !has_escape_final(pending) && Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !read_pending_input(pending, remaining) {
                    break;
                }
            }
        }
        escape_sequence_len(pending)
    } else if first.is_ascii() {
        1
    } else {
        let length = utf8_sequence_len(first);
        while pending.len() < length {
            if !read_pending_input(pending, timeout) {
                return None;
            }
        }
        length
    };
    Some(pending.drain(..key_len.min(pending.len())).collect())
}

#[cfg(not(unix))]
fn read_key(_: &mut Vec<u8>, _: Duration) -> Option<Vec<u8>> {
    None
}

fn escape_sequence_len(pending: &[u8]) -> usize {
    if pending.len() < 2 || !matches!(pending[1], b'[' | b'O') {
        return 1;
    }
    pending
        .iter()
        .enumerate()
        .skip(2)
        .find(|(_, byte)| (0x40..=0x7e).contains(*byte))
        .map_or(pending.len(), |(index, _)| index + 1)
}

fn has_escape_final(pending: &[u8]) -> bool {
    pending
        .iter()
        .skip(2)
        .any(|byte| (0x40..=0x7e).contains(byte))
}

#[cfg(unix)]
fn read_pending_input(pending: &mut Vec<u8>, timeout: Duration) -> bool {
    let mut descriptor = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    if ready <= 0 {
        return false;
    }
    let mut buffer = [0u8; 64];
    let read = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len(),
        )
    };
    if read <= 0 {
        return false;
    }
    pending.extend_from_slice(&buffer[..read as usize]);
    true
}

#[cfg(unix)]
fn discard_ready_input() {
    let mut pending = Vec::new();
    while read_pending_input(&mut pending, Duration::ZERO) {
        pending.clear();
    }
}

#[cfg(not(unix))]
fn discard_ready_input() {}

fn utf8_sequence_len(first: u8) -> usize {
    match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}
