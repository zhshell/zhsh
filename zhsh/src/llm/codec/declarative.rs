//! 有界声明式 Codec 求值器；不执行插件代码。

use super::super::model::EncodeProfile;
use super::super::plugin::{
    ErrorKindSpec, Expr, FinishCondition, FinishOutcome, FinishSpec, HeaderPartSpec, InputField,
    MessageField, MissingFinish, RuleMode, TextSpec, VerifiedPlugin,
};
use super::super::{CompletionRequest, FinishReason, ProviderError, ProviderErrorKind, TokenUsage};
use super::types::{
    DecodedResponse, HeaderPart, HeaderTemplate, PlannedJson, QueryParam, RequestPlan, WireResponse,
};
use crate::common::{AppError, AppResult};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(crate) struct CodecEvaluator;

impl CodecEvaluator {
    pub(crate) fn encode(
        &self,
        plugin: &VerifiedPlugin,
        profile: EncodeProfile,
        request: CompletionRequest,
    ) -> AppResult<RequestPlan> {
        if request.model.trim().is_empty() {
            return Err(AppError::input("LLM model 不能为空"));
        }
        let encode = plugin.package.codec.encode.select(profile)?;
        let relative_path = encode.path.clone();
        let query = encode
            .query
            .iter()
            .map(|item| QueryParam {
                name: item.name.clone(),
                parts: item.parts.iter().map(template_part).collect(),
            })
            .collect();
        let headers = encode
            .headers
            .iter()
            .map(|header| HeaderTemplate {
                name: header.name.clone(),
                parts: header.parts.iter().map(template_part).collect(),
            })
            .collect();
        let body = evaluate(&encode.body, &request, None)?;
        Ok(RequestPlan {
            relative_path,
            query,
            headers,
            body,
        })
    }

