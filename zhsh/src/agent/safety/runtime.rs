//! 进程级 Safety 规则快照、时间序列覆盖、候选校验和批量安装。

use super::{builtin, decide, external, ExternalAnalyzer, SafetyEngine};
use crate::common::{
    ensure_private_tree, persist_private_file, read_file_snapshot, rollback_created_private_file,
    PersistPolicy,
};
use crate::shell::{
    AgentCommandPlan, AgentTrust, SafetyAssessmentView, SafetyCatalogView, SafetyInstallEntryView,
    SafetyInstallOutcomeView, SafetyInstallPlan, SafetyInstallReport, SafetyInstallRequest,
    SafetyInstallStateView, SafetyInstalledEntryView, SafetyManagementPort, SafetyOperationReport,
    SafetyRuleRowView, SafetyRuleSourceView, SafetyRuleStatusView, SafetySourceValidationView,
    SafetyTargetView,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, FileTimes, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

pub(super) struct ProgramRoute {
    pub(super) linux_core: bool,
    /// 按 `(mtime, filename)` 从旧到新排列。
    pub(super) local: Vec<Arc<ExternalAnalyzer>>,
}

pub(super) struct SafetySnapshot {
    pub(super) generation: u64,
    pub(super) routes: BTreeMap<String, ProgramRoute>,
    catalog: Vec<SafetyRuleRowView>,
    summary: CatalogSummary,
}

#[derive(Clone, Copy, Default)]
struct CatalogSummary {
    local_rules: usize,
    shadowed_builtin_programs: usize,
}

struct LoadedRule {
    analyzer: Arc<ExternalAnalyzer>,
    order: usize,
    modified: SystemTime,
    filename: String,
}

impl SafetySnapshot {
    fn build(generation: u64, local: Vec<LoadedRule>) -> Self {
        let mut routes = BTreeMap::new();
        for program in builtin::programs() {
            routes.insert(
                (*program).to_owned(),
                ProgramRoute {
                    linux_core: true,
                    local: Vec::new(),
                },
            );
        }
        for loaded in &local {
            for program in loaded.analyzer.programs() {
                routes
                    .entry(program.clone())
                    .or_insert_with(empty_route)
                    .local
                    .push(Arc::clone(&loaded.analyzer));
            }
        }

        let mut catalog = Vec::new();
        for program in builtin::programs() {
            let shadowed = routes
                .get(*program)
                .is_some_and(|route| !route.local.is_empty());
            catalog.push(SafetyRuleRowView {
                order: 0,
                source: SafetyRuleSourceView::Builtin,
                name: "linux-core".into(),
                program: (*program).to_owned(),
                rule_count: None,
                builtin: true,
                status: if shadowed {
                    SafetyRuleStatusView::Shadowed
                } else {
                    SafetyRuleStatusView::Active
                },
                modified: None,
                location_or_diagnostic: "<builtin>".into(),
            });
        }
        for loaded in &local {
            for program in loaded.analyzer.programs() {
                catalog.push(SafetyRuleRowView {
                    order: loaded.order,
                    source: SafetyRuleSourceView::Local,
                    name: loaded.analyzer.name().to_owned(),
                    program: program.clone(),
                    rule_count: Some(loaded.analyzer.rule_count()),
                    builtin: false,
                    status: SafetyRuleStatusView::Loaded,
                    modified: Some(loaded.modified),
                    location_or_diagnostic: safe_text(&loaded.filename),
                });
            }
        }
        catalog.sort_by(|left, right| {
            (left.order, left.program.as_str(), left.name.as_str()).cmp(&(
                right.order,
                right.program.as_str(),
                right.name.as_str(),
            ))
        });
        let summary = CatalogSummary {
            local_rules: local.len(),
            shadowed_builtin_programs: routes
                .values()
                .filter(|route| route.linux_core && !route.local.is_empty())
                .count(),
        };
        Self {
            generation,
            routes,
            catalog,
            summary,
        }
    }

    pub(super) fn builtin_only() -> Arc<Self> {
        Arc::new(Self::build(1, Vec::new()))
    }

    #[cfg(test)]
    pub(super) fn from_analyzers(
        earlier: Vec<ExternalAnalyzer>,
        later: Vec<ExternalAnalyzer>,
    ) -> Arc<Self> {
        let local = earlier
            .into_iter()
            .chain(later)
            .enumerate()
            .map(|(index, analyzer)| LoadedRule {
                filename: format!("test-{index}.zhse.json"),
                analyzer: Arc::new(analyzer),
                order: index + 1,
                modified: SystemTime::UNIX_EPOCH,
            })
            .collect();
        Arc::new(Self::build(1, local))
    }
}

fn empty_route() -> ProgramRoute {
    ProgramRoute {
        linux_core: false,
        local: Vec::new(),
    }
}

struct CandidateBuild {
    local: Vec<LoadedRule>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

impl CandidateBuild {
    fn summary(&self) -> CatalogSummary {
        let local_programs: BTreeSet<_> = self
            .local
            .iter()
            .flat_map(|rule| rule.analyzer.programs().iter().map(String::as_str))
            .collect();
        CatalogSummary {
            local_rules: self.local.len(),
            shadowed_builtin_programs: local_programs
                .into_iter()
                .filter(|program| builtin::handles_program(program))
                .count(),
        }
    }

    fn into_snapshot(self, generation: u64) -> SafetySnapshot {
        SafetySnapshot::build(generation, self.local)
    }
}

/// 在整个 zhsh 进程内共享的 Safety 规则运行时。
pub(crate) struct SafetyRuntime {
    local_dir: Option<PathBuf>,
    user_home: Option<PathBuf>,
    current: RwLock<Arc<SafetySnapshot>>,
    mutation_serial: Mutex<()>,
    startup_notices: Vec<String>,
}

impl SafetyRuntime {
    /// 从启动时固定 HOME 创建 generation 1，不读取系统级应用规则目录。
    pub(crate) fn from_startup(home: Option<&Path>) -> Arc<Self> {
        let home = home
            .filter(|home| home.is_absolute())
            .map(Path::to_path_buf);
        let local_dir = home
            .as_deref()
            .map(|home| home.join(".zhsh/plugins/safety"));
        Self::from_directory(local_dir, home)
    }

    fn from_directory(local_dir: Option<PathBuf>, user_home: Option<PathBuf>) -> Arc<Self> {
        let candidate = build_candidate(local_dir.as_deref());
        let mut startup_notices = candidate.warnings.clone();
        startup_notices.extend(candidate.errors.clone());
        let snapshot = if candidate.errors.is_empty() {
            candidate.into_snapshot(1)
        } else {
            SafetySnapshot::build(1, Vec::new())
        };
        Arc::new(Self {
            local_dir,
            user_home,
            current: RwLock::new(Arc::new(snapshot)),
            mutation_serial: Mutex::new(()),
            startup_notices,
        })
    }

    pub(super) fn snapshot(&self) -> Arc<SafetySnapshot> {
        Arc::clone(
            &self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub(crate) fn startup_notices(&self) -> &[String] {
        &self.startup_notices
    }

    #[cfg(test)]
    pub(super) fn for_test(local_dir: PathBuf) -> Arc<Self> {
        let user_home = local_home(&local_dir);
        Self::from_directory(Some(local_dir), user_home)
    }

    fn operation_report(
        &self,
        success: bool,
        generation: u64,
        summary: CatalogSummary,
        warnings: Vec<String>,
        errors: Vec<String>,
    ) -> SafetyOperationReport {
        SafetyOperationReport {
            success,
            generation,
            local_rules: summary.local_rules,
            shadowed_builtin_programs: summary.shadowed_builtin_programs,
            warnings,
            errors,
            source: None,
        }
    }
}

impl SafetyManagementPort for SafetyRuntime {
    fn catalog(&self) -> SafetyCatalogView {
        let snapshot = self.snapshot();
        SafetyCatalogView {
            generation: snapshot.generation,
            rows: snapshot.catalog.clone(),
        }
    }

    fn assess(&self, plan: AgentCommandPlan, trust: AgentTrust) -> SafetyAssessmentView {
        let task_root = std::fs::canonicalize(&plan.cwd).unwrap_or_else(|_| plan.cwd.clone());
        let assessment = SafetyEngine::from_runtime(self).assess_plan_for_task(&plan, &task_root);
        let invocation = plan.invocations.first();
        let decision = decide(trust, &assessment);
        let reason = if decision == super::SafetyDecision::AutoExecute {
            "all_policy_conditions_satisfied".into()
        } else {
            assessment
                .reasons
                .iter()
                .find(|reason| {
                    reason.starts_with(&format!(
                        "{}:",
                        invocation
                            .map(|value| value.original.as_str())
                            .unwrap_or("")
                    ))
                })
                .cloned()
                .unwrap_or_else(|| assessment.primary_reason().into())
        };
        let targets = plan
            .invocations
            .iter()
            .map(|invocation| SafetyTargetView {
                program: invocation.semantic_name().to_owned(),
                target: invocation.target_path().map(Path::to_path_buf),
            })
            .collect::<Vec<_>>();
        SafetyAssessmentView {
            targets,
            semantic: assessment.semantic_level.as_str(),
            rule: if plan.invocations.len() == 1 {
                assessment
                    .semantic_rule
                    .clone()
                    .unwrap_or_else(|| assessment.semantic_source.as_str().into())
            } else {
                "aggregate".into()
            },
            binding: assessment.binding.as_str(),
            decision: decision.as_str(),
            reason,
        }
    }

    fn test_candidate(&self) -> SafetyOperationReport {
        let candidate = build_candidate(self.local_dir.as_deref());
        let summary = candidate.summary();
        self.operation_report(
            candidate.errors.is_empty(),
            self.snapshot().generation,
            summary,
            candidate.warnings,
            candidate.errors,
        )
    }

    fn test_source(&self, source: PathBuf) -> SafetyOperationReport {
        let generation = self.snapshot().generation;
        match parse_source_for_test(&source) {
            Ok((parsed, sha256)) => {
                let summary = summary_for_documents(&parsed.documents);
                let programs = parsed
                    .documents
                    .iter()
                    .flat_map(|document| document.analyzer.programs())
                    .collect::<BTreeSet<_>>()
                    .len();
                let rules = parsed
                    .documents
                    .iter()
                    .map(|document| document.analyzer.rule_count())
                    .sum();
                let mut report =
                    self.operation_report(true, generation, summary, Vec::new(), Vec::new());
                report.source = Some(SafetySourceValidationView {
                    kind: parsed.kind.into(),
                    version: parsed.version,
                    rules,
                    programs,
                    sha256,
                });
                report
            }
            Err(error) => self.operation_report(
                false,
                generation,
                CatalogSummary::default(),
                Vec::new(),
                vec![error],
            ),
        }
    }

    fn reload(&self) -> SafetyOperationReport {
        let _serial = match self.mutation_serial.lock() {
            Ok(serial) => serial,
            Err(_) => {
                let snapshot = self.snapshot();
                return self.operation_report(
                    false,
                    snapshot.generation,
                    snapshot.summary,
                    Vec::new(),
                    vec!["Safety reload 串行锁已损坏".into()],
                );
            }
        };
        let candidate = build_candidate(self.local_dir.as_deref());
        let summary = candidate.summary();
        if !candidate.errors.is_empty() {
            return self.operation_report(
                false,
                self.snapshot().generation,
                summary,
                candidate.warnings,
                candidate.errors,
            );
        }
        let warnings = candidate.warnings.clone();
        let mut current = match self.current.write() {
            Ok(current) => current,
            Err(error) => {
                let snapshot = Arc::clone(error.get_ref());
                return self.operation_report(
                    false,
                    snapshot.generation,
                    summary,
                    warnings,
                    vec!["Safety 当前快照锁已损坏".into()],
                );
            }
        };
        let generation = current.generation.saturating_add(1);
        *current = Arc::new(candidate.into_snapshot(generation));
        self.operation_report(true, generation, summary, warnings, Vec::new())
    }

    fn plan_install(&self, sources: Vec<PathBuf>) -> SafetyInstallPlan {
        let generation = self.snapshot().generation;
        let Some(directory) = self.local_dir.as_deref() else {
            return install_plan_error(generation, "用户 HOME 不可用，无法安装 Safety 规则");
        };
        match prepare_install(&sources, directory) {
            Ok(prepared) => prepared.into_view(generation),
            Err(error) => install_plan_error(generation, error),
        }
    }

    fn install(&self, request: SafetyInstallRequest) -> SafetyInstallReport {
        let generation = self.snapshot().generation;
        let Some(home) = self.user_home.as_deref() else {
            return install_error(generation, "用户 HOME 不可用，无法安装 Safety 规则");
        };
        let _serial = match self.mutation_serial.lock() {
            Ok(serial) => serial,
            Err(_) => return install_error(generation, "Safety 安装串行锁已损坏"),
        };
        let directory = match ensure_private_tree(home, &[".zhsh", "plugins", "safety"]) {
            Ok(directory) => directory,
            Err(error) => return install_error(generation, error.to_string()),
        };
        if self.local_dir.as_deref() != Some(directory.as_path()) {
            return install_error(generation, "Safety 用户目录与启动固定 HOME 不一致");
        }
        let _process_lock = match ProcessInstallLock::acquire(home) {
            Ok(lock) => lock,
            Err(error) => return install_error(generation, error),
        };
        let prepared = match prepare_install(&request.sources, &directory) {
            Ok(prepared) => prepared,
            Err(error) => return install_error(generation, error),
        };
        if request.expected_plan_id.as_deref() != Some(prepared.plan_id.as_str()) {
            return install_error(generation, "Safety 安装计划已经变化，请重新执行命令");
        }
        if prepared
            .entries
            .iter()
            .any(|entry| entry.state == SafetyInstallStateView::Conflict)
            && !request.overwrite
        {
            return install_error(generation, "存在未授权覆盖的 Safety 规则");
        }

        let marker = match PendingInstallMarker::create(home) {
            Ok(marker) => marker,
            Err(error) => return install_error(generation, error),
        };
        let committed = match commit_prepared(&directory, &prepared) {
            Ok(committed) => committed,
            Err(error) => {
                let _ = marker.clear();
                return install_error(generation, error);
            }
        };
        if let Err(error) = marker.clear() {
            return install_error(generation, error);
        }

        let candidate = build_candidate(self.local_dir.as_deref());
        let candidate_reloadable = candidate.errors.is_empty();
        let mut warnings = candidate.warnings;
        if !candidate_reloadable {
            warnings.push(format!(
                "规则已经写入，但完整 local 候选存在 {} 个错误；修复后再 reload",
                candidate.errors.len()
            ));
        }
        SafetyInstallReport {
            success: true,
            entries: committed,
            generation,
            candidate_reloadable,
            warnings,
            errors: Vec::new(),
        }
    }
}

struct PreparedInstall {
    entries: Vec<PreparedEntry>,
    plan_id: String,
}

struct PreparedEntry {
    document: external::ParsedRuleDocument,
    state: SafetyInstallStateView,
    destination: PathBuf,
}

impl PreparedInstall {
    fn into_view(self, generation: u64) -> SafetyInstallPlan {
        SafetyInstallPlan {
            success: true,
            plan_id: Some(self.plan_id),
            generation,
            entries: self
                .entries
                .into_iter()
                .map(|entry| SafetyInstallEntryView {
                    name: entry.document.analyzer.name().to_owned(),
                    programs: entry.document.analyzer.programs().to_vec(),
                    state: entry.state,
                    destination: entry.destination,
                })
                .collect(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }
}

fn prepare_install(sources: &[PathBuf], directory: &Path) -> Result<PreparedInstall, String> {
    let documents = parse_sources(sources)?;
    let mut hasher = Sha256::new();
    let mut entries = Vec::with_capacity(documents.len());
    for document in documents {
        let destination = directory.join(document.target_basename());
        let existing = canonical_existing(&destination)?;
        let state = match existing.as_deref() {
            None => SafetyInstallStateView::New,
            Some(bytes) if bytes == document.canonical_bytes => SafetyInstallStateView::Unchanged,
            Some(_) => SafetyInstallStateView::Conflict,
        };
        hasher.update(document.target_basename().as_bytes());
        hasher.update([0]);
        hasher.update(&document.canonical_bytes);
        hasher.update([0]);
        if let Some(bytes) = existing {
            hasher.update(bytes);
        }
        hasher.update([0xff]);
        entries.push(PreparedEntry {
            document,
            state,
            destination,
        });
    }
    entries.sort_by(|left, right| {
        left.document
            .target_basename()
            .cmp(&right.document.target_basename())
    });
    Ok(PreparedInstall {
        entries,
        plan_id: hex_digest(hasher.finalize().as_slice()),
    })
}

fn parse_sources(sources: &[PathBuf]) -> Result<Vec<external::ParsedRuleDocument>, String> {
    if sources.is_empty() {
        return Err("至少需要一个 Safety 规则输入".into());
    }
    let bundle_count = sources
        .iter()
        .filter(|source| {
            source
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(external::BUNDLE_SUFFIX))
        })
        .count();
    if bundle_count != 0 && (bundle_count != 1 || sources.len() != 1) {
        return Err("全量 bundle 必须作为唯一安装输入".into());
    }
    let mut documents = Vec::new();
    let mut names = BTreeSet::new();
    for source in sources {
        let limit = if source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(external::BUNDLE_SUFFIX))
        {
            external::BUNDLE_FILE_LIMIT
        } else {
            external::RULE_FILE_LIMIT
        };
        let snapshot = read_file_snapshot(source, limit).map_err(|error| error.to_string())?;
        if snapshot.basename.chars().any(is_terminal_format_control) {
            return Err("Safety 输入文件名包含不可见控制字符".into());
        }
        for document in
            external::parse_install_source(source, &snapshot.basename, &snapshot.bytes)?.documents
        {
            if !names.insert(document.analyzer.name().to_owned()) {
                return Err(format!(
                    "安装批次包含重复规则集名称：{}",
                    document.analyzer.name()
                ));
            }
            documents.push(document);
        }
    }
    Ok(documents)
}

fn parse_source_for_test(source: &Path) -> Result<(external::ParsedInstallSource, String), String> {
    let limit = if source
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(external::BUNDLE_SUFFIX))
    {
        external::BUNDLE_FILE_LIMIT
    } else {
        external::RULE_FILE_LIMIT
    };
    let snapshot = read_file_snapshot(source, limit).map_err(|error| error.to_string())?;
    if snapshot.basename.chars().any(is_terminal_format_control) {
        return Err("Safety 输入文件名包含不可见控制字符".into());
    }
    let sha256 = hex_digest(&Sha256::digest(&snapshot.bytes));
    let parsed = external::parse_install_source(source, &snapshot.basename, &snapshot.bytes)?;
    Ok((parsed, sha256))
}

fn canonical_existing(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file()
                || !has_trusted_permissions(&metadata)
                || !owned_by_user(&metadata)
            {
                return Err(format!(
                    "现有 Safety 目标不是当前用户拥有的可信普通文件：{}",
                    safe_text(&path.display().to_string())
                ));
            }
            let snapshot = read_file_snapshot(path, external::RULE_FILE_LIMIT)
                .map_err(|error| error.to_string())?;
            let mut parsed =
                external::parse_install_source(path, &snapshot.basename, &snapshot.bytes)?
                    .documents;
            let document = parsed
                .pop()
                .ok_or_else(|| "现有 Safety 文件为空".to_owned())?;
            Ok(Some(document.canonical_bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("无法检查现有 Safety 目标: {error}")),
    }
}

fn summary_for_documents(documents: &[external::ParsedRuleDocument]) -> CatalogSummary {
    let programs: BTreeSet<_> = documents
        .iter()
        .flat_map(|document| document.analyzer.programs().iter().map(String::as_str))
        .collect();
    CatalogSummary {
        local_rules: documents.len(),
        shadowed_builtin_programs: programs
            .into_iter()
            .filter(|program| builtin::handles_program(program))
            .count(),
    }
}

fn build_candidate(local_dir: Option<&Path>) -> CandidateBuild {
    let mut candidate = CandidateBuild {
        local: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
    };
    let Some(directory) = local_dir else {
        return candidate;
    };
    if let Some(plugins) = directory.parent() {
        match std::fs::symlink_metadata(plugins.join(".safety-install.pending")) {
            Ok(_) => {
                candidate.errors.push(
                    "local:检测到未完成的 Safety 安装事务；清理或恢复后才能加载 local 规则".into(),
                );
                return candidate;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                candidate
                    .errors
                    .push(format!("local:无法检查 Safety 安装事务状态：{error}"));
                return candidate;
            }
        }
    }
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return candidate,
        Err(error) => {
            candidate.errors.push(format!(
                "local:{}：无法检查目录：{error}",
                safe_text(&directory.display().to_string())
            ));
            return candidate;
        }
    };
    if !metadata.file_type().is_dir()
        || !has_trusted_permissions(&metadata)
        || !owned_by_user(&metadata)
    {
        candidate.errors.push(format!(
            "local:{}：目录必须是当前用户拥有且 group/other 不可写的普通目录",
            safe_text(&directory.display().to_string())
        ));
        return candidate;
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            candidate.errors.push(format!(
                "local:{}：无法读取目录：{error}",
                safe_text(&directory.display().to_string())
            ));
            return candidate;
        }
    };
    let mut discovered = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                candidate
                    .errors
                    .push(format!("local:无法读取目录项：{error}"));
                continue;
            }
        };
        let Some(filename) = entry.file_name().to_str().map(str::to_owned) else {
            candidate
                .errors
                .push("local:规则文件名不是有效 UTF-8".into());
            continue;
        };
        if !filename.ends_with(external::SAFETY_RULE_SUFFIX) {
            continue;
        }
        if filename.chars().any(is_terminal_format_control) {
            candidate
                .errors
                .push(format!("local:{filename}：文件名包含不可见控制字符"));
            continue;
        }
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                candidate.errors.push(format!("local:{filename}：{error}"));
                continue;
            }
        };
        if !metadata.file_type().is_file()
            || !has_trusted_permissions(&metadata)
            || !owned_by_user(&metadata)
        {
            candidate.errors.push(format!(
                "local:{filename}：必须是当前用户拥有且 group/other 不可写的非符号链接普通文件"
            ));
            continue;
        }
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) => {
                candidate
                    .errors
                    .push(format!("local:{filename}：无法读取 mtime：{error}"));
                continue;
            }
        };
        discovered.push((modified, filename, path));
    }
    discovered
        .sort_by(|left, right| (&left.0, left.1.as_bytes()).cmp(&(&right.0, right.1.as_bytes())));
    for (index, (modified, filename, path)) in discovered.into_iter().enumerate() {
        match ExternalAnalyzer::load(&path) {
            Ok(analyzer) => candidate.local.push(LoadedRule {
                analyzer: Arc::new(analyzer),
                order: index + 1,
                modified,
                filename,
            }),
            Err(error) => candidate.errors.push(format!(
                "local:{}：{}",
                safe_text(&path.display().to_string()),
                safe_text(&error)
            )),
        }
    }
    candidate
}

