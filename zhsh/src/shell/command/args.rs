//! 命令路由使用的轻量字面参数分类器。
//!
//! 本模块不属于任何具体内建命令，也不实现完整 Shell 语言。它只解析足以安全路由
//! 内建命令的引号、空参数和反斜杠转义；变量与命令展开、重定向、管道、通配符、
//! 注释和控制运算符通过 [`ParseError::NeedsBash`] 整体转交 Bash。

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// 输入包含必须由完整 Shell 解释器处理的语法。
    NeedsBash,
    /// 输入看似字面参数，但引号或转义没有闭合。
    Syntax(&'static str),
}

#[derive(Clone, Copy)]
enum Quote {
    Single,
    Double,
}

/// 将可能的内建命令输入解析为字面参数。
///
/// # Arguments
///
/// - `input`：未经展开或修改的完整用户/Agent 输入。
///
/// # Returns
///
/// 纯字面输入返回参数列表；需要完整 Shell 语义时返回 [`ParseError::NeedsBash`]。
///
/// # Errors
///
/// 未闭合引号或转义返回 [`ParseError::Syntax`]。调用方只能在首词确认为内建命令时
/// 把它显示为语法错误；其他情况仍应交给 Bash 给出原生诊断。
///
/// # Examples
///
/// ```text
/// export NAME="value with spaces"  -> ["export", "NAME=value with spaces"]
/// printf '%s' "$HOME"              -> NeedsBash
/// ```
pub(crate) fn parse(input: &str) -> Result<Vec<String>, ParseError> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut quote = None;
    let mut characters = input.chars().peekable();

    while let Some(character) = characters.next() {
        match quote {
            Some(Quote::Single) => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
            }
            Some(Quote::Double) => match character {
                '"' => quote = None,
                '$' | '`' => return Err(ParseError::NeedsBash),
                '\\' => {
                    let Some(next) = characters.next() else {
                        return Err(ParseError::Syntax("双引号内存在未完成的转义"));
                    };
                    if matches!(next, '$' | '`' | '"' | '\\') {
                        word.push(next);
                    } else if next != '\n' {
                        word.push('\\');
                        word.push(next);
                    }
                }
                _ => word.push(character),
            },
            None => match character {
                character if character.is_whitespace() => {
                    if word_started {
                        words.push(std::mem::take(&mut word));
                        word_started = false;
                    }
                }
                '\'' => {
                    quote = Some(Quote::Single);
                    word_started = true;
                }
                '"' => {
                    quote = Some(Quote::Double);
                    word_started = true;
                }
                '\\' => {
                    let Some(next) = characters.next() else {
                        return Err(ParseError::Syntax("存在未完成的转义"));
                    };
                    if next != '\n' {
                        word.push(next);
                        word_started = true;
                    }
                }
                '$' | '`' | '&' | ';' | '|' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | ']'
                | '{' | '}' | '\n' | '\r' => return Err(ParseError::NeedsBash),
                '#' if !word_started => return Err(ParseError::NeedsBash),
                _ => {
                    word.push(character);
                    word_started = true;
                }
            },
        }
    }

    if let Some(quote) = quote {
        return Err(ParseError::Syntax(match quote {
            Quote::Single => "未闭合的单引号",
            Quote::Double => "未闭合的双引号",
        }));
    }
    if word_started {
        words.push(word);
    }
    Ok(words)
}

/// Native 的完整字面调用；仅空格/TAB 分词，不委托 Shell 或执行展开。
pub(crate) fn parse_native_literal(input: &str) -> Result<Vec<String>, ParseError> {
    parse_native_words(input, false)
}

/// 已有内建自行处理目录参数中的 ~；不增加通用参数展开。
pub(crate) fn parse_native_builtin_literal(input: &str) -> Result<Vec<String>, ParseError> {
    parse_native_words(input, true)
}

