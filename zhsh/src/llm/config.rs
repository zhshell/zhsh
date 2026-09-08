//! LLM 配置名、Base URL、FORMAT 和模型名的领域校验。

use url::{Host, Url};

const HTTP_TARGET_ERROR: &str =
    "HTTP 仅允许 localhost、回环地址或明确的私有 IP；公网、域名和链路本地地址必须使用 HTTPS";

/// Base URL 对应的传输安全类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportSecurity {
    Https,
    LoopbackHttp,
    PrivateHttp,
}

impl TransportSecurity {
    pub(crate) const fn status_label(self) -> &'static str {
        match self {
            Self::Https => "HTTPS",
            Self::LoopbackHttp => "HTTP（本机）",
            Self::PrivateHttp => "HTTP（私网明文）",
        }
    }

    pub(crate) const fn requires_plaintext_warning(self) -> bool {
        matches!(self, Self::PrivateHttp)
    }
}

/// 完成语法、主机和传输范围校验的规范 Base URL。
pub(crate) struct ValidatedBaseUrl {
    normalized: String,
    transport: TransportSecurity,
}

impl ValidatedBaseUrl {
    pub(crate) fn normalized(&self) -> &str {
        &self.normalized
    }

    pub(crate) fn into_normalized(self) -> String {
        self.normalized
    }

    pub(crate) fn transport(&self) -> TransportSecurity {
        self.transport
    }
}

pub(crate) fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name.contains("..")
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_whitespace)
        || name.chars().any(char::is_control)
    {
        return Err("配置名不能为空，且不能包含空白、控制字符、路径分隔符或 ..".into());
    }
    Ok(())
}

/// 校验 Base URL，并返回规范字符串及其确定性的传输分类。
pub(crate) fn parse_base_url(value: &str) -> Result<ValidatedBaseUrl, String> {
    let parsed = Url::parse(value.trim()).map_err(|_| "请输入完整有效的 Base URL".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Base URL 仅支持 http 或 https".into());
    }
    let host = parsed.host().ok_or("Base URL 必须包含主机名")?;
    validate_host(host.clone())?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("Base URL 不能包含凭据、query 或 fragment".into());
    }
    let transport = classify_transport(parsed.scheme(), host)?;
    Ok(ValidatedBaseUrl {
        normalized: parsed.as_str().trim_end_matches('/').to_string(),
        transport,
    })
}

/// 校验并返回不带末尾 `/` 的规范 Base URL。
pub(crate) fn normalize_base_url(value: &str) -> Result<String, String> {
    parse_base_url(value).map(ValidatedBaseUrl::into_normalized)
}

#[cfg(test)]
pub(crate) fn validate_base_url(value: &str) -> Result<(), String> {
    parse_base_url(value).map(|_| ())
}

pub(crate) fn transport_status(value: &str) -> Result<&'static str, String> {
    parse_base_url(value).map(|base| base.transport().status_label())
}

pub(crate) fn plaintext_private_warning(value: &str, access_token: &str) -> Option<String> {
    let transport = parse_base_url(value).ok()?.transport();
    if !transport.requires_plaintext_warning() {
        return None;
    }
    let mut warning = "! 私网 HTTP 为明文传输，提示词和命令输出可能被读取或篡改".to_string();
    if !access_token.trim().is_empty() {
        warning.push_str("；access-token 也将明文发送");
    }
    Some(warning)
}

fn classify_transport(scheme: &str, host: Host<&str>) -> Result<TransportSecurity, String> {
    if scheme == "https" {
        return Ok(TransportSecurity::Https);
    }
    match host {
        Host::Domain(domain) => classify_http_domain(domain),
        Host::Ipv4(address) => classify_http_v4(address),
        Host::Ipv6(address) => classify_http_v6(address),
    }
    .ok_or_else(|| HTTP_TARGET_ERROR.to_string())
}

fn classify_http_domain(domain: &str) -> Option<TransportSecurity> {
    domain
        .trim_end_matches('.')
        .eq_ignore_ascii_case("localhost")
        .then_some(TransportSecurity::LoopbackHttp)
}

fn classify_http_v4(address: std::net::Ipv4Addr) -> Option<TransportSecurity> {
    if address.is_loopback() {
        return Some(TransportSecurity::LoopbackHttp);
    }
    let octets = address.octets();
    let private = octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168);
    private.then_some(TransportSecurity::PrivateHttp)
}

