//! `~/.zhsh` 下保持稳定字段契约的 LLM 配置和活动标记持久化。

use super::config::{parse_base_url, validate_format, validate_model, validate_name};
use super::{
    CodecRuntime, JsonSchemaMode, LlmConfig, LlmProfile, LlmProfileDraft, LlmProfileIssue,
    LlmProfileReadiness, ModelTier, ModelTiers,
};
use crate::common::{AppError, AppResult};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn validate_home(home: &Path) -> AppResult<()> {
    if home.is_absolute() {
        Ok(())
    } else {
        Err(AppError::input("HOME 必须是绝对路径"))
    }
}

pub(crate) fn directory(home: &Path) -> PathBuf {
    home.join(".zhsh/llm")
}

pub(crate) fn active_path(home: &Path) -> PathBuf {
    home.join(".zhsh/active-llm")
}

fn configuration_path(home: &Path, name: &str) -> AppResult<PathBuf> {
    validate_name(name).map_err(AppError::input)?;
    let base = directory(home);
    let path = base.join(format!("{name}.llm"));
    if path.parent() != Some(base.as_path()) {
        return Err(AppError::input("配置路径超出 ~/.zhsh/llm"));
    }
    Ok(path)
}

fn verify_existing_containment(
    home: &Path,
    allowed_directory: &Path,
    path: &Path,
) -> AppResult<()> {
    validate_home(home)?;
    let home = std::fs::canonicalize(home)
        .map_err(|error| AppError::io(format!("无法解析 HOME: {error}")))?;
    let root = std::fs::canonicalize(home.join(".zhsh"))
        .map_err(|error| AppError::io(format!("无法解析 ~/.zhsh: {error}")))?;
    let base = std::fs::canonicalize(allowed_directory)
        .map_err(|error| AppError::io(format!("无法解析配置目录: {error}")))?;
    if !root.starts_with(&home) || !base.starts_with(&root) {
        return Err(AppError::input(
            "LLM 配置目录不能通过符号链接指向 ~/.zhsh 之外",
        ));
    }
    if path.exists() {
        let resolved = std::fs::canonicalize(path)
            .map_err(|error| AppError::io(format!("无法解析 LLM 配置文件: {error}")))?;
        if !resolved.starts_with(&base) {
            return Err(AppError::input(
                "LLM 配置文件不能通过符号链接指向配置目录之外",
            ));
        }
    }
    Ok(())
}

fn verify_private_directory(path: &Path) -> AppResult<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查 {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::input(format!(
            "{} 必须是非符号链接目录",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(AppError::input(format!(
                "{} 所有者或权限不安全；要求当前用户和 0700",
                path.display()
            )));
        }
    }
    Ok(())
}

