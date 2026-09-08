//! Bash PS0/PS1/PS2 提示符渲染。
//!
//! 本层只读取会话状态并展开提示符，不执行命令替换。PS3/PS4 由保存到 SessionState 的
//! Bash 变量声明传给实际 Bash 子进程消费。

use crate::shell::SessionState;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

pub(crate) struct PromptRenderer {
    hostname: OnceLock<HostnameSnapshot>,
}

struct HostnameSnapshot {
    full: String,
    short: String,
}

#[derive(Clone, Copy)]
pub(crate) struct PromptContext {
    pub(crate) history_number: usize,
    pub(crate) command_number: u64,
}

impl PromptRenderer {
    pub(crate) const fn new() -> Self {
        Self {
            hostname: OnceLock::new(),
        }
    }

    pub(crate) fn prompt(&self, session: &SessionState, context: PromptContext) -> String {
        self.render(
            session,
            session
                .prompt_variable("PS1")
                .unwrap_or(SessionState::DEFAULT_PS1),
            context,
        )
    }

    pub(crate) fn secondary_prompt(
        &self,
        session: &SessionState,
        context: PromptContext,
    ) -> String {
        self.render(
            session,
            session
                .prompt_variable("PS2")
                .unwrap_or(SessionState::DEFAULT_PS2),
            context,
        )
    }

    pub(crate) fn pre_command_prompt(
        &self,
        session: &SessionState,
        context: PromptContext,
    ) -> Option<String> {
        session
            .prompt_variable("PS0")
            .map(|value| self.render(session, value, context))
    }

    fn render(&self, session: &SessionState, template: &str, context: PromptContext) -> String {
        let escaped = self.expand_backslash_escapes(session, template, context);
        expand_parameters(session, &escaped)
    }

    fn hostname(&self) -> &HostnameSnapshot {
        self.hostname.get_or_init(load_hostname)
    }

    fn expand_backslash_escapes(
        &self,
        session: &SessionState,
        template: &str,
        context: PromptContext,
    ) -> String {
        let mut output = String::with_capacity(template.len());
        let mut characters = template.chars().peekable();
        while let Some(character) = characters.next() {
            if character != '\\' {
                output.push(character);
                continue;
            }
            let Some(escape) = characters.next() else {
                output.push('\\');
                break;
            };
            match escape {
                'a' => output.push('\x07'),
                'd' => output.push_str(&format_time("%a %b %d")),
                'D' if characters.peek() == Some(&'{') => {
                    characters.next();
                    let mut format = String::new();
                    let mut closed = false;
                    for value in characters.by_ref() {
                        if value == '}' {
                            closed = true;
                            break;
                        }
                        format.push(value);
                    }
                    if closed {
                        output.push_str(&format_time(&format));
                    } else {
                        output.push_str("\\D{");
                        output.push_str(&format);
                    }
                }
                'e' | 'E' => output.push('\x1b'),
                'h' => output.push_str(&self.hostname().short),
                'H' => output.push_str(&self.hostname().full),
                'j' => output.push('0'),
                'l' => output.push_str(&terminal_name()),
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                's' => output.push_str("zhsh"),
                't' => output.push_str(&format_time("%H:%M:%S")),
                'T' => output.push_str(&format_time("%I:%M:%S")),
                '@' => output.push_str(&format_time("%I:%M %p")),
                'A' => output.push_str(&format_time("%H:%M")),
                'u' => output.push_str(
                    session
                        .env
                        .get("USER")
                        .map(String::as_str)
                        .unwrap_or("user"),
                ),
                'v' => output.push_str(short_version()),
                'V' => output.push_str(env!("CARGO_PKG_VERSION")),
                'w' => output.push_str(&display_cwd(session, false)),
                'W' => output.push_str(&display_cwd(session, true)),
                '!' => output.push_str(&context.history_number.to_string()),
                '#' => output.push_str(&context.command_number.to_string()),
                '$' => output.push(if effective_user_is_root() { '#' } else { '$' }),
                '[' | ']' => {}
                '\\' => output.push('\\'),
                first @ '0'..='7' => {
                    let mut octal = String::from(first);
                    while octal.len() < 3
                        && characters
                            .peek()
                            .is_some_and(|value| matches!(value, '0'..='7'))
                    {
                        octal.push(characters.next().expect("peeked octal digit"));
                    }
                    if let Ok(value) = u8::from_str_radix(&octal, 8) {
                        output.push(value as char);
                    }
                }
                other => {
                    output.push('\\');
                    output.push(other);
                }
            }
        }
        output
    }
}

