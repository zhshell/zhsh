//! Agent 命令的交互模式判定与前台终端所有权管理。
//!
//! 普通 Agent 命令仍通过管道有界捕获；需要密码、终端行规程或全屏界面的命令则
//! 使用当前真实终端。行式交互允许在实时显示的同时形成输出证据；完整终端会话
//! 只透传，不把屏幕内容误写成稳定文本。这里的判定是运行时安全网，不依赖 LLM
//! 是否遵守 Prompt。

use std::io;

/// Agent 命令对当前真实终端和输出采集的需求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalMode {
    /// 不需要前台终端，沿用普通有界捕获。
    None,
    /// 需要真实终端输入，同时实时显示并有界采集行式输出。
    Captured,
    /// 需要完整终端语义，输出不转换为 Agent 文本证据。
    Opaque,
}

impl TerminalMode {
    pub(super) fn requires_terminal(self) -> bool {
        self != Self::None
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Opaque, _) | (_, Self::Opaque) => Self::Opaque,
            (Self::Captured, _) | (_, Self::Captured) => Self::Captured,
            _ => Self::None,
        }
    }
}

/// 判断 Bash 命令中是否存在需要真实终端的前台程序。
///
/// 只检查命令位置，不会因为 `echo sudo` 或引号中的普通参数误判。解析器刻意保持
/// 保守和轻量；完整 Shell 语义仍由 Bash 负责。
pub(super) fn requires_terminal(input: &str) -> bool {
    terminal_mode(input).requires_terminal()
}

/// 判断命令需要普通捕获、真实终端行式捕获还是不透明终端透传。
pub(super) fn terminal_mode(input: &str) -> TerminalMode {
    let mut mode = TerminalMode::None;
    let mut words = Vec::new();
    for token in tokens(input) {
        match token {
            Token::Boundary => {
                mode = mode.merge(segment_mode(&words));
                words.clear();
            }
            Token::Word(word) => words.push(word),
        }
    }
    mode.merge(segment_mode(&words))
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_wrapper(program: &str) -> bool {
    matches!(program, "command" | "exec" | "env" | "nohup" | "time")
}

fn is_command_introducer(word: &str) -> bool {
    matches!(word, "!" | "if" | "then" | "elif" | "else" | "do")
}

fn segment_mode(words: &[String]) -> TerminalMode {
    let mut index = 0;
    while index < words.len()
        && (is_command_introducer(&words[index]) || is_assignment(&words[index]))
    {
        index += 1;
    }
    while let Some(word) = words.get(index) {
        let program = semantic_name(word);
        if !is_wrapper(program) {
            return program_mode(program, &words[index + 1..], 0);
        }
        index += 1;
        while index < words.len() && (words[index].starts_with('-') || is_assignment(&words[index]))
        {
            index += 1;
        }
    }
    TerminalMode::None
}

fn program_mode(program: &str, arguments: &[String], depth: usize) -> TerminalMode {
    if depth >= 8 {
        return TerminalMode::Opaque;
    }
    if is_opaque_program(program) {
        return TerminalMode::Opaque;
    }
    if is_line_interactive_program(program) {
        return TerminalMode::Captured;
    }
    match program {
        "sudo" => privilege_wrapper_mode(arguments, depth + 1, PrivilegeWrapper::Sudo),
        "doas" => privilege_wrapper_mode(arguments, depth + 1, PrivilegeWrapper::Doas),
        "pkexec" => privilege_wrapper_mode(arguments, depth + 1, PrivilegeWrapper::Pkexec),
        "su" => su_mode(arguments, depth + 1),
        "ssh" => ssh_mode(arguments),
        "rsync" => rsync_mode(arguments),
        "docker" => docker_exec_mode(arguments),
        "kubectl" => kubectl_exec_mode(arguments),
        _ => TerminalMode::None,
    }
}

#[derive(Clone, Copy)]
enum PrivilegeWrapper {
    Sudo,
    Doas,
    Pkexec,
}

fn privilege_wrapper_mode(
    arguments: &[String],
    depth: usize,
    wrapper: PrivilegeWrapper,
) -> TerminalMode {
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--" {
            index += 1;
            break;
        }
        if matches!(wrapper, PrivilegeWrapper::Sudo)
            && matches!(argument.as_str(), "-i" | "--login" | "-s" | "--shell")
        {
            return TerminalMode::Opaque;
        }
        if !argument.starts_with('-') || argument == "-" {
            break;
        }
        let takes_value = match wrapper {
            PrivilegeWrapper::Sudo => matches!(
                argument.as_str(),
                "-u" | "--user"
                    | "-g"
                    | "--group"
                    | "-h"
                    | "--host"
                    | "-p"
                    | "--prompt"
                    | "-C"
                    | "--close-from"
                    | "-T"
                    | "--command-timeout"
                    | "-R"
                    | "--chroot"
                    | "-D"
                    | "--chdir"
            ),
            PrivilegeWrapper::Doas => {
                matches!(argument.as_str(), "-u" | "-C" | "--config")
            }
            PrivilegeWrapper::Pkexec => matches!(argument.as_str(), "--user"),
        };
        index += 1 + usize::from(takes_value && index + 1 < arguments.len());
    }
    let Some(program) = arguments.get(index) else {
        return TerminalMode::Captured;
    };
    let nested = program_mode(semantic_name(program), &arguments[index + 1..], depth);
    TerminalMode::Captured.merge(nested)
}

