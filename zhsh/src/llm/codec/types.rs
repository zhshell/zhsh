//! 声明式 Codec 与 Core 策略边界之间的非可信数据类型。

use super::super::{FinishReason, TokenUsage};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HeaderPart {
    Literal(String),
    SecretSlot(String),
}

#[derive(Debug, Clone)]
pub(crate) struct HeaderTemplate {
    pub(crate) name: String,
    pub(crate) parts: Vec<HeaderPart>,
}

#[derive(Debug, Clone)]
pub(crate) struct QueryParam {
    pub(crate) name: String,
    pub(crate) parts: Vec<HeaderPart>,
}

#[derive(Debug, Clone)]
pub(crate) struct RequestPlan {
    pub(crate) relative_path: String,
    pub(crate) query: Vec<QueryParam>,
    pub(crate) headers: Vec<HeaderTemplate>,
    pub(crate) body: PlannedJson,
}

/// 尚未物化运行时密钥的 JSON 请求树。
///
/// SecretSlot 使用独立变体，不能与供应商协议中的普通 JSON 字符串发生碰撞。
#[derive(Debug, Clone)]
pub(crate) enum PlannedJson {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<PlannedJson>),
    Object(BTreeMap<String, PlannedJson>),
    SecretSlot(String),
}

impl PlannedJson {
    pub(crate) fn from_value(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) => Self::Number(value),
            Value::String(value) => Self::String(value),
            Value::Array(values) => Self::Array(values.into_iter().map(Self::from_value).collect()),
            Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(name, value)| (name, Self::from_value(value)))
                    .collect(),
            ),
        }
    }

    pub(crate) fn collect_secret_slots(&self, output: &mut Vec<String>) {
        match self {
            Self::Array(values) => {
                for value in values {
                    value.collect_secret_slots(output);
                }
            }
            Self::Object(values) => {
                for value in values.values() {
                    value.collect_secret_slots(output);
                }
            }
            Self::SecretSlot(name) => output.push(name.clone()),
            Self::Null | Self::Bool(_) | Self::Number(_) | Self::String(_) => {}
        }
    }

    pub(crate) fn materialize(self, access_token: &str) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(|value| value.materialize(access_token))
                    .collect(),
            ),
            Self::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(name, value)| (name, value.materialize(access_token)))
                    .collect(),
            ),
            Self::SecretSlot(_) => Value::String(access_token.trim().to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResponseHeader {
    pub(crate) name: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone)]
pub(crate) struct WireResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<ResponseHeader>,
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedResponse {
    pub(crate) text: String,
    pub(crate) finish_reason: FinishReason,
    pub(crate) usage: Option<TokenUsage>,
}