    pub(crate) fn decode(
        &self,
        plugin: &VerifiedPlugin,
        response: WireResponse,
    ) -> AppResult<DecodedResponse> {
        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|error| AppError::protocol(format!("Codec 无法解析 JSON 响应: {error}")))?;
        let decode = &plugin.package.codec.decode;
        let text = match &decode.text {
            TextSpec::Value { path, default } => body
                .pointer(path)
                .and_then(Value::as_str)
                .unwrap_or(default)
                .to_string(),
            TextSpec::Concat {
                array_path,
                value_path,
                filter_path,
                filter_values,
                include_missing_filter,
            } => {
                let values = body
                    .pointer(array_path)
                    .and_then(Value::as_array)
                    .ok_or_else(|| AppError::protocol("Codec 响应缺少声明的文本数组"))?;
                let mut text = String::new();
                for value in values {
                    let accepted = match filter_path {
                        None => true,
                        Some(path) => match value.pointer(path).and_then(Value::as_str) {
                            Some(marker) => filter_values.iter().any(|allowed| allowed == marker),
                            None => *include_missing_filter,
                        },
                    };
                    if accepted {
                        if let Some(part) = value.pointer(value_path).and_then(Value::as_str) {
                            text.push_str(part);
                        }
                    }
                }
                text
            }
            TextSpec::NestedConcat {
                outer_array_path,
                outer_filter_path,
                outer_filter_values,
                inner_array_path,
                inner_filter_path,
                inner_filter_values,
                value_path,
            } => nested_values(
                &body,
                outer_array_path,
                outer_filter_path.as_deref(),
                outer_filter_values,
                inner_array_path,
                inner_filter_path.as_deref(),
                inner_filter_values,
                value_path,
            )?
            .into_iter()
            .collect(),
        };
        let finish_reason = decode_finish(&body, &decode.finish, &text)?;
        let usage = decode.usage.as_ref().and_then(|usage| {
            let input = body.pointer(&usage.input_path).and_then(Value::as_u64);
            let output = body.pointer(&usage.output_path).and_then(Value::as_u64);
            (input.is_some() || output.is_some()).then_some(TokenUsage { input, output })
        });
        Ok(DecodedResponse {
            text,
            finish_reason,
            usage,
        })
    }

    pub(crate) fn decode_error(
        &self,
        plugin: &VerifiedPlugin,
        response: WireResponse,
    ) -> AppResult<ProviderError> {
        let body: Value = serde_json::from_slice(&response.body).map_err(|error| {
            AppError::protocol(format!("Codec 无法解析供应商错误 JSON: {error}"))
        })?;
        let spec = &plugin.package.codec.decode_error;
        let message = first_scalar(&body, &spec.message_paths)
            .unwrap_or_else(|| format!("provider returned HTTP {}", response.status));
        let provider_code = first_scalar(&body, &spec.code_paths);
        let marker = spec
            .marker_paths
            .iter()
            .filter_map(|path| scalar(body.pointer(path)?))
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let kind_spec = spec
            .rules
            .iter()
            .find(|rule| rule_matches(rule, response.status, &marker))
            .map(|rule| rule.kind)
            .unwrap_or(ErrorKindSpec::Unknown);
        let request_id = first_scalar(&body, &spec.request_id_paths).or_else(|| {
            spec.request_id_headers.iter().find_map(|name| {
                response
                    .headers
                    .iter()
                    .find(|header| header.name.eq_ignore_ascii_case(name))
                    .map(|header| header.value.clone())
            })
        });
        let retry_after_ms = spec.retry_after_header.as_ref().and_then(|name| {
            response
                .headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case(name))
                .and_then(|header| header.value.trim().parse::<u64>().ok())
                .and_then(|seconds| seconds.checked_mul(1000))
        });
        Ok(ProviderError {
            kind: error_kind(kind_spec),
            message,
            provider_code,
            request_id,
            retryable_hint: spec.retryable_kinds.contains(&kind_spec),
            retry_after_ms,
        })
    }

    pub(crate) fn validate_official_contract(&self, plugin: &VerifiedPlugin) -> AppResult<()> {
        if !plugin.official {
            return Ok(());
        }
        let (default_bytes, schema_bytes): (&[u8], &[u8]) = match plugin.package.format().as_str() {
            "openai@0.3.0" => (
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/llm-codecs/openai-responses.golden.json"
                )),
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/llm-codecs/openai-responses-structured.golden.json"
                )),
            ),
            "anthropic@0.3.0" => (
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/llm-codecs/anthropic-messages.golden.json"
                )),
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/llm-codecs/anthropic-messages-structured.golden.json"
                )),
            ),
            // Golden 向量只锁定随当前 zhsh 发布的基线。后续官方 FORMAT 已经经过
            // payload/AST 通用验证，不能因为没有编译进当前二进制而被重新绑定到 zhsh 发版。
            _ => return Ok(()),
        };
        for (profile, bytes) in [
            (EncodeProfile::Default, default_bytes),
            (EncodeProfile::JsonSchema, schema_bytes),
        ] {
            let fixture: Value = serde_json::from_slice(bytes)
                .map_err(|error| AppError::internal(format!("官方 Codec 契约损坏: {error}")))?;
            let request = contract_request(&fixture)?;
            let plan = self.encode(plugin, profile, request)?;
            let body = plan.body.materialize("contract-secret");
            if plan.relative_path != fixture["expected_request"]["relative_path"]
                || body != fixture["expected_request"]["json_body"]
            {
                return Err(AppError::protocol(
                    "官方 Codec 请求编码未通过运行时契约自检",
                ));
            }
        }
        let fixture: Value = serde_json::from_slice(default_bytes)
            .map_err(|error| AppError::internal(format!("官方 Codec 契约损坏: {error}")))?;
        let decoded = self.decode(plugin, contract_wire(&fixture["success_response"])?)?;
        if decoded.text != fixture["expected_success"]["text"]
            || finish_name(&decoded.finish_reason) != fixture["expected_success"]["finish_reason"]
        {
            return Err(AppError::protocol(
                "官方 Codec 成功响应未通过运行时契约自检",
            ));
        }
        let error = self.decode_error(plugin, contract_wire(&fixture["error_response"])?)?;
        if error_kind_name(error.kind) != fixture["expected_error"]["kind"]
            || error.retry_after_ms != fixture["expected_error"]["retry_after_ms"].as_u64()
        {
            return Err(AppError::protocol(
                "官方 Codec 错误响应未通过运行时契约自检",
            ));
        }
        Ok(())
    }
}

