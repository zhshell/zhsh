use ring::signature::{UnparsedPublicKey, ED25519};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;

const MAGIC: &[u8; 8] = b"ZHCODEC\0";
const HEADER_BYTES: usize = 46;
const SIGNATURE_BYTES: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfficialLock {
    schema_version: u32,
    artifacts: Vec<LockedArtifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LockedArtifact {
    format: String,
    file: String,
    sha256: String,
    key_fingerprint: String,
    source_crate: String,
    rustc: String,
    sdk: String,
    payload_schema: u32,
    upstream_protocol: String,
}

fn main() {
    if let Err(error) = generate_official_catalog() {
        panic!("official Codec lock validation failed: {error}");
    }
}

fn generate_official_catalog() -> Result<(), String> {
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|error| error.to_string())?);
    let directory = manifest.join("assets/llm-codecs");
    // Directory changes must invalidate a warm build as well: an added unlisted artifact is a
    // strict release failure, not a file Cargo may ignore until the next clean build.
    println!("cargo:rerun-if-changed={}", directory.display());
    let lock_path = directory.join("official-codecs.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());

    let lock: OfficialLock = serde_json::from_slice(
        &fs::read(&lock_path).map_err(|error| format!("{}: {error}", lock_path.display()))?,
    )
    .map_err(|error| format!("{}: {error}", lock_path.display()))?;
    if lock.schema_version != 1 {
        return Err(format!("unsupported lock schema {}", lock.schema_version));
    }
    if lock.artifacts.is_empty() {
        return Err("official lock has no artifacts".to_string());
    }

    let official_key = fs::read(manifest.join("assets/official-codec.pub"))
        .map_err(|error| format!("unable to read official public key: {error}"))?;
    let official_fingerprint = hex_digest(&official_key);
    let mut formats = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut generated = String::from("static OFFICIAL_CODEC_LOCKS: &[LockedOfficialCodec] = &[\n");

    for entry in &lock.artifacts {
        validate_entry_metadata(entry)?;
        if !formats.insert(entry.format.clone()) {
            return Err(format!("duplicate FORMAT {}", entry.format));
        }
        if !files.insert(entry.file.clone()) {
            return Err(format!("duplicate artifact file {}", entry.file));
        }
        if entry.key_fingerprint != official_fingerprint {
            return Err(format!(
                "{} signer does not match assets/official-codec.pub",
                entry.format
            ));
        }
        let path = directory.join(&entry.file);
        println!("cargo:rerun-if-changed={}", path.display());
        let artifact = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        validate_artifact(entry, &artifact)?;
        let digest = parse_hex_digest(&entry.sha256)?;
        generated.push_str(&format!(
            "    LockedOfficialCodec {{ format: {:?}, artifact_len: {}, sha256: {:?} }},\n",
            entry.format,
            artifact.len(),
            digest,
        ));
    }
    generated.push_str("];\n");

    let discovered = fs::read_dir(&directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("zhcodec"))
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect::<BTreeSet<_>>();
    if files != discovered {
        let unlisted = discovered.difference(&files).cloned().collect::<Vec<_>>();
        let missing = files.difference(&discovered).cloned().collect::<Vec<_>>();
        return Err(format!(
            "lock/directory mismatch; unlisted={unlisted:?}, missing={missing:?}"
        ));
    }

    let output = PathBuf::from(env::var("OUT_DIR").map_err(|error| error.to_string())?)
        .join("official_codecs.rs");
    fs::write(&output, generated).map_err(|error| format!("{}: {error}", output.display()))
}

fn validate_entry_metadata(entry: &LockedArtifact) -> Result<(), String> {
    let (id, version) = entry
        .format
        .split_once('@')
        .ok_or_else(|| format!("invalid FORMAT {}", entry.format))?;
    let parsed = semver::Version::parse(version)
        .map_err(|_| format!("{} does not contain canonical SemVer", entry.format))?;
    if parsed.to_string() != version || entry.file != format!("{}.zhcodec", entry.format) {
        return Err(format!(
            "{} has inconsistent version or filename",
            entry.format
        ));
    }
    if entry.source_crate != format!("zhcodec-{id}") {
        return Err(format!("{} has inconsistent source crate", entry.format));
    }
    if entry.sdk != "zhcodec-sdk@0.3.0" || entry.payload_schema != 3 {
        return Err(format!(
            "{} must use zhcodec-sdk@0.3.0 and payload schema 3",
            entry.format
        ));
    }
    for (name, value) in [
        ("rustc", &entry.rustc),
        ("sdk", &entry.sdk),
        ("upstream_protocol", &entry.upstream_protocol),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{} has empty {name}", entry.format));
        }
    }
    if entry.sha256.len() != 64 || entry.key_fingerprint.len() != 64 {
        return Err(format!("{} contains a malformed digest", entry.format));
    }
    Ok(())
}

fn validate_artifact(entry: &LockedArtifact, artifact: &[u8]) -> Result<(), String> {
    if hex_digest(artifact) != entry.sha256 {
        return Err(format!("{} SHA-256 differs from the lock", entry.format));
    }
    if artifact.len() < HEADER_BYTES + SIGNATURE_BYTES || artifact.get(..8) != Some(MAGIC) {
        return Err(format!("{} is not a zhcodec container", entry.format));
    }
    if hex_digest(&artifact[14..HEADER_BYTES]) != entry.key_fingerprint {
        return Err(format!(
            "{} embedded signer differs from the lock",
            entry.format
        ));
    }
    let payload_len =
        u32::from_be_bytes([artifact[10], artifact[11], artifact[12], artifact[13]]) as usize;
    let payload_end = HEADER_BYTES
        .checked_add(payload_len)
        .ok_or_else(|| format!("{} payload length overflow", entry.format))?;
    if payload_end + SIGNATURE_BYTES != artifact.len() {
        return Err(format!("{} container length is inconsistent", entry.format));
    }
    UnparsedPublicKey::new(&ED25519, &artifact[14..HEADER_BYTES])
        .verify(&artifact[..payload_end], &artifact[payload_end..])
        .map_err(|_| format!("{} Ed25519 signature is invalid", entry.format))?;
    let payload: Value = serde_json::from_slice(&artifact[HEADER_BYTES..payload_end])
        .map_err(|error| format!("{} payload: {error}", entry.format))?;
    let payload_format = format!(
        "{}@{}",
        payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        payload
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    if payload_format != entry.format {
        return Err(format!(
            "{} payload declares {payload_format}",
            entry.format
        ));
    }
    if payload.get("schema_version").and_then(Value::as_u64)
        != Some(u64::from(entry.payload_schema))
    {
        return Err(format!(
            "{} payload schema differs from the lock",
            entry.format
        ));
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn parse_hex_digest(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("SHA-256 digest must contain exactly 64 hexadecimal characters".to_string());
    }
    let mut digest = [0u8; 32];
    for (index, output) in digest.iter_mut().enumerate() {
        let offset = index * 2;
        *output = u8::from_str_radix(&value[offset..offset + 2], 16)
            .map_err(|_| "SHA-256 digest contains a non-hexadecimal character".to_string())?;
    }
    Ok(digest)
}
