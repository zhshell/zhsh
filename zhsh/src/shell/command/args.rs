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
