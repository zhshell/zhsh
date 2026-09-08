//! Core 独占的 HTTP 传输、状态分流和有界响应读取。

use super::super::codec::{
    contains_secret, validate_decoded_text, validate_plan, visible_headers, CodecEvaluator,
    WireResponse,
};
use super::super::plugin::VerifiedPlugin;
use super::super::{CompletionRequest, LlmConfig, LlmResponse, ProviderError};
use super::secret::{inject, AuthorizedHttpRequest};
use crate::common::{AppError, AppResult};

pub(crate) async fn call(
    client: &reqwest::Client,
    evaluator: &CodecEvaluator,
    plugin: &VerifiedPlugin,
    base_url: &str,
    config: &LlmConfig,
    request: CompletionRequest,
) -> AppResult<LlmResponse> {
    let plan = evaluator.encode(plugin, config.json_schema.effective_profile(), request)?;
    let plan = validate_plan(plugin, plan)?;
    let request = inject(base_url, plan, &config.access_token)?;
    let response = send(client, &request).await?;
    let status = response.status().as_u16();
    let core_retry_after_ms = parse_retry_after_ms(&response);
    let headers = visible_headers(&response, &plugin.package.http);
    let limit = if response.status().is_success() {
        plugin.package.limits.max_success_body
    } else {
        plugin.package.limits.max_error_body
    };
    let body = read_bounded(response, limit).await?;
    let wire = WireResponse {
        status,
        headers,
        body,
    };
    if contains_secret(&wire, &request.secret_values()) {
        return Err(AppError::protocol(
            "供应商响应直接回显了密钥，已拒绝交给 Codec",
        ));
    }
    if (200..300).contains(&status) {
        let decoded = evaluator.decode(plugin, wire)?;
        validate_decoded_text(&decoded.text)?;
        return Ok(LlmResponse {
            text: decoded.text,
            finish_reason: decoded.finish_reason,
            usage: decoded.usage,
        });
    }

    let decoded_error = evaluator.decode_error(plugin, wire);
    match decoded_error {
        Ok(error) => Err(provider_failure(status, core_retry_after_ms, error)),
        Err(_) => Err(AppError::http(format!(
            "LLM 服务返回 HTTP {status}，错误正文无法由 Codec 安全解析"
        ))),
    }
}

async fn send(
    client: &reqwest::Client,
    request: &AuthorizedHttpRequest,
) -> AppResult<reqwest::Response> {
    request.validate_target()?;
    let mut builder = client
        .post(request.url().clone())
        .header("user-agent", concat!("zhsh/", env!("CARGO_PKG_VERSION")))
        .body(request.body().to_vec());
    for (name, value) in request.headers() {
        builder = builder.header(name, value);
    }
    builder
        .send()
        .await
        .map_err(|error| AppError::http(format!("HTTP 传输失败: {}", error.without_url())))
}

async fn read_bounded(mut response: reqwest::Response, limit: usize) -> AppResult<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| AppError::io(format!("读取 LLM 响应失败: {error}")))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(AppError::io(format!("LLM 响应正文超过 {limit} 字节限制")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn provider_failure(
    status: u16,
    core_retry_after_ms: Option<u64>,
    mut error: ProviderError,
) -> AppError {
    error.message = sanitize(&error.message, 4096);
    error.provider_code = error
        .provider_code
        .as_deref()
        .map(|value| sanitize(value, 256))
        .filter(|value| !value.is_empty());
    error.request_id = error
        .request_id
        .as_deref()
        .map(|value| sanitize(value, 256))
        .filter(|value| !value.is_empty());
    if matches!(status, 401 | 403) {
        error.retryable_hint = false;
        error.retry_after_ms = None;
    }
    let retry_after_ms = if matches!(status, 401 | 403) {
        None
    } else {
        match (core_retry_after_ms, error.retry_after_ms) {
            (Some(core), Some(plugin)) => Some(core.max(plugin)),
            (core, plugin) => core.or(plugin),
        }
    };
    let mut detail = format!("LLM 服务返回 HTTP {status}: {}", error.message);
    if let Some(code) = error.provider_code {
        detail.push_str(&format!(" [code={code}]"));
    }
    if let Some(request_id) = error.request_id {
        detail.push_str(&format!(" [request-id={request_id}]"));
    }
    if let Some(delay) = retry_after_ms {
        detail.push_str(&format!(" [retry-after={delay}ms]"));
    }
    AppError::http(detail)
}

fn parse_retry_after_ms(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1000)
}

fn sanitize(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == ' ')
        .take(limit)
        .collect()
}