fn install_plan_error(generation: u64, error: impl Into<String>) -> SafetyInstallPlan {
    SafetyInstallPlan {
        success: false,
        plan_id: None,
        generation,
        entries: Vec::new(),
        warnings: Vec::new(),
        errors: vec![error.into()],
    }
}

fn install_error(generation: u64, error: impl Into<String>) -> SafetyInstallReport {
    SafetyInstallReport {
        success: false,
        entries: Vec::new(),
        generation,
        candidate_reloadable: false,
        warnings: Vec::new(),
        errors: vec![error.into()],
    }
}

struct ExistingFile {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    modified: Option<SystemTime>,
}

fn commit_prepared(
    directory: &Path,
    prepared: &PreparedInstall,
) -> Result<Vec<SafetyInstalledEntryView>, String> {
    let mut backups = Vec::new();
    let mut committed = Vec::new();
    let batch_time = SystemTime::now();
    for entry in &prepared.entries {
        if entry.state == SafetyInstallStateView::Unchanged {
            committed.push(SafetyInstalledEntryView {
                name: entry.document.analyzer.name().to_owned(),
                outcome: SafetyInstallOutcomeView::Identical,
                destination: entry.destination.clone(),
            });
            continue;
        }
        let backup = match ExistingFile::capture(&entry.destination) {
            Ok(backup) => backup,
            Err(error) => {
                rollback_install(&backups);
                return Err(error);
            }
        };
        let policy = if entry.state == SafetyInstallStateView::Conflict {
            PersistPolicy::Replace
        } else {
            PersistPolicy::IdenticalOnly
        };
        let receipt = match persist_private_file(
            directory,
            &entry.document.target_basename(),
            &entry.document.canonical_bytes,
            policy,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                rollback_install(&backups);
                return Err(error.to_string());
            }
        };
        backups.push((backup, receipt, entry.document.canonical_bytes.clone()));
        committed.push(SafetyInstalledEntryView {
            name: entry.document.analyzer.name().to_owned(),
            outcome: match entry.state {
                SafetyInstallStateView::New => SafetyInstallOutcomeView::Created,
                SafetyInstallStateView::Conflict => SafetyInstallOutcomeView::Replaced,
                SafetyInstallStateView::Unchanged => SafetyInstallOutcomeView::Identical,
            },
            destination: entry.destination.clone(),
        });
    }
    for entry in &prepared.entries {
        if entry.state != SafetyInstallStateView::Unchanged {
            if let Err(error) = set_modified(&entry.destination, batch_time) {
                rollback_install(&backups);
                return Err(error);
            }
        }
    }
    Ok(committed)
}