fn marker_matches(
    value: &Value,
    path: Option<&str>,
    allowed: &[String],
    include_missing: bool,
) -> bool {
    match path {
        None => true,
        Some(path) => match value.pointer(path).and_then(Value::as_str) {
            Some(marker) => allowed.iter().any(|candidate| candidate == marker),
            None => include_missing,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn nested_values(
    body: &Value,
    outer_array_path: &str,
    outer_filter_path: Option<&str>,
    outer_filter_values: &[String],
    inner_array_path: &str,
    inner_filter_path: Option<&str>,
    inner_filter_values: &[String],
    value_path: &str,
) -> AppResult<Vec<String>> {
    let outer = body
        .pointer(outer_array_path)
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::protocol("Codec 响应缺少声明的外层数组"))?;
    let mut result = Vec::new();
    for item in outer {
        if !marker_matches(item, outer_filter_path, outer_filter_values, false) {
            continue;
        }
        let inner = item
            .pointer(inner_array_path)
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::protocol("Codec 响应缺少声明的内层数组"))?;
        for block in inner {
            if marker_matches(block, inner_filter_path, inner_filter_values, false) {
                if let Some(value) = block.pointer(value_path).and_then(Value::as_str) {
                    result.push(value.to_string());
                }
            }
        }
    }
    Ok(result)
}

fn decode_finish(body: &Value, spec: &FinishSpec, text: &str) -> AppResult<FinishReason> {
    match spec {
        FinishSpec::SinglePath(spec) => {
            let value = body.pointer(&spec.path).and_then(Value::as_str);
            Ok(match value {
                Some(value) if spec.completed.iter().any(|item| item == value) => {
                    FinishReason::Completed
                }
                Some(value) if spec.output_limit.iter().any(|item| item == value) => {
                    FinishReason::OutputLimit
                }
                Some(value) if spec.content_filtered.iter().any(|item| item == value) => {
                    FinishReason::ContentFiltered
                }
                Some(value) => FinishReason::Unknown(value.to_string()),
                None => match spec.missing {
                    MissingFinish::Completed => FinishReason::Completed,
                    MissingFinish::OutputLimit => FinishReason::OutputLimit,
                    MissingFinish::ContentFiltered => FinishReason::ContentFiltered,
                    MissingFinish::Unknown => FinishReason::Unknown("missing".into()),
                },
            })
        }
        FinishSpec::Rules(spec) => {
            let outcome = spec
                .rules
                .iter()
                .find(|rule| {
                    rule.conditions
                        .iter()
                        .all(|condition| finish_condition_matches(body, condition))
                })
                .map_or(spec.default, |rule| rule.outcome);
            if matches!(outcome, FinishOutcome::ProtocolError) {
                return Err(AppError::protocol("Codec 响应状态不符合声明的协议"));
            }
            if matches!(outcome, FinishOutcome::Completed)
                && spec.require_text_for_completed
                && text.is_empty()
            {
                return Err(AppError::protocol("Codec 完成响应缺少文本"));
            }
            Ok(match outcome {
                FinishOutcome::Completed => FinishReason::Completed,
                FinishOutcome::OutputLimit => FinishReason::OutputLimit,
                FinishOutcome::ContentFiltered => FinishReason::ContentFiltered,
                FinishOutcome::ProtocolError => unreachable!(),
            })
        }
    }
}

fn finish_condition_matches(body: &Value, condition: &FinishCondition) -> bool {
    match condition {
        FinishCondition::PointerEquals { path, values } => body
            .pointer(path)
            .and_then(Value::as_str)
            .is_some_and(|value| values.iter().any(|candidate| candidate == value)),
        FinishCondition::NestedAny {
            outer_array_path,
            outer_filter_path,
            outer_filter_values,
            inner_array_path,
            value_path,
            values,
        } => body
            .pointer(outer_array_path)
            .and_then(Value::as_array)
            .is_some_and(|outer| {
                outer.iter().any(|item| {
                    marker_matches(
                        item,
                        outer_filter_path.as_deref(),
                        outer_filter_values,
                        false,
                    ) && item
                        .pointer(inner_array_path)
                        .and_then(Value::as_array)
                        .is_some_and(|inner| {
                            inner.iter().any(|block| {
                                block
                                    .pointer(value_path)
                                    .and_then(Value::as_str)
                                    .is_some_and(|value| {
                                        values.iter().any(|candidate| candidate == value)
                                    })
                            })
                        })
                })
            }),
    }
}

fn contract_request(value: &Value) -> AppResult<CompletionRequest> {
    let request = &value["request"];
    let messages = request["messages"]
        .as_array()
        .ok_or_else(|| AppError::internal("官方 Codec 契约缺少 messages"))?
        .iter()
        .map(|message| {
            Ok(super::super::LlmMessage::new(
                message["role"]
                    .as_str()
                    .ok_or_else(|| AppError::internal("官方 Codec 契约 role 无效"))?,
                message["content"]
                    .as_str()
                    .ok_or_else(|| AppError::internal("官方 Codec 契约 content 无效"))?,
            ))
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(CompletionRequest {
        system: request["system"].as_str().unwrap_or_default().into(),
        messages,
        model: request["model"].as_str().unwrap_or_default().into(),
        max_output_tokens: request["max_output_tokens"]
            .as_u64()
            .and_then(|value| value.try_into().ok()),
    })
}

fn contract_wire(value: &Value) -> AppResult<WireResponse> {
    let headers = value["headers"]
        .as_object()
        .ok_or_else(|| AppError::internal("官方 Codec 契约 headers 无效"))?
        .iter()
        .map(|(name, value)| {
            Ok(super::types::ResponseHeader {
                name: name.clone(),
                value: value
                    .as_str()
                    .ok_or_else(|| AppError::internal("官方 Codec 契约 Header 值无效"))?
                    .into(),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(WireResponse {
        status: value["status"].as_u64().unwrap_or_default() as u16,
        headers,
        body: serde_json::to_vec(&value["body"])
            .map_err(|error| AppError::internal(format!("官方 Codec 契约序列化失败: {error}")))?,
    })
}

fn finish_name(reason: &FinishReason) -> &str {
    match reason {
        FinishReason::Completed => "completed",
        FinishReason::OutputLimit => "output_limit",
        FinishReason::ContentFiltered => "content_filtered",
        FinishReason::Unknown(_) => "unknown",
    }
}

fn error_kind_name(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::Authentication => "authentication",
        ProviderErrorKind::Permission => "permission",
        ProviderErrorKind::RateLimited => "rate_limited",
        ProviderErrorKind::QuotaExhausted => "quota_exhausted",
        ProviderErrorKind::InvalidRequest => "invalid_request",
        ProviderErrorKind::ModelNotFound => "model_not_found",
        ProviderErrorKind::ContextLimit => "context_limit",
        ProviderErrorKind::ContentRejected => "content_rejected",
        ProviderErrorKind::ServiceUnavailable => "service_unavailable",
        ProviderErrorKind::ProviderInternal => "provider_internal",
        ProviderErrorKind::Unknown => "unknown",
    }
}

fn template_part(part: &HeaderPartSpec) -> HeaderPart {
    match part {
        HeaderPartSpec::Literal { value } => HeaderPart::Literal(value.clone()),
        HeaderPartSpec::SecretSlot { name } => HeaderPart::SecretSlot(name.clone()),
    }
}

fn evaluate(
    expr: &Expr,
    request: &CompletionRequest,
    message: Option<&super::super::LlmMessage>,
) -> AppResult<PlannedJson> {
    Ok(match expr {
        Expr::Literal { value } => PlannedJson::from_value(value.clone()),
        Expr::Input { field, default } => input_value(*field, request)
            .or_else(|| default.clone())
            .map(PlannedJson::from_value)
            .unwrap_or(PlannedJson::Null),
        Expr::Message { field, mappings } => {
            let message =
                message.ok_or_else(|| AppError::protocol("Codec message 表达式缺少消息上下文"))?;
            let value = match field {
                MessageField::Role => &message.role,
                MessageField::Content => &message.content,
            };
            PlannedJson::String(mappings.get(value).unwrap_or(value).clone())
        }
        Expr::Messages {
            include_system,
            item,
        } => {
            let mut values =
                Vec::with_capacity(request.messages.len() + usize::from(*include_system));
            if *include_system {
                let system = super::super::LlmMessage::new("system", &request.system);
                values.push(evaluate(item, request, Some(&system))?);
            }
            for message in &request.messages {
                values.push(evaluate(item, request, Some(message))?);
            }
            PlannedJson::Array(values)
        }
        Expr::SecretSlot { name } => PlannedJson::SecretSlot(name.clone()),
        Expr::Object { fields } => {
            let mut object = BTreeMap::new();
            for (name, value) in fields {
                object.insert(name.clone(), evaluate(value, request, message)?);
            }
            PlannedJson::Object(object)
        }
        Expr::Array { items } => PlannedJson::Array(
            items
                .iter()
                .map(|item| evaluate(item, request, message))
                .collect::<AppResult<Vec<_>>>()?,
        ),
    })
}

fn input_value(field: InputField, request: &CompletionRequest) -> Option<Value> {
    match field {
        InputField::System => Some(Value::String(request.system.clone())),
        InputField::Model => Some(Value::String(request.model.clone())),
        InputField::MaxOutputTokens => request
            .max_output_tokens
            .map(|value| Value::Number(Number::from(value))),
    }
}

fn first_scalar(body: &Value, paths: &[String]) -> Option<String> {
    paths.iter().find_map(|path| scalar(body.pointer(path)?))
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn rule_matches(rule: &super::super::plugin::ErrorRule, status: u16, marker: &str) -> bool {
    let mut conditions = Vec::with_capacity(3);
    if !rule.statuses.is_empty() {
        conditions.push(rule.statuses.contains(&status));
    }
    if rule.status_min.is_some() || rule.status_max.is_some() {
        conditions.push(
            rule.status_min.is_none_or(|minimum| status >= minimum)
                && rule.status_max.is_none_or(|maximum| status <= maximum),
        );
    }
    if !rule.contains_any.is_empty() {
        conditions.push(
            rule.contains_any
                .iter()
                .any(|value| marker.contains(&value.to_ascii_lowercase())),
        );
    }
    if !rule.contains_all.is_empty() {
        conditions.push(
            rule.contains_all
                .iter()
                .all(|value| marker.contains(&value.to_ascii_lowercase())),
        );
    }
    match rule.mode {
        RuleMode::Any => conditions.into_iter().any(|matched| matched),
        RuleMode::All => conditions.into_iter().all(|matched| matched),
    }
}

fn error_kind(kind: ErrorKindSpec) -> ProviderErrorKind {
    match kind {
        ErrorKindSpec::Authentication => ProviderErrorKind::Authentication,
        ErrorKindSpec::Permission => ProviderErrorKind::Permission,
        ErrorKindSpec::RateLimited => ProviderErrorKind::RateLimited,
        ErrorKindSpec::QuotaExhausted => ProviderErrorKind::QuotaExhausted,
        ErrorKindSpec::InvalidRequest => ProviderErrorKind::InvalidRequest,
        ErrorKindSpec::ModelNotFound => ProviderErrorKind::ModelNotFound,
        ErrorKindSpec::ContextLimit => ProviderErrorKind::ContextLimit,
        ErrorKindSpec::ContentRejected => ProviderErrorKind::ContentRejected,
        ErrorKindSpec::ServiceUnavailable => ProviderErrorKind::ServiceUnavailable,
        ErrorKindSpec::ProviderInternal => ProviderErrorKind::ProviderInternal,
        ErrorKindSpec::Unknown => ProviderErrorKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::plugin::{PluginCatalog, TestCodecPackage};
    use super::super::super::LlmMessage;
    use super::super::types::ResponseHeader;
    use super::*;
    use std::sync::Arc;

    const OPENAI_RESPONSES_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/llm-codecs/openai-responses.golden.json"
    ));
    const ANTHROPIC_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/llm-codecs/anthropic-messages.golden.json"
    ));
    const OPENAI_STRUCTURED_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/llm-codecs/openai-responses-structured.golden.json"
    ));
    const ANTHROPIC_STRUCTURED_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/llm-codecs/anthropic-messages-structured.golden.json"
    ));

    fn plugin(format: &str) -> VerifiedPlugin {
        let catalog = PluginCatalog::load("").unwrap();
        catalog.resolve(format).unwrap()
    }

    fn request(value: &Value) -> CompletionRequest {
        CompletionRequest {
            system: value["system"].as_str().unwrap().into(),
            messages: value["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| {
                    LlmMessage::new(
                        message["role"].as_str().unwrap(),
                        message["content"].as_str().unwrap(),
                    )
                })
                .collect(),
            model: value["model"].as_str().unwrap().into(),
            max_output_tokens: value["max_output_tokens"]
                .as_u64()
                .map(|value| value as u32),
        }
    }

    fn wire(value: &Value) -> WireResponse {
        WireResponse {
            status: value["status"].as_u64().unwrap() as u16,
            headers: value["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| ResponseHeader {
                    name: name.clone(),
                    value: value.as_str().unwrap().into(),
                })
                .collect(),
            body: serde_json::to_vec(&value["body"]).unwrap(),
        }
    }

    fn finish_name(reason: &FinishReason) -> &str {
        match reason {
            FinishReason::Completed => "completed",
            FinishReason::OutputLimit => "output_limit",
            FinishReason::ContentFiltered => "content_filtered",
            FinishReason::Unknown(_) => "unknown",
        }
    }

    fn error_kind_name(kind: ProviderErrorKind) -> &'static str {
        match kind {
            ProviderErrorKind::Authentication => "authentication",
            ProviderErrorKind::Permission => "permission",
            ProviderErrorKind::RateLimited => "rate_limited",
            ProviderErrorKind::QuotaExhausted => "quota_exhausted",
            ProviderErrorKind::InvalidRequest => "invalid_request",
            ProviderErrorKind::ModelNotFound => "model_not_found",
            ProviderErrorKind::ContextLimit => "context_limit",
            ProviderErrorKind::ContentRejected => "content_rejected",
            ProviderErrorKind::ServiceUnavailable => "service_unavailable",
            ProviderErrorKind::ProviderInternal => "provider_internal",
            ProviderErrorKind::Unknown => "unknown",
        }
    }

    fn assert_golden(plugin_format: &str, profile: EncodeProfile, fixture: &[u8]) {
        let fixture: Value = serde_json::from_slice(fixture).unwrap();
        let evaluator = CodecEvaluator;
        let plugin = plugin(plugin_format);
        let plan = evaluator
            .encode(&plugin, profile, request(&fixture["request"]))
            .unwrap();
        assert_eq!(
            plan.relative_path,
            fixture["expected_request"]["relative_path"]
                .as_str()
                .unwrap()
        );
        let body = plan.body.materialize("fixture-secret");
        assert_eq!(body, fixture["expected_request"]["json_body"]);
        assert!(plan.headers.iter().any(|header| {
            header
                .parts
                .iter()
                .any(|part| matches!(part, HeaderPart::SecretSlot(_)))
        }));

        let decoded = evaluator
            .decode(&plugin, wire(&fixture["success_response"]))
            .unwrap();
        assert_eq!(decoded.text, fixture["expected_success"]["text"]);
        assert_eq!(
            finish_name(&decoded.finish_reason),
            fixture["expected_success"]["finish_reason"]
        );

        let error = evaluator
            .decode_error(&plugin, wire(&fixture["error_response"]))
            .unwrap();
        assert_eq!(
            error_kind_name(error.kind),
            fixture["expected_error"]["kind"]
        );
        assert_eq!(
            error.retry_after_ms,
            fixture["expected_error"]["retry_after_ms"].as_u64()
        );
    }

    fn third_format_plugin(id: &str) -> VerifiedPlugin {
        let payload = serde_json::json!({
            "schema_version": 3,
            "id": id,
            "version": "1.0.0",
            "publisher": "local-user",
            "http": {
                "allowed_paths": ["/generate"],
                "allowed_query_keys": [],
                "allowed_request_headers": ["content-type"],
                "visible_response_headers": []
            },
            "secrets": [],
            "limits": {
                "max_request_body": 2097152,
                "max_success_body": 2097152,
                "max_error_body": 65536
            },
            "codec": {
                "encode": {
                    "default": {
                        "path": "/generate",
                        "query": [],
                        "headers": [{
                            "name": "content-type",
                            "parts": [{"kind": "literal", "value": "application/json"}]
                        }],
                        "body": {"op": "object", "fields": {
                            "deployment": {"op": "input", "field": "model", "default": null},
                            "dialog": {"op": "messages", "include_system": false, "item": {
                                "op": "object", "fields": {
                                    "speaker": {"op": "message", "field": "role", "mappings": {
                                        "assistant": "bot"
                                    }},
                                    "segments": {"op": "array", "items": [{
                                        "op": "object", "fields": {
                                            "text": {"op": "message", "field": "content"}
                                        }
                                    }]}
                                }
                            }}
                        }}
                    }
                },
                "decode": {
                    "text": {"op": "value", "path": "/result/segments/0/text", "default": ""},
                    "finish": {
                        "path": "/result/status", "completed": ["done"],
                        "output_limit": [], "content_filtered": [], "missing": "completed"
                    },
                    "usage": null
                },
                "decode_error": {
                    "message_paths": ["/error/message"], "code_paths": [], "marker_paths": [],
                    "request_id_paths": [], "request_id_headers": [], "rules": [],
                    "retryable_kinds": [], "retry_after_header": null
                }
            }
        });
        let package = TestCodecPackage::parse(&serde_json::to_vec(&payload).unwrap()).unwrap();
        VerifiedPlugin {
            package: Arc::new(package),
            sha256: [0; 32],
            key_fingerprint: "test".repeat(16),
            official: false,
        }
    }

    #[test]
    fn openai_golden_encode_decode_and_decode_error() {
        assert_golden(
            "openai@0.3.0",
            EncodeProfile::Default,
            OPENAI_RESPONSES_FIXTURE,
        );
        assert_golden(
            "openai@0.3.0",
            EncodeProfile::JsonSchema,
            OPENAI_STRUCTURED_FIXTURE,
        );
    }

    #[test]
    fn anthropic_golden_encode_decode_and_decode_error() {
        assert_golden("anthropic@0.3.0", EncodeProfile::Default, ANTHROPIC_FIXTURE);
        assert_golden(
            "anthropic@0.3.0",
            EncodeProfile::JsonSchema,
            ANTHROPIC_STRUCTURED_FIXTURE,
        );
    }

    #[test]
    fn official_structured_codecs_use_the_same_agent_schema() {
        let openai: Value = serde_json::from_slice(OPENAI_STRUCTURED_FIXTURE).unwrap();
        let anthropic: Value = serde_json::from_slice(ANTHROPIC_STRUCTURED_FIXTURE).unwrap();
        assert_eq!(
            openai["expected_request"]["json_body"]["text"]["format"]["schema"],
            anthropic["expected_request"]["json_body"]["output_config"]["format"]["schema"]
        );
    }

    #[test]
    fn openai_responses_finish_rules_reject_invalid_states() {
        let evaluator = CodecEvaluator;
        let plugin = plugin("openai@0.3.0");
        let refusal = evaluator
            .decode(
                &plugin,
                WireResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: br#"{"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"no"}]}]}"#.to_vec(),
                },
            )
            .unwrap();
        assert_eq!(refusal.finish_reason, FinishReason::ContentFiltered);

        let limited = evaluator
            .decode(
                &plugin,
                WireResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: br#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}"#.to_vec(),
                },
            )
            .unwrap();
        assert_eq!(limited.finish_reason, FinishReason::OutputLimit);

        let filtered = evaluator
            .decode(
                &plugin,
                WireResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: br#"{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[]}"#.to_vec(),
                },
            )
            .unwrap();
        assert_eq!(filtered.finish_reason, FinishReason::ContentFiltered);

        for body in [
            br#"{"status":"failed","output":[]}"#.as_slice(),
            br#"{"status":"completed","output":[]}"#.as_slice(),
        ] {
            assert!(evaluator
                .decode(
                    &plugin,
                    WireResponse {
                        status: 200,
                        headers: Vec::new(),
                        body: body.to_vec(),
                    },
                )
                .is_err());
        }
    }

    #[test]
    fn third_format_maps_nested_messages_without_core_provider_branch() {
        let evaluator = CodecEvaluator;
        let request = CompletionRequest {
            system: "system".into(),
            messages: vec![
                LlmMessage::new("user", "hello"),
                LlmMessage::new("assistant", "world"),
            ],
            model: "custom-model".into(),
            max_output_tokens: Some(512),
        };
        let first = evaluator
            .encode(
                &third_format_plugin("format-a"),
                EncodeProfile::Default,
                request.clone(),
            )
            .unwrap();
        let second = evaluator
            .encode(
                &third_format_plugin("format-b"),
                EncodeProfile::Default,
                request,
            )
            .unwrap();
        let first_body = first.body.materialize("");
        let second_body = second.body.materialize("");
        assert_eq!(first_body, second_body);
        assert_eq!(
            first_body,
            serde_json::json!({
                "deployment": "custom-model",
                "dialog": [
                    {"speaker": "user", "segments": [{"text": "hello"}]},
                    {"speaker": "bot", "segments": [{"text": "world"}]}
                ]
            })
        );
        let decoded = evaluator
            .decode(
                &third_format_plugin("format-a"),
                WireResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: br#"{"result":{"segments":[{"text":"ok"}],"status":"done"}}"#.to_vec(),
                },
            )
            .unwrap();
        assert_eq!(decoded.text, "ok");
        assert_eq!(decoded.finish_reason, FinishReason::Completed);
    }

    #[test]
    fn body_secret_slot_preserves_an_empty_configured_value() {
        let request = CompletionRequest {
            system: String::new(),
            messages: Vec::new(),
            model: "model".into(),
            max_output_tokens: None,
        };
        let value = evaluate(
            &Expr::SecretSlot {
                name: "api-key".into(),
            },
            &request,
            None,
        )
        .unwrap();

        let mut slots = Vec::new();
        value.collect_secret_slots(&mut slots);
        assert_eq!(value.clone().materialize(""), Value::String(String::new()));
        assert_eq!(slots, ["api-key"]);
    }
}
