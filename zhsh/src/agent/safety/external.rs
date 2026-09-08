//! 声明式 `*.zhse.json` 命令安全规则加载与求值。

use super::{
    AnalyzerDecision, BindingClass, CommandSafetyAnalyzer, DisclosureClass, SafetyAssessment,
    SafetyCommand, SafetyLevel, StateScope, SupervisionClass,
};
use crate::common::read_file_snapshot;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

pub(super) const SAFETY_RULE_SCHEMA: u8 = 1;
pub(super) const SAFETY_RULE_SUFFIX: &str = ".zhse.json";
pub(super) const RULE_FILE_LIMIT: usize = 256 * 1024;
pub(super) const BUNDLE_FILE_LIMIT: usize = 4 * 1024 * 1024;
pub(super) const BUNDLE_SUFFIX: &str = ".zhse.json.gz";
const BUNDLE_EXPANDED_LIMIT: usize = 8 * 1024 * 1024;
const BUNDLE_MAX_RATIO: usize = 100;
const MAX_BUNDLE_RULES: usize = 128;
const MAX_PROGRAMS: usize = 128;
const MAX_RULES: usize = 256;
const MAX_MATCH_VALUES: usize = 128;

pub(super) struct ExternalAnalyzer {
    name: String,
    programs: Vec<String>,
    rules: Vec<Rule>,
    default: DefaultRule,
}

impl ExternalAnalyzer {
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let input = read_file_snapshot(path, RULE_FILE_LIMIT)
            .map_err(|error| error.to_string())?
            .bytes;
        Self::parse(path, &input)
    }

    pub(super) fn parse(_path: &Path, input: &[u8]) -> Result<Self, String> {
        let mut rule_file: RuleFile =
            serde_json::from_slice(input).map_err(|error| format!("JSON 格式无效: {error}"))?;
        rule_file.validate()?;
        rule_file.programs.sort();
        rule_file.programs.dedup();
        Ok(Self {
            name: rule_file.name,
            programs: rule_file.programs,
            rules: rule_file.rules,
            default: rule_file.default,
        })
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn programs(&self) -> &[String] {
        &self.programs
    }

    pub(super) fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub(super) fn explicit_match(&self, command: &SafetyCommand<'_>) -> Option<SafetyAssessment> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matcher.matches(command))
            .map(|rule| rule.assessment.to_assessment(&rule.id))
    }

    pub(super) fn default_assessment(&self) -> SafetyAssessment {
        self.default.assessment.to_assessment(&self.default.id)
    }

    fn evaluate(&self, command: &SafetyCommand<'_>) -> SafetyAssessment {
        self.explicit_match(command)
            .unwrap_or_else(|| self.default_assessment())
    }
}

/// 已经严格验证、可按规范名称持久化的一个规则文档。
pub(super) struct ParsedRuleDocument {
    pub(super) analyzer: ExternalAnalyzer,
    pub(super) canonical_bytes: Vec<u8>,
}

/// 一次外部输入的严格解析结果；`kind/version` 只描述数据格式，不表达信任。
pub(super) struct ParsedInstallSource {
    pub(super) documents: Vec<ParsedRuleDocument>,
    pub(super) kind: &'static str,
    pub(super) version: String,
}

impl ParsedRuleDocument {
    pub(super) fn target_basename(&self) -> String {
        format!("{}{}", self.analyzer.name(), SAFETY_RULE_SUFFIX)
    }
}

/// 解析普通规则文件或 gzip 聚合规则包；外部文件名不进入规则身份。
pub(super) fn parse_install_source(
    path: &Path,
    basename: &str,
    input: &[u8],
) -> Result<ParsedInstallSource, String> {
    if basename.ends_with(BUNDLE_SUFFIX) {
        parse_bundle(path, input)
    } else if basename.ends_with(SAFETY_RULE_SUFFIX) {
        Ok(ParsedInstallSource {
            documents: vec![parse_document(path, input)?],
            kind: "rule",
            version: SAFETY_RULE_SCHEMA.to_string(),
        })
    } else {
        Err("Safety 输入必须以 .zhse.json 或 .zhse.json.gz 结尾".into())
    }
}

fn parse_document(_path: &Path, input: &[u8]) -> Result<ParsedRuleDocument, String> {
    let mut rule_file: RuleFile =
        serde_json::from_slice(input).map_err(|error| format!("JSON 格式无效: {error}"))?;
    rule_file.validate()?;
    rule_file.programs.sort();
    let analyzer = ExternalAnalyzer {
        name: rule_file.name.clone(),
        programs: rule_file.programs.clone(),
        rules: rule_file.rules.clone(),
        default: rule_file.default.clone(),
    };
    let mut canonical_bytes = serde_json::to_vec_pretty(&rule_file)
        .map_err(|error| format!("无法规范序列化 Safety 规则: {error}"))?;
    canonical_bytes.push(b'\n');
    if canonical_bytes.len() > RULE_FILE_LIMIT {
        return Err("规范化 Safety 规则超过资源上限".into());
    }
    Ok(ParsedRuleDocument {
        analyzer,
        canonical_bytes,
    })
}