impl ExistingFile {
    fn capture(path: &Path) -> Result<Self, String> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                let snapshot = read_file_snapshot(path, external::RULE_FILE_LIMIT)
                    .map_err(|error| error.to_string())?;
                Ok(Self {
                    path: path.to_path_buf(),
                    bytes: Some(snapshot.bytes),
                    modified: metadata.modified().ok(),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_path_buf(),
                bytes: None,
                modified: None,
            }),
            Err(error) => Err(format!("无法备份 Safety 目标: {error}")),
        }
    }
}

fn rollback_install(backups: &[(ExistingFile, crate::common::PersistReceipt, Vec<u8>)]) {
    for (backup, receipt, expected) in backups.iter().rev() {
        if let Some(bytes) = &backup.bytes {
            if let Some(directory) = backup.path.parent() {
                if let Some(basename) = backup.path.file_name().and_then(|name| name.to_str()) {
                    let _ =
                        persist_private_file(directory, basename, bytes, PersistPolicy::Replace);
                    if let Some(modified) = backup.modified {
                        let _ = set_modified(&backup.path, modified);
                    }
                }
            }
        } else {
            let _ = rollback_created_private_file(receipt, expected);
        }
    }
}

fn set_modified(path: &Path, modified: SystemTime) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| format!("无法打开 Safety 目标以设置 mtime: {error}"))?;
    file.set_times(FileTimes::new().set_modified(modified))
        .map_err(|error| format!("无法统一 Safety 批次 mtime: {error}"))
}

