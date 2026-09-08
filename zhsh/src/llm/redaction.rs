//! Agent transcript 在离开本机前使用的已知 LLM 密钥精确脱敏。

use super::{store, CodecRuntime, LlmConfig};
use std::collections::BTreeSet;
use std::path::Path;

const REDACTION_MARKER: &str = "[zhsh: 已脱敏 LLM access-token]";

/// 保存任务启动时已成功验证的密钥集合，但不提供读取密钥的接口。
pub(crate) struct SecretRedactor {
    secrets: Vec<String>,
}

impl SecretRedactor {
    /// 从固定用户状态根和当前内存配置创建任务级快照。
    pub(crate) fn for_task(
        home: Option<&Path>,
        active: Option<&LlmConfig>,
        codecs: &CodecRuntime,
    ) -> Self {
        let mut secrets = BTreeSet::new();
        if let Some(config) = active {
            insert_secret(&mut secrets, &config.access_token);
        }
        if let Some(home) = home {
            if let Ok(names) = store::list_valid(home) {
                for name in names {
                    if let Ok(config) = store::load(home, &name, codecs) {
                        insert_secret(&mut secrets, &config.access_token);
                    }
                }
            }
        }
        let mut secrets: Vec<_> = secrets.into_iter().collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Self { secrets }
    }

    /// 替换输入中的所有已知明文密钥；匹配只基于原始输入，不会再次扫描替换标记。
    pub(crate) fn redact(&self, input: &str) -> String {
        if input.is_empty() || self.secrets.is_empty() {
            return input.to_owned();
        }
        let mut output = String::with_capacity(input.len());
        let mut cursor = 0;
        while cursor < input.len() {
            let next = self
                .secrets
                .iter()
                .filter_map(|secret| {
                    input[cursor..]
                        .find(secret)
                        .map(|offset| (cursor + offset, secret.len()))
                })
                .min_by(|left, right| left.0.cmp(&right.0).then(right.1.cmp(&left.1)));
            let Some((start, length)) = next else {
                output.push_str(&input[cursor..]);
                break;
            };
            output.push_str(&input[cursor..start]);
            output.push_str(REDACTION_MARKER);
            cursor = start + length;
        }
        output
    }

    #[cfg(test)]
    fn from_secrets(values: &[&str]) -> Self {
        let mut secrets = BTreeSet::new();
        for value in values {
            insert_secret(&mut secrets, value);
        }
        let mut secrets: Vec<_> = secrets.into_iter().collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Self { secrets }
    }
}

fn insert_secret(secrets: &mut BTreeSet<String>, value: &str) {
    if !value.is_empty() {
        secrets.insert(value.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(name: &str, token: &str) -> LlmConfig {
        LlmConfig {
            name: name.into(),
            url: "https://example.com".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: super::super::JsonSchemaResolution::Off,
            access_token: token.into(),
            models: super::super::ModelTiers {
                flash: "fast".into(),
                standard: "standard".into(),
                max: "max".into(),
            },
            tier: super::super::ModelTier::Flash,
        }
    }

    #[test]
    fn redacts_secrets_at_every_position_and_multiple_values() {
        let redactor = SecretRedactor::from_secrets(&["first-secret", "second-secret"]);
        let output =
            redactor.redact("first-secret at start, second-secret in middle, and first-secret");
        assert_eq!(output.matches(REDACTION_MARKER).count(), 3);
        assert!(!output.contains("first-secret"));
        assert!(!output.contains("second-secret"));
    }

    #[test]
    fn redacts_a_value_even_when_capture_chunks_split_it() {
        let redactor = SecretRedactor::from_secrets(&["chunk-boundary-token"]);
        let captured = ["before chunk-boundary-", "token after"].concat();
        let output = redactor.redact(&captured);
        assert_eq!(output, format!("before {REDACTION_MARKER} after"));
    }

    #[test]
    fn chooses_the_longest_secret_at_the_same_position_without_rescanning_marker() {
        let redactor = SecretRedactor::from_secrets(&["token", "token-long", "zhsh"]);
        assert_eq!(
            redactor.redact("token-long"),
            REDACTION_MARKER,
            "短前缀不能留下长密钥的后缀，替换标记也不能被再次改写"
        );
    }

    #[test]
    fn task_snapshot_collects_every_valid_persisted_configuration() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!(
            "zhsh-redactor-configs-{}-{unique}",
            std::process::id()
        ));
        super::super::install_test_openai_codec(&home);
        let codecs = CodecRuntime::load(Some(&home));
        store::save(&home, &config("first", "first-config-token"), &codecs).unwrap();
        store::save(&home, &config("second", "second-config-token"), &codecs).unwrap();
        std::fs::write(home.join(".zhsh/llm/invalid name.llm"), "ignored").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                home.join(".zhsh/llm/invalid name.llm"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }

        let redactor = SecretRedactor::for_task(Some(&home), None, &codecs);
        let output = redactor.redact("first-config-token + second-config-token");

        assert_eq!(output.matches(REDACTION_MARKER).count(), 2);
        assert!(!output.contains("first-config-token"));
        assert!(!output.contains("second-config-token"));
        let _ = std::fs::remove_dir_all(home);
    }
}