fn parse_bundle(path: &Path, input: &[u8]) -> Result<ParsedInstallSource, String> {
    let mut decoder = GzDecoder::new(input);
    let mut expanded = Vec::new();
    decoder
        .by_ref()
        .take(BUNDLE_EXPANDED_LIMIT as u64 + 1)
        .read_to_end(&mut expanded)
        .map_err(|error| format!("gzip 解压失败: {error}"))?;
    if expanded.len() > BUNDLE_EXPANDED_LIMIT {
        return Err("gzip 展开内容超过资源上限".into());
    }
    if !input.is_empty() && expanded.len() > input.len().saturating_mul(BUNDLE_MAX_RATIO) {
        return Err("gzip 压缩比超过资源上限".into());
    }
    let bundle: RuleBundle = serde_json::from_slice(&expanded)
        .map_err(|error| format!("bundle JSON 格式无效: {error}"))?;
    if bundle.bundle_schema != 1 {
        return Err("bundle schema 版本不受支持".into());
    }
    semver::Version::parse(&bundle.version)
        .map_err(|error| format!("bundle version 不是有效 SemVer: {error}"))?;
    if bundle.rules.is_empty() || bundle.rules.len() > MAX_BUNDLE_RULES {
        return Err("bundle rules 数量不合法".into());
    }
    let version = bundle.version;
    let mut names = BTreeSet::new();
    let mut parsed = Vec::with_capacity(bundle.rules.len());
    for value in bundle.rules {
        let bytes = serde_json::to_vec(&value)
            .map_err(|error| format!("无法读取 bundle 规则对象: {error}"))?;
        let document = parse_document(path, &bytes)?;
        if !names.insert(document.analyzer.name().to_owned()) {
            return Err(format!(
                "bundle 包含重复规则集名称：{}",
                document.analyzer.name()
            ));
        }
        parsed.push(document);
    }
    Ok(ParsedInstallSource {
        documents: parsed,
        kind: "bundle",
        version,
    })
}

