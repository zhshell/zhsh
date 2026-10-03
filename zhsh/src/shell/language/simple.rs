//! 简单命令的增量解析：识别引用、转义和注释，并保留词片段及源码位置供命令准备使用。
use crate::common::CancellationToken;
use std::ops::Range;
const INPUT_LIMIT: usize = 1024 * 1024;
const WORD_LIMIT: usize = 65536;
const PART_LIMIT: usize = 262144;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    Bare,
    Single,
    Double,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    span: Range<usize>,
    quote: Quote,
    protected: bool,
    value: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word {
    pub value: String,
    pub span: Range<usize>,
    parts: Vec<Part>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SimpleUnit {
    pub original: String,
    pub words: Vec<Word>,
    pub line_offset: usize,
}
#[derive(Debug, Clone)]
pub(crate) struct InputError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}
impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "第 {} 行，第 {} 列: {}",
            self.line, self.column, self.message
        )
    }
}
impl std::error::Error for InputError {}
impl SimpleUnit {
    /// 检查输入中首个完整单元之后的文本，拒绝粘贴多行时夹带的额外命令。
    pub fn check_trailing(&self, submission: &str) -> Result<(), InputError> {
        let end = self.original.len();
        let Some(tail) = submission.get(end..) else {
            return Ok(());
        };
        let extra = parse_single(tail, None).map_err(|mut error| {
            error.line += self.original.bytes().filter(|b| *b == b'\n').count();
            error
        })?;
        if !extra.words.is_empty() {
            return Err(diagnostic(
                submission,
                end,
                "Native 本次调用只支持一个简单命令",
            ));
        }
        Ok(())
    }
    pub fn error(&self, offset: usize, message: &str) -> InputError {
        {
            let mut error = diagnostic(&self.original, offset, message);
            error.line += self.line_offset;
            error
        }
    }
    pub fn values(&self) -> Vec<String> {
        self.words.iter().map(|w| w.value.clone()).collect()
    }
    /// 检查命令使用的语法与展开是否受支持，内建和作业参数的例外由调用方指定。
    pub fn validate(&self, builtin: bool, job_builtin: bool) -> Result<(), InputError> {
        for (index, word) in self.words.iter().enumerate() {
            let raw = &self.original[word.span.clone()];
            let unquoted = word
                .parts
                .iter()
                .all(|p| p.quote == Quote::Bare && (!p.protected || p.value.is_empty()));
            if index == 0 {
                if word.value.is_empty() {
                    return Err(self.error(word.span.start, "Native 程序名或路径不能为空"));
                }
                if unquoted
                    && matches!(
                        word.value.as_str(),
                        "!" | "[["
                            | "]]"
                            | "{"
                            | "}"
                            | "case"
                            | "coproc"
                            | "do"
                            | "done"
                            | "elif"
                            | "else"
                            | "esac"
                            | "fi"
                            | "for"
                            | "function"
                            | "if"
                            | "in"
                            | "select"
                            | "then"
                            | "time"
                            | "until"
                            | "while"
                    )
                {
                    return Err(self.error(word.span.start, "Native 当前不支持该控制语法"));
                }
                if let Some((name, _)) = raw.split_once('=') {
                    if valid_name(&name.replace("\\\n", "")) {
                        return Err(self.error(word.span.start, "Native 当前不支持前置赋值"));
                    }
                }
            }
            let mut close_bracket = None;
            let mut close_brace = None;
            let mut brace_separator = None;
            for part in &word.parts {
                if part.quote == Quote::Bare && !part.protected {
                    for (n, c) in self.original[part.span.clone()].char_indices() {
                        let pos = part.span.start + n;
                        match c {
                            ']' => close_bracket = Some(pos),
                            '}' => close_brace = Some(pos),
                            ',' => brace_separator = Some(pos),
                            '.' if self.original[pos..part.span.end].starts_with("..") => {
                                brace_separator = Some(pos)
                            }
                            _ => {}
                        }
                    }
                }
            }
            for part in &word.parts {
                if part.protected || part.quote == Quote::Single {
                    continue;
                }
                for (n, c) in self.original[part.span.clone()].char_indices() {
                    let pos = part.span.start + n;
                    let rest = &self.original[pos + c.len_utf8()..];
                    let mut logical_rest = rest;
                    while let Some(tail) = logical_rest.strip_prefix("\\\n") {
                        logical_rest = tail;
                    }
                    let next = logical_rest.chars().next();
                    let expansion = c == '`'
                        || c == '$'
                            && next.is_some_and(|c| {
                                c.is_ascii_alphanumeric()
                                    || matches!(
                                        c,
                                        '_' | '{' | '(' | '@' | '*' | '#' | '?' | '$' | '!' | '-'
                                    )
                            });
                    let bare = part.quote == Quote::Bare;
                    let expansion =
                        expansion || bare && c == '$' && matches!(next, Some('\'' | '"'));
                    let job_selector = job_builtin
                        && index > 0
                        && c == '?'
                        && pos == word.span.start + 1
                        && raw.starts_with("%?");
                    let glob = bare
                        && (c == '*'
                            || c == '?' && !job_selector
                            || c == '[' && close_bracket.is_some_and(|end| end > pos + 1));
                    let brace = bare
                        && c == '{'
                        && close_brace.is_some_and(|end| {
                            end > pos && brace_separator.is_some_and(|sep| sep > pos && sep < end)
                        });
                    let tilde =
                        bare && c == '~' && pos == word.span.start && !(builtin && index > 0);
                    if expansion || glob || brace || tilde {
                        return Err(self.error(pos, "Native 当前不支持该 Shell 展开"));
                    }
                }
            }
        }
        Ok(())
    }
}
fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
fn diagnostic(text: &str, offset: usize, message: &str) -> InputError {
    let prefix = &text[..offset.min(text.len())];
    InputError {
        message: message.into(),
        line: prefix.bytes().filter(|c| *c == b'\n').count() + 1,
        column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
    }
}
#[derive(Debug)]
pub(crate) enum ReadResult {
    Complete(SimpleUnit),
    NeedMore,
}
pub(crate) struct SimpleReader {
    text: String,
    words: Vec<Word>,
    parts: Vec<Part>,
    value: String,
    start: Option<usize>,
    quote: Quote,
    opener: usize,
    escape: Option<(usize, Quote)>,
    comment: bool,
    count: usize,
    finished: bool,
}
impl Default for SimpleReader {
    fn default() -> Self {
        Self {
            text: String::new(),
            words: Vec::new(),
            parts: Vec::new(),
            value: String::new(),
            start: None,
            quote: Quote::Bare,
            opener: 0,
            escape: None,
            comment: false,
            count: 0,
            finished: false,
        }
    }
}
impl SimpleReader {
    fn part(
        &mut self,
        span: Range<usize>,
        value: &str,
        quote: Quote,
        protected: bool,
    ) -> Result<(), InputError> {
        self.value.push_str(value);
        if let Some(last) = self.parts.last_mut() {
            if last.span.end == span.start && last.quote == quote && last.protected == protected {
                last.span.end = span.end;
                last.value.push_str(value);
                return Ok(());
            }
        }
        self.count += 1;
        if self.count > PART_LIMIT {
            return Err(diagnostic(
                &self.text,
                span.start,
                "Native 输入片段超过限制",
            ));
        }
        self.parts.push(Part {
            span,
            quote,
            protected,
            value: value.into(),
        });
        Ok(())
    }
    fn word(&mut self, end: usize) -> Result<(), InputError> {
        if let Some(start) = self.start.take() {
            if self.words.len() >= WORD_LIMIT {
                return Err(diagnostic(&self.text, start, "Native 参数数量超过限制"));
            }
            self.words.push(Word {
                value: std::mem::take(&mut self.value),
                span: start..end,
                parts: std::mem::take(&mut self.parts),
            });
        }
        Ok(())
    }
    fn complete(&mut self) -> Result<ReadResult, InputError> {
        self.word(self.text.len())?;
        self.finished = true;
        Ok(ReadResult::Complete(SimpleUnit {
            original: std::mem::take(&mut self.text),
            words: std::mem::take(&mut self.words),
            line_offset: 0,
        }))
    }
    /// 读取至首个逻辑换行或输入结束；返回单元的原文长度表示累计消费的字节数。
    pub fn feed(
        &mut self,
        chunk: &str,
        eof: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<ReadResult, InputError> {
        assert!(!self.finished, "completed reader reused");
        for c in chunk.chars() {
            let pos = self.text.len();
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Err(diagnostic(&self.text, pos, "输入已取消"));
            }
            if pos + c.len_utf8() > INPUT_LIMIT {
                return Err(diagnostic(&self.text, pos, "Native 输入单元超过 1 MiB"));
            }
            self.text.push(c);
            if c == '\0' {
                return Err(diagnostic(&self.text, pos, "Native 参数不能包含 NUL"));
            }
            if self.comment {
                if c == '\n' {
                    return self.complete();
                }
                continue;
            }
            if let Some((escape, quote)) = self.escape.take() {
                if c == '\n' {
                    self.part(escape..self.text.len(), "", quote, true)?;
                    continue;
                }
                self.start.get_or_insert(escape);
                let value = if quote == Quote::Double && !matches!(c, '$' | '`' | '"' | '\\') {
                    format!("\\{c}")
                } else {
                    c.to_string()
                };
                self.part(escape..self.text.len(), &value, quote, true)?;
                continue;
            }
            if self.quote == Quote::Single {
                let value = if c == '\'' {
                    String::new()
                } else {
                    c.to_string()
                };
                self.part(pos..self.text.len(), &value, Quote::Single, true)?;
                if c == '\'' {
                    self.quote = Quote::Bare;
                }
                continue;
            }
            if self.quote == Quote::Double {
                if c == '"' {
                    self.part(pos..self.text.len(), "", Quote::Double, true)?;
                    self.quote = Quote::Bare;
                } else if c == '\\' {
                    self.escape = Some((pos, Quote::Double));
                } else {
                    self.part(pos..self.text.len(), &c.to_string(), Quote::Double, false)?;
                }
                continue;
            }
            match c {
                ' ' | '\t' => self.word(pos)?,
                '\n' => {
                    self.word(pos)?;
                    return self.complete();
                }
                '#' if self.start.is_none() => self.comment = true,
                '\\' => self.escape = Some((pos, Quote::Bare)),
                '\'' | '"' => {
                    self.start.get_or_insert(pos);
                    self.quote = if c == '\'' {
                        Quote::Single
                    } else {
                        Quote::Double
                    };
                    self.opener = pos;
                    self.part(pos..self.text.len(), "", self.quote, true)?;
                }
                ';' | '&' | '|' | '<' | '>' | '(' | ')' => {
                    return Err(diagnostic(
                        &self.text,
                        pos,
                        "Native 当前不支持该 Shell 组合或重定向语法",
                    ))
                }
                _ => {
                    self.start.get_or_insert(pos);
                    self.part(pos..self.text.len(), &c.to_string(), Quote::Bare, false)?;
                }
            }
        }
        if eof {
            if self.escape.is_some() || self.quote != Quote::Bare {
                return Err(diagnostic(
                    &self.text,
                    self.escape.map_or(self.opener, |v| v.0),
                    "引用或转义未闭合（EOF）",
                ));
            }
            self.complete()
        } else {
            Ok(ReadResult::NeedMore)
        }
    }
}
pub(crate) fn read_unit(
    text: &str,
    cancel: Option<&CancellationToken>,
) -> Result<SimpleUnit, InputError> {
    match SimpleReader::default().feed(text, true, cancel)? {
        ReadResult::Complete(u) => Ok(u),
        ReadResult::NeedMore => unreachable!(),
    }
}
pub(crate) fn parse_single(
    text: &str,
    cancel: Option<&CancellationToken>,
) -> Result<SimpleUnit, InputError> {
    let mut offset = 0;
    let mut found = None;
    let mut line_offset = 0;
    while offset < text.len() {
        let mut unit = read_unit(&text[offset..], cancel).map_err(|mut e| {
            e.line += line_offset;
            e
        })?;
        unit.line_offset = line_offset;
        line_offset += unit.original.bytes().filter(|b| *b == b'\n').count();
        let size = unit.original.len();
        if !unit.words.is_empty() {
            if found.is_some() {
                return Err(diagnostic(
                    text,
                    offset,
                    "Native 本次调用只支持一个简单命令",
                ));
            }
            found = Some(unit);
        }
        offset += size;
    }
    Ok(found.unwrap_or(SimpleUnit {
        original: text.into(),
        words: Vec::new(),
        line_offset: 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_words_and_comments_preserve_bash_values() {
        for (input, expected) in [
            (
                "probe -n \"a b\" 'c d' e\\ f \"\" a\"b\"'c'",
                vec!["probe", "-n", "a b", "c d", "e f", "", "abc"],
            ),
            (
                "probe a\u{2003}b\t中文",
                vec!["probe", "a\u{2003}b", "中文"],
            ),
            (
                "probe x#y '#' \\# # ignored $x | '\n",
                vec!["probe", "x#y", "#", "#"],
            ),
            (
                r#"probe '$HOME' \* "a\q" "\$HOME" "$""#,
                vec!["probe", "$HOME", "*", "a\\q", "$HOME", "$"],
            ),
            (
                "probe 'a\nb' ab\\\ncd x\\ ",
                vec!["probe", "a\nb", "abcd", "x "],
            ),
            ("'A'=b", vec!["A=b"]),
            ("probe [ {plain} a=b", vec!["probe", "[", "{plain}", "a=b"]),
        ] {
            let unit = parse_single(input, None).unwrap();
            unit.validate(false, false).unwrap();
            assert_eq!(unit.values(), expected, "{input:?}");
        }
    }
    #[test]
    fn incremental_quoting_matches_finite_read_and_tracks_source() {
        let text = "probe '中文\n值' ab\\\ncd \"a\\q\"\n";
        let expected = read_unit(text, None).unwrap();
        // 逐字符分块输入，验证跨块的引用、转义和中文解析与一次性读取一致。
        let mut reader = SimpleReader::default();
        for (n, c) in text.char_indices() {
            match reader.feed(&c.to_string(), false, None).unwrap() {
                ReadResult::NeedMore => assert!(n + 1 < text.len()),
                ReadResult::Complete(unit) => assert_eq!(unit, expected),
            }
        }
        let first = read_unit("probe one\nprobe two", None).unwrap();
        assert_eq!(first.original, "probe one\n");
        assert!(first.check_trailing("probe one\nprobe two").is_err());
        assert!(first.check_trailing("probe one\n# comment").is_ok());
        assert_eq!(&first.original[first.words[1].span.clone()], "one");
        assert_eq!(
            parse_single("probe ab\\\n", None).unwrap().values(),
            ["probe", "ab"]
        );
    }
    #[test]
    fn unsupported_forms_reject_before_lowering_and_keep_builtin_exceptions() {
        for text in [
            "probe $HOME",
            "probe $\\\nHOME",
            "\\\nif x",
            "A\\\n=x probe",
            "probe \"$HOME\"",
            "probe $(date)",
            "probe *.rs",
            "probe [ab]",
            "probe {a,b}",
            "probe ~",
            "probe | cat",
            "probe; touch x",
            "probe > x",
            "A=x probe",
            "if x",
            "''",
            "probe 'unfinished",
            "probe a\\",
            "probe \0",
            "probe a\nprobe b",
            "probe $'x'",
        ] {
            assert!(
                parse_single(text, None)
                    .and_then(|u| u.validate(false, false))
                    .is_err(),
                "{text:?}"
            );
        }
        parse_single("fg %?part", None)
            .unwrap()
            .validate(true, true)
            .unwrap();
        assert!(parse_single("probe %?part", None)
            .unwrap()
            .validate(false, false)
            .is_err());
        parse_single("cd ~", None)
            .unwrap()
            .validate(true, false)
            .unwrap();
        parse_single("export A=x", None)
            .unwrap()
            .validate(true, false)
            .unwrap();
        assert!(parse_single("# comment\n \t", None)
            .unwrap()
            .words
            .is_empty());
    }
    #[test]
    fn eof_limits_and_cancellation_are_bounded() {
        let error = parse_single("probe 'a\nb", None).unwrap_err();
        assert_eq!((error.line, error.column), (1, 7));
        assert!(read_unit(&"x".repeat(INPUT_LIMIT), None).is_ok());
        assert!(read_unit(&"x".repeat(INPUT_LIMIT + 1), None).is_err());
        assert!(read_unit(&"x ".repeat(WORD_LIMIT + 1), None).is_err());
        let cancel = CancellationToken::default();
        cancel.cancel();
        assert!(read_unit("probe", Some(&cancel)).is_err());
    }
}
