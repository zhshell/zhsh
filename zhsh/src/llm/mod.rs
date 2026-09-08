//! 供应商中立的 LLM 配置、声明式 Codec 与 Core HTTP 传输。

mod codec;
mod config;
mod model;
mod plugin;
mod redaction;
mod store;
mod transport;

use crate::common::{AppError, AppResult, CancellationToken};
use codec::CodecEvaluator;
use plugin::PluginCatalog;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::{watch, Semaphore};

pub(crate) use config::{
    normalize_base_url, parse_base_url, plaintext_private_warning, transport_status,
    validate_format, validate_model, validate_name, TransportSecurity,
};
pub(crate) use model::CompletionRequest;
pub use model::{
    FinishReason, JsonSchemaMode, JsonSchemaResolution, LlmConfig, LlmMessage, LlmResponse,
    ModelTier, ModelTiers, ProviderError, ProviderErrorKind, TokenUsage,
};
pub(crate) use model::{LlmProfile, LlmProfileDraft, LlmProfileIssue, LlmProfileReadiness};
pub(crate) use plugin::{
    export_user_codec, inspect_codec, install_user_codec, uninstall_user_codec, CodecTrustState,
    PluginSource, PluginSummary,
};
pub(crate) use redaction::SecretRedactor;
pub(crate) use store::{
    assess_profile, list as list_configs, load_active, load_active_profile,
    load_profile as load_config_profile, save as save_config, save_profile as save_config_profile,
    set_active,
};

pub fn mask_auth(auth: Option<&str>) -> String {
    match auth {
        Some(value) if !value.trim().is_empty() => {
            let chars: Vec<_> = value.trim().chars().collect();
            if chars.len() < 16 {
                return "*****".to_string();
            }
            let head: String = chars.iter().take(4).collect();
            let tail: String = chars.iter().skip(chars.len() - 4).collect();
            format!("{head}*****{tail}")
        }
        _ => "(unset)".to_string(),
    }
}

/// 一次完整加载后形成的不可变 Codec 快照。
pub(crate) struct CodecGeneration {
    number: u64,
    disk_revision: u64,
    catalog: PluginCatalog,
    summaries: Arc<[PluginSummary]>,
    issues: Arc<[String]>,
    trusted_user_keys: Arc<[[u8; 32]]>,
    official_contract_error: Option<String>,
    coordination_error: Option<String>,
}

impl CodecGeneration {
    fn resolve(&self, format: &str) -> AppResult<plugin::VerifiedPlugin> {
        if let Some(error) = &self.coordination_error {
            return Err(AppError::protocol(error.clone()));
        }
        if let Some(error) = &self.official_contract_error {
            return Err(AppError::protocol(error.clone()));
        }
        self.catalog.resolve(format)
    }
}

/// 在整个 zhsh 进程共享、可原子发布不可变 generation 的 Codec 事实源。
pub(crate) struct CodecRuntime {
    user_home: Option<std::path::PathBuf>,
    current: RwLock<Arc<CodecGeneration>>,
    publish_serial: Mutex<()>,
    next_generation: AtomicU64,
}

impl CodecRuntime {
    /// 从固定系统根和可选用户根构造初始 generation。
    pub(crate) fn load(home: Option<&std::path::Path>) -> Self {
        let cancellation = CancellationToken::default();
        let (report, disk_revision, coordination_error) = match home {
            Some(home) => match plugin::ManagementLock::acquire(
                home,
                plugin::LockMode::Shared,
                false,
                &cancellation,
            ) {
                Ok(Some(lock)) => match lock.read_revision() {
                    Ok(revision) => (PluginCatalog::load_runtime(Some(home)), revision, None),
                    Err(error) => (
                        PluginCatalog::load_runtime(Some(home)),
                        0,
                        Some(error.to_string()),
                    ),
                },
                Ok(None) => {
                    let before = plugin::read_revision(Some(home));
                    let report = PluginCatalog::load_runtime(Some(home));
                    let after = plugin::read_revision(Some(home));
                    match (before, after) {
                        (Ok(before), Ok(after)) if before == after => (report, before, None),
                        (Ok(_), Ok(_)) => (
                            report,
                            0,
                            Some("Codec 磁盘状态在启动加载期间发生变化".into()),
                        ),
                        (Err(error), _) | (_, Err(error)) => (report, 0, Some(error.to_string())),
                    }
                }
                Err(error) => (
                    PluginCatalog::load_runtime(Some(home)),
                    0,
                    Some(error.to_string()),
                ),
            },
            None => (PluginCatalog::load_runtime(None), 0, None),
        };
        let generation = Arc::new(Self::generation_from_report(
            report,
            1,
            disk_revision,
            coordination_error,
        ));
        Self {
            user_home: home.map(std::path::Path::to_path_buf),
            current: RwLock::new(generation),
            publish_serial: Mutex::new(()),
            next_generation: AtomicU64::new(2),
        }
    }

