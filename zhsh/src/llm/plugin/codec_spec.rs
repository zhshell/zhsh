//! 非图灵完备的声明式 Codec schema。

use super::super::model::EncodeProfile;
use super::package::{valid_header_name, valid_option_name, valid_relative_path};
use crate::common::{AppError, AppResult};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

const MAX_EXPR_DEPTH: usize = 16;
const MAX_EXPR_NODES: usize = 256;
const MAX_RULES: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodecSpec {
    pub encode: EncodeProfiles,
    pub decode: DecodeSpec,
    pub decode_error: ErrorDecodeSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EncodeProfiles {
    pub default: EncodeSpec,
    #[serde(default)]
    pub json_schema: Option<EncodeSpec>,
}

impl EncodeProfiles {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &EncodeSpec> {
        std::iter::once(&self.default).chain(self.json_schema.iter())
    }

    pub(crate) fn select(&self, profile: EncodeProfile) -> AppResult<&EncodeSpec> {
        match profile {
            EncodeProfile::Default => Ok(&self.default),
            EncodeProfile::JsonSchema => self
                .json_schema
                .as_ref()
                .ok_or_else(|| AppError::protocol("LLM Codec 缺少 JSON Schema 编码 Profile")),
        }
    }

    pub(crate) fn supports_json_schema(&self) -> bool {
        self.json_schema.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EncodeSpec {
    pub path: String,
    #[serde(default)]
    pub query: Vec<QuerySpec>,
    pub headers: Vec<HeaderSpec>,
    pub body: Expr,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuerySpec {
    pub name: String,
    pub parts: Vec<HeaderPartSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeaderSpec {
    pub name: String,
    pub parts: Vec<HeaderPartSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum HeaderPartSpec {
    Literal { value: String },
    SecretSlot { name: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Expr {
    Literal {
        value: Value,
    },
    Input {
        field: InputField,
        default: Option<Value>,
    },
    Message {
        field: MessageField,
        #[serde(default)]
        mappings: BTreeMap<String, String>,
    },
    Messages {
        include_system: bool,
        item: Box<Expr>,
    },
    SecretSlot {
        name: String,
    },
    Object {
        fields: BTreeMap<String, Expr>,
    },
    Array {
        items: Vec<Expr>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InputField {
    System,
    Model,
    MaxOutputTokens,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageField {
    Role,
    Content,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecodeSpec {
    pub text: TextSpec,
    pub finish: FinishSpec,
    pub usage: Option<UsageSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TextSpec {
    Value {
        path: String,
        #[serde(default)]
        default: String,
    },
    Concat {
        array_path: String,
        value_path: String,
        filter_path: Option<String>,
        #[serde(default)]
        filter_values: Vec<String>,
        #[serde(default)]
        include_missing_filter: bool,
    },
    NestedConcat {
        outer_array_path: String,
        outer_filter_path: Option<String>,
        #[serde(default)]
        outer_filter_values: Vec<String>,
        inner_array_path: String,
        inner_filter_path: Option<String>,
        #[serde(default)]
        inner_filter_values: Vec<String>,
        value_path: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum FinishSpec {
    SinglePath(SinglePathFinishSpec),
    Rules(RuleFinishSpec),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SinglePathFinishSpec {
    pub path: String,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default)]
    pub output_limit: Vec<String>,
    #[serde(default)]
    pub content_filtered: Vec<String>,
    pub missing: MissingFinish,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleFinishSpec {
    pub rules: Vec<FinishRule>,
    pub default: FinishOutcome,
    pub require_text_for_completed: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FinishRule {
    pub conditions: Vec<FinishCondition>,
    pub outcome: FinishOutcome,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FinishCondition {
    PointerEquals {
        path: String,
        values: Vec<String>,
    },
    NestedAny {
        outer_array_path: String,
        outer_filter_path: Option<String>,
        #[serde(default)]
        outer_filter_values: Vec<String>,
        inner_array_path: String,
        value_path: String,
        values: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinishOutcome {
    Completed,
    OutputLimit,
    ContentFiltered,
    ProtocolError,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MissingFinish {
    Completed,
    OutputLimit,
    ContentFiltered,
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageSpec {
    pub input_path: String,
    pub output_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ErrorDecodeSpec {
    pub message_paths: Vec<String>,
    #[serde(default)]
    pub code_paths: Vec<String>,
    #[serde(default)]
    pub marker_paths: Vec<String>,
    #[serde(default)]
    pub request_id_paths: Vec<String>,
    #[serde(default)]
    pub request_id_headers: Vec<String>,
    pub rules: Vec<ErrorRule>,
    #[serde(default)]
    pub retryable_kinds: Vec<ErrorKindSpec>,
    pub retry_after_header: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ErrorRule {
    pub kind: ErrorKindSpec,
    #[serde(default)]
    pub statuses: Vec<u16>,
    pub status_min: Option<u16>,
    pub status_max: Option<u16>,
    #[serde(default)]
    pub contains_any: Vec<String>,
    #[serde(default)]
    pub contains_all: Vec<String>,
    #[serde(default)]
    pub mode: RuleMode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuleMode {
    #[default]
    Any,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKindSpec {
    Authentication,
    Permission,
    RateLimited,
    QuotaExhausted,
    InvalidRequest,
    ModelNotFound,
    ContextLimit,
    ContentRejected,
    ServiceUnavailable,
    ProviderInternal,
    Unknown,
}

impl CodecSpec {
    pub(super) fn validate(&self) -> AppResult<()> {
        for encode in self.encode.iter() {
            encode.validate()?;
        }
        self.decode.validate()?;
        self.decode_error.validate()?;
        Ok(())
    }
}

impl EncodeSpec {
    fn validate(&self) -> AppResult<()> {
        if !valid_relative_path(&self.path) {
            return Err(AppError::protocol("LLM Codec 请求路径不合法"));
        }
        if self.query.len() > 32
            || self.headers.len() > 64
            || self
                .query
                .iter()
                .any(|query| !valid_option_name(&query.name) || !valid_parts(&query.parts))
        {
            return Err(AppError::protocol("LLM Codec query 声明不合法"));
        }
        let mut header_names = HashSet::new();
        for header in &self.headers {
            if !valid_header_name(&header.name)
                || !header_names.insert(header.name.to_ascii_lowercase())
                || header.parts.is_empty()
                || header.parts.len() > 16
            {
                return Err(AppError::protocol("LLM Codec Header 声明不合法"));
            }
            if !valid_parts(&header.parts) {
                return Err(AppError::protocol("LLM Codec Header part 不合法"));
            }
        }
        let mut nodes = 0;
        validate_expr(&self.body, 0, &mut nodes, false)?;
        Ok(())
    }
}

impl DecodeSpec {
    fn validate(&self) -> AppResult<()> {
        match &self.text {
            TextSpec::Value { path, default } => {
                validate_pointer(path)?;
                if default.len() > 4096 {
                    return Err(AppError::protocol("LLM Codec 默认文本过长"));
                }
            }
            TextSpec::Concat {
                array_path,
                value_path,
                filter_path,
                filter_values,
                ..
            } => {
                validate_pointer(array_path)?;
                validate_pointer(value_path)?;
                if let Some(path) = filter_path {
                    validate_pointer(path)?;
                }
                if filter_values.len() > 32 || filter_values.iter().any(|value| value.len() > 128) {
                    return Err(AppError::protocol("LLM Codec 文本过滤器过大"));
                }
            }
            TextSpec::NestedConcat {
                outer_array_path,
                outer_filter_path,
                outer_filter_values,
                inner_array_path,
                inner_filter_path,
                inner_filter_values,
                value_path,
            } => {
                validate_pointer(outer_array_path)?;
                validate_pointer(inner_array_path)?;
                validate_pointer(value_path)?;
                if let Some(path) = outer_filter_path {
                    validate_pointer(path)?;
                }
                if let Some(path) = inner_filter_path {
                    validate_pointer(path)?;
                }
                validate_filter(
                    outer_filter_path.as_deref(),
                    outer_filter_values,
                    "LLM Codec 外层文本过滤器不合法",
                )?;
                validate_filter(
                    inner_filter_path.as_deref(),
                    inner_filter_values,
                    "LLM Codec 内层文本过滤器不合法",
                )?;
            }
        }
        match &self.finish {
            FinishSpec::SinglePath(SinglePathFinishSpec {
                path,
                completed,
                output_limit,
                content_filtered,
                ..
            }) => {
                validate_pointer(path)?;
                if completed.len() + output_limit.len() + content_filtered.len() > 64 {
                    return Err(AppError::protocol("LLM Codec finish 映射过大"));
                }
            }
            FinishSpec::Rules(RuleFinishSpec { rules, .. }) => {
                if rules.is_empty() || rules.len() > MAX_RULES {
                    return Err(AppError::protocol("LLM Codec finish 规则规模不合法"));
                }
                for rule in rules {
                    if rule.conditions.is_empty() || rule.conditions.len() > 8 {
                        return Err(AppError::protocol("LLM Codec finish 规则条件不合法"));
                    }
                    for condition in &rule.conditions {
                        validate_finish_condition(condition)?;
                    }
                }
            }
        }
        if let Some(usage) = &self.usage {
            validate_pointer(&usage.input_path)?;
            validate_pointer(&usage.output_path)?;
        }
        Ok(())
    }
}

fn validate_finish_condition(condition: &FinishCondition) -> AppResult<()> {
    match condition {
        FinishCondition::PointerEquals { path, values } => {
            validate_pointer(path)?;
            validate_values(values, "LLM Codec finish 条件值不合法")
        }
        FinishCondition::NestedAny {
            outer_array_path,
            outer_filter_path,
            outer_filter_values,
            inner_array_path,
            value_path,
            values,
        } => {
            validate_pointer(outer_array_path)?;
            validate_pointer(inner_array_path)?;
            validate_pointer(value_path)?;
            if let Some(path) = outer_filter_path {
                validate_pointer(path)?;
            }
            validate_filter(
                outer_filter_path.as_deref(),
                outer_filter_values,
                "LLM Codec finish 外层过滤器不合法",
            )?;
            validate_values(values, "LLM Codec finish 条件值不合法")
        }
    }
}

fn validate_filter(path: Option<&str>, values: &[String], message: &str) -> AppResult<()> {
    match path {
        Some(_) => validate_values(values, message),
        None if values.is_empty() => Ok(()),
        None => Err(AppError::protocol(message)),
    }
}

fn validate_values(values: &[String], message: &str) -> AppResult<()> {
    if values.is_empty()
        || values.len() > 32
        || values
            .iter()
            .any(|value| value.is_empty() || value.len() > 128)
    {
        return Err(AppError::protocol(message));
    }
    Ok(())
}

impl ErrorDecodeSpec {
    fn validate(&self) -> AppResult<()> {
        if self.message_paths.is_empty()
            || self.rules.len() > MAX_RULES
            || self.message_paths.len()
                + self.code_paths.len()
                + self.marker_paths.len()
                + self.request_id_paths.len()
                > 64
        {
            return Err(AppError::protocol("LLM Codec 错误映射规模不合法"));
        }
        for path in self
            .message_paths
            .iter()
            .chain(&self.code_paths)
            .chain(&self.marker_paths)
            .chain(&self.request_id_paths)
        {
            validate_pointer(path)?;
        }
        if self
            .request_id_headers
            .iter()
            .any(|name| !valid_header_name(name))
            || self
                .retry_after_header
                .as_deref()
                .is_some_and(|name| !valid_header_name(name))
        {
            return Err(AppError::protocol("LLM Codec 错误 Header 声明不合法"));
        }
        for rule in &self.rules {
            if rule.statuses.is_empty()
                && rule.status_min.is_none()
                && rule.status_max.is_none()
                && rule.contains_any.is_empty()
                && rule.contains_all.is_empty()
            {
                return Err(AppError::protocol("LLM Codec 错误规则没有条件"));
            }
            if rule
                .contains_any
                .iter()
                .chain(&rule.contains_all)
                .any(|value| value.is_empty() || value.len() > 128)
            {
                return Err(AppError::protocol("LLM Codec 错误规则关键词不合法"));
            }
            if rule
                .status_min
                .zip(rule.status_max)
                .is_some_and(|(min, max)| min > max)
            {
                return Err(AppError::protocol("LLM Codec 错误状态范围不合法"));
            }
        }
        Ok(())
    }
}

fn valid_parts(parts: &[HeaderPartSpec]) -> bool {
    !parts.is_empty()
        && parts.len() <= 16
        && parts.iter().all(|part| match part {
            HeaderPartSpec::Literal { value } => {
                value.len() <= 8192 && !value.contains(['\r', '\n', '\0'])
            }
            HeaderPartSpec::SecretSlot { name } => valid_option_name(name),
        })
}

fn validate_expr(
    expr: &Expr,
    depth: usize,
    nodes: &mut usize,
    message_context: bool,
) -> AppResult<()> {
    *nodes += 1;
    if depth > MAX_EXPR_DEPTH || *nodes > MAX_EXPR_NODES {
        return Err(AppError::protocol("LLM Codec 请求表达式超过复杂度限制"));
    }
    match expr {
        Expr::Literal { value } => {
            if serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > 64 * 1024) {
                return Err(AppError::protocol("LLM Codec literal 过大"));
            }
        }
        Expr::Input { default, .. } => {
            if default.as_ref().is_some_and(|value| {
                serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > 4096)
            }) {
                return Err(AppError::protocol("LLM Codec request 默认值过大"));
            }
        }
        Expr::Message { mappings, .. } => {
            if !message_context
                || mappings.len() > 32
                || mappings
                    .iter()
                    .any(|(from, to)| from.len() > 128 || to.len() > 128)
            {
                return Err(AppError::protocol(
                    "Codec message 字段只能在 messages 模板内使用",
                ));
            }
        }
        Expr::Messages { item, .. } => validate_expr(item, depth + 1, nodes, true)?,
        Expr::SecretSlot { name } => {
            if !valid_option_name(name) {
                return Err(AppError::protocol("Codec 密钥槽名称不合法"));
            }
        }
        Expr::Object { fields } => {
            if fields.len() > 128 || fields.keys().any(|key| key.len() > 256) {
                return Err(AppError::protocol("LLM Codec object 过大"));
            }
            for child in fields.values() {
                validate_expr(child, depth + 1, nodes, message_context)?;
            }
        }
        Expr::Array { items } => {
            if items.len() > 128 {
                return Err(AppError::protocol("LLM Codec array 过大"));
            }
            for child in items {
                validate_expr(child, depth + 1, nodes, message_context)?;
            }
        }
    }
    Ok(())
}

fn validate_pointer(path: &str) -> AppResult<()> {
    if !path.starts_with('/') || path.len() > 1024 || path.contains('\0') {
        return Err(AppError::protocol("LLM Codec JSON Pointer 不合法"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_PACKAGE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/llm-codecs/openai@0.3.0.zhcodec"
    ));

    fn openai_codec() -> Value {
        let envelope = super::super::container::verify(OPENAI_PACKAGE, |_, _| true).unwrap();
        serde_json::from_slice::<Value>(envelope.payload).unwrap()["codec"].clone()
    }

    #[test]
    fn accepts_official_codec_and_rejects_unknown_capabilities() {
        let mut value = openai_codec();
        let codec: CodecSpec = serde_json::from_value(value.clone()).unwrap();
        assert!(codec.validate().is_ok());
        value["execute"] = Value::String("/bin/sh".into());
        assert!(serde_json::from_value::<CodecSpec>(value).is_err());
    }

    #[test]
    fn rejects_expressions_beyond_the_static_complexity_limit() {
        let mut value = openai_codec();
        let mut expression = serde_json::json!({"op": "literal", "value": null});
        for _ in 0..=MAX_EXPR_DEPTH {
            expression = serde_json::json!({"op": "array", "items": [expression]});
        }
        value["encode"]["default"]["body"] = expression;
        let codec: CodecSpec = serde_json::from_value(value).unwrap();
        assert!(codec.validate().is_err());
    }
}
