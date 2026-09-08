//! 对非可信 RequestPlan 和 Codec 输出实施 Core 强制策略。

use super::super::plugin::{
    valid_header_name, valid_relative_path, HttpPolicy, SecretSpec, VerifiedPlugin,
    MAX_REQUEST_BODY,
};
use super::types::{HeaderPart, PlannedJson, RequestPlan, WireResponse};
use crate::common::{AppError, AppResult};
use std::collections::{HashMap, HashSet};

const MAX_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_HEADER_VALUE: usize = 8 * 1024;
const MAX_QUERY: usize = 32;
const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_OUTPUT_TEXT: usize = 2 * 1024 * 1024;
const PROTECTED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "proxy-authorization",
    "cookie",
    "user-agent",
];

#[derive(Debug)]
pub(crate) struct ValidatedRequestPlan {
    pub(crate) relative_path: String,
    pub(crate) query: Vec<ValidatedQuery>,
    pub(crate) headers: Vec<ValidatedHeader>,
    pub(crate) body: PlannedJson,
    pub(crate) max_request_body: usize,
}

#[derive(Debug)]
pub(crate) struct ValidatedQuery {
    pub(crate) name: String,
    pub(crate) parts: Vec<HeaderPart>,
}

#[derive(Debug)]
pub(crate) struct ValidatedHeader {
    pub(crate) name: String,
    pub(crate) parts: Vec<HeaderPart>,
}

pub(crate) fn validate_plan(
    plugin: &VerifiedPlugin,
    plan: RequestPlan,
) -> AppResult<ValidatedRequestPlan> {
    let package = &plugin.package;
    if !valid_relative_path(&plan.relative_path)
        || !package
            .http
            .allowed_paths
            .iter()
            .any(|allowed| allowed == &plan.relative_path)
    {
        return Err(AppError::protocol("Codec 返回了未授权的请求路径"));
    }
    if plan.query.len() > MAX_QUERY {
        return Err(AppError::protocol("Codec 返回的查询参数过多"));
    }
    let mut query_bytes = 0usize;
    let mut query = Vec::with_capacity(plan.query.len());
    let secret_by_slot: HashMap<_, _> = package
        .secrets
        .iter()
        .map(|secret| (secret.slot.as_str(), secret))
        .collect();
    let mut body_secret_slots = Vec::new();
    plan.body.collect_secret_slots(&mut body_secret_slots);
    let mut seen_slots: HashSet<String> = body_secret_slots.into_iter().collect();
    for parameter in plan.query {
        query_bytes = query_bytes
            .saturating_add(parameter.name.len())
            .saturating_add(template_literal_bytes(&parameter.parts));
        if query_bytes > MAX_QUERY_BYTES
            || !valid_template_parts(&parameter.parts, &secret_by_slot, &mut seen_slots)
            || !package
                .http
                .allowed_query_keys
                .iter()
                .any(|allowed| allowed == &parameter.name)
        {
            return Err(AppError::protocol("Codec 返回了非法查询参数"));
        }
        query.push(ValidatedQuery {
            name: parameter.name,
            parts: parameter.parts,
        });
    }
    if plan.headers.len() > MAX_HEADERS {
        return Err(AppError::protocol("Codec 返回的 Header 过多"));
    }
    let mut seen_headers = HashSet::new();
    let mut header_bytes = 0usize;
    let mut headers = Vec::with_capacity(plan.headers.len());
    for header in plan.headers {
        let name = header.name.to_ascii_lowercase();
        if !valid_header_name(&name)
            || !seen_headers.insert(name.clone())
            || PROTECTED_HEADERS.contains(&name.as_str())
            || !package
                .http
                .allowed_request_headers
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&name))
        {
            return Err(AppError::protocol("Codec 返回了未授权的 Header"));
        }
        let value_bytes = template_literal_bytes(&header.parts);
        if !valid_template_parts(&header.parts, &secret_by_slot, &mut seen_slots) {
            return Err(AppError::protocol("Codec Header 值或密钥槽不合法"));
        }
        if value_bytes > MAX_HEADER_VALUE {
            return Err(AppError::protocol("Codec Header 值超过限制"));
        }
        header_bytes = header_bytes
            .saturating_add(name.len())
            .saturating_add(value_bytes);
        headers.push(ValidatedHeader {
            name,
            parts: header.parts,
        });
    }
    if header_bytes > MAX_HEADER_BYTES
        || package
            .secrets
            .iter()
            .any(|secret| secret.required && !seen_slots.contains(&secret.slot))
    {
        return Err(AppError::protocol("Codec Header 总量超限或缺少必需密钥槽"));
    }
    let body_without_secrets = serde_json::to_vec(&plan.body.clone().materialize(""))
        .map_err(|error| AppError::protocol(format!("Codec 请求序列化失败: {error}")))?;
    if body_without_secrets.len() > package.limits.max_request_body
        || body_without_secrets.len() > MAX_REQUEST_BODY
    {
        return Err(AppError::protocol("Codec 请求体不是允许大小的完整 JSON"));
    }
    Ok(ValidatedRequestPlan {
        relative_path: plan.relative_path,
        query,
        headers,
        body: plan.body,
        max_request_body: package.limits.max_request_body.min(MAX_REQUEST_BODY),
    })
}

