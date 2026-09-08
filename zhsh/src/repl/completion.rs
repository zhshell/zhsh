//! rustyline 的上下文相关 Tab 补全。
//!
//! 命令位置候选来自 PATH、内建命令、别名和已加载的 Shell 函数；参数位置补全文件
//! 路径、`zh` 子命令、现有 LLM 配置名或模型档位。轻量词法上下文负责反解引用与
//! 反斜杠并安全生成替换文本。补全器持有轻量会话快照，由 REPL 在每条命令执行后
//! 重建；PATH 可执行文件由进程级缓存按需扫描并依据目录元数据失效。

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::Context;
use rustyline::Helper;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use crate::llm;
use crate::shell::Shell;

/// 基于一个会话快照为 rustyline 提供补全的辅助器。
pub(crate) struct ShellCompleter {
    cache: Arc<CompletionCache>,
    namespace: CompletionNamespace,
    cwd: PathBuf,
    user_home: Option<PathBuf>,
    exportable_codec_formats: Vec<String>,
    uninstallable_codec_formats: Vec<String>,
    home: String,
}

/// 在整个 REPL 生命周期中复用的 PATH 命令补全缓存。
pub(crate) struct CompletionCache {
    state: Mutex<CompletionCacheState>,
}

#[derive(Default)]
struct CompletionCacheState {
    path_key: Option<PathCacheKey>,
    directory_stamps: Vec<PathDirectoryStamp>,
    executables: Arc<[String]>,
    executable_generation: u64,
    merge_key: Option<CommandMergeKey>,
    commands: Arc<[CommandCandidate]>,
}