fn verify_private_file(path: &Path) -> AppResult<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查 {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::input(format!(
            "{} 必须是非符号链接普通文件",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o600 {
            return Err(AppError::input(format!(
                "{} 所有者或权限不安全；要求当前用户和 0600",
                path.display()
            )));
        }
    }
    Ok(())
}

fn verify_config_root(home: &Path) -> AppResult<()> {
    verify_private_directory(&home.join(".zhsh"))?;
    verify_private_directory(&directory(home))
}

pub(crate) fn list(home: &Path) -> AppResult<Vec<String>> {
    configuration_names(home, true)
}

/// 枚举可安全加载的配置名，同时隔离名称本身无效的无关目录项。
pub(super) fn list_valid(home: &Path) -> AppResult<Vec<String>> {
    configuration_names(home, false)
}

fn configuration_names(home: &Path, reject_invalid_name: bool) -> AppResult<Vec<String>> {
    validate_home(home)?;
    let directory = directory(home);
    match std::fs::symlink_metadata(&directory) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(AppError::io(format!("无法检查 LLM 配置目录: {error}"))),
    }
    verify_config_root(home)?;
    verify_existing_containment(home, &directory, &directory)?;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => return Err(AppError::io(format!("无法读取 LLM 配置目录: {error}"))),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| AppError::io(error.to_string()))?;
        if entry
            .file_type()
            .map(|kind| !kind.is_file())
            .unwrap_or(true)
        {
            continue;
        }
        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".llm"))
            .map(str::to_string)
        else {
            continue;
        };
        if let Err(error) = validate_name(&name) {
            if reject_invalid_name {
                return Err(AppError::protocol(format!(
                    "无效配置文件 {name}.llm: {error}"
                )));
            }
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

pub(crate) fn load(home: &Path, name: &str, codecs: &CodecRuntime) -> AppResult<LlmConfig> {
    let profile = load_profile(home, name, codecs)?;
    match profile.readiness {
        LlmProfileReadiness::Ready(config) => Ok(config),
        LlmProfileReadiness::Incomplete(issues) => Err(AppError::input(format!(
            "配置 {name} 不完整: {}",
            issue_summary(&issues)
        ))),
    }
}

pub(crate) fn load_profile(
    home: &Path,
    name: &str,
    codecs: &CodecRuntime,
) -> AppResult<LlmProfile> {
    validate_home(home)?;
    let path = configuration_path(home, name)?;
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            verify_config_root(home)?;
            verify_existing_containment(home, &directory(home), &path)?;
            verify_private_file(&path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AppError::io(format!("无法检查配置 {name}: {error}")));
        }
    }
    let content = std::fs::read_to_string(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AppError::input(format!("配置 {name} 不存在")),
        _ => AppError::io(format!("无法读取配置 {name}: {error}")),
    })?;
    parse_profile(&content, name, codecs)
}

#[cfg(test)]
fn parse(input: &str, fallback_name: &str, codecs: &CodecRuntime) -> AppResult<LlmConfig> {
    let profile = parse_profile(input, fallback_name, codecs)?;
    match profile.readiness {
        LlmProfileReadiness::Ready(config) => Ok(config),
        LlmProfileReadiness::Incomplete(issues) => Err(AppError::input(format!(
            "配置 {fallback_name} 不完整: {}",
            issue_summary(&issues)
        ))),
    }
}

fn parse_profile(input: &str, fallback_name: &str, codecs: &CodecRuntime) -> AppResult<LlmProfile> {
    const FIELDS: &[&str] = &[
        "NAME",
        "URL",
        "FORMAT",
        "JSON_SCHEMA",
        "ACCESS_TOKEN",
        "FLASH",
        "STANDARD",
        "MAX",
        "TIER",
    ];
    validate_name(fallback_name).map_err(AppError::input)?;
    let mut values = HashMap::new();
    for raw in input.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| AppError::protocol("配置行必须采用 KEY=VALUE 格式"))?;
        let key = key.trim();
        let value = value.trim();
        if !FIELDS.contains(&key) {
            return Err(AppError::protocol(format!("未知配置字段 {key}")));
        }
        if values.insert(key.to_string(), value.to_string()).is_some() {
            return Err(AppError::protocol(format!("重复配置字段 {key}")));
        }
    }
    let name = values
        .get("NAME")
        .cloned()
        .unwrap_or_else(|| fallback_name.into());
    validate_name(&name).map_err(AppError::protocol)?;
    if name != fallback_name {
        return Err(AppError::protocol(format!(
            "配置文件名 {fallback_name} 与 NAME={name} 不一致"
        )));
    }
    let draft = LlmProfileDraft {
        name,
        url: values.remove("URL").unwrap_or_default(),
        request_format: values.remove("FORMAT").unwrap_or_default(),
        json_schema: values
            .remove("JSON_SCHEMA")
            .unwrap_or_else(|| JsonSchemaMode::Off.as_str().into()),
        access_token: values.remove("ACCESS_TOKEN").unwrap_or_default(),
        flash: values.remove("FLASH").unwrap_or_default(),
        standard: values.remove("STANDARD").unwrap_or_default(),
        max: values.remove("MAX").unwrap_or_default(),
        tier: values
            .remove("TIER")
            .unwrap_or_else(|| ModelTier::Flash.as_str().into()),
    };
    Ok(assess_profile(draft, codecs))
}

