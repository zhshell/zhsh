//! 临时保存严格 Agent 协议无法解析的模型文本，供定位 Codec/Provider 格式偏差。
//!
//! 该文件包含模型原始输出，可能间接包含任务内容，因此只写入启动时固定 HOME 下的
//! `0600` 文件。写入失败不得改变六轮状态机或放宽协议。

use super::protocol::AgentResponseError;
use crate::common::{ensure_private_tree, terminal_safe_path, AppError, AppResult};
use crate::llm::{FinishReason, LlmConfig, LlmResponse};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_DIRECTORY: &[&str] = &[".zhsh", "diagnostics"];
const LOG_BASENAME: &str = "invalid-agent-responses.jsonl";

/// 当前进程固定的诊断策略。测试任务使用 [`Default`]，不会继承真实 HOME 或环境开关。
#[derive(Clone, Debug, Default)]
pub(super) struct InvalidResponseDiagnostics {
    home: Option<PathBuf>,
}

impl InvalidResponseDiagnostics {
    pub(super) fn from_startup(home: Option<&Path>, enabled: bool) -> Self {
        Self {
            home: enabled.then(|| home.map(Path::to_path_buf)).flatten(),
        }
    }

    pub(super) fn path(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|home| {
            LOG_DIRECTORY
                .iter()
                .fold(home.clone(), |path, component| path.join(component))
                .join(LOG_BASENAME)
        })
    }

    pub(super) fn append(
        &self,
        config: Option<&LlmConfig>,
        phase: u8,
        turn: i32,
        response: &LlmResponse,
        error: &AgentResponseError,
    ) -> AppResult<Option<PathBuf>> {
        append_to_home(self.home.as_deref(), config, phase, turn, response, error)
    }

    #[cfg(test)]
    fn enabled_for_test(home: &Path) -> Self {
        Self {
            home: Some(home.to_path_buf()),
        }
    }
}

#[derive(Serialize)]
struct InvalidResponseRecord<'a> {
    schema: u8,
    timestamp_unix_ms: u128,
    process_id: u32,
    phase: u8,
    turn: i32,
    profile: Option<&'a str>,
    model: Option<&'a str>,
    format: Option<&'a str>,
    json_schema: Option<&'a str>,
    finish_reason: String,
    parse_category: &'a str,
    parse_detail: &'a str,
    error_line: Option<usize>,
    error_column: Option<usize>,
    expected_closer: Option<&'a str>,
    unclosed_containers: Option<usize>,
    response_bytes: usize,
    raw_response: &'a str,
}

/// 追加一条 JSON Lines 诊断。成功时返回固定日志路径。
fn append_to_home(
    home: Option<&Path>,
    config: Option<&LlmConfig>,
    phase: u8,
    turn: i32,
    response: &LlmResponse,
    error: &AgentResponseError,
) -> AppResult<Option<PathBuf>> {
    let Some(home) = home else {
        return Ok(None);
    };
    let directory = ensure_private_tree(home, LOG_DIRECTORY)?;
    let path = directory.join(LOG_BASENAME);
    let record = InvalidResponseRecord {
        schema: 1,
        timestamp_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        process_id: std::process::id(),
        phase,
        turn,
        profile: config.map(|value| value.name.as_str()),
        model: config.map(LlmConfig::model),
        format: config.map(|value| value.request_format.as_str()),
        json_schema: config.map(|value| value.json_schema.status()),
        finish_reason: finish_reason(&response.finish_reason),
        parse_category: error.category(),
        parse_detail: error.detail(),
        error_line: error.line(),
        error_column: error.column(),
        expected_closer: error.expected_closer(),
        unclosed_containers: error.unclosed_containers(),
        response_bytes: response.text.len(),
        raw_response: &response.text,
    };
    let mut line = serde_json::to_vec(&record)
        .map_err(|error| AppError::internal(format!("无法编码 Agent 响应诊断: {error}")))?;
    line.push(b'\n');

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&path).map_err(|error| {
        AppError::io(format!(
            "无法打开 Agent 响应诊断文件 {}: {error}",
            terminal_safe_path(&path)
        ))
    })?;
    secure_file(&file)?;
    file.write_all(&line).map_err(|error| {
        AppError::io(format!(
            "无法写入 Agent 响应诊断文件 {}: {error}",
            terminal_safe_path(&path)
        ))
    })?;
    file.flush().map_err(|error| {
        AppError::io(format!(
            "无法刷新 Agent 响应诊断文件 {}: {error}",
            terminal_safe_path(&path)
        ))
    })?;
    Ok(Some(path))
}

fn finish_reason(reason: &FinishReason) -> String {
    match reason {
        FinishReason::Completed => "completed".into(),
        FinishReason::OutputLimit => "output_limit".into(),
        FinishReason::ContentFiltered => "content_filtered".into(),
        FinishReason::Unknown(value) => format!("unknown:{value}"),
    }
}

#[cfg(unix)]
fn secure_file(file: &std::fs::File) -> AppResult<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = file
        .metadata()
        .map_err(|error| AppError::io(format!("无法检查 Agent 响应诊断文件: {error}")))?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(AppError::input(
            "Agent 响应诊断目标必须是当前用户拥有的普通文件",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::io(format!("无法设置 Agent 响应诊断文件权限: {error}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_: &std::fs::File) -> AppResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{FinishReason, LlmResponse};

    #[test]
    fn invalid_response_is_recorded_as_private_json_line() {
        let home = std::env::temp_dir().join(format!(
            "zhsh-invalid-response-log-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let response = LlmResponse {
            text: "not json\nexact text".into(),
            finish_reason: FinishReason::Completed,
            usage: None,
        };
        let error =
            super::super::protocol::parse_agent_response_detailed(&response.text, &response)
                .unwrap_err();

        let path = InvalidResponseDiagnostics::enabled_for_test(&home)
            .append(None, 1, 3, &response, &error)
            .unwrap()
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();

        assert_eq!(value["phase"], 1);
        assert_eq!(value["turn"], 3);
        assert_eq!(value["parse_category"], "invalid_json_syntax");
        assert_eq!(value["raw_response"], response.text);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn diagnostics_are_disabled_without_an_explicit_home() {
        let response = LlmResponse {
            text: "not json".into(),
            finish_reason: FinishReason::Completed,
            usage: None,
        };
        let error =
            super::super::protocol::parse_agent_response_detailed(&response.text, &response)
                .unwrap_err();

        assert!(InvalidResponseDiagnostics::default()
            .append(None, 1, 1, &response, &error)
            .unwrap()
            .is_none());
    }
}