fn valid_template_parts(
    parts: &[HeaderPart],
    secret_by_slot: &HashMap<&str, &SecretSpec>,
    seen_slots: &mut HashSet<String>,
) -> bool {
    !parts.is_empty()
        && parts.iter().all(|part| match part {
            HeaderPart::Literal(value) => !value.contains(['\r', '\n', '\0']),
            HeaderPart::SecretSlot(slot) => {
                let known = secret_by_slot.contains_key(slot.as_str());
                if known {
                    seen_slots.insert(slot.clone());
                }
                known
            }
        })
}

fn template_literal_bytes(parts: &[HeaderPart]) -> usize {
    parts
        .iter()
        .filter_map(|part| match part {
            HeaderPart::Literal(value) => Some(value.len()),
            HeaderPart::SecretSlot(_) => None,
        })
        .sum()
}

pub(crate) fn validate_decoded_text(text: &str) -> AppResult<()> {
    if text.len() > MAX_OUTPUT_TEXT || text.chars().any(|ch| ch == '\0') {
        return Err(AppError::protocol("Codec 完成文本超过限制或包含 NUL"));
    }
    Ok(())
}

pub(crate) fn visible_headers(
    response: &reqwest::Response,
    policy: &HttpPolicy,
) -> Vec<super::types::ResponseHeader> {
    response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            policy
                .visible_response_headers
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&name))
                .then(|| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| super::types::ResponseHeader {
                            name,
                            value: value
                                .chars()
                                .filter(|ch| !ch.is_control())
                                .take(8192)
                                .collect(),
                        })
                })
                .flatten()
        })
        .collect()
}

pub(crate) fn contains_secret(response: &WireResponse, secrets: &[&[u8]]) -> bool {
    secrets.iter().any(|secret| {
        !secret.is_empty()
            && (response
                .body
                .windows(secret.len())
                .any(|window| window == *secret)
                || response.headers.iter().any(|header| {
                    header
                        .value
                        .as_bytes()
                        .windows(secret.len())
                        .any(|window| window == *secret)
                }))
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::plugin::PluginCatalog;
    use super::super::types::{HeaderTemplate, QueryParam, ResponseHeader};
    use super::*;

    fn fixture() -> (VerifiedPlugin, RequestPlan) {
        let catalog = PluginCatalog::load("").unwrap();
        let summary = catalog.summaries().into_iter().next().unwrap();
        let plugin = catalog.resolve(&summary.label()).unwrap();
        let secret = &plugin.package.secrets[0];
        let plan = RequestPlan {
            relative_path: plugin.package.http.allowed_paths[0].clone(),
            query: Vec::<QueryParam>::new(),
            headers: vec![HeaderTemplate {
                name: plugin.package.codec.encode.default.headers[1].name.clone(),
                parts: vec![HeaderPart::SecretSlot(secret.slot.clone())],
            }],
            body: PlannedJson::Object(std::collections::BTreeMap::new()),
        };
        (plugin, plan)
    }

    #[test]
    fn rejects_cross_origin_paths_unknown_slots_and_invalid_json() {
        let (plugin, plan) = fixture();
        assert!(validate_plan(&plugin, plan.clone()).is_ok());

        let mut absolute = plan.clone();
        absolute.relative_path = "//attacker.example/collect".into();
        assert!(validate_plan(&plugin, absolute).is_err());

        let mut unknown_slot = plan.clone();
        unknown_slot.headers[0].parts = vec![HeaderPart::SecretSlot("unknown".into())];
        assert!(validate_plan(&plugin, unknown_slot).is_err());

        let mut oversized = plan;
        oversized.body = PlannedJson::String("x".repeat(MAX_REQUEST_BODY + 1));
        assert!(validate_plan(&plugin, oversized).is_err());
    }

    #[test]
    fn detects_direct_secret_echo_in_body_or_visible_headers() {
        let response = WireResponse {
            status: 200,
            headers: vec![ResponseHeader {
                name: "request-id".into(),
                value: "safe".into(),
            }],
            body: b"contains exact-secret here".to_vec(),
        };
        assert!(contains_secret(&response, &[b"exact-secret"]));
        assert!(!contains_secret(&response, &[b"different-secret"]));
    }
}