/// 只展开提示符常用参数；命令替换、算术展开和反引号保持字面文本。
fn expand_parameters(session: &SessionState, template: &str) -> String {
    let mut output = String::with_capacity(template.len());
    let mut characters = template.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '$' {
            output.push(character);
            continue;
        }
        match characters.peek().copied() {
            Some('?') => {
                characters.next();
                output.push_str(&session.last_exit.to_string());
            }
            Some('$') => {
                characters.next();
                output.push_str(&std::process::id().to_string());
            }
            Some('{') => {
                characters.next();
                let mut name = String::new();
                let mut closed = false;
                for value in characters.by_ref() {
                    if value == '}' {
                        closed = true;
                        break;
                    }
                    name.push(value);
                }
                if closed && valid_parameter_name(&name) {
                    output.push_str(shell_value(session, &name).unwrap_or(""));
                } else {
                    output.push_str("${");
                    output.push_str(&name);
                    if closed {
                        output.push('}');
                    }
                }
            }
            Some(first) if first == '_' || first.is_ascii_alphabetic() => {
                let mut name = String::new();
                while characters
                    .peek()
                    .is_some_and(|value| *value == '_' || value.is_ascii_alphanumeric())
                {
                    name.push(characters.next().expect("peeked variable character"));
                }
                output.push_str(shell_value(session, &name).unwrap_or(""));
            }
            _ => output.push('$'),
        }
    }
    output
}

fn shell_value<'a>(session: &'a SessionState, name: &str) -> Option<&'a str> {
    session
        .prompt_variable(name)
        .or_else(|| session.env.get(name).map(String::as_str))
}

fn valid_parameter_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && characters.all(|value| value == '_' || value.is_ascii_alphanumeric())
}

fn display_cwd(session: &SessionState, basename_only: bool) -> String {
    let cwd = &session.cwd;
    let home = session.env.get("HOME").map(Path::new);
    if basename_only {
        if home.is_some_and(|home| cwd == home) {
            return "~".into();
        }
        return cwd
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/".into());
    }
    if let Some(home) = home {
        if cwd == home {
            return "~".into();
        }
        if let Ok(relative) = cwd.strip_prefix(home) {
            return format!("~/{}", relative.to_string_lossy());
        }
    }
    cwd.to_string_lossy().into_owned()
}

fn load_hostname() -> HostnameSnapshot {
    fn read() -> Option<String> {
        let file = std::fs::File::open("/etc/hostname").ok()?;
        let mut bytes = Vec::new();
        file.take(257).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 256 || bytes.contains(&0) {
            return None;
        }
        let value = std::str::from_utf8(&bytes).ok()?.trim().to_string();
        if value.is_empty() || value.contains(['\r', '\n']) {
            return None;
        }
        Some(value)
    }

    let full = read().unwrap_or_else(|| "host".into());
    let short = full
        .split('.')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("host")
        .to_string();
    HostnameSnapshot { full, short }
}

#[cfg(test)]
fn prompt(session: &SessionState, context: PromptContext) -> String {
    PromptRenderer::new().prompt(session, context)
}

#[cfg(test)]
fn secondary_prompt(session: &SessionState, context: PromptContext) -> String {
    PromptRenderer::new().secondary_prompt(session, context)
}

#[cfg(test)]
fn pre_command_prompt(session: &SessionState, context: PromptContext) -> Option<String> {
    PromptRenderer::new().pre_command_prompt(session, context)
}

fn short_version() -> &'static str {
    let version = env!("CARGO_PKG_VERSION");
    version.rsplit_once('.').map_or(version, |(short, _)| short)
}

#[cfg(unix)]
fn effective_user_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn effective_user_is_root() -> bool {
    false
}

#[cfg(unix)]
fn terminal_name() -> String {
    let mut buffer = [0i8; 256];
    if unsafe { libc::ttyname_r(libc::STDIN_FILENO, buffer.as_mut_ptr(), buffer.len()) } != 0 {
        return "tty".into();
    }
    let value = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
    Path::new(value.as_ref())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tty".into())
}

