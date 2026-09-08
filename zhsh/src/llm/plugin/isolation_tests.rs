use super::*;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zhsh-codec-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        secure(&path, 0o700);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn secure(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    secure(path, 0o600);
}

fn signing_key() -> Ed25519KeyPair {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap()
}

fn signed_user_codec(id: &str, key: &Ed25519KeyPair) -> Vec<u8> {
    let official = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/llm-codecs/openai@0.3.0.zhcodec"
    ));
    let envelope = container::verify(official, |_, _| true).unwrap();
    let mut payload: serde_json::Value = serde_json::from_slice(envelope.payload).unwrap();
    payload["id"] = id.into();
    payload["version"] = "1.0.0".into();
    let payload = serde_json::to_vec(&payload).unwrap();
    let public_key: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
    let mut artifact = Vec::new();
    artifact.extend_from_slice(container::MAGIC);
    artifact.extend_from_slice(&container::CONTAINER_VERSION.to_be_bytes());
    artifact.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    artifact.extend_from_slice(&public_key);
    artifact.extend_from_slice(&payload);
    artifact.extend_from_slice(key.sign(&artifact).as_ref());
    artifact
}

fn root(path: &Path, scope: PluginScope) -> PluginRoot<'_> {
    PluginRoot {
        path,
        expected_owner: current_uid(),
        scope,
        strict_permissions: true,
    }
}

#[test]
fn runtime_isolates_bad_keys_and_artifacts_in_deterministic_order() {
    let directory = TestDir::new("isolation");
    let trusted_keys = directory.path().join("trusted-keys");
    fs::create_dir(&trusted_keys).unwrap();
    secure(&trusted_keys, 0o700);

    let key = signing_key();
    let fingerprint = container::key_fingerprint(key.public_key().as_ref());
    write_file(
        &trusted_keys.join(format!("{fingerprint}.pub")),
        key.public_key().as_ref(),
    );
    write_file(&trusted_keys.join("a-invalid.pub"), b"short");
    write_file(
        &directory.path().join("valid.codec@1.0.0.zhcodec"),
        &signed_user_codec("valid.codec", &key),
    );
    write_file(
        &directory.path().join("a-broken@1.0.0.zhcodec"),
        b"not a codec",
    );
    write_file(
        &directory.path().join("wrong-name@1.0.0.zhcodec"),
        &signed_user_codec("actual-name", &key),
    );

    let report = load_from_roots(None, Some(root(directory.path(), PluginScope::User)));
    assert!(report.catalog.resolve("valid.codec@1.0.0").is_ok());
    assert_eq!(report.catalog.summaries().len(), 1);
    assert_eq!(
        report
            .issues
            .iter()
            .map(|issue| issue.kind)
            .collect::<Vec<_>>(),
        vec![
            PluginLoadIssueKind::ArtifactInvalid,
            PluginLoadIssueKind::KeyInvalid,
            PluginLoadIssueKind::FilenameMismatch,
        ]
    );
    let rendered = report
        .issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let repeated = load_from_roots(None, Some(root(directory.path(), PluginScope::User)))
        .issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(rendered, repeated);
    assert!(report.into_strict().is_err());
}

#[test]
fn artifact_limit_disables_the_whole_root_before_accepting_any_prefix() {
    let directory = TestDir::new("artifact-limit");
    for index in 0..=MAX_CODEC_FILES_PER_ROOT {
        write_file(
            &directory
                .path()
                .join(format!("codec-{index}@1.0.0.zhcodec")),
            b"invalid",
        );
    }

    let report = load_from_roots(None, Some(root(directory.path(), PluginScope::User)));
    assert!(report.catalog.summaries().is_empty());
    assert_eq!(report.issues.len(), 1);
    assert_eq!(
        report.issues[0].kind,
        PluginLoadIssueKind::RootLimitExceeded
    );
}

#[test]
fn key_limit_disables_the_whole_root_instead_of_using_the_first_keys() {
    let directory = TestDir::new("key-limit");
    let trusted_keys = directory.path().join("trusted-keys");
    fs::create_dir(&trusted_keys).unwrap();
    secure(&trusted_keys, 0o700);
    for index in 0..=MAX_TRUSTED_KEYS_PER_ROOT {
        write_file(&trusted_keys.join(format!("key-{index}.pub")), &[0; 32]);
    }

    let report = load_from_roots(None, Some(root(directory.path(), PluginScope::User)));
    assert!(report.catalog.summaries().is_empty());
    assert_eq!(report.issues.len(), 1);
    assert_eq!(
        report.issues[0].kind,
        PluginLoadIssueKind::RootLimitExceeded
    );
}

#[test]
fn identical_official_user_copy_keeps_the_system_codec_and_reports_it() {
    let system = TestDir::new("system-copy");
    let user = TestDir::new("user-copy");
    let artifact = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/llm-codecs/openai@0.3.0.zhcodec"
    ));
    write_file(&system.path().join("openai@0.3.0.zhcodec"), artifact);
    write_file(&user.path().join("openai@0.3.0.zhcodec"), artifact);

    let report = load_from_roots(
        Some(root(system.path(), PluginScope::System)),
        Some(root(user.path(), PluginScope::User)),
    );
    assert!(report.catalog.resolve("openai@0.3.0").is_ok());
    assert_eq!(report.catalog.summaries().len(), 1);
    assert_eq!(report.issues.len(), 1);
    assert_eq!(
        report.issues[0].kind,
        PluginLoadIssueKind::DuplicateOfficialCopy
    );
}

#[test]
fn broken_system_identity_cannot_fall_back_to_the_user_copy() {
    let system = TestDir::new("system-broken");
    let user = TestDir::new("user-fallback");
    write_file(&system.path().join("openai@0.3.0.zhcodec"), b"broken");
    write_file(
        &user.path().join("openai@0.3.0.zhcodec"),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        )),
    );

    let report = load_from_roots(
        Some(root(system.path(), PluginScope::System)),
        Some(root(user.path(), PluginScope::User)),
    );
    assert!(report.catalog.resolve("openai@0.3.0").is_err());
    assert_eq!(
        report
            .issues
            .iter()
            .map(|issue| (issue.scope, issue.kind))
            .collect::<Vec<_>>(),
        vec![
            (PluginScope::System, PluginLoadIssueKind::ArtifactInvalid),
            (PluginScope::User, PluginLoadIssueKind::ScopeConflict),
        ]
    );
}

#[cfg(unix)]
#[test]
fn unsafe_user_root_does_not_remove_valid_system_codecs() {
    let system = TestDir::new("safe-system");
    let user = TestDir::new("unsafe-user");
    write_file(
        &system.path().join("openai@0.3.0.zhcodec"),
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        )),
    );
    secure(user.path(), 0o777);

    let report = load_from_roots(
        Some(root(system.path(), PluginScope::System)),
        Some(root(user.path(), PluginScope::User)),
    );
    assert!(report.catalog.resolve("openai@0.3.0").is_ok());
    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.issues[0].scope, PluginScope::User);
    assert_eq!(report.issues[0].kind, PluginLoadIssueKind::RootUnsafe);
}

#[test]
fn issue_paths_escape_terminal_control_characters() {
    let directory = TestDir::new("terminal-path");
    write_file(
        &directory.path().join("bad\u{1b}[31m@1.0.0.zhcodec"),
        b"broken",
    );

    let report = load_from_roots(None, Some(root(directory.path(), PluginScope::User)));
    assert_eq!(report.issues.len(), 1);
    let rendered = report.issues[0].path.to_string_lossy();
    assert!(!rendered.contains('\u{1b}'));
    assert!(rendered.contains("\\x1b"));
}
