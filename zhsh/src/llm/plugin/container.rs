//! `.zhcodec` 二进制信封和 Ed25519 完整性验证。

use crate::common::{AppError, AppResult};
use ring::signature::{UnparsedPublicKey, ED25519};
use sha2::{Digest, Sha256};

pub(crate) const MAGIC: &[u8; 8] = b"ZHCODEC\0";
pub(crate) const CONTAINER_VERSION: u16 = 1;
pub(crate) const PUBLIC_KEY_BYTES: usize = 32;
pub(crate) const SIGNATURE_BYTES: usize = 64;
pub(crate) const HEADER_BYTES: usize = MAGIC.len() + 2 + 4 + PUBLIC_KEY_BYTES;
pub(crate) const MAX_ARTIFACT_BYTES: usize = 385 * 1024;

pub(crate) struct VerifiedEnvelope<'a> {
    pub(crate) payload: &'a [u8],
    pub(crate) public_key: [u8; PUBLIC_KEY_BYTES],
    pub(crate) key_fingerprint: String,
    pub(crate) artifact_sha256: [u8; 32],
}

pub(crate) fn verify<'a>(
    artifact: &'a [u8],
    trusted_key: impl FnOnce(&[u8; PUBLIC_KEY_BYTES], &str) -> bool,
) -> AppResult<VerifiedEnvelope<'a>> {
    let envelope = verify_signature(artifact)?;
    if !trusted_key(&envelope.public_key, &envelope.key_fingerprint) {
        return Err(AppError::protocol(format!(
            "Codec 发布密钥 {} 未受信任",
            &envelope.key_fingerprint[..12]
        )));
    }
    Ok(envelope)
}

/// 验证信封结构和签名，但不据此推断发布者已经受信任。
pub(crate) fn verify_signature(artifact: &[u8]) -> AppResult<VerifiedEnvelope<'_>> {
    if artifact.len() > MAX_ARTIFACT_BYTES
        || artifact.len() < HEADER_BYTES + SIGNATURE_BYTES
        || artifact.get(..MAGIC.len()) != Some(MAGIC)
    {
        return Err(AppError::protocol("文件不是有效的 .zhcodec 信封"));
    }
    let version = u16::from_be_bytes([artifact[8], artifact[9]]);
    if version != CONTAINER_VERSION {
        return Err(AppError::protocol("不支持的 .zhcodec 容器版本"));
    }
    let payload_len =
        u32::from_be_bytes([artifact[10], artifact[11], artifact[12], artifact[13]]) as usize;
    let expected = HEADER_BYTES
        .checked_add(payload_len)
        .and_then(|length| length.checked_add(SIGNATURE_BYTES))
        .ok_or_else(|| AppError::protocol(".zhcodec 长度溢出"))?;
    if expected != artifact.len() {
        return Err(AppError::protocol(".zhcodec 长度字段与文件不一致"));
    }
    let mut public_key = [0u8; PUBLIC_KEY_BYTES];
    public_key.copy_from_slice(&artifact[14..HEADER_BYTES]);
    let key_fingerprint = key_fingerprint(&public_key);
    let signed_end = HEADER_BYTES + payload_len;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&artifact[..signed_end], &artifact[signed_end..])
        .map_err(|_| AppError::protocol(".zhcodec Ed25519 签名无效，文件可能已被篡改"))?;
    Ok(VerifiedEnvelope {
        payload: &artifact[HEADER_BYTES..signed_end],
        public_key,
        key_fingerprint,
        artifact_sha256: sha256(artifact),
    })
}

pub(super) fn hex_sha256(digest: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(64);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(super) fn key_fingerprint(bytes: &[u8]) -> String {
    hex_sha256(&sha256(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_PACKAGE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/llm-codecs/openai@0.3.0.zhcodec"
    ));

    #[test]
    fn rejects_plain_json_and_truncated_envelopes() {
        assert!(verify(b"{}", |_, _| true).is_err());
        let mut truncated = Vec::from(*MAGIC);
        truncated.resize(HEADER_BYTES + SIGNATURE_BYTES - 1, 0);
        assert!(verify(&truncated, |_, _| true).is_err());
    }

    #[test]
    fn any_signed_byte_change_is_rejected() {
        assert!(verify(OPENAI_PACKAGE, |_, _| true).is_ok());
        let mut tampered = OPENAI_PACKAGE.to_vec();
        tampered[HEADER_BYTES] ^= 1;
        assert!(verify(&tampered, |_, _| true).is_err());
    }
}