fn classify_http_v6(address: std::net::Ipv6Addr) -> Option<TransportSecurity> {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return classify_http_v4(mapped);
    }
    if address.is_loopback() {
        Some(TransportSecurity::LoopbackHttp)
    } else if address.segments()[0] & 0xfe00 == 0xfc00 {
        Some(TransportSecurity::PrivateHttp)
    } else {
        None
    }
}

fn validate_host(host: Host<&str>) -> Result<(), String> {
    let Host::Domain(domain) = host else {
        return Ok(());
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if domain == "localhost" {
        return Ok(());
    }
    if !domain.contains('.') {
        return Err("域名必须至少包含两个 DNS 标签；本机服务请使用 localhost 或 IP 地址".into());
    }
    let labels: Vec<_> = domain.split('.').collect();
    if labels.iter().any(|label| {
        label.is_empty()
            || label.len() > 63
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Err("域名包含无效的 DNS 标签".into());
    }
    Ok(())
}

pub(crate) fn validate_model(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err("模型名不能为空".into())
    } else if value.contains(['\n', '\r', '\0']) {
        Err("模型名包含不支持的控制字符".into())
    } else {
        Ok(())
    }
}

pub(crate) fn validate_format(value: &str) -> Result<(), String> {
    let (id, version) = value
        .rsplit_once('@')
        .ok_or_else(|| "FORMAT 必须采用 id@exact-semver".to_string())?;
    if !super::plugin::valid_id(id) {
        return Err("FORMAT 中的 Codec ID 不合法".into());
    }
    let parsed =
        semver::Version::parse(version).map_err(|_| "FORMAT 版本必须是确切 SemVer".to_string())?;
    if parsed.to_string() != version {
        return Err("FORMAT 版本必须使用规范 SemVer".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_path_prefixed_base_urls_and_classifies_allowed_transports() {
        for valid in [
            "https://api.example.com",
            "https://api.example.com/anthropic",
            "https://api.example.com/gateway/openai/",
            "https://api.deepseek.com/anthropic",
            "http://localhost:11434",
            "http://127.0.0.1:11434",
            "http://[::1]:11434",
            "http://10.0.0.1:11434",
            "http://172.16.0.1:11434",
            "http://172.31.255.254:11434",
            "http://192.168.10.123:11434",
            "http://[fd00::1]:11434",
        ] {
            assert!(validate_base_url(valid).is_ok(), "rejected {valid:?}");
        }
        for invalid in [
            "http://api.example.com",
            "http://qwen.lan",
            "http://8.8.8.8",
            "http://172.15.255.255",
            "http://172.32.0.0",
            "http://100.64.0.1",
            "http://169.254.169.254",
            "http://0.0.0.0",
            "http://[::]",
            "http://[fe80::1]",
            "https://user:pass@example.com",
            "https://example.com?x=1",
            "https://example.com/path#section",
        ] {
            assert!(validate_base_url(invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(
            normalize_base_url("https://api.example.com/anthropic/").unwrap(),
            "https://api.example.com/anthropic"
        );
        assert_eq!(
            normalize_base_url("https://api.example.com/anthropic///").unwrap(),
            "https://api.example.com/anthropic"
        );
        assert_eq!(
            parse_base_url("https://api.example.com")
                .unwrap()
                .transport(),
            TransportSecurity::Https
        );
        assert_eq!(
            parse_base_url("http://localhost:11434")
                .unwrap()
                .transport(),
            TransportSecurity::LoopbackHttp
        );
        assert_eq!(
            parse_base_url("http://192.168.10.123:11434")
                .unwrap()
                .transport(),
            TransportSecurity::PrivateHttp
        );
    }

    #[test]
    fn private_http_warning_mentions_a_token_only_when_present() {
        assert_eq!(
            plaintext_private_warning("http://192.168.1.2:11434", "").unwrap(),
            "! 私网 HTTP 为明文传输，提示词和命令输出可能被读取或篡改"
        );
        assert!(
            plaintext_private_warning("http://192.168.1.2:11434", "secret")
                .unwrap()
                .ends_with("；access-token 也将明文发送")
        );
        assert!(plaintext_private_warning("http://localhost:11434", "secret").is_none());
    }

    #[test]
    fn format_is_an_exact_versioned_codec_id() {
        for valid in [
            "openai@0.3.0",
            "anthropic@0.3.0",
            "deepseek.junglelk.github.io@1.2.3",
            "vendor@1.0.0",
            "Internal_Format@2.0.0",
        ] {
            assert!(validate_format(valid).is_ok(), "rejected {valid}");
        }
        for invalid in ["openai", "openai@latest", "bad/name@1.0.0"] {
            assert!(validate_format(invalid).is_err(), "accepted {invalid}");
        }
    }
}