pub(crate) fn assess_profile(mut draft: LlmProfileDraft, codecs: &CodecRuntime) -> LlmProfile {
    let mut issues = Vec::new();
    let issue = |field: &'static str, message: String| LlmProfileIssue { field, message };

    if let Err(error) = validate_name(&draft.name) {
        issues.push(issue("NAME", error));
    }
    let url = if draft.url.trim().is_empty() {
        issues.push(issue("URL", "缺少 URL".into()));
        None
    } else {
        match parse_base_url(draft.url.trim()) {
            Ok(url) => Some(url.into_normalized()),
            Err(error) => {
                issues.push(issue("URL", error));
                None
            }
        }
    };
    let request_format = draft.request_format.trim().to_string();
    if request_format.is_empty() {
        issues.push(issue("FORMAT", "缺少 FORMAT".into()));
    } else if let Err(error) = validate_format(&request_format) {
        issues.push(issue("FORMAT", error));
    }
    let requested_json_schema = match JsonSchemaMode::parse(draft.json_schema.trim()) {
        Some(mode) => Some(mode),
        None => {
            issues.push(issue("JSON_SCHEMA", "JSON_SCHEMA 必须是 off 或 on".into()));
            None
        }
    };
    let json_schema = if request_format.is_empty() {
        None
    } else {
        requested_json_schema.and_then(|mode| {
            match codecs.resolve_json_schema(&request_format, mode) {
                Ok(resolution) => Some(resolution),
                Err(error) => {
                    issues.push(issue("FORMAT", error.to_string()));
                    None
                }
            }
        })
    };
    for (field, value) in [
        ("FLASH", draft.flash.as_str()),
        ("STANDARD", draft.standard.as_str()),
        ("MAX", draft.max.as_str()),
    ] {
        if value.trim().is_empty() {
            issues.push(issue(field, format!("缺少 {field}")));
        } else if let Err(error) = validate_model(value.trim()) {
            issues.push(issue(field, error));
        }
    }
    let tier = match ModelTier::parse(draft.tier.trim()) {
        Some(tier) => Some(tier),
        None => {
            issues.push(issue("TIER", "TIER 必须是 flash、standard 或 max".into()));
            None
        }
    };

    let readiness = if issues.is_empty() {
        draft.name = draft.name.trim().into();
        draft.url = url.expect("URL was validated");
        draft.request_format = request_format;
        draft.json_schema = requested_json_schema
            .expect("JSON_SCHEMA was validated")
            .as_str()
            .into();
        draft.flash = draft.flash.trim().into();
        draft.standard = draft.standard.trim().into();
        draft.max = draft.max.trim().into();
        draft.tier = tier.expect("TIER was validated").as_str().into();
        LlmProfileReadiness::Ready(LlmConfig {
            name: draft.name.clone(),
            url: draft.url.clone(),
            request_format: draft.request_format.clone(),
            json_schema: json_schema.expect("Codec capability was resolved"),
            access_token: draft.access_token.clone(),
            models: ModelTiers {
                flash: draft.flash.clone(),
                standard: draft.standard.clone(),
                max: draft.max.clone(),
            },
            tier: tier.expect("TIER was validated"),
        })
    } else {
        LlmProfileReadiness::Incomplete(issues)
    };
    LlmProfile { draft, readiness }
}

fn issue_summary(issues: &[LlmProfileIssue]) -> String {
    issues
        .iter()
        .map(|issue| issue.message.as_str())
        .collect::<Vec<_>>()
        .join("；")
}