fn su_mode(arguments: &[String], depth: usize) -> TerminalMode {
    let command = arguments.iter().enumerate().find_map(|(index, argument)| {
        if matches!(argument.as_str(), "-c" | "--command") {
            arguments.get(index + 1).map(String::as_str)
        } else {
            argument
                .strip_prefix("--command=")
                .filter(|command| !command.is_empty())
                .map(str::trim)
        }
    });
    let Some(command) = command else {
        return TerminalMode::Opaque;
    };
    TerminalMode::Captured.merge(terminal_mode_at_depth(command, depth))
}

fn terminal_mode_at_depth(input: &str, depth: usize) -> TerminalMode {
    if depth >= 8 {
        TerminalMode::Opaque
    } else {
        let mut mode = TerminalMode::None;
        let mut words = Vec::new();
        for token in tokens(input) {
            match token {
                Token::Boundary => {
                    mode = mode.merge(segment_mode_at_depth(&words, depth));
                    words.clear();
                }
                Token::Word(word) => words.push(word),
            }
        }
        mode.merge(segment_mode_at_depth(&words, depth))
    }
}

fn segment_mode_at_depth(words: &[String], depth: usize) -> TerminalMode {
    let Some(index) = words
        .iter()
        .position(|word| !is_command_introducer(word) && !is_assignment(word))
    else {
        return TerminalMode::None;
    };
    program_mode(semantic_name(&words[index]), &words[index + 1..], depth)
}

fn ssh_mode(arguments: &[String]) -> TerminalMode {
    let mut index = 0;
    let mut forces_terminal_protocol = false;
    let mut no_remote_command = false;
    let mut query_only = false;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--" {
            index += 1;
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            break;
        }
        if argument.starts_with("-t") || argument == "-N" {
            forces_terminal_protocol = true;
        }
        if matches!(argument.as_str(), "-G" | "-Q" | "-V") {
            query_only = true;
        }
        if argument == "-N" {
            no_remote_command = true;
        }
        let short = argument.as_bytes().get(1).copied().map(char::from);
        let takes_value = short.is_some_and(|option| {
            matches!(
                option,
                'B' | 'b'
                    | 'c'
                    | 'D'
                    | 'E'
                    | 'e'
                    | 'F'
                    | 'I'
                    | 'i'
                    | 'J'
                    | 'L'
                    | 'l'
                    | 'm'
                    | 'O'
                    | 'o'
                    | 'P'
                    | 'p'
                    | 'Q'
                    | 'R'
                    | 'S'
                    | 'W'
                    | 'w'
            )
        }) && argument.len() == 2;
        index += 1 + usize::from(takes_value && index + 1 < arguments.len());
    }
    let destination = index;
    if query_only {
        return TerminalMode::None;
    }
    let has_remote_command = !no_remote_command && destination + 1 < arguments.len();
    if has_remote_command && !forces_terminal_protocol {
        TerminalMode::Captured
    } else {
        TerminalMode::Opaque
    }
}

fn rsync_mode(arguments: &[String]) -> TerminalMode {
    let remote_shell = arguments.iter().enumerate().any(|(index, argument)| {
        matches!(argument.as_str(), "-e" | "--rsh")
            && arguments
                .get(index + 1)
                .is_some_and(|value| value.split_whitespace().next() == Some("ssh"))
            || argument
                .strip_prefix("--rsh=")
                .is_some_and(|value| value.split_whitespace().next() == Some("ssh"))
    });
    let remote_operand = arguments.iter().any(|argument| {
        !argument.starts_with('-')
            && (argument.contains("://")
                || argument
                    .split_once(':')
                    .is_some_and(|(host, _)| !host.is_empty() && !host.contains('/')))
    });
    if remote_shell || remote_operand {
        TerminalMode::Captured
    } else {
        TerminalMode::None
    }
}