impl CommandSafetyAnalyzer for ExternalAnalyzer {
    fn analyze(&self, command: &SafetyCommand<'_>) -> Result<AnalyzerDecision, String> {
        Ok(AnalyzerDecision::Assessed(self.evaluate(command)))
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    schema: u8,
    name: String,
    programs: Vec<String>,
    rules: Vec<Rule>,
    default: DefaultRule,
}

impl RuleFile {
    fn validate(&self) -> Result<(), String> {
        if self.schema != SAFETY_RULE_SCHEMA {
            return Err("规则 schema 版本不受支持".into());
        }
        validate_identifier(&self.name, "规则集名称")?;
        if self.programs.is_empty() || self.programs.len() > MAX_PROGRAMS {
            return Err("programs 数量不合法".into());
        }
        for program in &self.programs {
            validate_identifier(program, "程序名")?;
        }
        if self.programs.iter().collect::<BTreeSet<_>>().len() != self.programs.len() {
            return Err("programs 不得包含重复程序名".into());
        }
        if self.rules.len() > MAX_RULES {
            return Err("rules 数量超限".into());
        }
        validate_identifier(&self.default.id, "默认规则 id")?;
        let mut rule_ids = BTreeSet::new();
        for rule in &self.rules {
            rule.validate(&self.programs)?;
            if !rule_ids.insert(rule.id.as_str()) {
                return Err(format!("规则 id 重复：{}", rule.id));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    id: String,
    #[serde(rename = "match")]
    matcher: RuleMatcher,
    assessment: RuleAssessment,
}

impl Rule {
    fn validate(&self, declared_programs: &[String]) -> Result<(), String> {
        validate_identifier(&self.id, "规则 id")?;
        self.matcher.validate(declared_programs)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DefaultRule {
    id: String,
    assessment: RuleAssessment,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct RuleMatcher {
    programs: Vec<String>,
    any_arguments: Vec<String>,
    all_arguments: Vec<String>,
    none_arguments: Vec<String>,
    any_sequences: Vec<Vec<String>>,
}

impl RuleMatcher {
    fn validate(&self, declared_programs: &[String]) -> Result<(), String> {
        if self.programs.len() > MAX_PROGRAMS
            || self.any_arguments.len() > MAX_MATCH_VALUES
            || self.all_arguments.len() > MAX_MATCH_VALUES
            || self.none_arguments.len() > MAX_MATCH_VALUES
            || self.any_sequences.len() > MAX_MATCH_VALUES
            || self
                .any_sequences
                .iter()
                .any(|sequence| sequence.is_empty() || sequence.len() > 16)
        {
            return Err("规则匹配条件数量不合法".into());
        }
        if self
            .programs
            .iter()
            .any(|program| !declared_programs.contains(program))
        {
            return Err("规则引用了未在 programs 声明的程序".into());
        }
        Ok(())
    }

    fn matches(&self, command: &SafetyCommand<'_>) -> bool {
        (self.programs.is_empty() || self.programs.iter().any(|item| item == command.program))
            && (self.any_arguments.is_empty()
                || command
                    .arguments
                    .iter()
                    .any(|argument| self.any_arguments.contains(argument)))
            && self
                .all_arguments
                .iter()
                .all(|item| command.arguments.contains(item))
            && self
                .none_arguments
                .iter()
                .all(|item| !command.arguments.contains(item))
            && (self.any_sequences.is_empty()
                || self.any_sequences.iter().any(|sequence| {
                    command
                        .arguments
                        .windows(sequence.len())
                        .any(|window| window == sequence)
                }))
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuleAssessment {
    level: SafetyLevel,
    #[serde(default = "default_disclosure")]
    disclosure: DisclosureClass,
    #[serde(default = "default_binding")]
    binding: BindingClass,
    state_scope: Option<StateScope>,
    #[serde(default = "default_supervision")]
    supervision: SupervisionClass,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    privilege_change: bool,
    #[serde(default)]
    mandatory_confirmation: bool,
    #[serde(default)]
    session_mutation: bool,
    #[serde(default)]
    file_output: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleBundle {
    bundle_schema: u8,
    version: String,
    rules: Vec<serde_json::Value>,
}

impl RuleAssessment {
    fn to_assessment(&self, reason: &str) -> SafetyAssessment {
        let mut assessment = SafetyAssessment::new(self.level, reason);
        assessment.disclosure = self.disclosure;
        assessment.binding = self.binding;
        assessment.state_scope =
            self.state_scope
                .unwrap_or(if self.level >= SafetyLevel::StateChanging {
                    StateScope::OutsideOrUnknown
                } else {
                    StateScope::None
                });
        assessment.supervision = self.supervision;
        assessment.network = self.network;
        assessment.privilege_change = self.privilege_change;
        assessment.mandatory_confirmation = self.mandatory_confirmation;
        assessment.session_mutation = self.session_mutation;
        assessment.file_output = self.file_output;
        assessment
    }
}

fn default_disclosure() -> DisclosureClass {
    DisclosureClass::Metadata
}

fn default_binding() -> BindingClass {
    BindingClass::StaticSystemTrusted
}

fn default_supervision() -> SupervisionClass {
    SupervisionClass::FiniteForeground
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
    {
        Ok(())
    } else {
        Err(format!("{label} 不合法"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    #[test]
    fn later_explicit_rule_replaces_the_earlier_rule_in_one_document() {
        let input = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "name": "java",
            "programs": ["java"],
            "rules": [
                {"id": "earlier", "match": {"any_arguments": ["-version"]}, "assessment": {"level": "destructive"}},
                {"id": "later", "match": {"any_arguments": ["-version"]}, "assessment": {"level": "read_only"}}
            ],
            "default": {"id": "fallback", "assessment": {"level": "unknown"}}
        })).unwrap();
        let analyzer = ExternalAnalyzer::parse(Path::new("java.zhse.json"), &input).unwrap();
        let command = SafetyCommand {
            program: "java",
            arguments: &["-version".into()],
            nesting_depth: 0,
        };
        let assessment = analyzer.evaluate(&command);
        assert_eq!(assessment.level, SafetyLevel::ReadOnly);
        assert_eq!(assessment.reasons, vec!["later"]);
    }

    #[test]
    fn gzip_bundle_accepts_different_rule_sets_for_the_same_program() {
        let document = |name: &str| {
            serde_json::json!({
                "schema": 1,
                "name": name,
                "programs": ["java"],
                "rules": [],
                "default": {"id": "fallback", "assessment": {"level": "unknown"}}
            })
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "bundle_schema": 1,
            "version": "0.1.0",
            "rules": [document("java"), document("oracle")]
        }))
        .unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes).unwrap();
        let compressed = encoder.finish().unwrap();

        let parsed = parse_install_source(
            Path::new("rules.zhse.json.gz"),
            "rules.zhse.json.gz",
            &compressed,
        )
        .unwrap();

        assert_eq!(parsed.kind, "bundle");
        assert_eq!(parsed.version, "0.1.0");
        assert_eq!(parsed.documents.len(), 2);
        assert_eq!(parsed.documents[0].target_basename(), "java.zhse.json");
        assert_eq!(parsed.documents[1].target_basename(), "oracle.zhse.json");
    }
}