fn validate_field(field: &str, value: &str) -> AppResult<()> {
    if value.contains(['\n', '\r', '\0']) {
        Err(AppError::input(format!("{field} 包含配置文件不支持的字符")))
    } else {
        Ok(())
    }
}

fn ensure_directory(path: &Path) -> AppResult<()> {
    std::fs::create_dir_all(path).map_err(|error| AppError::io(error.to_string()))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::io(format!("无法检查 {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::input(format!(
            "{} 必须是非符号链接目录",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| AppError::io(error.to_string()))?;
    }
    verify_private_directory(path)
}

fn atomic_write(path: &Path, content: &[u8]) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::input("配置路径没有父目录"))?;
    ensure_directory(parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => verify_private_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(AppError::io(format!("无法检查配置目标: {error}"))),
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::input("配置文件名无效"))?;
    for _ in 0..100 {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(AppError::io(error.to_string())),
        };
        let result = (|| {
            file.write_all(content)
                .map_err(|error| AppError::io(error.to_string()))?;
            file.sync_all()
                .map_err(|error| AppError::io(error.to_string()))?;
            drop(file);
            std::fs::rename(&temporary, path).map_err(|error| AppError::io(error.to_string()))?;
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        return result;
    }
    Err(AppError::io("无法创建原子写入临时文件"))
}

pub(crate) fn save(home: &Path, config: &LlmConfig, codecs: &CodecRuntime) -> AppResult<()> {
    validate_home(home)?;
    validate_name(&config.name).map_err(AppError::input)?;
    validate_format(&config.request_format).map_err(AppError::input)?;
    let url = parse_base_url(&config.url)
        .map_err(AppError::input)?
        .into_normalized();
    if url != config.url {
        return Err(AppError::input(
            "URL 必须使用规范 Base URL 形式且不含末尾 /",
        ));
    }
    let current_resolution =
        codecs.resolve_json_schema(&config.request_format, config.json_schema.requested())?;
    if current_resolution != config.json_schema {
        return Err(AppError::input("Codec 能力快照与 LLM 配置解析结果不一致"));
    }
    let path = configuration_path(home, &config.name)?;
    for (field, value) in [
        ("NAME", config.name.as_str()),
        ("URL", config.url.as_str()),
        ("FORMAT", config.request_format.as_str()),
        ("JSON_SCHEMA", config.json_schema.requested().as_str()),
        ("ACCESS_TOKEN", config.access_token.as_str()),
        ("FLASH", config.models.flash.as_str()),
        ("STANDARD", config.models.standard.as_str()),
        ("MAX", config.models.max.as_str()),
    ] {
        validate_field(field, value)?;
    }
    let content = format!(
        "NAME={}\nURL={}\nFORMAT={}\nJSON_SCHEMA={}\nACCESS_TOKEN={}\nFLASH={}\nSTANDARD={}\nMAX={}\nTIER={}\n",
        config.name,
        config.url,
        config.request_format,
        config.json_schema.requested().as_str(),
        config.access_token,
        config.models.flash,
        config.models.standard,
        config.models.max,
        config.tier.as_str()
    );
    ensure_directory(&home.join(".zhsh"))?;
    ensure_directory(&directory(home))?;
    verify_existing_containment(home, &directory(home), &path)?;
    atomic_write(&path, content.as_bytes())
}