    fn generation_from_report(
        report: plugin::PluginLoadReport,
        number: u64,
        disk_revision: u64,
        coordination_error: Option<String>,
    ) -> CodecGeneration {
        let system_integrity_error = report
            .issues
            .iter()
            .find(|issue| issue.blocks_runtime())
            .map(ToString::to_string);
        let summaries = report.catalog.summaries();
        let codec = CodecEvaluator;
        let official_contract_error = system_integrity_error.or_else(|| {
            summaries
                .iter()
                .filter(|summary| summary.official)
                .find_map(|summary| {
                    report
                        .catalog
                        .resolve(&summary.label())
                        .and_then(|plugin| codec.validate_official_contract(&plugin))
                        .err()
                        .map(|error| error.to_string())
                })
        });
        let mut issues = report.issue_messages();
        if let Some(error) = coordination_error.as_deref() {
            issues.push(format!("[codec.revision.invalid] {error}"));
        }
        CodecGeneration {
            number,
            disk_revision,
            catalog: report.catalog,
            summaries: summaries.into(),
            issues: issues.into(),
            trusted_user_keys: report.trusted_user_keys.into(),
            official_contract_error,
            coordination_error,
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<CodecGeneration> {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 返回当前 generation 的 Codec 摘要。
    pub(crate) fn summaries(&self) -> Vec<PluginSummary> {
        self.snapshot().summaries.to_vec()
    }

    /// 返回当前 generation 的局部隔离诊断。
    pub(crate) fn issue_messages(&self) -> Vec<String> {
        self.snapshot().issues.to_vec()
    }

    fn resolve(&self, format: &str) -> AppResult<plugin::VerifiedPlugin> {
        self.snapshot().resolve(format)
    }

    pub(crate) fn resolve_json_schema(
        &self,
        format: &str,
        requested: JsonSchemaMode,
    ) -> AppResult<JsonSchemaResolution> {
        self.resolve(format)?.resolve_json_schema(requested)
    }

    pub(crate) fn generation_number(&self) -> u64 {
        self.snapshot().number
    }

    pub(crate) fn disk_revision(&self) -> u64 {
        self.snapshot().disk_revision
    }

    pub(crate) fn is_key_trusted(&self, key: &[u8; 32]) -> bool {
        *key == *plugin::OFFICIAL_PUBLIC_KEY
            || self
                .snapshot()
                .trusted_user_keys
                .iter()
                .any(|candidate| candidate == key)
    }

    pub(crate) fn current_revision(&self) -> AppResult<u64> {
        plugin::read_revision(self.user_home.as_deref())
    }

    pub(crate) fn is_stale(&self) -> AppResult<bool> {
        Ok(self.current_revision()? != self.disk_revision())
    }

    pub(crate) fn publish_report(
        &self,
        report: plugin::PluginLoadReport,
        disk_revision: u64,
    ) -> AppResult<u64> {
        let number = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let candidate = Self::generation_from_report(report, number, disk_revision, None);
        if let Some(error) = candidate.official_contract_error.as_deref() {
            return Err(AppError::protocol(error));
        }
        Ok(self.publish_candidate(candidate))
    }

    pub(crate) fn validate_report(&self, report: &plugin::PluginLoadReport) -> AppResult<()> {
        let candidate = Self::generation_from_report(report.clone(), 0, 0, None);
        if let Some(error) = candidate.official_contract_error {
            return Err(AppError::protocol(error));
        }
        Ok(())
    }

    fn publish_candidate(&self, candidate: CodecGeneration) -> u64 {
        let number = candidate.number;
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(candidate);
        number
    }

    pub(crate) fn refresh_if_stale(&self, cancellation: &CancellationToken) -> AppResult<bool> {
        let disk_revision = self.current_revision()?;
        if disk_revision == self.disk_revision() {
            return Ok(false);
        }
        let _serial = self
            .publish_serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(lock) = plugin::ManagementLock::acquire(
            self.user_home
                .as_deref()
                .ok_or_else(|| AppError::input("用户 HOME 不可用"))?,
            plugin::LockMode::Shared,
            false,
            cancellation,
        )?
        else {
            return Err(AppError::protocol("Codec revision 已变化但管理锁不存在"));
        };
        let disk_revision = lock.read_revision()?;
        if disk_revision == self.disk_revision() {
            return Ok(false);
        }
        let report = PluginCatalog::load_runtime(self.user_home.as_deref());
        self.publish_report(report, disk_revision)?;
        Ok(true)
    }

    pub(crate) fn reload(&self, cancellation: &CancellationToken) -> AppResult<u64> {
        let _serial = self
            .publish_serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lock = plugin::ManagementLock::acquire(
            self.user_home
                .as_deref()
                .ok_or_else(|| AppError::input("用户 HOME 不可用"))?,
            plugin::LockMode::Exclusive,
            true,
            cancellation,
        )?
        .ok_or_else(|| AppError::internal("未取得 Codec 管理锁"))?;
        let revision = lock
            .read_revision()?
            .checked_add(1)
            .ok_or_else(|| AppError::protocol("Codec revision 数值溢出"))?;
        let report = PluginCatalog::load_runtime(self.user_home.as_deref());
        self.validate_report(&report)?;
        let rechecked = PluginCatalog::load_runtime(self.user_home.as_deref());
        if !report.equivalent_to(&rechecked) {
            return Err(AppError::protocol(
                "Codec 受管目录在 reload 提交前发生变化；请重试",
            ));
        }
        lock.write_revision(revision)?;
        self.publish_report(rechecked, revision)
    }
}

#[cfg(test)]
pub(crate) fn install_test_openai_codec(home: &std::path::Path) {
    let directory = home.join(".zhsh/plugins/llm");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("openai@0.3.0.zhcodec");
    std::fs::write(
        &path,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/llm-codecs/openai@0.3.0.zhcodec"
        )),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[derive(Clone)]
pub struct LlmClient {
    runtime: Arc<Runtime>,
    http: HttpClients,
    codec: CodecEvaluator,
    codecs: Arc<CodecRuntime>,
    request_gate: Arc<Semaphore>,
    next_request_id: Arc<AtomicU64>,
}

#[derive(Clone)]
struct HttpClients {
    default: reqwest::Client,
    direct_local_http: reqwest::Client,
}

impl HttpClients {
    fn new() -> AppResult<Self> {
        ensure_tls_crypto_provider()?;
        let default = client_builder()
            .build()
            .map_err(|error| AppError::internal(format!("无法初始化 HTTP 客户端: {error}")))?;
        let direct_local_http = client_builder()
            .no_proxy()
            .build()
            .map_err(|error| AppError::internal(format!("无法初始化直连 HTTP 客户端: {error}")))?;
        Ok(Self {
            default,
            direct_local_http,
        })
    }

    fn for_transport(&self, transport: TransportSecurity) -> &reqwest::Client {
        match transport {
            TransportSecurity::Https => &self.default,
            TransportSecurity::LoopbackHttp | TransportSecurity::PrivateHttp => {
                &self.direct_local_http
            }
        }
    }
}

/// Reqwest 在关闭默认 TLS Provider 后不会替 Core 猜测密码实现。zhsh 显式安装与 Codec
/// 验签一致的 Ring Provider，并把结果缓存到进程生命周期，保证多个 `LlmClient` 幂等初始化。
fn ensure_tls_crypto_provider() -> AppResult<()> {
    static TLS_PROVIDER: OnceLock<Result<(), String>> = OnceLock::new();
    TLS_PROVIDER
        .get_or_init(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| "进程已安装其他 Rustls CryptoProvider，无法启用 Ring".into())
        })
        .clone()
        .map_err(|error| AppError::internal(format!("无法初始化 TLS 密码实现: {error}")))
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
}

pub(crate) struct CompletionHandle {
    task_id: u64,
    request_id: u64,
    receiver: mpsc::Receiver<AppResult<LlmResponse>>,
}

impl CompletionHandle {
    pub(crate) fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<AppResult<LlmResponse>, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    pub(crate) fn task_id(&self) -> u64 {
        self.task_id
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    /// 在终端控制事件与已经出队的响应同时到达时，把原结果重新放回同一请求身份。
    pub(crate) fn from_completed(
        task_id: u64,
        request_id: u64,
        result: AppResult<LlmResponse>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let _ = sender.send(result);
        Self {
            task_id,
            request_id,
            receiver,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_receiver(
        task_id: u64,
        receiver: mpsc::Receiver<AppResult<LlmResponse>>,
    ) -> Self {
        Self {
            task_id,
            request_id: 0,
            receiver,
        }
    }
}

pub(crate) trait CompletionPort {
    fn start_completion(
        &self,
        config: LlmConfig,
        system_prompt: String,
        messages: Vec<LlmMessage>,
        cancellation: Arc<CancellationToken>,
    ) -> CompletionHandle;
}

impl LlmClient {
    pub(crate) fn new(codecs: Arc<CodecRuntime>) -> AppResult<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("zhsh-http")
            .enable_all()
            .build()
            .map_err(|error| AppError::internal(format!("无法初始化 HTTP 运行时: {error}")))?;
        let http = HttpClients::new()?;
        let codec = CodecEvaluator;
        Ok(Self {
            runtime: Arc::new(runtime),
            http,
            codec,
            codecs,
            request_gate: Arc::new(Semaphore::new(1)),
            next_request_id: Arc::new(AtomicU64::new(1)),
        })
    }
}

impl CompletionPort for LlmClient {
    fn start_completion(
        &self,
        config: LlmConfig,
        system_prompt: String,
        messages: Vec<LlmMessage>,
        cancellation: Arc<CancellationToken>,
    ) -> CompletionHandle {
        let task_id = cancellation.id();
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let http = self.http.clone();
        let codec = self.codec.clone();
        let generation = self.codecs.snapshot();
        let plugin = generation.resolve(&config.request_format);
        let gate = Arc::clone(&self.request_gate);
        let mut cancelled = cancellation.subscribe();
        let (sender, receiver) = mpsc::channel();
        self.runtime.spawn(async move {
            let _generation = generation;
            let _cancellation = cancellation;
            let result = match plugin {
                Ok(plugin) => {
                    complete_cancellable(
                        &http,
                        &codec,
                        gate,
                        &plugin,
                        &config,
                        system_prompt,
                        messages,
                        &mut cancelled,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let _ = sender.send(result);
        });
        CompletionHandle {
            task_id,
            request_id,
            receiver,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn complete_cancellable(
    clients: &HttpClients,
    codec: &CodecEvaluator,
    gate: Arc<Semaphore>,
    plugin: &plugin::VerifiedPlugin,
    config: &LlmConfig,
    system_prompt: String,
    messages: Vec<LlmMessage>,
    cancelled: &mut watch::Receiver<bool>,
) -> AppResult<LlmResponse> {
    let permit = tokio::select! {
        biased;
        _ = wait_cancelled(cancelled) => return Err(AppError::cancelled()),
        permit = gate.acquire_owned() => permit
            .map_err(|_| AppError::internal("LLM request gate closed"))?,
    };
    let base_url = parse_base_url(&config.url).map_err(AppError::input)?;
    let client = clients.for_transport(base_url.transport());
    let request = CompletionRequest {
        system: system_prompt,
        messages,
        model: config.model().to_string(),
        max_output_tokens: Some(2048),
    };
    let result = tokio::select! {
        biased;
        _ = wait_cancelled(cancelled) => {
            Err(AppError::cancelled())
        },
        result = transport::call(
            client,
            codec,
            plugin,
            base_url.normalized(),
            config,
            request,
        ) => result,
    };
    drop(permit);
    result
}

async fn wait_cancelled(cancelled: &mut watch::Receiver<bool>) {
    if *cancelled.borrow() {
        return;
    }
    loop {
        if cancelled.changed().await.is_err() || *cancelled.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_access_tokens_consistently() {
        assert_eq!(mask_auth(Some("123456789012345")), "*****");
        assert_eq!(mask_auth(Some("1234567890123456")), "1234*****3456");
        assert_eq!(mask_auth(None), "(unset)");
    }

    #[test]
    fn local_and_private_http_share_the_direct_client_only() {
        let clients = HttpClients::new().unwrap();

        assert!(std::ptr::eq(
            clients.for_transport(TransportSecurity::Https),
            &clients.default
        ));
        for transport in [
            TransportSecurity::LoopbackHttp,
            TransportSecurity::PrivateHttp,
        ] {
            assert!(std::ptr::eq(
                clients.for_transport(transport),
                &clients.direct_local_http
            ));
        }
    }

    #[test]
    fn bundled_plugins_are_verified_by_the_same_package_loader() {
        let catalog = PluginCatalog::load_strict(None).unwrap();
        let codec = CodecEvaluator;
        let summaries = catalog.summaries();
        assert_eq!(summaries.len(), 2);
        for summary in summaries {
            let plugin = catalog.resolve(&summary.label()).unwrap();
            codec.validate_official_contract(&plugin).unwrap();
        }
    }
}
