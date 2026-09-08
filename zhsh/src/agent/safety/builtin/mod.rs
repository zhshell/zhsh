//! zhsh 随发行版提供的 Linux 核心工具安全分析器。

use super::{AnalyzerDecision, CommandSafetyAnalyzer, SafetyAssessment, SafetyCommand};

mod disclosure;
mod rules;

pub(super) use disclosure::apply as apply_disclosure;

pub(in super::super) fn known_write_targets<'a>(
    program: &str,
    arguments: &'a [String],
) -> Vec<&'a str> {
    rules::known_write_targets(program, arguments)
}

pub(in super::super) fn handles_program(program: &str) -> bool {
    rules::handles_program(program)
}

pub(in super::super) fn programs() -> &'static [&'static str] {
    rules::programs()
}

/// zhsh 唯一内置的程序语义分析器。
pub(super) struct LinuxCoreAnalyzer;

impl CommandSafetyAnalyzer for LinuxCoreAnalyzer {
    fn analyze(&self, command: &SafetyCommand<'_>) -> Result<AnalyzerDecision, String> {
        Ok(
            match assess_program_signature(
                command.program,
                command.arguments,
                command.nesting_depth,
            ) {
                Some(assessment) => AnalyzerDecision::Assessed(assessment),
                None => AnalyzerDecision::Abstain,
            },
        )
    }
}

pub(super) fn assess_program_signature(
    program: &str,
    arguments: &[String],
    depth: usize,
) -> Option<SafetyAssessment> {
    if observation_signatures()
        .iter()
        .any(|signature| signature.matches(program, arguments))
    {
        return Some(SafetyAssessment::read_only());
    }
    rules::assess_program(program, arguments, depth)
}

struct CommandSignature {
    programs: &'static [&'static str],
    argument_sets: &'static [&'static [&'static str]],
}

impl CommandSignature {
    fn matches(&self, program: &str, arguments: &[String]) -> bool {
        self.programs.contains(&program)
            && self.argument_sets.iter().any(|expected| {
                arguments.len() == expected.len()
                    && arguments
                        .iter()
                        .map(String::as_str)
                        .eq(expected.iter().copied())
            })
    }
}

fn observation_signatures() -> &'static [CommandSignature] {
    const ZH_QUERY_ARGUMENTS: &[&[&str]] = &[
        &[],
        &["status"],
        &["ls"],
        &["trust"],
        &["help"],
        &["-h"],
        &["--help"],
    ];
    const SIGNATURES: &[CommandSignature] = &[CommandSignature {
        programs: &["zh"],
        argument_sets: ZH_QUERY_ARGUMENTS,
    }];
    SIGNATURES
}

#[cfg(test)]
mod tests {
    use super::super::SafetyLevel;
    use super::*;

    #[test]
    fn zh_trust_query_is_observational_but_mutation_is_not() {
        assert_eq!(
            assess_program_signature("zh", &["trust".into()], 0)
                .unwrap()
                .level,
            SafetyLevel::ReadOnly
        );
        assert!(assess_program_signature("zh", &["trust".into(), "trusted".into()], 0).is_none());
    }
}