/// 原子保存一份配置草稿。该操作只验证安全的文件边界，不要求草稿已经可启动 Agent。
pub(crate) fn save_profile(home: &Path, draft: &LlmProfileDraft) -> AppResult<()> {
    validate_home(home)?;
    validate_name(&draft.name).map_err(AppError::input)?;
    let path = configuration_path(home, &draft.name)?;
    for (field, value) in [
        ("NAME", draft.name.as_str()),
        ("URL", draft.url.as_str()),
        ("FORMAT", draft.request_format.as_str()),
        ("JSON_SCHEMA", draft.json_schema.as_str()),
        ("ACCESS_TOKEN", draft.access_token.as_str()),
        ("FLASH", draft.flash.as_str()),
        ("STANDARD", draft.standard.as_str()),
        ("MAX", draft.max.as_str()),
        ("TIER", draft.tier.as_str()),
    ] {
        validate_field(field, value)?;
    }
    let content = format!(
        "NAME={}\nURL={}\nFORMAT={}\nJSON_SCHEMA={}\nACCESS_TOKEN={}\nFLASH={}\nSTANDARD={}\nMAX={}\nTIER={}\n",
        draft.name,
        draft.url,
        draft.request_format,
        draft.json_schema,
        draft.access_token,
        draft.flash,
        draft.standard,
        draft.max,
        draft.tier
    );
    ensure_directory(&home.join(".zhsh"))?;
    ensure_directory(&directory(home))?;
    verify_existing_containment(home, &directory(home), &path)?;
    atomic_write(&path, content.as_bytes())
}

pub(crate) fn set_active(home: &Path, name: &str) -> AppResult<()> {
    validate_home(home)?;
    validate_name(name).map_err(AppError::input)?;
    atomic_write(&active_path(home), format!("{name}\n").as_bytes())
}

pub(crate) fn load_active(home: &Path, codecs: &CodecRuntime) -> AppResult<Option<LlmConfig>> {
    let Some(profile) = load_active_profile(home, codecs)? else {
        return Ok(None);
    };
    match profile.readiness {
        LlmProfileReadiness::Ready(config) => Ok(Some(config)),
        LlmProfileReadiness::Incomplete(issues) => Err(AppError::input(format!(
            "配置 {} 不完整: {}",
            profile.draft.name,
            issue_summary(&issues)
        ))),
    }
}

pub(crate) fn load_active_profile(
    home: &Path,
    codecs: &CodecRuntime,
) -> AppResult<Option<LlmProfile>> {
    validate_home(home)?;
    let path = active_path(home);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::io(format!("无法检查当前 LLM 配置标记: {error}"))),
    }
    verify_private_directory(&home.join(".zhsh"))?;
    verify_existing_containment(home, &home.join(".zhsh"), &path)?;
    verify_private_file(&path)?;
    let name = std::fs::read_to_string(&path)
        .map_err(|error| AppError::io(format!("无法读取当前 LLM 配置标记: {error}")))?;
    load_profile(home, name.trim(), codecs).map(Some)
}

#[cfg(test)]
mod tests {
    use super::super::JsonSchemaResolution;
    use super::*;

    fn test_config() -> LlmConfig {
        LlmConfig {
            name: "test".into(),
            url: "https://example.com".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: super::super::JsonSchemaResolution::Off,
            access_token: "secret-token".into(),
            models: ModelTiers {
                flash: "fast".into(),
                standard: "standard".into(),
                max: "max".into(),
            },
            tier: ModelTier::Flash,
        }
    }

    #[test]
    fn stable_contract_uses_base_url_and_exact_format() {
        let codecs = CodecRuntime::load(None);
        let input = "NAME=test\nURL=https://example.com/gateway\nFORMAT=openai@0.3.0\nACCESS_TOKEN=secret\nFLASH=fast\nSTANDARD=balanced\nMAX=best\n";
        let config = parse(input, "test", &codecs).unwrap();
        assert_eq!(config.url, "https://example.com/gateway");
        assert_eq!(config.request_format, "openai@0.3.0");
        assert_eq!(config.json_schema, JsonSchemaResolution::Off);
        let enabled = parse(
            &input.replace(
                "FORMAT=openai@0.3.0\n",
                "FORMAT=openai@0.3.0\nJSON_SCHEMA=on\n",
            ),
            "test",
            &codecs,
        )
        .unwrap();
        assert_eq!(enabled.json_schema, JsonSchemaResolution::On);
        assert!(parse(
            &input.replace(
                "FORMAT=openai@0.3.0\n",
                "FORMAT=openai@0.3.0\nJSON_SCHEMA=ON\n"
            ),
            "test",
            &codecs,
        )
        .is_err());
        assert!(parse(&format!("{input}PLUGIN=openai@0.3.0\n"), "test", &codecs).is_err());
    }