fn parse_native_words(input: &str, builtin_arguments: bool) -> Result<Vec<String>, ParseError> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut assignment_prefix = true;
    let mut quote = None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\0' {
            return Err(ParseError::Syntax("Native 参数不能包含 NUL"));
        }
        match quote {
            Some(Quote::Single) => {
                if c == '\'' {
                    quote = None;
                } else {
                    word.push(c);
                }
            }
            Some(Quote::Double) => match c {
                '"' => quote = None,
                '$' | '`' => return Err(ParseError::NeedsBash),
                '\\' => {
                    let next = chars.next().ok_or(ParseError::Syntax("不完整的转义"))?;
                    if matches!(next, '\n' | '\r' | '\0') {
                        return Err(ParseError::NeedsBash);
                    }
                    if !matches!(next, '$' | '`' | '"' | '\\') {
                        word.push('\\');
                    }
                    word.push(next);
                }
                _ => word.push(c),
            },
            None => match c {
                ' ' | '\t' => {
                    if started {
                        native_finish_word(&mut words, &mut word, quoted)?;
                        started = false;
                        quoted = false;
                        assignment_prefix = true;
                    }
                }
                '\'' | '"' => {
                    quote = Some(if c == '\'' {
                        Quote::Single
                    } else {
                        Quote::Double
                    });
                    started = true;
                    quoted = true;
                    assignment_prefix = false;
                }
                '\\' => {
                    let next = chars.next().ok_or(ParseError::Syntax("不完整的转义"))?;
                    if matches!(next, '\n' | '\r' | '\0') {
                        return Err(ParseError::NeedsBash);
                    }
                    word.push(next);
                    started = true;
                    quoted = true;
                    assignment_prefix = false;
                }
                '$' | '`' | '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '&' | ';' | '|'
                | '<' | '>' | '\n' | '\r' => return Err(ParseError::NeedsBash),
                '#' if !started => return Err(ParseError::NeedsBash),
                '~' if !started && !(builtin_arguments && !words.is_empty()) => {
                    return Err(ParseError::NeedsBash)
                }
                '=' if words.is_empty() && assignment_prefix && native_variable_name(&word) => {
                    return Err(ParseError::NeedsBash);
                }
                _ => {
                    word.push(c);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err(ParseError::Syntax("引用未闭合"));
    }
    if started {
        native_finish_word(&mut words, &mut word, quoted)?;
    }
    Ok(words)
}

fn native_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn native_finish_word(
    words: &mut Vec<String>,
    word: &mut String,
    quoted: bool,
) -> Result<(), ParseError> {
    if words.is_empty() {
        if word.is_empty() || word.contains('/') {
            return Err(ParseError::Syntax("Native 本期仅支持非空 PATH 程序名"));
        }
        if !quoted
            && matches!(
                word.as_str(),
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
            return Err(ParseError::NeedsBash);
        }
    }
    words.push(std::mem::take(word));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quotes_spaces_empty_values_and_escapes() {
        assert_eq!(
            parse(r#"export A="value with space" B='two words' C=three\ words D="""#),
            Ok(vec![
                "export".into(),
                "A=value with space".into(),
                "B=two words".into(),
                "C=three words".into(),
                "D=".into(),
            ])
        );
        assert_eq!(
            parse("cd 'path with space'"),
            Ok(vec!["cd".into(), "path with space".into()])
        );
    }

    #[test]
    fn delegates_real_shell_syntax() {
        for input in [
            "pwd > output",
            "export NOW=$(date)",
            "export COPY=$HOME",
            "pwd | cat",
            "cd dir*",
            "pwd # comment",
        ] {
            assert_eq!(parse(input), Err(ParseError::NeedsBash), "{input}");
        }
    }

    #[test]
    fn reports_incomplete_quotes_without_executing() {
        assert_eq!(
            parse("cd 'missing"),
            Err(ParseError::Syntax("未闭合的单引号"))
        );
        assert_eq!(
            parse("export A=broken\\"),
            Err(ParseError::Syntax("存在未完成的转义"))
        );
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    #[test]
    fn native_literal_arguments_preserve_boundaries() {
        for (input, expected) in [
            (
                r#"probe -n "a b" 'c d' e\ f """#,
                vec!["probe", "-n", "a b", "c d", "e f", ""],
            ),
            (
                r#"probe a"b"'c' '$HOME' \* "a\q""#,
                vec!["probe", "abc", "$HOME", "*", "a\\q"],
            ),
            (
                "probe a\u{2003}b\t中文",
                vec!["probe", "a\u{2003}b", "中文"],
            ),
            ("'if' x", vec!["if", "x"]),
            ("'A'=b", vec!["A=b"]),
            ("probe 'a\nb'", vec!["probe", "a\nb"]),
        ] {
            assert_eq!(parse_native_literal(input).unwrap(), expected, "{input}");
        }
    }
    #[test]
    fn native_rejects_whole_unsupported_requests() {
        for input in [
            "probe $HOME",
            "probe ~",
            "probe *.rs",
            "probe a; touch x",
            "probe > x",
            "if x",
            "A=\"x\" probe",
            "probe a\nb",
            "probe a\rb",
            "probe a\\\nb",
            "probe \"a\\\nb\"",
            "probe 'unfinished",
            "probe a\\",
            "probe # x",
            "/bin/true",
            "''",
            "probe \0",
        ] {
            assert!(parse_native_literal(input).is_err(), "{input:?}");
        }
        assert!(parse_native_literal(" \t ").unwrap().is_empty());
    }
}