struct ProcessInstallLock {
    file: File,
}

struct PendingInstallMarker {
    path: PathBuf,
}

impl PendingInstallMarker {
    fn create(home: &Path) -> Result<Self, String> {
        let plugins =
            ensure_private_tree(home, &[".zhsh", "plugins"]).map_err(|error| error.to_string())?;
        let path = plugins.join(".safety-install.pending");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&path).map_err(|error| {
            format!("无法创建 Safety 安装事务标记（可能存在未完成事务）：{error}")
        })?;
        file.sync_all()
            .map_err(|error| format!("无法同步 Safety 安装事务标记: {error}"))?;
        Ok(Self { path })
    }

    fn clear(self) -> Result<(), String> {
        std::fs::remove_file(&self.path)
            .map_err(|error| format!("无法清除 Safety 安装事务标记: {error}"))
    }
}

impl ProcessInstallLock {
    fn acquire(home: &Path) -> Result<Self, String> {
        let plugins =
            ensure_private_tree(home, &[".zhsh", "plugins"]).map_err(|error| error.to_string())?;
        let path = plugins.join(".safety-install.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options
            .open(&path)
            .map_err(|error| format!("无法打开 Safety 安装锁: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("无法复核 Safety 安装锁: {error}"))?;
        if !metadata.is_file() || !has_trusted_permissions(&metadata) || !owned_by_user(&metadata) {
            return Err("Safety 安装锁必须是当前用户拥有且 group/other 不可写的普通文件".into());
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(format!(
                    "无法取得 Safety 跨进程安装锁: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(Self { file })
    }
}

impl Drop for ProcessInstallLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

#[cfg(test)]
fn local_home(directory: &Path) -> Option<PathBuf> {
    let safety = directory.file_name()?.to_str()?;
    let plugins = directory.parent()?.file_name()?.to_str()?;
    let zhsh = directory.parent()?.parent()?.file_name()?.to_str()?;
    (safety == "safety" && plugins == "plugins" && zhsh == ".zhsh").then(|| {
        directory
            .parent()?
            .parent()?
            .parent()
            .map(Path::to_path_buf)
    })?
}

fn safe_text(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if is_terminal_format_control(character) => {
                output.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => output.push(character),
        }
    }
    output
}

fn is_terminal_format_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character as u32,
            0x061c | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2069 | 0xfeff
        )
}

#[cfg(unix)]
fn has_trusted_permissions(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn has_trusted_permissions(_: &std::fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn owned_by_user(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn owned_by_user(_: &std::fs::Metadata) -> bool {
    true
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::{SafetyEngine, SafetyLevel};
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, UNIX_EPOCH};

    fn rule(name: &str, program: &str, id: &str, level: &str) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": 1,
            "name": name,
            "programs": [program],
            "rules": [{
                "id": id,
                "match": {"any_arguments": ["-version"]},
                "assessment": {"level": level}
            }],
            "default": {"id": "fallback", "assessment": {"level": "unknown"}}
        }))
        .unwrap()
    }

    fn write_rule(path: &Path, contents: &str, modified: SystemTime) {
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        set_modified(path, modified).unwrap();
    }

    #[test]
    fn newer_then_lexically_later_rule_wins_and_reload_keeps_old_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-safety-order-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("home");
        let directory = home.join(".zhsh/plugins/safety");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(home.join(".zhsh"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::set_permissions(
            home.join(".zhsh/plugins"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let same_time = UNIX_EPOCH + Duration::from_secs(10);
        write_rule(
            &directory.join("java.zhse.json"),
            &rule("java", "java", "java", "destructive"),
            same_time,
        );
        write_rule(
            &directory.join("oracle.zhse.json"),
            &rule("oracle", "java", "oracle", "read_only"),
            same_time,
        );

        let runtime = SafetyRuntime::from_startup(Some(&home));
        let old = SafetyEngine::from_snapshot(runtime.snapshot());
        assert_eq!(old.assess("java -version").level, SafetyLevel::ReadOnly);

        write_rule(
            &directory.join("oracle.zhse.json"),
            &rule("oracle", "java", "oracle-new", "destructive"),
            same_time + Duration::from_secs(1),
        );
        assert_eq!(old.assess("java -version").level, SafetyLevel::ReadOnly);
        assert!(runtime.reload().success);
        assert_eq!(
            SafetyEngine::from_snapshot(runtime.snapshot())
                .assess("java -version")
                .level,
            SafetyLevel::Destructive
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_invalid_local_file_disables_the_entire_local_set() {
        let root = std::env::temp_dir().join(format!("zhsh-safety-invalid-{}", std::process::id()));
        let directory = root.join(".zhsh/plugins/safety");
        std::fs::create_dir_all(&directory).unwrap();
        for path in [
            root.as_path(),
            root.join(".zhsh").as_path(),
            root.join(".zhsh/plugins").as_path(),
            directory.as_path(),
        ] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        write_rule(
            &directory.join("valid.zhse.json"),
            &rule("valid", "tool", "valid", "read_only"),
            SystemTime::now(),
        );
        write_rule(&directory.join("broken.zhse.json"), "{", SystemTime::now());
        let runtime = SafetyRuntime::from_startup(Some(&root));
        assert_eq!(
            SafetyEngine::from_snapshot(runtime.snapshot())
                .assess("tool -version")
                .level,
            SafetyLevel::Unknown
        );
        assert!(!runtime.startup_notices().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_test_reports_format_counts_and_raw_digest() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-safety-source-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("download.zhse.json");
        let bytes = rule("java", "java", "java", "read_only");
        std::fs::write(&source, &bytes).unwrap();
        let runtime = SafetyRuntime::from_startup(None);

        let report = runtime.test_source(source);
        let source = report.source.unwrap();
        assert!(report.success);
        assert_eq!(source.kind, "rule");
        assert_eq!(source.version, "1");
        assert_eq!(source.rules, 1);
        assert_eq!(source.programs, 1);
        assert_eq!(source.sha256, hex_digest(&Sha256::digest(bytes)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn batch_install_uses_document_names_and_one_shared_mtime() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-safety-batch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = root.join("home");
        let sources = root.join("sources");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let first = sources.join("download-a.zhse.json");
        let second = sources.join("download-b.zhse.json");
        std::fs::write(&first, rule("java", "java", "java", "read_only")).unwrap();
        std::fs::write(&second, rule("oracle", "java", "oracle", "destructive")).unwrap();
        let runtime = SafetyRuntime::from_startup(Some(&home));
        let input = vec![first, second];
        let plan = runtime.plan_install(input.clone());
        assert!(plan.success, "{:?}", plan.errors);
        let report = runtime.install(SafetyInstallRequest {
            sources: input,
            overwrite: false,
            expected_plan_id: plan.plan_id,
        });
        assert!(report.success, "{:?}", report.errors);
        let directory = home.join(".zhsh/plugins/safety");
        let java = directory.join("java.zhse.json");
        let oracle = directory.join("oracle.zhse.json");
        assert!(java.is_file());
        assert!(oracle.is_file());
        assert_eq!(
            std::fs::metadata(java).unwrap().modified().unwrap(),
            std::fs::metadata(oracle).unwrap().modified().unwrap()
        );
        assert!(runtime.reload().success);
        assert_eq!(
            SafetyEngine::from_snapshot(runtime.snapshot())
                .assess("java -version")
                .level,
            SafetyLevel::Destructive
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