    #[test]
    fn access_token_may_be_empty_or_omitted() {
        let codecs = CodecRuntime::load(None);
        let input = "NAME=test\nURL=http://192.168.10.123:11434\nFORMAT=openai@0.3.0\nACCESS_TOKEN=\nFLASH=fast\nSTANDARD=balanced\nMAX=best\nTIER=flash\n";
        let config = parse(input, "test", &codecs).unwrap();
        assert_eq!(config.url, "http://192.168.10.123:11434");
        assert!(config.access_token.is_empty());
        assert!(
            parse(&input.replace("ACCESS_TOKEN=\n", ""), "test", &codecs)
                .unwrap()
                .access_token
                .is_empty()
        );
    }

    #[test]
    fn incomplete_profiles_round_trip_without_becoming_runtime_configs() {
        let home = std::env::temp_dir().join(format!(
            "zhsh-incomplete-profile-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        super::super::install_test_openai_codec(&home);
        let codecs = CodecRuntime::load(Some(&home));
        let mut draft = LlmProfileDraft::empty("draft");
        draft.request_format = "openai@0.3.0".into();

        save_profile(&home, &draft).unwrap();
        let profile = load_profile(&home, "draft", &codecs).unwrap();

        assert!(profile.config().is_none());
        assert!(profile.issues().iter().any(|issue| issue.field == "URL"));
        assert!(load(&home, "draft", &codecs).is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn every_store_operation_rejects_a_relative_home() {
        let home = Path::new("relative-home");
        let codecs = CodecRuntime::load(None);
        assert!(list(home).is_err());
        assert!(load(home, "test", &codecs).is_err());
        assert!(save(home, &test_config(), &codecs).is_err());
        assert!(set_active(home, "test").is_err());
        assert!(load_active(home, &codecs).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn persisted_state_is_private_and_unsafe_modes_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!(
            "zhsh-store-permissions-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        super::super::install_test_openai_codec(&home);
        let codecs = CodecRuntime::load(Some(&home));

        save(&home, &test_config(), &codecs).unwrap();
        set_active(&home, "test").unwrap();

        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&home.join(".zhsh")), 0o700);
        assert_eq!(mode(&directory(&home)), 0o700);
        assert_eq!(mode(&configuration_path(&home, "test").unwrap()), 0o600);
        assert_eq!(mode(&active_path(&home)), 0o600);
        assert!(load_active(&home, &codecs).unwrap().is_some());

        let config_path = configuration_path(&home, "test").unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&home, "test", &codecs).is_err());

        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(active_path(&home), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(load_active(&home, &codecs).is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_store_entries_fail_closed() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "zhsh-store-symlinks-{}-{unique}",
            std::process::id()
        ));
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".zhsh")).unwrap();
        std::fs::set_permissions(home.join(".zhsh"), std::fs::Permissions::from_mode(0o700))
            .unwrap();

        symlink(root.join("missing-directory"), directory(&home)).unwrap();
        assert!(list(&home).is_err());
        std::fs::remove_file(directory(&home)).unwrap();

        super::super::install_test_openai_codec(&home);
        let codecs = CodecRuntime::load(Some(&home));
        save(&home, &test_config(), &codecs).unwrap();
        let config = configuration_path(&home, "test").unwrap();
        std::fs::remove_file(&config).unwrap();
        let outside = root.join("outside.llm");
        std::fs::write(&outside, "not a config").unwrap();
        symlink(&outside, &config).unwrap();
        assert!(load(&home, "test", &codecs).is_err());

        symlink(root.join("missing-active"), active_path(&home)).unwrap();
        assert!(set_active(&home, "test").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