fn docker_exec_mode(arguments: &[String]) -> TerminalMode {
    let Some(index) = arguments.iter().position(|argument| argument == "exec") else {
        return TerminalMode::None;
    };
    let options = arguments[index + 1..]
        .iter()
        .take_while(|argument| argument.starts_with('-'));
    if options
        .into_iter()
        .any(|argument| is_interactive_exec_option(argument))
    {
        TerminalMode::Opaque
    } else {
        TerminalMode::None
    }
}

fn kubectl_exec_mode(arguments: &[String]) -> TerminalMode {
    let Some(index) = arguments.iter().position(|argument| argument == "exec") else {
        return TerminalMode::None;
    };
    let options = arguments[index + 1..]
        .iter()
        .take_while(|argument| argument.as_str() != "--");
    if options
        .into_iter()
        .any(|argument| is_interactive_exec_option(argument))
    {
        TerminalMode::Opaque
    } else {
        TerminalMode::None
    }
}

fn is_interactive_exec_option(argument: &str) -> bool {
    matches!(argument, "-i" | "--interactive" | "-t" | "--tty")
        || argument.strip_prefix('-').is_some_and(|options| {
            !options.starts_with('-') && (options.contains('i') || options.contains('t'))
        })
}

fn semantic_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_line_interactive_program(program: &str) -> bool {
    matches!(
        program,
        // 单行输入、凭据和磁盘口令。
        "passwd" | "chsh" | "chfn" | "cryptsetup" | "read" | "select" | "scp"
    )
}

fn is_opaque_program(program: &str) -> bool {
    matches!(
        program,
        // 持续远程会话和协议终端。
        "mosh"
            | "sftp"
            | "ftp"
            | "telnet"
            | "gpg"
            | "pinentry"
            // 分页器、编辑器和全屏终端程序。
            | "less"
            | "more"
            | "man"
            | "info"
            | "vim"
            | "vi"
            | "nvim"
            | "nano"
            | "pico"
            | "emacs"
            | "top"
            | "htop"
            | "btop"
            | "glances"
            | "watch"
            | "tmux"
            | "screen"
            | "ranger"
            | "mc"
            | "nnn"
            | "lf"
            | "nmtui"
            | "alsamixer"
            | "pulsemixer"
            | "dialog"
            | "whiptail"
            | "gdb"
            // 常见交互式数据库终端和 Shell 内建读取。
            | "mysql"
            | "mariadb"
            | "psql"
            | "sqlite3"
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Boundary,
}

/// 提取判断命令位置所需的最小 Shell token，同时忽略引号内部的控制运算符。
fn tokens(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in input.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
            }
            Some('"') => match character {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => word.push(character),
            },
            Some(_) => unreachable!(),
            None => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => escaped = true,
                '|' | '&' | ';' | '\n' | '\r' | '(' | ')' => {
                    push_word(&mut tokens, &mut word);
                    if !matches!(tokens.last(), Some(Token::Boundary)) {
                        tokens.push(Token::Boundary);
                    }
                }
                character if character.is_whitespace() => push_word(&mut tokens, &mut word),
                _ => word.push(character),
            },
        }
    }
    if escaped {
        word.push('\\');
    }
    push_word(&mut tokens, &mut word);
    tokens
}

fn push_word(tokens: &mut Vec<Token>, word: &mut String) {
    if !word.is_empty() {
        tokens.push(Token::Word(std::mem::take(word)));
    }
}

/// 把独立 Agent 进程组临时设置为当前终端的前台进程组。
#[cfg(unix)]
pub(super) struct ForegroundTerminal {
    fd: libc::c_int,
    owner: libc::pid_t,
    original: libc::termios,
}

#[cfg(unix)]
impl ForegroundTerminal {
    pub(super) fn give_to(process_group: u32) -> io::Result<Self> {
        Self::give_to_with_settings(process_group, None)
    }