#[cfg(not(unix))]
fn terminal_name() -> String {
    "tty".into()
}

#[cfg(unix)]
fn format_time(format: &str) -> String {
    let Ok(format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    let timestamp = unsafe { libc::time(std::ptr::null_mut()) };
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    if unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) }.is_null() {
        return String::new();
    }
    let local = unsafe { local.assume_init() };
    let mut output = [0u8; 1024];
    let written = unsafe {
        libc::strftime(
            output.as_mut_ptr().cast::<libc::c_char>(),
            output.len(),
            format.as_ptr(),
            &local,
        )
    };
    String::from_utf8_lossy(&output[..written]).into_owned()
}

#[cfg(not(unix))]
fn format_time(_: &str) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn default_prompt_uses_the_compact_zhsh_style() {
        let mut session = SessionState::test();
        session.env.insert("USER".into(), "jungle".into());
        session.env.insert("HOME".into(), "/home/alice".into());
        session.cwd = PathBuf::from("/home/alice/work");
        let rendered = prompt(
            &session,
            PromptContext {
                history_number: 1,
                command_number: 1,
            },
        );
        assert!(rendered.starts_with("\x1b[38;5;124m｢zh｣\x1b[0m\x1b[38;5;36mjungle@"));
        assert!(rendered.contains(":\x1b[36m~/work\x1b[0m"));
        assert!(rendered.ends_with(if effective_user_is_root() { "# " } else { "$ " }));
    }

    #[test]
    fn custom_ps1_expands_bash_prompt_escapes_and_safe_parameters() {
        let mut session = SessionState::test();
        session.env.insert("USER".into(), "alice".into());
        session.env.insert("HOME".into(), "/home/alice".into());
        session.env.insert("PROJECT".into(), "zhsh".into());
        session.cwd = PathBuf::from("/home/alice/project");
        session.last_exit = 7;
        session
            .set_prompt_variable(
                "PS1",
                r"\[\e[31m\]\u:\w:\W:\!:042:\#:9:$?:${PROJECT}:$(literal)\[\e[0m\] ",
            )
            .unwrap();

        let rendered = prompt(
            &session,
            PromptContext {
                history_number: 42,
                command_number: 9,
            },
        );

        assert_eq!(
            rendered,
            "\x1b[31malice:~/project:project:42:042:9:9:7:zhsh:$(literal)\x1b[0m "
        );
    }

    #[test]
    fn explicitly_empty_ps1_disables_the_primary_prompt() {
        let mut session = SessionState::test();
        session.set_prompt_variable("PS1", "").unwrap();
        assert_eq!(
            prompt(
                &session,
                PromptContext {
                    history_number: 1,
                    command_number: 1,
                }
            ),
            ""
        );
    }

    #[test]
    fn hostname_is_loaded_only_when_a_prompt_escape_needs_it() {
        let mut session = SessionState::test();
        let renderer = PromptRenderer::new();
        let context = PromptContext {
            history_number: 1,
            command_number: 1,
        };
        session.set_prompt_variable("PS1", r"\u:\w\$ ").unwrap();
        renderer.prompt(&session, context);
        assert!(renderer.hostname.get().is_none());

        session.set_prompt_variable("PS1", r"\h:\H\$ ").unwrap();
        renderer.prompt(&session, context);
        let first = renderer.hostname.get().unwrap() as *const HostnameSnapshot;
        renderer.prompt(&session, context);
        assert_eq!(first, renderer.hostname.get().unwrap() as *const _);
    }

    #[test]
    fn ps0_and_ps2_share_the_safe_prompt_renderer() {
        let mut session = SessionState::test();
        session.env.insert("USER".into(), "alice".into());
        session.set_prompt_variable("PS0", r"run:\u:$? ").unwrap();
        session.set_prompt_variable("PS2", r"more:\#> ").unwrap();
        session.last_exit = 3;
        let context = PromptContext {
            history_number: 8,
            command_number: 5,
        };

        assert_eq!(
            pre_command_prompt(&session, context).as_deref(),
            Some("run:alice:3 ")
        );
        assert_eq!(secondary_prompt(&session, context), "more:5> ");
    }
}
