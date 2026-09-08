//! 单文件 `.zhcodec` 包的严格 schema 与领域校验。

use super::codec_spec::{CodecSpec, Expr, HeaderPartSpec};
use crate::common::{AppError, AppResult};
use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

pub(crate) const MAX_PACKAGE_BYTES: usize = 384 * 1024;
const MAX_CODEC_BYTES: usize = 256 * 1024;
pub(crate) const MAX_REQUEST_BODY: usize = 2 * 1024 * 1024;
pub(crate) const MAX_SUCCESS_BODY: usize = 2 * 1024 * 1024;
pub(crate) const MAX_ERROR_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodecPackage {
    pub schema_version: u32,
    pub id: String,
    pub version: Version,
    pub publisher: String,
    pub http: HttpPolicy,
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,
    pub limits: Limits,
    pub codec: CodecSpec,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpPolicy {
    pub allowed_paths: Vec<String>,
    pub allowed_query_keys: Vec<String>,
    pub allowed_request_headers: Vec<String>,
    pub visible_response_headers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretSpec {
    pub slot: String,
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub max_request_body: usize,
    pub max_success_body: usize,
    pub max_error_body: usize,
}

impl CodecPackage {
    pub(crate) fn parse(input: &[u8]) -> AppResult<Self> {
        if input.len() > MAX_PACKAGE_BYTES {
            return Err(AppError::protocol("LLM Codec 包超过 384 KiB"));
        }
        let value: Value = serde_json::from_slice(input)
            .map_err(|error| AppError::protocol(format!("LLM Codec 包不是有效 JSON: {error}")))?;
        let codec_size = value
            .get("codec")
            .and_then(|codec| serde_json::to_vec(codec).ok())
            .map_or(usize::MAX, |bytes| bytes.len());
        if codec_size > MAX_CODEC_BYTES {
            return Err(AppError::protocol("LLM Codec 声明超过 256 KiB"));
        }
        let package: Self = serde_json::from_value(value)
            .map_err(|error| AppError::protocol(format!("LLM Codec 包 schema 无效: {error}")))?;
        package.validate()?;
        Ok(package)
    }

    fn validate(&self) -> AppResult<()> {
        if self.schema_version != 3 {
            return Err(AppError::protocol("LLM Codec schema 版本不兼容"));
        }
        if !valid_id(&self.id)
            || self.publisher.is_empty()
            || self.publisher.len() > 128
            || self.publisher.chars().any(char::is_control)
        {
            return Err(AppError::protocol("LLM 插件 ID 或 publisher 不合法"));
        }
        self.codec.validate()?;
        let expected_paths: BTreeSet<_> = self
            .codec
            .encode
            .iter()
            .map(|encode| encode.path.as_str())
            .collect();
        let allowed_paths: BTreeSet<_> =
            self.http.allowed_paths.iter().map(String::as_str).collect();
        if expected_paths != allowed_paths || allowed_paths.len() != self.http.allowed_paths.len() {
            return Err(AppError::protocol("LLM 插件路径策略与编码声明不一致"));
        }
        for headers in [
            &self.http.allowed_request_headers,
            &self.http.visible_response_headers,
        ] {
            if headers.iter().any(|name| !valid_header_name(name)) {
                return Err(AppError::protocol("LLM 插件 Header 允许表不合法"));
            }
        }
        let expected_query: HashSet<_> = self
            .codec
            .encode
            .iter()
            .flat_map(|encode| encode.query.iter())
            .map(|query| query.name.as_str())
            .collect();
        let allowed_query: HashSet<_> = self
            .http
            .allowed_query_keys
            .iter()
            .map(String::as_str)
            .collect();
        let expected_headers: HashSet<_> = self
            .codec
            .encode
            .iter()
            .flat_map(|encode| encode.headers.iter())
            .map(|header| header.name.to_ascii_lowercase())
            .collect();
        let allowed_headers: HashSet<_> = self
            .http
            .allowed_request_headers
            .iter()
            .map(|header| header.to_ascii_lowercase())
            .collect();
        if expected_query != allowed_query
            || expected_headers != allowed_headers
            || allowed_query.len() != self.http.allowed_query_keys.len()
            || allowed_headers.len() != self.http.allowed_request_headers.len()
        {
            return Err(AppError::protocol("LLM 插件 HTTP 允许表与编码声明不一致"));
        }
        let mut declared_slots = HashSet::new();
        for secret in &self.secrets {
            if !declared_slots.insert(secret.slot.as_str()) || !valid_option_name(&secret.slot) {
                return Err(AppError::protocol("LLM 插件密钥槽声明不合法"));
            }
        }
        let mut used_slots = HashSet::new();
        for encode in self.codec.encode.iter() {
            collect_expr_secrets(&encode.body, &mut used_slots);
            for part in encode
                .headers
                .iter()
                .flat_map(|header| &header.parts)
                .chain(encode.query.iter().flat_map(|query| &query.parts))
            {
                if let HeaderPartSpec::SecretSlot { name } = part {
                    used_slots.insert(name.as_str());
                }
            }
        }
        if used_slots != declared_slots {
            return Err(AppError::protocol("LLM 插件密钥槽声明与实际引用不一致"));
        }
        if self.limits.max_request_body == 0
            || self.limits.max_request_body > MAX_REQUEST_BODY
            || self.limits.max_success_body == 0
            || self.limits.max_success_body > MAX_SUCCESS_BODY
            || self.limits.max_error_body == 0
            || self.limits.max_error_body > MAX_ERROR_BODY
        {
            return Err(AppError::protocol("LLM 插件资源限制超过 Core 上限"));
        }
        Ok(())
    }

    pub(crate) fn format(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }
}

fn collect_expr_secrets<'a>(expr: &'a Expr, output: &mut HashSet<&'a str>) {
    match expr {
        Expr::SecretSlot { name } => {
            output.insert(name);
        }
        Expr::Messages { item, .. } => collect_expr_secrets(item, output),
        Expr::Object { fields } => {
            for child in fields.values() {
                collect_expr_secrets(child, output);
            }
        }
        Expr::Array { items } => {
            for child in items {
                collect_expr_secrets(child, output);
            }
        }
        _ => {}
    }
}

pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

pub(crate) fn valid_option_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn valid_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

pub(crate) fn valid_relative_path(path: &str) -> bool {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['\\', '\0', '?', '#'])
        || path.chars().any(char::is_control)
    {
        return false;
    }
    path.split('/').skip(1).all(|segment| {
        let lower = segment.to_ascii_lowercase();
        !matches!(segment, "." | "..")
            && !["%2e", "%2f", "%5c", "%00", "%25"]
                .iter()
                .any(|escape| lower.contains(escape))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_v3_and_rejects_unpublished_v2_payloads() {
        let artifact = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        ));
        let envelope = super::super::container::verify(artifact, |_, _| true).unwrap();
        let mut payload: Value = serde_json::from_slice(envelope.payload).unwrap();
        assert!(CodecPackage::parse(envelope.payload).is_ok());
        payload["schema_version"] = Value::from(2);
        assert!(CodecPackage::parse(&serde_json::to_vec(&payload).unwrap()).is_err());
    }
}
