//! 最左侧 zhsh builtin 管道源的边界识别。
//!
//! 本模块只解析第一条顶层控制运算符和管道左段。右侧文本保持为 Bash 输入，既不
//! 拆词也不重写。

use super::{args, is_builtin, pipeline_source_disposition, BuiltinPipelineSourceDisposition};
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub(in super::super) enum BuiltinPipelineSourceAnalysis {
    NotCandidate,
    Candidate(BuiltinPipelineSourcePlan),
    Invalid(BuiltinPipelineSourceError),
}

#[derive(Debug, PartialEq, Eq)]
pub(in super::super) struct BuiltinPipelineSourcePlan {
    pub(in super::super) name: String,
    pub(in super::super) arguments: Vec<String>,
    pub(in super::super) tail: String,
    pub(in super::super) disposition: BuiltinPipelineSourceDisposition,
}

#[derive(Debug, PartialEq, Eq)]
pub(in super::super) struct BuiltinPipelineSourceError {
    message: &'static str,
}

impl BuiltinPipelineSourceError {
    fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for BuiltinPipelineSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

enum FirstOperator {
    Pipe { index: usize, end: usize },
    Unsupported { index: usize },
    None,
}

#[derive(Clone, Copy)]
enum Quote {
    Single,
    Double,
}

/// 分析整行是否为受支持的“builtin 字面左段 + 普通管道 + Bash tail”。
pub(in super::super) fn analyze(input: &str) -> BuiltinPipelineSourceAnalysis {
    let (pipe_index, pipe_end) = match first_operator(input) {
        FirstOperator::Pipe { index, end } => (index, end),
        FirstOperator::Unsupported { index } => {
            return if begins_with_builtin(&input[..index]) {
                BuiltinPipelineSourceAnalysis::Invalid(BuiltinPipelineSourceError::new(
                    "内建命令管道左段只支持字面参数和普通 `|`",
                ))
            } else {
                BuiltinPipelineSourceAnalysis::NotCandidate
            };
        }
        FirstOperator::None => return BuiltinPipelineSourceAnalysis::NotCandidate,
    };

    let left = &input[..pipe_index];
    let words = match args::parse(left) {
        Ok(words) => words,
        Err(args::ParseError::NeedsBash) => {
            return if begins_with_builtin(left) {
                BuiltinPipelineSourceAnalysis::Invalid(BuiltinPipelineSourceError::new(
                    "内建命令管道左段只支持字面参数",
                ))
            } else {
                BuiltinPipelineSourceAnalysis::NotCandidate
            };
        }
        Err(args::ParseError::Syntax(_)) => {
            return if begins_with_builtin(left) {
                BuiltinPipelineSourceAnalysis::Invalid(BuiltinPipelineSourceError::new(
                    "内建命令管道左段存在未闭合的引号或转义",
                ))
            } else {
                BuiltinPipelineSourceAnalysis::NotCandidate
            };
        }
    };
    let Some((name, arguments)) = words.split_first() else {
        return BuiltinPipelineSourceAnalysis::NotCandidate;
    };
    let Some(disposition) = pipeline_source_disposition(name, arguments) else {
        return BuiltinPipelineSourceAnalysis::NotCandidate;
    };
    let tail = input[pipe_end..].trim_start();
    if tail.is_empty() {
        return BuiltinPipelineSourceAnalysis::Invalid(BuiltinPipelineSourceError::new(
            "内建命令管道缺少右侧命令",
        ));
    }

    BuiltinPipelineSourceAnalysis::Candidate(BuiltinPipelineSourcePlan {
        name: name.clone(),
        arguments: arguments.to_vec(),
        tail: tail.to_owned(),
        disposition,
    })
}

fn begins_with_builtin(prefix: &str) -> bool {
    prefix.split_whitespace().next().is_some_and(is_builtin)
}

fn first_operator(input: &str) -> FirstOperator {
    let mut characters = input.char_indices().peekable();
    let mut quote = None;

    while let Some((index, character)) = characters.next() {
        match quote {
            Some(Quote::Single) => {
                if character == '\'' {
                    quote = None;
                }
            }
            Some(Quote::Double) => match character {
                '"' => quote = None,
                '\\' => {
                    characters.next();
                }
                _ => {}
            },
            None => match character {
                '\'' => quote = Some(Quote::Single),
                '"' => quote = Some(Quote::Double),
                '\\' => {
                    characters.next();
                }
                '|' => match characters.peek().copied() {
                    Some((_, '|' | '&')) => return FirstOperator::Unsupported { index },
                    _ => {
                        return FirstOperator::Pipe {
                            index,
                            end: index + character.len_utf8(),
                        };
                    }
                },
                ';' | '&' | '\n' | '\r' | '(' | ')' | '<' | '>' => {
                    return FirstOperator::Unsupported { index };
                }
                _ => {}
            },
        }
    }
    FirstOperator::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(input: &str) -> BuiltinPipelineSourcePlan {
        match analyze(input) {
            BuiltinPipelineSourceAnalysis::Candidate(plan) => plan,
            other => panic!("expected candidate, got {other:?}"),
        }
    }

    #[test]
    fn recognizes_literal_builtin_source_and_preserves_the_bash_tail() {
        let plan = plan("history 100 | grep cargo | tail -5");
        assert_eq!(plan.name, "history");
        assert_eq!(plan.arguments, ["100"]);
        assert_eq!(plan.tail, "grep cargo | tail -5");
        assert_eq!(
            plan.disposition,
            BuiltinPipelineSourceDisposition::InternalOutput
        );

        assert!(matches!(
            analyze("printf 'a|b' | grep a"),
            BuiltinPipelineSourceAnalysis::NotCandidate
        ));
        assert!(matches!(
            analyze("printf x | history"),
            BuiltinPipelineSourceAnalysis::NotCandidate
        ));
    }

    #[test]
    fn classifies_stateful_forms_without_executing_them() {
        assert_eq!(
            plan("cd / | pwd").disposition,
            BuiltinPipelineSourceDisposition::BashSubshell
        );
        assert!(matches!(
            plan("dirs -c | cat").disposition,
            BuiltinPipelineSourceDisposition::Reject(_)
        ));
        assert!(matches!(
            plan("zh safety reload | cat").disposition,
            BuiltinPipelineSourceDisposition::Reject(_)
        ));
        assert!(matches!(
            plan("fg | cat").disposition,
            BuiltinPipelineSourceDisposition::Reject(_)
        ));
    }

    #[test]
    fn rejects_ambiguous_builtin_left_syntax() {
        for input in [
            "history |& grep cargo",
            "history > snapshot | grep cargo",
            "history || grep cargo",
            "history |",
        ] {
            assert!(
                matches!(analyze(input), BuiltinPipelineSourceAnalysis::Invalid(_)),
                "{input}"
            );
        }
    }
}