    /// 把终端交给前台进程组，并在继续暂停命令前恢复该命令保存的终端属性。
    pub(super) fn give_to_with_settings(
        process_group: u32,
        settings: Option<&libc::termios>,
    ) -> io::Result<Self> {
        let fd = libc::STDIN_FILENO;
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `original` 指向可写的 termios 空间，成功后才读取；其余调用只使用
        // 当前进程拥有的控制终端和已创建的子进程组编号。
        unsafe {
            if libc::isatty(fd) != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "当前输入不是终端，无法运行交互命令",
                ));
            }
            if libc::tcgetattr(fd, original.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let original = original.assume_init();
            let owner = libc::tcgetpgrp(fd);
            if owner < 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(settings) = settings {
                if libc::tcsetattr(fd, libc::TCSADRAIN, settings) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Err(error) = set_foreground(fd, process_group as libc::pid_t) {
                libc::tcsetattr(fd, libc::TCSANOW, &original);
                return Err(error);
            }
            // 子进程可能在父进程交出终端前因 SIGTTIN/SIGTTOU 暂停。
            libc::kill(-(process_group as libc::pid_t), libc::SIGCONT);
            Ok(Self {
                fd,
                owner,
                original,
            })
        }
    }

    /// 保存暂停瞬间由前台程序设置的终端属性，供后续 `fg` 恢复。
    pub(super) fn current_settings(&self) -> io::Result<libc::termios> {
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: settings 指向可写空间，成功后才读取。
        unsafe {
            if libc::tcgetattr(self.fd, settings.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(settings.assume_init())
        }
    }
}

#[cfg(unix)]
impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        // SAFETY: owner 是进入守卫时从同一控制终端读取的前台进程组；original 是
        // tcgetattr 成功返回的值。Drop 中只能尽力恢复。
        unsafe {
            let _ = set_foreground(self.fd, self.owner);
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
fn set_foreground(fd: libc::c_int, process_group: libc::pid_t) -> io::Result<()> {
    // 调用方在收回终端时暂时属于后台进程组。线程级阻塞 SIGTTOU，避免恢复动作
    // 自己把 zhsh 暂停；随后原样恢复调用线程的信号掩码。
    let mut blocked = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: 两个 sigset_t 都由 libc 初始化；pthread_sigmask 和 tcsetpgrp 不保存指针。
    unsafe {
        libc::sigemptyset(blocked.as_mut_ptr());
        libc::sigaddset(blocked.as_mut_ptr(), libc::SIGTTOU);
        let blocked = blocked.assume_init();
        if libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, previous.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let result = libc::tcsetpgrp(fd, process_group);
        let error = io::Error::last_os_error();
        let previous = previous.assume_init();
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if result == 0 {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_privilege_remote_and_tui_commands_at_command_positions() {
        for command in [
            "sudo dpkg -i package.deb",
            "/usr/bin/sudo -v",
            "TOKEN=x env LANG=C sudo apt install package",
            "true && ssh server",
            "printf done; nvim file",
            "if true; then /usr/bin/passwd; fi",
        ] {
            assert!(requires_terminal(command), "{command}");
        }
    }

    #[test]
    fn distinguishes_line_capture_from_opaque_terminal_sessions() {
        for command in [
            "sudo apt upgrade -y",
            "sudo -u root java -version",
            "ssh server uptime",
            "su -c 'id'",
            "read -rp 'Value: ' value",
            "scp file server:/tmp/file",
            "rsync -e ssh file server:/tmp/file",
        ] {
            assert_eq!(terminal_mode(command), TerminalMode::Captured, "{command}");
        }
        for command in [
            "sudo vim /etc/hosts",
            "sudo -i",
            "ssh server",
            "ssh -t server uptime",
            "su -",
            "vim file",
            "docker exec -it container sh",
            "kubectl exec --stdin --tty pod -- sh",
        ] {
            assert_eq!(terminal_mode(command), TerminalMode::Opaque, "{command}");
        }
        for command in [
            "ssh -V",
            "ssh -G server",
            "rsync source destination",
            "docker exec container uname -r",
            "kubectl exec pod -- uname -r",
        ] {
            assert_eq!(terminal_mode(command), TerminalMode::None, "{command}");
        }
    }

    #[test]
    fn ignores_interactive_program_names_used_as_plain_arguments() {
        for command in [
            "echo sudo",
            "printf '%s\\n' ssh",
            "rg 'sudo' src",
            "printf \"vim; ssh\"",
        ] {
            assert!(!requires_terminal(command), "{command}");
        }
    }
}