#[derive(Clone)]
struct CompletionNamespace {
    path: String,
    cwd: PathBuf,
    aliases: Vec<String>,
    functions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathCacheKey {
    raw_path: String,
    cwd_if_relative: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathDirectoryStamp {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandMergeKey {
    executable_generation: u64,
    aliases: Vec<String>,
    functions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandCandidate {
    value: String,
    append_space: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteMode {
    None,
    Single,
    Double,
}

#[derive(Debug, PartialEq, Eq)]
struct CompletionSite {
    start: usize,
    partial: String,
    quote: QuoteMode,
    words: Vec<String>,
    command_position: bool,
    disabled: bool,
}

impl ShellCompleter {
    /// 从当前命令、cwd 和 HOME 创建轻量补全快照。
    ///
    /// 此构造函数不遍历 PATH。REPL 应在每条可能修改相关状态的命令之后重新创建
    /// helper，并始终复用同一个 [`CompletionCache`]。
    pub(crate) fn new(shell: &Shell, cache: Arc<CompletionCache>) -> Self {
        let mut aliases = shell.aliases.keys().cloned().collect::<Vec<_>>();
        aliases.sort();
        let mut functions = shell.functions.keys().cloned().collect::<Vec<_>>();
        functions.sort();
        let namespace = CompletionNamespace {
            path: shell.env.get("PATH").cloned().unwrap_or_default(),
            cwd: shell.cwd.clone(),
            aliases,
            functions,
        };
        let summaries = shell.codec_runtime().summaries();
        let exportable_codec_formats = summaries
            .iter()
            .filter(|summary| {
                !summary.official && summary.source == llm::PluginSource::UserInstalled
            })
            .map(|summary| summary.label())
            .collect();
        let uninstallable_codec_formats = summaries
            .iter()
            .filter(|summary| summary.source == llm::PluginSource::UserInstalled)
            .map(|summary| summary.label())
            .collect();
        ShellCompleter {
            cache,
            namespace,
            cwd: shell.cwd.clone(),
            user_home: shell.user_home().map(Path::to_path_buf),
            exportable_codec_formats,
            uninstallable_codec_formats,
            home: shell.env.get("HOME").cloned().unwrap_or_default(),
        }
    }
}

impl CompletionCache {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(CompletionCacheState::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, CompletionCacheState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                *state = CompletionCacheState::default();
                state
            }
        }
    }

    fn commands(&self, namespace: &CompletionNamespace) -> Arc<[CommandCandidate]> {
        let path_key = path_cache_key(&namespace.path, &namespace.cwd);
        let directories = resolved_path_directories(&namespace.path, &namespace.cwd);
        let current_stamps = directory_stamps(&directories);
        let mut state = self.state();
        let path_changed = state.path_key.as_ref() != Some(&path_key);
        let stamps_changed = current_stamps
            .as_ref()
            .is_none_or(|stamps| stamps != &state.directory_stamps);

        if path_changed || stamps_changed {
            state.executables = Shell::executable_names(&namespace.path, &namespace.cwd).into();
            state.executable_generation = state.executable_generation.saturating_add(1);
            state.path_key = Some(path_key);
            state.directory_stamps = current_stamps.unwrap_or_default();
            state.merge_key = None;
        }

        let merge_key = CommandMergeKey {
            executable_generation: state.executable_generation,
            aliases: namespace.aliases.clone(),
            functions: namespace.functions.clone(),
        };
        if state.merge_key.as_ref() != Some(&merge_key) {
            state.commands =
                collect_commands(&state.executables, &namespace.aliases, &namespace.functions)
                    .into();
            state.merge_key = Some(merge_key);
        }
        Arc::clone(&state.commands)
    }
}

fn path_cache_key(path: &str, cwd: &Path) -> PathCacheKey {
    let depends_on_cwd = path
        .split(':')
        .any(|directory| directory.is_empty() || !Path::new(directory).is_absolute());
    PathCacheKey {
        raw_path: path.to_string(),
        cwd_if_relative: depends_on_cwd.then(|| cwd.to_path_buf()),
    }
}

fn resolved_path_directories(path: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    path.split(':')
        .map(|directory| {
            if directory.is_empty() {
                cwd.to_path_buf()
            } else {
                let directory = Path::new(directory);
                if directory.is_absolute() {
                    directory.to_path_buf()
                } else {
                    cwd.join(directory)
                }
            }
        })
        .filter(|directory| seen.insert(directory.clone()))
        .collect()
}

fn directory_stamps(directories: &[PathBuf]) -> Option<Vec<PathDirectoryStamp>> {
    directories
        .iter()
        .map(|path| {
            let metadata = fs::metadata(path).ok()?;
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                Some((metadata.dev(), metadata.ino()))
            };
            #[cfg(not(unix))]
            let identity = None;
            Some(PathDirectoryStamp {
                path: path.clone(),
                identity,
                modified: Some(metadata.modified().ok()?),
            })
        })
        .collect()
}

/// 合并 PATH 程序、内建命令、别名和 Shell 函数，并稳定排序去重。
fn collect_commands(
    executables: &[String],
    aliases: &[String],
    functions: &[String],
) -> Vec<CommandCandidate> {
    let mut seen = HashSet::new();
    let mut commands = Vec::new();

    fn insert(
        commands: &mut Vec<CommandCandidate>,
        seen: &mut HashSet<String>,
        value: String,
        append_space: bool,
    ) {
        if seen.insert(value.clone()) {
            commands.push(CommandCandidate {
                value,
                append_space,
            });
        }
    }

    for name in executables {
        insert(&mut commands, &mut seen, name.clone(), true);
    }

    // 内建命令
    for builtin in Shell::builtin_names() {
        insert(
            &mut commands,
            &mut seen,
            builtin.to_string(),
            Shell::builtin_takes_arguments(builtin),
        );
    }

    // 别名
    for name in aliases {
        insert(&mut commands, &mut seen, name.clone(), true);
    }

    for name in functions {
        insert(&mut commands, &mut seen, name.clone(), true);
    }

    commands.sort_by(|left, right| left.value.cmp(&right.value));
    commands
}

/// 解析光标前的轻量 Shell 词法上下文。
///
/// 这里只识别补全需要的引号、反斜杠、空白和命令分隔符，不承担执行语法解析。
fn completion_site(before: &str) -> CompletionSite {
    let mut words = Vec::new();
    let mut word_start = None;
    let mut word = String::new();
    let mut quote = QuoteMode::None;
    let mut leading_quote = QuoteMode::None;
    let mut disabled = false;
    let mut chars = before.char_indices().peekable();

    fn finish_word(words: &mut Vec<String>, word_start: &mut Option<usize>, word: &mut String) {
        if word_start.take().is_some() {
            words.push(std::mem::take(word));
        }
    }

    while let Some((index, character)) = chars.next() {
        match quote {
            QuoteMode::Single => {
                if character == '\'' {
                    quote = QuoteMode::None;
                    leading_quote = QuoteMode::None;
                } else {
                    word.push(character);
                }
            }
            QuoteMode::Double => match character {
                '"' => {
                    quote = QuoteMode::None;
                    leading_quote = QuoteMode::None;
                }
                '\\' => {
                    if let Some((_, escaped)) = chars.next() {
                        if matches!(escaped, '$' | '`' | '"' | '\\') {
                            word.push(escaped);
                        } else if escaped != '\n' {
                            word.push('\\');
                            word.push(escaped);
                        }
                    } else {
                        word.push('\\');
                    }
                }
                _ => word.push(character),
            },
            QuoteMode::None => match character {
                '\\' => {
                    word_start.get_or_insert(index);
                    if let Some((_, escaped)) = chars.next() {
                        if escaped != '\n' {
                            word.push(escaped);
                        }
                    } else {
                        word.push('\\');
                    }
                }
                '\'' => {
                    let starts_here = word_start.is_none();
                    word_start.get_or_insert(index);
                    quote = QuoteMode::Single;
                    if starts_here {
                        leading_quote = QuoteMode::Single;
                    }
                }
                '"' => {
                    let starts_here = word_start.is_none();
                    word_start.get_or_insert(index);
                    quote = QuoteMode::Double;
                    if starts_here {
                        leading_quote = QuoteMode::Double;
                    }
                }
                '\n' => {
                    finish_word(&mut words, &mut word_start, &mut word);
                    words.clear();
                    leading_quote = QuoteMode::None;
                }
                character if character.is_whitespace() => {
                    finish_word(&mut words, &mut word_start, &mut word);
                    leading_quote = QuoteMode::None;
                }
                ';' | '|' | '&' | '(' | ')' => {
                    finish_word(&mut words, &mut word_start, &mut word);
                    words.clear();
                    leading_quote = QuoteMode::None;
                }
                '<' | '>' => {
                    finish_word(&mut words, &mut word_start, &mut word);
                    leading_quote = QuoteMode::None;
                }
                '#' if word_start.is_none() => {
                    disabled = true;
                    break;
                }
                _ => {
                    word_start.get_or_insert(index);
                    word.push(character);
                }
            },
        }
    }

    let raw_start = word_start.unwrap_or(before.len());
    let preserve_quote = leading_quote == quote && quote != QuoteMode::None;
    let start = if preserve_quote {
        raw_start + 1
    } else {
        raw_start
    };
    let quote = if preserve_quote {
        quote
    } else {
        QuoteMode::None
    };
    let command_position = expects_command(&words);

    CompletionSite {
        start,
        partial: word,
        quote,
        words,
        command_position,
        disabled,
    }
}

fn expects_command(words: &[String]) -> bool {
    let words = words
        .iter()
        .skip_while(|word| is_assignment(word))
        .collect::<Vec<_>>();
    let Some((command, arguments)) = words.split_first() else {
        return true;
    };
    match command.as_str() {
        "command" | "exec" | "nohup" | "time" | "sudo" => {
            arguments.iter().all(|argument| argument.starts_with('-'))
        }
        "env" => arguments
            .iter()
            .all(|argument| argument.starts_with('-') || is_assignment(argument)),
        _ => false,
    }
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

fn path_completions(
    partial: &str,
    cwd: &Path,
    home: &str,
    quote: QuoteMode,
    executable_only: bool,
) -> Vec<Pair> {
    if quote == QuoteMode::None && partial == "~" && !home.is_empty() {
        return vec![Pair {
            display: "~/".into(),
            replacement: "~/".into(),
        }];
    }

    let (typed_directory, prefix) = match partial.rfind('/') {
        Some(position) => (&partial[..=position], &partial[position + 1..]),
        None => ("", partial),
    };
    let directory = if quote == QuoteMode::None && typed_directory.starts_with("~/") {
        if home.is_empty() {
            return Vec::new();
        }
        PathBuf::from(home).join(&typed_directory[2..])
    } else if Path::new(typed_directory).is_absolute() {
        PathBuf::from(typed_directory)
    } else {
        cwd.join(typed_directory)
    };

    let show_hidden = prefix.starts_with('.');
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.chars().any(char::is_control)
                || !name.starts_with(prefix)
                || (!show_hidden && name.starts_with('.'))
            {
                continue;
            }
            let Ok(metadata) = fs::metadata(entry.path()) else {
                continue;
            };
            let is_directory = metadata.is_dir();
            if executable_only && !is_directory && !metadata_is_executable(&metadata) {
                continue;
            }

            let mut value = format!("{typed_directory}{name}");
            if is_directory {
                value.push('/');
            }
            results.push(Pair {
                display: if is_directory {
                    format!("{name}/")
                } else {
                    name
                },
                replacement: render_replacement(&value, quote, !is_directory),
            });
        }
    }
    results.sort_by(|left, right| left.display.cmp(&right.display));
    results
}

fn metadata_is_executable(metadata: &fs::Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn render_replacement(value: &str, quote: QuoteMode, finish_word: bool) -> String {
    let mut rendered = match quote {
        QuoteMode::None => escape_unquoted(value),
        QuoteMode::Single => value.replace('\'', "'\\''"),
        QuoteMode::Double => value
            .chars()
            .flat_map(|character| {
                if matches!(character, '$' | '`' | '"' | '\\') {
                    vec!['\\', character]
                } else {
                    vec![character]
                }
            })
            .collect(),
    };
    if finish_word {
        match quote {
            QuoteMode::None => rendered.push(' '),
            QuoteMode::Single => rendered.push_str("' "),
            QuoteMode::Double => rendered.push_str("\" "),
        }
    }
    rendered
}

fn escape_unquoted(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_whitespace()
            || matches!(
                character,
                '\\' | '\''
                    | '"'
                    | '`'
                    | '$'
                    | '&'
                    | ';'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '*'
                    | '?'
                    | '!'
                    | '#'
                    | '='
            )
        {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn zh_completions(words: &[&str], word: &str, home: Option<&Path>) -> Option<Vec<String>> {
    let candidates: Vec<String> = match words {
        ["zh"] => [
            "status", "ls", "use", "llm", "tier", "trust", "safety", "codec", "help",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        ["zh", "use"] => home
            .and_then(|home| llm::list_configs(home).ok())
            .unwrap_or_default(),
        ["zh", "llm"] => ["-m", "--modify", "--help"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["zh", "llm", "-m" | "--modify"] => home
            .and_then(|home| llm::list_configs(home).ok())
            .unwrap_or_default(),
        ["zh", "llm", "-m" | "--modify", _] => Vec::new(),
        ["zh", "tier"] => ["flash", "standard", "max"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["zh", "trust"] => Shell::agent_trust_values()
            .iter()
            .copied()
            .chain(std::iter::once("-w"))
            .map(str::to_string)
            .collect(),
        ["zh", "trust", "-w"] => Shell::agent_trust_values()
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        ["zh", "trust", "balanced" | "confirm" | "trusted"] => vec!["-w".into()],
        ["zh", "safety"] => ["-t", "assess", "reload", "install", "help", "-h", "--help"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["zh", "safety", "install", rest @ ..] => {
            if rest.contains(&"--overwrite") {
                Vec::new()
            } else {
                vec!["--overwrite".into()]
            }
        }
        ["zh", "safety", "-t" | "assess" | "reload" | "help" | "-h" | "--help", ..] => Vec::new(),
        ["zh", "codec"] => [
            "ls",
            "-t",
            "install",
            "uninstall",
            "export",
            "reload",
            "help",
            "-h",
            "--help",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        ["zh", "codec", "-t" | "install"] | ["zh", "codec", "-t" | "install", "--"] => Vec::new(),
        ["zh", "codec", "export"] => ["-o", "--output-dir"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["zh", "codec", "export", "-o" | "--output-dir"] => Vec::new(),
        ["zh", "codec", "export", _] => ["-o", "--output-dir"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["zh", "codec", "uninstall"] => Vec::new(),
        ["zh", "codec", "ls" | "reload" | "help" | "-h" | "--help"] => Vec::new(),
        _ => return None,
    };
    Some(
        candidates
            .into_iter()
            .filter(|candidate| candidate.starts_with(word))
            .collect(),
    )
}

fn zh_replacement(words: &[&str], candidate: &str) -> String {
    if (matches!(words, ["zh"])
        && matches!(
            candidate,
            "use" | "llm" | "tier" | "trust" | "safety" | "codec"
        ))
        || (matches!(words, ["zh", "trust"]) && candidate == "-w")
        || (matches!(words, ["zh", "llm"]) && matches!(candidate, "-m" | "--modify"))
        || (matches!(words, ["zh", "safety"]) && matches!(candidate, "assess" | "install"))
        || (matches!(words, ["zh", "safety", "install", ..]) && candidate == "--overwrite")
        || (matches!(words, ["zh", "codec"])
            && matches!(candidate, "-t" | "install" | "uninstall" | "export"))
        || (matches!(words, ["zh", "codec", "export"])
            && matches!(candidate, "-o" | "--output-dir"))
        || (matches!(
            words,
            ["zh", "codec", "export"] | ["zh", "codec", "uninstall"]
        ) && candidate.contains('@'))
    {
        format!("{} ", candidate)
    } else {
        candidate.to_string()
    }
}

fn zh_path_argument(words: &[&str]) -> bool {
    (words.len() >= 3 && words[..3] == ["zh", "safety", "install"])
        || matches!(
            words,
            ["zh", "safety", "-t"]
                | ["zh", "codec", "-t"]
                | ["zh", "codec", "-t", "--"]
                | ["zh", "codec", "install"]
                | ["zh", "codec", "install", "--"]
                | ["zh", "codec", "export", "-o" | "--output-dir"]
                | ["zh", "codec", "export", _, "-o" | "--output-dir"]
        )
}

fn zh_config_argument(words: &[&str]) -> bool {
    matches!(words, ["zh", "use"] | ["zh", "llm", "-m" | "--modify"])
}

impl Completer for ShellCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Pair>), ReadlineError> {
        let before = &line[..pos];
        let site = completion_site(before);
        if site.disabled {
            return Ok((pos, Vec::new()));
        }
        let words = site.words.iter().map(String::as_str).collect::<Vec<_>>();

        let codec_formats = match words.as_slice() {
            ["zh", "codec", "export"] => Some(
                self.exportable_codec_formats
                    .iter()
                    .filter(|format| format.starts_with(&site.partial))
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            ["zh", "codec", "uninstall"] => Some(
                self.uninstallable_codec_formats
                    .iter()
                    .filter(|format| format.starts_with(&site.partial))
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        if let Some(mut hits) = zh_completions(&words, &site.partial, self.user_home.as_deref()) {
            if let Some(formats) = codec_formats {
                hits.extend(formats);
            }
            let mut pairs: Vec<Pair> = hits
                .into_iter()
                .map(|hit| Pair {
                    display: hit.clone(),
                    replacement: if zh_config_argument(&words) {
                        render_replacement(&hit, site.quote, true)
                    } else {
                        zh_replacement(&words, &hit)
                    },
                })
                .collect();
            if zh_path_argument(&words) {
                pairs.extend(path_completions(
                    &site.partial,
                    &self.cwd,
                    &self.home,
                    site.quote,
                    false,
                ));
                pairs.sort_by(|left, right| left.display.cmp(&right.display));
                pairs.dedup_by(|left, right| left.replacement == right.replacement);
            }
            return Ok((site.start, pairs));
        }

        if site.command_position && !site.partial.contains('/') {
            let commands = self.cache.commands(&self.namespace);
            let hits = commands
                .iter()
                .filter(|candidate| candidate.value.starts_with(&site.partial))
                .map(|candidate| Pair {
                    display: candidate.value.clone(),
                    replacement: render_replacement(
                        &candidate.value,
                        site.quote,
                        candidate.append_space,
                    ),
                })
                .collect();
            return Ok((site.start, hits));
        }

        Ok((
            site.start,
            path_completions(
                &site.partial,
                &self.cwd,
                &self.home,
                site.quote,
                site.command_position,
            ),
        ))
    }
}

impl Helper for ShellCompleter {}
impl Highlighter for ShellCompleter {
    fn highlight<'l>(&self, l: &'l str, _: usize) -> std::borrow::Cow<'l, str> {
        std::borrow::Cow::Borrowed(l)
    }
    fn highlight_char(&self, _: &str, _: usize, _: CmdKind) -> bool {
        false
    }
}
impl Hinter for ShellCompleter {
    type Hint = String;
    fn hint(&self, _: &str, _: usize, _: &Context<'_>) -> Option<String> {
        None
    }
}
impl Validator for ShellCompleter {
    fn validate(
        &self,
        _: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        Ok(rustyline::validate::ValidationResult::Valid(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::DefaultHistory;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zhsh-completion-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        root
    }

    fn completer(cwd: &Path, commands: &[&str]) -> ShellCompleter {
        ShellCompleter {
            cache: Arc::new(CompletionCache::new()),
            namespace: CompletionNamespace {
                path: "/definitely/missing".into(),
                cwd: cwd.to_path_buf(),
                aliases: commands.iter().map(|value| (*value).to_string()).collect(),
                functions: Vec::new(),
            },
            cwd: cwd.to_path_buf(),
            user_home: Some(PathBuf::from("/home/tester")),
            exportable_codec_formats: Vec::new(),
            uninstallable_codec_formats: Vec::new(),
            home: "/home/tester".into(),
        }
    }

    fn complete(completer: &ShellCompleter, line: &str) -> (usize, Vec<Pair>) {
        let history = DefaultHistory::new();
        completer
            .complete(line, line.len(), &Context::new(&history))
            .unwrap()
    }

    #[test]
    fn completes_top_level_zh_commands() {
        let hits = zh_completions(&["zh"], "st", Some(Path::new("/missing"))).unwrap();
        assert_eq!(hits, vec!["status"]);
        let hits = zh_completions(&["zh"], "tr", Some(Path::new("/missing"))).unwrap();
        assert_eq!(hits, vec!["trust"]);
        let hits = zh_completions(&["zh"], "sa", Some(Path::new("/missing"))).unwrap();
        assert_eq!(hits, vec!["safety"]);
        let hits = zh_completions(&["zh"], "co", Some(Path::new("/missing"))).unwrap();
        assert_eq!(hits, vec!["codec"]);
    }

    #[test]
    fn collects_history_as_a_builtin_command() {
        assert!(collect_commands(&[], &[], &[])
            .iter()
            .any(|candidate| candidate.value == "history"));
    }

    #[cfg(unix)]
    #[test]
    fn collects_executable_commands_from_the_session_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = fixture();
        let executable = root.join("zhsh-path-probe");
        std::fs::write(&executable, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.join("not-executable"), "fixture").unwrap();
        let mut shell = Shell::new();
        shell
            .env
            .insert("PATH".into(), root.to_string_lossy().into_owned());

        let commands = Shell::executable_names(shell.env.get("PATH").unwrap(), &shell.cwd);
        assert!(commands
            .iter()
            .any(|candidate| candidate == "zhsh-path-probe"));
        assert!(!commands
            .iter()
            .any(|candidate| candidate == "not-executable"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn path_cache_is_lazy_and_refreshes_when_the_directory_changes() {
        use std::os::unix::fs::PermissionsExt;

        let root = fixture();
        let mut shell = Shell::new();
        shell
            .env
            .insert("PATH".into(), root.to_string_lossy().into_owned());
        shell.cwd = root.clone();
        let completer = ShellCompleter::new(&shell, Arc::new(CompletionCache::new()));

        let first = root.join("late-command");
        std::fs::write(&first, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(complete(&completer, "late-").1[0].display, "late-command");

        let replaced = root.with_extension("replaced");
        let _ = std::fs::remove_dir_all(&replaced);
        std::fs::rename(&root, &replaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        let second = root.join("new-command");
        std::fs::write(&second, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(complete(&completer, "new-").1[0].display, "new-command");

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(replaced);
    }

    #[test]
    fn completes_tier_values() {
        let hits = zh_completions(&["zh", "tier"], "st", Some(Path::new("/missing"))).unwrap();
        assert_eq!(hits, vec!["standard"]);
    }

    #[test]
    fn completes_llm_modify_options_and_configuration_names() {
        let root = fixture();
        let directory = root.join(".zhsh/llm");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("deepseek.llm");
        std::fs::write(&config, "NAME=deepseek\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join(".zhsh"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        assert_eq!(
            zh_completions(&["zh", "llm"], "--m", Some(&root)).unwrap(),
            vec!["--modify"]
        );
        assert_eq!(
            zh_completions(&["zh", "llm", "-m"], "dee", Some(&root)).unwrap(),
            vec!["deepseek"]
        );
        assert!(zh_completions(&["zh", "llm"], "dee", Some(&root))
            .unwrap()
            .is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn completes_trust_values_and_write_option() {
        assert_eq!(
            zh_completions(&["zh", "trust"], "tr", Some(Path::new("/missing"))).unwrap(),
            vec!["trusted"]
        );
        assert_eq!(
            zh_completions(&["zh", "trust", "-w"], "c", Some(Path::new("/missing"))).unwrap(),
            vec!["confirm"]
        );
        assert_eq!(
            zh_completions(
                &["zh", "trust", "balanced"],
                "-",
                Some(Path::new("/missing"))
            )
            .unwrap(),
            vec!["-w"]
        );
    }

    #[test]
    fn completes_safety_operations() {
        assert_eq!(
            zh_completions(&["zh", "safety"], "r", Some(Path::new("/missing"))).unwrap(),
            vec!["reload"]
        );
        assert!(
            zh_completions(&["zh", "safety", "reload"], "", Some(Path::new("/missing")))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            zh_completions(
                &["zh", "safety", "install"],
                "--o",
                Some(Path::new("/missing"))
            )
            .unwrap(),
            vec!["--overwrite"]
        );
        assert!(zh_completions(
            &["zh", "codec", "install"],
            "--t",
            Some(Path::new("/missing"))
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            zh_completions(&["zh", "codec"], "un", Some(Path::new("/missing"))).unwrap(),
            vec!["uninstall"]
        );
    }

    #[test]
    fn plugin_install_completion_keeps_options_and_paths_together() {
        let root = fixture();
        std::fs::write(root.join("company.zhse.json"), "{}").unwrap();
        std::fs::write(root.join("provider.zhcodec"), "codec").unwrap();
        let completer = completer(&root, &[]);

        let (_, safety) = complete(&completer, "zh safety install ");
        assert!(safety.iter().any(|item| item.display == "--overwrite"));
        assert!(safety
            .iter()
            .any(|item| item.display == "company.zhse.json"));

        let (_, codec) = complete(&completer, "zh codec install ");
        assert!(codec.iter().any(|item| item.display == "provider.zhcodec"));
        assert!(!codec.iter().any(|item| item.display == "--trust-key"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parameterized_zh_commands_complete_with_a_space() {
        assert_eq!(zh_replacement(&["zh"], "use"), "use ");
        assert_eq!(zh_replacement(&["zh"], "llm"), "llm ");
        assert_eq!(zh_replacement(&["zh"], "tier"), "tier ");
        assert_eq!(zh_replacement(&["zh"], "trust"), "trust ");
        assert_eq!(zh_replacement(&["zh"], "safety"), "safety ");
        assert_eq!(zh_replacement(&["zh", "llm"], "-m"), "-m ");
        assert_eq!(zh_replacement(&["zh", "trust"], "-w"), "-w ");
        assert_eq!(zh_replacement(&["zh"], "status"), "status");
    }

    #[test]
    fn shell_context_decodes_escaped_spaces_and_pipeline_commands() {
        let site = completion_site("cd One\\ Piece/Sea");
        assert_eq!(site.start, 3);
        assert_eq!(site.partial, "One Piece/Sea");
        assert_eq!(site.words, ["cd"]);
        assert!(!site.command_position);

        let site = completion_site("printf ok | gi");
        assert_eq!(site.start, "printf ok | ".len());
        assert_eq!(site.partial, "gi");
        assert!(site.command_position);
    }

    #[test]
    fn directory_completion_escapes_spaces_and_preserves_path_prefix() {
        let root = fixture();
        std::fs::create_dir(root.join("Season 23")).unwrap();
        std::fs::create_dir(root.join("One Piece")).unwrap();
        std::fs::create_dir(root.join("One Piece/Season 23")).unwrap();
        let completer = completer(&root, &[]);

        let (start, hits) = complete(&completer, "cd Sea");
        assert_eq!(start, 3);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display, "Season 23/");
        assert_eq!(hits[0].replacement, "Season\\ 23/");

        let (start, hits) = complete(&completer, "cd One\\ Piece/Sea");
        assert_eq!(start, 3);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].replacement, "One\\ Piece/Season\\ 23/");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn completion_respects_an_open_quote() {
        let root = fixture();
        std::fs::create_dir(root.join("Season 23")).unwrap();
        let completer = completer(&root, &[]);

        let (start, hits) = complete(&completer, "cd \"Sea");
        assert_eq!(start, 4);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].replacement, "Season 23/");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn path_commands_complete_at_start_and_after_a_pipeline() {
        let root = fixture();
        let completer = completer(&root, &["git"]);

        let (start, hits) = complete(&completer, "gi");
        assert_eq!(start, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].replacement, "git ");

        let line = "printf ok | gi";
        let (start, hits) = complete(&completer, line);
        assert_eq!(start, line.find("gi").unwrap());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].replacement, "git ");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn external_command_arguments_complete_files_with_shell_escaping() {
        let root = fixture();
        std::fs::write(root.join("One Piece.txt"), "fixture").unwrap();
        let completer = completer(&root, &["cat"]);

        let (start, hits) = complete(&completer, "cat One");
        assert_eq!(start, 4);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].replacement, "One\\ Piece.txt ");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hidden_paths_appear_only_after_a_dot_prefix() {
        let root = fixture();
        std::fs::write(root.join(".secret"), "fixture").unwrap();
        let completer = completer(&root, &["cat"]);

        assert!(complete(&completer, "cat ").1.is_empty());
        assert_eq!(complete(&completer, "cat .s").1[0].display, ".secret");

        let _ = std::fs::remove_dir_all(root);
    }
}
