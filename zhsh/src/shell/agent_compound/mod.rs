//! Agent 受限组合命令的词法、语法和静态绑定。
//!
//! 本模块只接受能够在授权前冻结目标、参数、glob 和重定向的有限 Shell 子集。任何
//! 变量展开、命令替换、复杂 alias、function 或其他 Bash 语义会使整个表达式回退动态路径；
//! 纯字面 alias 会先冻结展开后再绑定。

use super::agent_plan::{CommandTargetKind, ExecutableBinding, ResolvedInvocation};
use super::{command, SessionState};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub(super) use super::command::resolver::FileIdentity;

const MAX_COMMANDS: usize = 32;
const MAX_ARGUMENTS: usize = 256;
const MAX_GLOB_RESULTS: usize = 4096;
const MAX_GLOB_PATTERN_BYTES: usize = 4096;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundExpression {
    Command(Box<BoundCommand>),
    Pipeline(Vec<BoundCommand>),
    And(Box<BoundExpression>, Box<BoundExpression>),
    Or(Box<BoundExpression>, Box<BoundExpression>),
    Sequence(Vec<BoundExpression>),
}

impl BoundExpression {
    pub(super) fn external_identities_are_current(&self) -> bool {
        match self {
            Self::Command(command) => command.external_identity_is_current(),
            Self::Pipeline(commands) => commands
                .iter()
                .all(BoundCommand::external_identity_is_current),
            Self::And(left, right) | Self::Or(left, right) => {
                left.external_identities_are_current() && right.external_identities_are_current()
            }
            Self::Sequence(expressions) => expressions
                .iter()
                .all(BoundExpression::external_identities_are_current),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundCommand {
    pub(crate) target: BoundTarget,
    pub(crate) arguments: Vec<OsString>,
    pub(crate) redirections: Vec<BoundRedirection>,
    pub(crate) invocation: ResolvedInvocation,
    pub(crate) expanded_paths: Vec<BoundPath>,
}

impl BoundCommand {
    fn external_identity_is_current(&self) -> bool {
        let target_current = match &self.target {
            BoundTarget::External { path, identity } => {
                command::resolver::identity_matches(path, *identity)
            }
            BoundTarget::ZhshQueryBuiltin(_) => true,
        };
        target_current
            && self.expanded_paths.iter().all(BoundPath::is_current)
            && self
                .redirections
                .iter()
                .all(|redirection| match redirection {
                    BoundRedirection::InputFile { path, .. }
                    | BoundRedirection::OutputFile { path, .. } => path.is_current(),
                    _ => true,
                })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundTarget {
    ZhshQueryBuiltin(QueryBuiltin),
    External {
        path: PathBuf,
        identity: FileIdentity,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryBuiltin {
    Pwd,
    Type,
    CommandV,
    LiteralEcho,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundPath {
    pub(crate) original: String,
    pub(crate) absolute: PathBuf,
    pub(crate) canonical: Option<PathBuf>,
    pub(crate) identity: Option<FileIdentity>,
    parent_canonical: Option<PathBuf>,
    parent_identity: Option<FileIdentity>,
}

impl BoundPath {
    fn is_current(&self) -> bool {
        let parent_current = match (
            self.absolute.parent(),
            self.parent_canonical.as_deref(),
            self.parent_identity,
        ) {
            (Some(parent), Some(expected_path), Some(expected_identity)) => {
                std::fs::canonicalize(parent).is_ok_and(|current| current == expected_path)
                    && command::resolver::identity_matches(expected_path, expected_identity)
            }
            _ => false,
        };
        parent_current
            && match (self.canonical.as_deref(), self.identity) {
                (Some(expected_path), Some(expected_identity)) => {
                    std::fs::canonicalize(&self.absolute)
                        .is_ok_and(|current| current == expected_path)
                        && command::resolver::identity_matches(expected_path, expected_identity)
                }
                (None, None) => !self.absolute.exists(),
                _ => false,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundRedirection {
    InputFile {
        fd: u32,
        path: BoundPath,
    },
    OutputFile {
        fd: u32,
        path: BoundPath,
        mode: OutputMode,
    },
    Duplicate {
        from: u32,
        to: u32,
    },
    Close {
        fd: u32,
    },
    Null {
        fd: u32,
    },
    StandardStream {
        fd: u32,
        target: StandardStream,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Overwrite,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StandardStream {
    Stdin,
    Stdout,
    Stderr,
}

/// 解析并完整绑定受限组合表达式。失败时返回已经识别到的目标证据，调用方整体降级 Bash。
pub(super) fn bind(
    session: &SessionState,
    input: &str,
) -> Result<(BoundExpression, Vec<ResolvedInvocation>), Vec<ResolvedInvocation>> {
    let tokens = lex(input).map_err(|_| Vec::new())?;
    let raw = Parser::new(tokens).parse().map_err(|_| Vec::new())?;
    let mut context = BindContext {
        session,
        invocations: Vec::new(),
        argument_bytes: 0,
    };
    match context.expression(raw) {
        Ok(expression) => Ok((expression, context.invocations)),
        Err(()) => Err(context.invocations),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedWord {
    value: String,
    unquoted_glob: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectKind {
    Input,
    Overwrite,
    Append,
    Duplicate,
    BothOverwrite,
    BothAppend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RedirectOperator {
    source: Option<u32>,
    kind: RedirectKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(ParsedWord),
    Redirect(RedirectOperator),
    Pipe,
    AndIf,
    OrIf,
    Semicolon,
    Newline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
}

fn lex(input: &str) -> Result<Vec<Token>, ()> {
    let characters: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut unquoted_glob = false;
    let mut quote = None;
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];
        match quote {
            Some(Quote::Single) => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
                index += 1;
            }
            Some(Quote::Double) => match character {
                '"' => {
                    quote = None;
                    index += 1;
                }
                '$' | '`' => return Err(()),
                '\\' => {
                    index += 1;
                    let Some(next) = characters.get(index).copied() else {
                        return Err(());
                    };
                    if matches!(next, '$' | '`' | '"' | '\\') {
                        word.push(next);
                    } else if next != '\n' {
                        word.push('\\');
                        word.push(next);
                    }
                    index += 1;
                }
                _ => {
                    word.push(character);
                    index += 1;
                }
            },
            None => match character {
                ' ' | '\t' => {
                    finish_word(
                        &mut tokens,
                        &mut word,
                        &mut word_started,
                        &mut unquoted_glob,
                    );
                    index += 1;
                }
                '\n' | '\r' => {
                    finish_word(
                        &mut tokens,
                        &mut word,
                        &mut word_started,
                        &mut unquoted_glob,
                    );
                    if !matches!(tokens.last(), Some(Token::Newline)) {
                        tokens.push(Token::Newline);
                    }
                    index += 1;
                }
                '\'' => {
                    quote = Some(Quote::Single);
                    word_started = true;
                    index += 1;
                }
                '"' => {
                    quote = Some(Quote::Double);
                    word_started = true;
                    index += 1;
                }
                '\\' => {
                    index += 1;
                    let Some(next) = characters.get(index).copied() else {
                        return Err(());
                    };
                    if next != '\n' {
                        word.push(next);
                        word_started = true;
                    }
                    index += 1;
                }
                '$' | '`' | '(' | ')' | '{' | '}' => return Err(()),
                '#' if !word_started => return Err(()),
                '*' | '?' | '[' | ']' => {
                    word.push(character);
                    word_started = true;
                    unquoted_glob = true;
                    index += 1;
                }
                ';' => {
                    finish_word(
                        &mut tokens,
                        &mut word,
                        &mut word_started,
                        &mut unquoted_glob,
                    );
                    tokens.push(Token::Semicolon);
                    index += 1;
                }
                '|' => {
                    finish_word(
                        &mut tokens,
                        &mut word,
                        &mut word_started,
                        &mut unquoted_glob,
                    );
                    if characters.get(index + 1) == Some(&'|') {
                        tokens.push(Token::OrIf);
                        index += 2;
                    } else if characters.get(index + 1) == Some(&'&') {
                        return Err(());
                    } else {
                        tokens.push(Token::Pipe);
                        index += 1;
                    }
                }
                '&' => {
                    if characters.get(index + 1) == Some(&'&') {
                        finish_word(
                            &mut tokens,
                            &mut word,
                            &mut word_started,
                            &mut unquoted_glob,
                        );
                        tokens.push(Token::AndIf);
                        index += 2;
                    } else if characters.get(index + 1) == Some(&'>') {
                        finish_word(
                            &mut tokens,
                            &mut word,
                            &mut word_started,
                            &mut unquoted_glob,
                        );
                        let append = characters.get(index + 2) == Some(&'>');
                        tokens.push(Token::Redirect(RedirectOperator {
                            source: None,
                            kind: if append {
                                RedirectKind::BothAppend
                            } else {
                                RedirectKind::BothOverwrite
                            },
                        }));
                        index += if append { 3 } else { 2 };
                    } else {
                        return Err(());
                    }
                }
                '>' | '<' => {
                    let source = if word_started && word.chars().all(|value| value.is_ascii_digit())
                    {
                        let source = word.parse().map_err(|_| ())?;
                        word.clear();
                        word_started = false;
                        unquoted_glob = false;
                        Some(source)
                    } else {
                        finish_word(
                            &mut tokens,
                            &mut word,
                            &mut word_started,
                            &mut unquoted_glob,
                        );
                        None
                    };
                    let kind = if character == '<' {
                        if characters.get(index + 1) == Some(&'&') {
                            index += 2;
                            RedirectKind::Duplicate
                        } else {
                            index += 1;
                            RedirectKind::Input
                        }
                    } else if characters.get(index + 1) == Some(&'>') {
                        index += 2;
                        RedirectKind::Append
                    } else if characters.get(index + 1) == Some(&'&') {
                        index += 2;
                        RedirectKind::Duplicate
                    } else {
                        index += 1;
                        RedirectKind::Overwrite
                    };
                    let source = if character == '<'
                        && kind == RedirectKind::Duplicate
                        && source.is_none()
                    {
                        Some(0)
                    } else {
                        source
                    };
                    tokens.push(Token::Redirect(RedirectOperator { source, kind }));
                }
                _ => {
                    if character == '~' && !word_started {
                        return Err(());
                    }
                    word.push(character);
                    word_started = true;
                    index += 1;
                }
            },
        }
    }
    if quote.is_some() {
        return Err(());
    }
    finish_word(
        &mut tokens,
        &mut word,
        &mut word_started,
        &mut unquoted_glob,
    );
    Ok(tokens)
}

fn finish_word(tokens: &mut Vec<Token>, word: &mut String, started: &mut bool, glob: &mut bool) {
    if *started {
        tokens.push(Token::Word(ParsedWord {
            value: std::mem::take(word),
            unquoted_glob: *glob,
        }));
        *started = false;
        *glob = false;
    }
}

#[derive(Debug)]
enum RawExpression {
    Command(RawCommand),
    Pipeline(Vec<RawCommand>),
    And(Box<RawExpression>, Box<RawExpression>),
    Or(Box<RawExpression>, Box<RawExpression>),
    Sequence(Vec<RawExpression>),
}

#[derive(Debug)]
struct RawCommand {
    words: Vec<ParsedWord>,
    redirections: Vec<(RedirectOperator, ParsedWord)>,
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    command_count: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            index: 0,
            command_count: 0,
        }
    }

    fn parse(mut self) -> Result<RawExpression, ()> {
        while matches!(self.peek(), Some(Token::Newline)) {
            self.index += 1;
        }
        let mut expressions = vec![self.and_or()?];
        loop {
            let mut separator = false;
            while matches!(self.peek(), Some(Token::Semicolon | Token::Newline)) {
                separator = true;
                self.index += 1;
            }
            if self.peek().is_none() {
                break;
            }
            if !separator {
                return Err(());
            }
            expressions.push(self.and_or()?);
        }
        if expressions.len() == 1 {
            Ok(expressions.pop().unwrap())
        } else {
            Ok(RawExpression::Sequence(expressions))
        }
    }

    fn and_or(&mut self) -> Result<RawExpression, ()> {
        let mut expression = self.pipeline()?;
        loop {
            let operator = match self.peek() {
                Some(Token::AndIf) => Some(true),
                Some(Token::OrIf) => Some(false),
                _ => None,
            };
            let Some(and) = operator else { break };
            self.index += 1;
            let right = self.pipeline()?;
            expression = if and {
                RawExpression::And(Box::new(expression), Box::new(right))
            } else {
                RawExpression::Or(Box::new(expression), Box::new(right))
            };
        }
        Ok(expression)
    }

    fn pipeline(&mut self) -> Result<RawExpression, ()> {
        let mut commands = vec![self.command()?];
        while matches!(self.peek(), Some(Token::Pipe)) {
            self.index += 1;
            commands.push(self.command()?);
        }
        if commands.len() == 1 {
            Ok(RawExpression::Command(commands.pop().unwrap()))
        } else {
            Ok(RawExpression::Pipeline(commands))
        }
    }

    fn command(&mut self) -> Result<RawCommand, ()> {
        self.command_count += 1;
        if self.command_count > MAX_COMMANDS {
            return Err(());
        }
        let mut words = Vec::new();
        let mut redirections = Vec::new();
        loop {
            match self.peek().cloned() {
                Some(Token::Word(word)) => {
                    self.index += 1;
                    words.push(word);
                }
                Some(Token::Redirect(operator)) => {
                    self.index += 1;
                    let Some(Token::Word(target)) = self.peek().cloned() else {
                        return Err(());
                    };
                    self.index += 1;
                    redirections.push((operator, target));
                }
                _ => break,
            }
        }
        if words.is_empty() || words.len() > MAX_ARGUMENTS + 1 {
            return Err(());
        }
        Ok(RawCommand {
            words,
            redirections,
        })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }
}

struct BindContext<'a> {
    session: &'a SessionState,
    invocations: Vec<ResolvedInvocation>,
    argument_bytes: usize,
}

impl BindContext<'_> {
    fn expression(&mut self, raw: RawExpression) -> Result<BoundExpression, ()> {
        Ok(match raw {
            RawExpression::Command(command) => {
                BoundExpression::Command(Box::new(self.command(command)?))
            }
            RawExpression::Pipeline(commands) => {
                let commands: Vec<_> = commands
                    .into_iter()
                    .map(|command| self.command(command))
                    .collect::<Result<_, _>>()?;
                if commands
                    .iter()
                    .any(|command| matches!(command.target, BoundTarget::ZhshQueryBuiltin(_)))
                {
                    return Err(());
                }
                BoundExpression::Pipeline(commands)
            }
            RawExpression::And(left, right) => BoundExpression::And(
                Box::new(self.expression(*left)?),
                Box::new(self.expression(*right)?),
            ),
            RawExpression::Or(left, right) => BoundExpression::Or(
                Box::new(self.expression(*left)?),
                Box::new(self.expression(*right)?),
            ),
            RawExpression::Sequence(expressions) => BoundExpression::Sequence(
                expressions
                    .into_iter()
                    .map(|expression| self.expression(expression))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }

    fn command(&mut self, raw: RawCommand) -> Result<BoundCommand, ()> {
        let raw = match self.expand_literal_alias(raw) {
            Ok(raw) => raw,
            Err(name) => {
                self.record_unbound(&name);
                return Err(());
            }
        };
        let (head, raw_arguments) = raw.words.split_first().ok_or(())?;
        if head.unquoted_glob
            || is_assignment(&head.value)
            || is_reserved_word(&head.value)
            || self.session.functions.contains_key(&head.value)
        {
            self.record_unbound(&head.value);
            return Err(());
        }

        let target = if let Some(query) = query_builtin(&head.value, raw_arguments) {
            let invocation = ResolvedInvocation::named(
                head.value.clone(),
                CommandTargetKind::ZhshBuiltin,
                ExecutableBinding::SystemTrusted,
                None,
            );
            self.invocations.push(invocation);
            BoundTarget::ZhshQueryBuiltin(query)
        } else {
            if command::is_builtin(&head.value)
                || is_bash_builtin(&head.value)
                || is_wrapper(&head.value)
                || requires_dynamic_terminal(&head.value)
                || has_nested_execution(&head.value, raw_arguments)
            {
                self.record_unbound(&head.value);
                return Err(());
            }
            let Some(external) =
                command::resolver::resolve_agent_executable(self.session, &head.value)
            else {
                self.record_unbound(&head.value);
                return Err(());
            };
            let invocation = ResolvedInvocation::external(head.value.clone(), &external);
            self.invocations.push(invocation);
            BoundTarget::External {
                path: external.canonical_path,
                identity: external.identity,
            }
        };

        let invocation = self.invocations.last().cloned().ok_or(())?;
        let mut arguments = Vec::new();
        let mut expanded_paths = Vec::new();
        for word in raw_arguments {
            let expanded = self.expand_word(word)?;
            for (argument, path) in expanded {
                self.argument_bytes = self
                    .argument_bytes
                    .checked_add(argument.to_string_lossy().len())
                    .ok_or(())?;
                if self.argument_bytes > MAX_ARGUMENT_BYTES || arguments.len() >= MAX_ARGUMENTS {
                    return Err(());
                }
                arguments.push(argument);
                if let Some(path) = path {
                    expanded_paths.push(path);
                }
            }
        }
        let mut redirections = Vec::new();
        for (operator, target) in raw.redirections {
            if target.unquoted_glob {
                return Err(());
            }
            self.bind_redirection(operator, target, &mut redirections)?;
        }
        if matches!(target, BoundTarget::ZhshQueryBuiltin(_)) && !redirections.is_empty() {
            return Err(());
        }
        Ok(BoundCommand {
            target,
            arguments,
            redirections,
            invocation,
            expanded_paths,
        })
    }

    /// 将不含 Shell 语法的 alias 冻结为字面命令词；复杂 alias 仍整体降级确认。
    fn expand_literal_alias(&self, mut raw: RawCommand) -> Result<RawCommand, String> {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..3 {
            let Some(head) = raw.words.first() else {
                return Err(String::new());
            };
            let alias_name = head.value.clone();
            let Some(alias) = self.session.aliases.get(&alias_name) else {
                return Ok(raw);
            };
            if !seen.insert(alias_name.clone()) {
                return Err(alias_name);
            }
            let words = command::args::parse(alias).map_err(|_| alias_name.clone())?;
            if words.is_empty() {
                return Err(alias_name);
            }
            let mut expanded = words
                .into_iter()
                .map(|value| ParsedWord {
                    value,
                    unquoted_glob: false,
                })
                .collect::<Vec<_>>();
            expanded.extend(raw.words.into_iter().skip(1));
            raw.words = expanded;
            if raw
                .words
                .first()
                .is_some_and(|word| word.value == alias_name)
            {
                // Bash 不会在同一次展开中递归展开同名 alias。
                return Ok(raw);
            }
        }
        let head = raw
            .words
            .first()
            .map(|word| word.value.clone())
            .unwrap_or_default();
        if self.session.aliases.contains_key(&head) {
            Err(head)
        } else {
            Ok(raw)
        }
    }

    fn record_unbound(&mut self, name: &str) {
        let (kind, reason) = if self.session.aliases.contains_key(name) {
            (CommandTargetKind::Alias, "命中会话 alias")
        } else if self.session.functions.contains_key(name) {
            (CommandTargetKind::Function, "命中会话 Bash function")
        } else if command::is_builtin(name) {
            (
                CommandTargetKind::ZhshBuiltin,
                "组合中不是受支持的只读 builtin",
            )
        } else if is_bash_builtin(name) {
            (CommandTargetKind::BashBuiltin, "目标由 Bash builtin 解释")
        } else {
            (
                CommandTargetKind::DynamicOrUnresolved,
                "无法静态绑定执行目标",
            )
        };
        self.invocations.push(ResolvedInvocation::named(
            name.into(),
            kind,
            ExecutableBinding::Dynamic,
            Some(reason.into()),
        ));
    }

    fn expand_word(&self, word: &ParsedWord) -> Result<Vec<(OsString, Option<BoundPath>)>, ()> {
        if !word.unquoted_glob {
            return Ok(vec![(OsString::from(&word.value), None)]);
        }
        if word.value.len() > MAX_GLOB_PATTERN_BYTES {
            return Err(());
        }
        let paths = expand_glob(&self.session.cwd, &word.value)?;
        if paths.is_empty() {
            return Ok(vec![(OsString::from(&word.value), None)]);
        }
        if paths.len() > MAX_GLOB_RESULTS {
            return Err(());
        }
        Ok(paths
            .into_iter()
            .map(|(argument, absolute)| {
                let path = bound_path(argument.clone(), absolute);
                (OsString::from(argument), Some(path))
            })
            .collect())
    }

    fn bind_redirection(
        &self,
        operator: RedirectOperator,
        target: ParsedWord,
        output: &mut Vec<BoundRedirection>,
    ) -> Result<(), ()> {
        match operator.kind {
            RedirectKind::Duplicate => {
                let from = operator.source.unwrap_or(1);
                if target.value == "-" {
                    output.push(BoundRedirection::Close { fd: from });
                } else {
                    let to = target.value.parse().map_err(|_| ())?;
                    if from > 2 || to > 2 {
                        return Err(());
                    }
                    output.push(BoundRedirection::Duplicate { from, to });
                }
            }
            RedirectKind::Input => {
                let fd = operator.source.unwrap_or(0);
                if fd > 2 {
                    return Err(());
                }
                output.push(classify_path_redirection(
                    fd,
                    &target.value,
                    None,
                    &self.session.cwd,
                ));
            }
            RedirectKind::Overwrite | RedirectKind::Append => {
                let fd = operator.source.unwrap_or(1);
                if fd > 2 {
                    return Err(());
                }
                let mode = if operator.kind == RedirectKind::Append {
                    OutputMode::Append
                } else {
                    OutputMode::Overwrite
                };
                output.push(classify_path_redirection(
                    fd,
                    &target.value,
                    Some(mode),
                    &self.session.cwd,
                ));
            }
            RedirectKind::BothOverwrite | RedirectKind::BothAppend => {
                let mode = if operator.kind == RedirectKind::BothAppend {
                    OutputMode::Append
                } else {
                    OutputMode::Overwrite
                };
                output.push(classify_path_redirection(
                    1,
                    &target.value,
                    Some(mode),
                    &self.session.cwd,
                ));
                output.push(BoundRedirection::Duplicate { from: 2, to: 1 });
            }
        }
        Ok(())
    }
}

fn query_builtin(name: &str, arguments: &[ParsedWord]) -> Option<QueryBuiltin> {
    if arguments.iter().any(|argument| argument.unquoted_glob) {
        return None;
    }
    let values: Vec<_> = arguments
        .iter()
        .map(|argument| argument.value.as_str())
        .collect();
    match name {
        "pwd" if matches!(values.as_slice(), [] | ["-L"] | ["-P"] | ["--"]) => {
            Some(QueryBuiltin::Pwd)
        }
        "type" if !values.is_empty() => Some(QueryBuiltin::Type),
        "command"
            if values.first().is_some_and(|value| {
                value.strip_prefix('-').is_some_and(|options| {
                    !options.is_empty() && options.chars().all(|option| matches!(option, 'v' | 'V'))
                })
            }) && values.len() > 1 =>
        {
            Some(QueryBuiltin::CommandV)
        }
        "echo"
            if values.iter().all(|value| {
                !value.strip_prefix('-').is_some_and(|options| {
                    !options.is_empty()
                        && options
                            .chars()
                            .all(|option| matches!(option, 'n' | 'e' | 'E'))
                }) && !value.contains('\\')
            }) =>
        {
            Some(QueryBuiltin::LiteralEcho)
        }
        _ => None,
    }
}

fn classify_path_redirection(
    fd: u32,
    value: &str,
    output: Option<OutputMode>,
    cwd: &Path,
) -> BoundRedirection {
    match value {
        "/dev/null" => BoundRedirection::Null { fd },
        "/dev/stdin" => BoundRedirection::StandardStream {
            fd,
            target: StandardStream::Stdin,
        },
        "/dev/stdout" => BoundRedirection::StandardStream {
            fd,
            target: StandardStream::Stdout,
        },
        "/dev/stderr" => BoundRedirection::StandardStream {
            fd,
            target: StandardStream::Stderr,
        },
        "/dev/fd/0" | "/proc/self/fd/0" => BoundRedirection::StandardStream {
            fd,
            target: StandardStream::Stdin,
        },
        "/dev/fd/1" | "/proc/self/fd/1" => BoundRedirection::StandardStream {
            fd,
            target: StandardStream::Stdout,
        },
        "/dev/fd/2" | "/proc/self/fd/2" => BoundRedirection::StandardStream {
            fd,
            target: StandardStream::Stderr,
        },
        _ => {
            let absolute = absolute_path(cwd, value);
            let path = bound_path(value.into(), absolute);
            match output {
                Some(mode) => BoundRedirection::OutputFile { fd, path, mode },
                None => BoundRedirection::InputFile { fd, path },
            }
        }
    }
}

fn bound_path(original: String, absolute: PathBuf) -> BoundPath {
    let canonical = std::fs::canonicalize(&absolute).ok();
    let identity = canonical
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| file_identity(&metadata));
    let parent_canonical = absolute
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok());
    let parent_identity = parent_canonical
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| file_identity(&metadata));
    BoundPath {
        original,
        absolute,
        canonical,
        identity,
        parent_canonical,
        parent_identity,
    }
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode(),
        size: metadata.size(),
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
fn file_identity(_: &std::fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: 0,
        ino: 0,
        uid: 0,
        gid: 0,
        mode: 0,
        size: 0,
        mtime: 0,
        mtime_nsec: 0,
        ctime: 0,
        ctime_nsec: 0,
    }
}

fn absolute_path(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn expand_glob(cwd: &Path, pattern: &str) -> Result<Vec<(String, PathBuf)>, ()> {
    let absolute_pattern = absolute_path(cwd, pattern);
    let components: Vec<String> = absolute_pattern
        .components()
        .map(|component| component.as_os_str().to_str().map(str::to_owned).ok_or(()))
        .collect::<Result<_, _>>()?;
    let mut candidates = if absolute_pattern.is_absolute() {
        vec![PathBuf::from("/")]
    } else {
        vec![PathBuf::new()]
    };
    for component in components
        .iter()
        .filter(|component| component.as_str() != "/")
    {
        if has_glob(component) {
            let mut next = Vec::new();
            for directory in &candidates {
                let entries = std::fs::read_dir(directory).map_err(|_| ())?;
                for entry in entries.flatten() {
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if (!name.starts_with('.') || component.starts_with('.'))
                        && wildcard_match(component, &name)
                    {
                        next.push(entry.path());
                        if next.len() > MAX_GLOB_RESULTS {
                            return Err(());
                        }
                    }
                }
            }
            candidates = next;
        } else {
            candidates = candidates
                .into_iter()
                .map(|candidate| candidate.join(component))
                .collect();
        }
        if candidates.is_empty() {
            break;
        }
    }
    candidates.retain(|candidate| candidate.exists());
    candidates.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    candidates.dedup();
    candidates
        .into_iter()
        .map(|absolute| {
            let argument = if Path::new(pattern).is_absolute() {
                absolute.to_str().map(str::to_owned)
            } else {
                absolute
                    .strip_prefix(cwd)
                    .ok()
                    .and_then(Path::to_str)
                    .map(str::to_owned)
            }
            .ok_or(())?;
            Ok((argument, absolute))
        })
        .collect()
}

fn has_glob(value: &str) -> bool {
    value.contains(['*', '?', '['])
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    fn matches(
        pattern: &[char],
        value: &[char],
        pattern_index: usize,
        value_index: usize,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[pattern_index][value_index] {
            return result;
        }
        let result = match pattern.get(pattern_index) {
            None => value_index == value.len(),
            Some('*') => {
                matches(pattern, value, pattern_index + 1, value_index, memo)
                    || value_index < value.len()
                        && matches(pattern, value, pattern_index, value_index + 1, memo)
            }
            Some('?') => {
                value_index < value.len()
                    && matches(pattern, value, pattern_index + 1, value_index + 1, memo)
            }
            Some('[') => {
                let rest = &pattern[pattern_index + 1..];
                let Some(end) = rest.iter().position(|character| *character == ']') else {
                    return value.get(value_index) == Some(&'[')
                        && matches(pattern, value, pattern_index + 1, value_index + 1, memo);
                };
                if value_index >= value.len() {
                    false
                } else {
                    let class = &rest[..end];
                    let negated = class
                        .first()
                        .is_some_and(|value| matches!(value, '!' | '^'));
                    let class = if negated { &class[1..] } else { class };
                    let mut included = false;
                    let mut index = 0;
                    while index < class.len() {
                        if index + 2 < class.len() && class[index + 1] == '-' {
                            included |= class[index] <= value[value_index]
                                && value[value_index] <= class[index + 2];
                            index += 3;
                        } else {
                            included |= class[index] == value[value_index];
                            index += 1;
                        }
                    }
                    included != negated
                        && matches(
                            pattern,
                            value,
                            pattern_index + end + 2,
                            value_index + 1,
                            memo,
                        )
                }
            }
            Some(literal) => {
                value.get(value_index) == Some(literal)
                    && matches(pattern, value, pattern_index + 1, value_index + 1, memo)
            }
        };
        memo[pattern_index][value_index] = Some(result);
        result
    }

    let pattern: Vec<_> = pattern.chars().collect();
    let value: Vec<_> = value.chars().collect();
    let mut memo = vec![vec![None; value.len() + 1]; pattern.len() + 1];
    matches(&pattern, &value, 0, 0, &mut memo)
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

fn is_reserved_word(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "select"
            | "function"
            | "in"
            | "time"
            | "coproc"
    )
}

fn is_wrapper(name: &str) -> bool {
    matches!(
        name,
        "command"
            | "exec"
            | "time"
            | "nohup"
            | "env"
            | "xargs"
            | "bash"
            | "sh"
            | "dash"
            | "zsh"
            | "fish"
            | "eval"
    )
}

fn requires_dynamic_terminal(name: &str) -> bool {
    let name = Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name);
    matches!(
        name,
        "sudo" | "su" | "ssh" | "less" | "more" | "man" | "vim" | "vi" | "nano"
    )
}

fn has_nested_execution(name: &str, arguments: &[ParsedWord]) -> bool {
    name == "find"
        && arguments.iter().any(|argument| {
            matches!(
                argument.value.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir"
            )
        })
        || name == "sort"
            && arguments
                .iter()
                .any(|argument| argument.value.starts_with("--compress-program"))
}

fn is_bash_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "true"
            | "false"
            | "test"
            | "["
            | "[["
            | "printf"
            | "echo"
            | "read"
            | "mapfile"
            | "readarray"
            | "set"
            | "shopt"
            | "declare"
            | "typeset"
            | "local"
            | "caller"
            | "getopts"
            | "hash"
            | "jobs"
            | "wait"
            | "builtin"
            | "enable"
            | "eval"
            | "compgen"
            | "complete"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionState {
        let mut session = SessionState::test();
        session.env.insert("PATH".into(), "/usr/bin:/bin".into());
        session
    }

    #[test]
    fn binds_static_pipeline_and_boolean_structure() {
        let (expression, invocations) =
            bind(&session(), "ls -la | head -5 && pwd; uname -a").unwrap();
        assert!(matches!(expression, BoundExpression::Sequence(_)));
        assert_eq!(invocations.len(), 4);
        assert!(invocations
            .iter()
            .all(|invocation| invocation.kind != CommandTargetKind::DynamicOrUnresolved));
    }

    #[test]
    fn rejects_dynamic_and_background_syntax() {
        for command in ["ls $HOME", "echo $(id)", "sleep 1 &", "ls |& head"] {
            assert!(bind(&session(), command).is_err(), "{command}");
        }
    }

    #[test]
    fn parses_fd_redirections_without_treating_ampersand_as_background() {
        let (expression, _) = bind(&session(), "ls -la 2>&1 | head -3").unwrap();
        let BoundExpression::Pipeline(commands) = expression else {
            panic!("expected pipeline");
        };
        assert!(matches!(
            commands[0].redirections.as_slice(),
            [BoundRedirection::Duplicate { from: 2, to: 1 }]
        ));
    }
}
