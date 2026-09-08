//! 在声明式计划通过校验后把密钥槽物化为授权 HTTP 请求。

use super::super::codec::{HeaderPart, ValidatedRequestPlan};
use super::super::plugin::valid_relative_path;
use crate::common::{AppError, AppResult};
use url::Url;

pub(crate) struct AuthorizedHttpRequest {
    url: Url,
    expected_scheme: String,
    expected_host: String,
    expected_port: Option<u16>,
    expected_path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    secret_values: Vec<Vec<u8>>,
}

impl AuthorizedHttpRequest {
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    pub(crate) fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    pub(crate) fn secret_values(&self) -> Vec<&[u8]> {
        self.secret_values.iter().map(Vec::as_slice).collect()
    }

    pub(crate) fn validate_target(&self) -> AppResult<()> {
        if self.url.scheme() != self.expected_scheme
            || self.url.host_str() != Some(self.expected_host.as_str())
            || self.url.port_or_known_default() != self.expected_port
            || self.url.path() != self.expected_path
        {
            return Err(AppError::protocol("授权 HTTP 请求脱离了 Provider 请求域"));
        }
        Ok(())
    }
}

pub(crate) fn inject(
    base_url: &str,
    plan: ValidatedRequestPlan,
    access_token: &str,
) -> AppResult<AuthorizedHttpRequest> {
    if access_token.contains(['\r', '\n', '\0']) {
        return Err(AppError::input("access-token 包含非法控制字符"));
    }
    if !valid_relative_path(&plan.relative_path) {
        return Err(AppError::protocol(
            "Codec endpoint 不是安全的单斜杠相对路径",
        ));
    }
    let base =
        Url::parse(base_url).map_err(|error| AppError::input(format!("Base URL 无效: {error}")))?;
    let endpoint = format!(
        "{}{}",
        base.as_str().trim_end_matches('/'),
        plan.relative_path
    );
    let mut url = Url::parse(&endpoint)
        .map_err(|error| AppError::protocol(format!("无法拼接 Codec 请求路径: {error}")))?;
    let expected_path = format!(
        "{}{}",
        base.path().trim_end_matches('/'),
        plan.relative_path
    );
    if url.scheme() != base.scheme()
        || url.host() != base.host()
        || url.port_or_known_default() != base.port_or_known_default()
        || url.path() != expected_path
    {
        return Err(AppError::protocol(
            "Codec 请求路径脱离了 Base URL 的 Provider 请求域",
        ));
    }
    let mut secret_values = Vec::new();
    let token = access_token.trim();
    if !token.is_empty() {
        secret_values.push(token.as_bytes().to_vec());
    }
    if !plan.query.is_empty() {
        let mut query = url.query_pairs_mut();
        for parameter in plan.query {
            let (value, contains_secret) = materialize(&parameter.parts, access_token)?;
            if contains_secret {
                secret_values.push(value.as_bytes().to_vec());
            }
            query.append_pair(&parameter.name, &value);
        }
    }
    let mut headers = Vec::with_capacity(plan.headers.len());
    for header in plan.headers {
        let (value, contains_secret) = materialize(&header.parts, access_token)?;
        if value.contains(['\r', '\n', '\0']) {
            return Err(AppError::protocol("授权 Header 包含控制字符"));
        }
        if contains_secret {
            secret_values.push(value.as_bytes().to_vec());
        }
        headers.push((header.name, value));
    }
    let body = serde_json::to_vec(&plan.body.materialize(access_token))
        .map_err(|error| AppError::protocol(format!("Codec 请求序列化失败: {error}")))?;
    if body.len() > plan.max_request_body {
        return Err(AppError::protocol("Codec 请求体超过允许的大小"));
    }
    let request = AuthorizedHttpRequest {
        expected_scheme: base.scheme().to_string(),
        expected_host: base
            .host_str()
            .ok_or_else(|| AppError::input("Base URL 缺少主机名"))?
            .to_string(),
        expected_port: base.port_or_known_default(),
        expected_path,
        url,
        headers,
        body,
        secret_values,
    };
    request.validate_target()?;
    Ok(request)
}

fn materialize(parts: &[HeaderPart], access_token: &str) -> AppResult<(String, bool)> {
    let token = access_token.trim();
    let mut value = String::new();
    let mut contains_secret = false;
    for part in parts {
        match part {
            HeaderPart::Literal(literal) => value.push_str(literal),
            HeaderPart::SecretSlot(_) => {
                contains_secret |= !token.is_empty();
                value.push_str(token);
            }
        }
    }
    Ok((value, contains_secret))
}

#[cfg(test)]
mod tests {
    use super::super::super::codec::{
        HeaderPart, ValidatedHeader, ValidatedQuery, ValidatedRequestPlan,
    };
    use super::*;

    fn plan(path: &str) -> ValidatedRequestPlan {
        ValidatedRequestPlan {
            relative_path: path.into(),
            query: vec![ValidatedQuery {
                name: "beta".into(),
                parts: vec![HeaderPart::Literal("true".into())],
            }],
            headers: vec![ValidatedHeader {
                name: "authorization".into(),
                parts: vec![
                    HeaderPart::Literal("Bearer ".into()),
                    HeaderPart::SecretSlot("api-key".into()),
                ],
            }],
            body: super::super::super::codec::PlannedJson::Object(std::collections::BTreeMap::new()),
            max_request_body: 1024,
        }
    }

    #[test]
    fn base_path_and_anthropic_path_are_appended_before_secret_injection() {
        let request = inject(
            "https://api.example.com:8443/anthropic",
            plan("/v1/messages"),
            "secret",
        )
        .unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://api.example.com:8443/anthropic/v1/messages?beta=true"
        );
        assert_eq!(
            request.headers(),
            &[("authorization".into(), "Bearer secret".into())]
        );
    }

    #[test]
    fn repeated_slash_at_the_base_endpoint_boundary_is_removed_only_from_base() {
        let request = inject(
            "https://api.example.com/gateway///",
            plan("/v1/messages"),
            "secret",
        )
        .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://api.example.com/gateway/v1/messages?beta=true"
        );
        assert!(inject(
            "https://api.example.com/gateway",
            plan("//attacker.example/messages"),
            "secret"
        )
        .is_err());
    }

    #[test]
    fn base_path_and_openai_path_are_appended_the_same_way() {
        let request = inject(
            "https://gateway.example.com/provider/openai/",
            plan("/v1/chat/completions"),
            "secret",
        )
        .unwrap();

        assert_eq!(
            request.url().as_str(),
            "https://gateway.example.com/provider/openai/v1/chat/completions?beta=true"
        );
    }

    #[test]
    fn empty_secret_slot_keeps_the_request_structure_without_redaction_values() {
        let request = inject("http://127.0.0.1:11434", plan("/v1/chat/completions"), "").unwrap();

        assert_eq!(
            request.headers(),
            &[("authorization".into(), "Bearer ".into())]
        );
        assert!(request.secret_values().is_empty());
    }
}
