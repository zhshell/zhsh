//! `zh codec`：检查、安装、卸载、列出、导出和重载已签名 Codec。

use super::super::SessionState;
use super::BuiltinResult;
use crate::application::{
    ActiveLlmResolution, CodecInstallRequest, CodecLifecycleService, CodecManagementUi,
    CodecTrustView, PluginInstallOutcome,
};
use crate::common::{terminal_safe_path, ActiveCancellation, CancellationToken, ErrorKind};
use crate::llm::CodecRuntime;
use std::path::Path;
use std::sync::Arc;

const HELP: &str = "Codec：管理 zhsh 与 LLM Provider 之间的请求/响应转换规则。\n\n用法：\n  zh codec ls\n  zh codec -t <FILE.zhcodec>\n  zh codec install <FILE.zhcodec>\n  zh codec uninstall <FORMAT>\n  zh codec export [<FORMAT>] [-o|--output-dir <DIR>]\n  zh codec reload\n\n命令：\n  ls         列出当前进程已加载的 Codec\n  -t         校验 Codec 文件，但不安装、不建立发布者信任\n  install    安装并立即加载 Codec；首次第三方发布者需要确认公钥指纹\n  uninstall  删除当前用户安装的精确 FORMAT；不管理系统软件包\n  export     导出已安装的用户 Codec；官方 Codec 不可导出\n  reload     接纳对 Codec 和信任目录的手工磁盘修改\n\nPUBLISHER 表示 official 或用户已经授权的 trusted 发布者。\nSOURCE 表示 bundled、system-package 或 user-installed 安装来源。\nFORMAT 是 Codec 的精确 id@version 身份，例如 openai@0.3.0。\nSCHEMA_PROFILE 表示 Codec 是否具备 JSON Schema 编码 Profile，不表示当前配置已经启用。\nSIGNER 是发布公钥 SHA-256 指纹的短摘要。\ngeneration 是当前进程快照编号；revision 用于发现其他 zhsh 进程提交的磁盘变化。\n\n详细说明：man zhsh\n";
const LIST_HELP: &str = "zh codec ls：列出当前进程已经加载的 Codec。\n\n用法：zh codec ls\n\n该命令只读取当前 generation 和一次磁盘 revision，不重新扫描或验签全部文件。\n详细说明：man zhsh\n";
const TEST_HELP: &str = "zh codec -t：校验一个 Codec 文件。\n\n用法：zh codec -t <FILE.zhcodec>\n\n校验不会安装文件或建立发布者信任；校验通过不表示发布者已经可信。\n详细说明：man zhsh\n";
const INSTALL_HELP: &str = "zh codec install：安装并立即加载一个 Codec 文件。\n\n用法：zh codec install <FILE.zhcodec>\n\n外部文件名不作为 Codec 身份；首次第三方发布者需要确认公钥指纹，官方签名制品无需重复建立信任。若 system scope 已有完全相同制品，则直接报告已存在且不写入用户目录；否则按签名 FORMAT 保存并立即发布新 generation。不自动切换活动配置。\n详细说明：man zhsh\n";
const UNINSTALL_HELP: &str = "zh codec uninstall：删除当前用户安装的一个精确 Codec。\n\n用法：zh codec uninstall <FORMAT>\n\n只删除 user-installed 制品；bundled 和 system-package 制品应由 apt、dnf 等包管理器卸载。活动 FORMAT 删除前需要确认，配置文件和 Publisher Trust Anchor 均会保留。\n详细说明：man zhsh\n";
const EXPORT_HELP: &str = "zh codec export：导出一个已验证的用户 Codec。\n\n用法：zh codec export [<FORMAT>] [-o|--output-dir <DIR>]\n\n选项：\n  -o, --output-dir  指定导出目录；省略时使用当前目录\n\n省略 FORMAT 时使用活动 Codec；官方 Codec 和过期 generation 不可导出；不会覆盖内容不同的同名文件。\n详细说明：man zhsh\n";
const RELOAD_HELP: &str = "zh codec reload：接纳 Codec 和信任目录的手工磁盘变化。\n\n用法：zh codec reload\n\n正常执行 install 后不需要 reload；只有手工修改受管目录后才需要执行。\n详细说明：man zhsh\n";

pub(crate) fn execute(
    shell: &mut SessionState,
    args: &[String],
    runtime: Option<&Arc<CodecRuntime>>,
    ui: Option<&dyn CodecManagementUi>,
) -> BuiltinResult {
    match args {
        [] => return BuiltinResult::stdout(HELP),
        [value] if matches!(value.as_str(), "help" | "-h" | "--help") => {
            return BuiltinResult::stdout(HELP);
        }
        [command, value]
            if command == "ls" && matches!(value.as_str(), "help" | "-h" | "--help") =>
        {
            return BuiltinResult::stdout(LIST_HELP)
        }
        [command, value]
            if command == "-t" && matches!(value.as_str(), "help" | "-h" | "--help") =>
        {
            return BuiltinResult::stdout(TEST_HELP)
        }
        [command, value]
            if command == "install" && matches!(value.as_str(), "help" | "-h" | "--help") =>
        {
            return BuiltinResult::stdout(INSTALL_HELP)
        }
        [command, value]
            if command == "uninstall" && matches!(value.as_str(), "help" | "-h" | "--help") =>
        {
            return BuiltinResult::stdout(UNINSTALL_HELP)
        }
        [command, value]
            if command == "export" && matches!(value.as_str(), "help" | "-h" | "--help") =>
        {
            return BuiltinResult::stdout(EXPORT_HELP)
        }
        [command, value]
            if command == "reload" && matches!(value.as_str(), "help" | "-h" | "--help") =>
        {
            return BuiltinResult::stdout(RELOAD_HELP)
        }
        _ => {}
    }
    let Some(runtime) = runtime.cloned() else {
        return BuiltinResult::error("zh codec: Codec 运行时未注入\n");
    };
    let service =
        CodecLifecycleService::new(shell.user_home().map(Path::to_path_buf), runtime.clone());
    match args {
        [command] if command == "ls" => list(shell, &service),
        [command, rest @ ..] if command == "-t" => match single_source(rest) {
            Some(source) => inspect(shell, &service, source),
            None => BuiltinResult::error(
                "zh codec -t: 需要一个 FILE.zhcodec\n运行 `zh codec -t -h` 查看用法。\n",
            ),
        },
        [command, rest @ ..] if command == "install" => match single_source(rest) {
            Some(source) => install(shell, &service, source, ui),
            None => BuiltinResult::error(
                "zh codec install: 需要一个 FILE.zhcodec\n运行 `zh codec install -h` 查看用法。\n",
            ),
        },
        [command, rest @ ..] if command == "uninstall" => match single_source(rest) {
            Some(format) => uninstall(shell, &service, format, ui),
            None => BuiltinResult::error(
                "zh codec uninstall: 需要一个精确 FORMAT\n运行 `zh codec uninstall -h` 查看用法。\n",
            ),
        },
        [command] if command == "reload" => reload(shell, &service),
        [command, rest @ ..] if command == "export" => export(shell, &service, rest),
        _ => BuiltinResult::error("zh codec: 参数无效\n运行 `zh codec -h` 查看用法。\n"),
    }
}

fn single_source(args: &[String]) -> Option<&str> {
    match args {
        [source] if !source.starts_with('-') => Some(source),
        [separator, source] if separator == "--" => Some(source),
        _ => None,
    }
}

fn inspect(shell: &SessionState, service: &CodecLifecycleService, source: &str) -> BuiltinResult {
    let source = match super::plugin_path::resolve_source(shell, source) {
        Ok(source) => source,
        Err(error) => return BuiltinResult::error(format!("zh codec -t: {error}\n")),
    };
    match service.inspect(source) {
        Ok(report) => {
            let trust = match report.trust {
                CodecTrustView::Official => "official",
                CodecTrustView::Trusted => "trusted",
                CodecTrustView::Untrusted => "untrusted",
            };
            let mut output = format!(
                "Codec 校验通过\nFORMAT: {}\nPUBLISHER: {}\nSOURCE: external-file\nCOMPATIBILITY: compatible\nPublisher name: {}\nSHA-256: {}\n发布公钥指纹: {}\ngeneration: {}\nJSON Schema: {}\n",
                report.format,
                trust,
                report.publisher,
                report.codec_sha256,
                report.key_fingerprint,
                report.generation,
                if report.supports_json_schema { "支持" } else { "不支持" },
            );
            if report.query_secret_warning {
                output.push_str("警告: 此 Codec 会把凭据放入 Provider URL 查询参数\n");
            }
            BuiltinResult::stdout(output)
        }
        Err(error) => BuiltinResult::error(format!("zh codec -t: {error}\n")),
    }
}

fn uninstall(
    shell: &mut SessionState,
    service: &CodecLifecycleService,
    format: &str,
    ui: Option<&dyn CodecManagementUi>,
) -> BuiltinResult {
    let cancellation = Arc::new(CancellationToken::default());
    let _active = ActiveCancellation::register(Arc::clone(&cancellation));
    match service.uninstall(format, ui, &cancellation) {
        Ok(report) => {
            let active_warning = commit_active_resolution(shell, &report.active);
            let mut output = format!(
                "Codec 已卸载\nFORMAT: {}\nPUBLISHER: {}\nSOURCE: user-installed\n删除位置: {}\nPUBLISHER TRUST: retained\n发布公钥指纹: {}\ngeneration: {}\nrevision: {}\n",
                report.format,
                report.publisher,
                display_managed_path(shell, &report.artifact_path),
                report.key_fingerprint,
                report.generation,
                report.disk_revision,
            );
            if let Some(warning) = active_warning {
                output.push_str(&format!("! {warning}\n"));
            }
            BuiltinResult::stdout(output)
        }
        Err(error) if error.kind() == ErrorKind::Cancelled => {
            BuiltinResult::stdout(format!("Codec 卸载已取消 · {error} · 未删除文件\n"))
        }
        Err(error) => {
            let warning = commit_active_resolution(shell, &service.active_resolution())
                .map(|warning| format!("! {warning}\n"))
                .unwrap_or_default();
            BuiltinResult::error(format!("zh codec uninstall: {error}\n{warning}"))
        }
    }
}

fn install(
    shell: &mut SessionState,
    service: &CodecLifecycleService,
    source: &str,
    ui: Option<&dyn CodecManagementUi>,
) -> BuiltinResult {
    let source = match super::plugin_path::resolve_source(shell, source) {
        Ok(source) => source,
        Err(error) => return BuiltinResult::error(format!("zh codec install: {error}\n")),
    };
    let cancellation = Arc::new(CancellationToken::default());
    let _active = ActiveCancellation::register(Arc::clone(&cancellation));
    match service.install(CodecInstallRequest { source }, ui, &cancellation) {
        Ok(report) => {
            let active_warning = commit_active_resolution(shell, &report.active);
            if report.artifact_outcome == PluginInstallOutcome::Identical
                && report.source != "user-installed"
            {
                let mut output = format!(
                    "Codec 已存在\nFORMAT: {}\nPUBLISHER: {}\nSOURCE: {}\n现有位置: {}\n未写入用户目录\ngeneration: {}\nrevision: {}\n",
                    report.format,
                    report.publisher,
                    report.source,
                    display_managed_path(shell, &report.artifact_path),
                    report.generation,
                    report.disk_revision,
                );
                if shell
                    .llm
                    .as_ref()
                    .is_some_and(|config| config.request_format.as_str() != report.format.as_str())
                {
                    output.push_str("当前 LLM 配置未切换；使用 `zh llm -m` 选择该 FORMAT\n");
                }
                if let Some(warning) = active_warning {
                    output.push_str(&format!("! {warning}\n"));
                }
                return BuiltinResult::stdout(output);
            }
            let action = match report.artifact_outcome {
                PluginInstallOutcome::Created => "已安装",
                PluginInstallOutcome::Replaced => "已替换",
                PluginInstallOutcome::Identical => "已存在",
            };
            let mut output = format!(
                "已识别 FORMAT：{}\nPUBLISHER: {}\nSOURCE: {}\n安装位置：{}\nCodec {action}并加载（generation {}，revision {}）\n",
                report.format,
                report.publisher,
                report.source,
                display_managed_path(shell, &report.artifact_path),
                report.generation,
                report.disk_revision,
            );
            if let Some(path) = report.trusted_key_path.as_deref() {
                let key_action = match report.trusted_key_outcome {
                    Some(PluginInstallOutcome::Created) => "已建立",
                    Some(PluginInstallOutcome::Replaced) => "已替换",
                    _ => "已存在",
                };
                output.push_str(&format!(
                    "Publisher Trust Anchor {key_action}：{} -> {}\n",
                    report.key_fingerprint,
                    display_managed_path(shell, path)
                ));
            }
            if shell
                .llm
                .as_ref()
                .is_some_and(|config| config.request_format.as_str() != report.format.as_str())
            {
                output.push_str("当前 LLM 配置未切换；使用 `zh llm -m` 选择新 FORMAT\n");
            }
            if let Some(warning) = active_warning {
                output.push_str(&format!("! {warning}\n"));
            }
            BuiltinResult::stdout(output)
        }
        Err(error) => {
            let warning = commit_active_resolution(shell, &service.active_resolution())
                .map(|warning| format!("! {warning}\n"))
                .unwrap_or_default();
            BuiltinResult::error(format!("zh codec install: {error}\n{warning}"))
        }
    }
}

fn list(shell: &SessionState, service: &CodecLifecycleService) -> BuiltinResult {
    match service.list() {
        Ok(report) => {
            let active = shell
                .llm
                .as_ref()
                .map(|config| config.request_format.as_str());
            let state = if report.stale { "stale" } else { "current" };
            let rows = report
                .rows
                .into_iter()
                .map(|row| {
                    let is_active = active == Some(row.format.as_str());
                    [
                        row.format,
                        row.publisher.into(),
                        row.source.into(),
                        if is_active { "yes".into() } else { "no".into() },
                        if row.supports_json_schema {
                            "yes".into()
                        } else {
                            "no".into()
                        },
                        row.signer,
                    ]
                })
                .collect();
            let mut output = super::table::render(
                [
                    "FORMAT",
                    "PUBLISHER",
                    "SOURCE",
                    "ACTIVE",
                    "SCHEMA_PROFILE",
                    "SIGNER",
                ],
                rows,
            );
            output.push('\n');
            output.push_str(&format!(
                "generation {} · revision {} · {}\n",
                report.generation, report.disk_revision, state
            ));
            BuiltinResult::stdout(output)
        }
        Err(error) => BuiltinResult::error(format!("zh codec ls: {error}\n")),
    }
}

fn reload(shell: &mut SessionState, service: &CodecLifecycleService) -> BuiltinResult {
    let cancellation = Arc::new(CancellationToken::default());
    let _active = ActiveCancellation::register(Arc::clone(&cancellation));
    match service.reload(&cancellation) {
        Ok(report) => {
            let warning = commit_active_resolution(shell, &report.active)
                .map(|warning| format!("! {warning}\n"))
                .unwrap_or_default();
            let diagnostics = report
                .issues
                .iter()
                .map(|issue| format!("! {issue}\n"))
                .collect::<String>();
            BuiltinResult::stdout(format!(
                "Codec 已重新加载（generation {}，revision {}）\n{diagnostics}{warning}",
                report.generation, report.disk_revision
            ))
        }
        Err(error) => BuiltinResult::error(format!("zh codec reload: {error}\n")),
    }
}

fn export(shell: &SessionState, service: &CodecLifecycleService, args: &[String]) -> BuiltinResult {
    let mut format = None;
    let mut output_dir = None;
    let mut options = true;
    let mut index = 0;
    while index < args.len() {
        let value = &args[index];
        if options && value == "--" {
            options = false;
        } else if options && matches!(value.as_str(), "-o" | "--output-dir") {
            if output_dir.is_some() || index + 1 >= args.len() {
                return BuiltinResult::error(
                    "zh codec export: -o|--output-dir 需要且只能指定一个目录\n运行 `zh codec export -h` 查看用法。\n",
                );
            }
            index += 1;
            output_dir = Some(args[index].as_str());
        } else {
            if (options && value.starts_with('-')) || format.is_some() {
                return BuiltinResult::error(format!(
                    "zh codec export: 无法解析参数 {value}\n运行 `zh codec export -h` 查看用法。\n"
                ));
            }
            format = Some(value.as_str());
        }
        index += 1;
    }
    let format = match format.or_else(|| {
        shell
            .llm
            .as_ref()
            .map(|config| config.request_format.as_str())
    }) {
        Some(format) => format,
        None => {
            return BuiltinResult::error("zh codec export: 未指定 FORMAT，且当前没有活动 Codec\n")
        }
    };
    let require_owned_output = output_dir.is_some();
    let output_dir = match output_dir {
        Some(path) => match super::plugin_path::resolve_source(shell, path) {
            Ok(path) => path,
            Err(error) => return BuiltinResult::error(format!("zh codec export: {error}\n")),
        },
        None => shell.cwd.clone(),
    };
    let cancellation = Arc::new(CancellationToken::default());
    let _active = ActiveCancellation::register(Arc::clone(&cancellation));
    match service.export(format, output_dir, require_owned_output, &cancellation) {
        Ok(report) => BuiltinResult::stdout(format!(
            "Codec {}：{} -> {}\n",
            if report.identical {
                "已存在"
            } else {
                "已导出"
            },
            report.format,
            terminal_safe_path(&report.destination)
        )),
        Err(error) => BuiltinResult::error(format!("zh codec export: {error}\n")),
    }
}

fn commit_active_resolution(
    shell: &mut SessionState,
    active: &ActiveLlmResolution,
) -> Option<String> {
    match active {
        ActiveLlmResolution::NotConfigured => {
            shell.clear_active_llm();
            None
        }
        ActiveLlmResolution::Available(config) => {
            let warning = config.json_schema_downgrade_warning();
            shell.commit_ready_llm(config.clone());
            warning
        }
        ActiveLlmResolution::Unavailable(error) => {
            let name = shell.active_llm_name().map(str::to_string);
            shell.commit_unavailable_llm(name, error.clone());
            Some(format!("活动 LLM 配置不可用：{error}"))
        }
    }
}

fn display_managed_path(shell: &SessionState, path: &Path) -> String {
    if let Some(home) = shell.user_home() {
        if let Ok(relative) = path.strip_prefix(home) {
            return format!("~/{}", relative.display());
        }
    }
    terminal_safe_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_management_arguments_remain_builtin_errors() {
        let mut shell = SessionState::test();
        let runtime = Arc::new(CodecRuntime::load(shell.user_home()));
        assert_eq!(execute(&mut shell, &[], Some(&runtime), None).code, 0);
        assert_eq!(
            execute(&mut shell, &["install".into()], Some(&runtime), None).code,
            1
        );
        assert_eq!(
            execute(&mut shell, &["uninstall".into()], Some(&runtime), None).code,
            1
        );
        assert_eq!(
            execute(
                &mut shell,
                &["export".into(), "a".into(), "b".into()],
                Some(&runtime),
                None
            )
            .code,
            1
        );
    }

    #[test]
    fn codec_catalog_uses_aligned_columns_without_tabs_or_repeated_state() {
        let shell = SessionState::test();
        let runtime = Arc::new(CodecRuntime::load(shell.user_home()));
        let service = CodecLifecycleService::new(shell.user_home().map(Path::to_path_buf), runtime);

        let result = list(&shell, &service);

        assert_eq!(result.code, 0);
        assert!(result.stdout.starts_with("FORMAT"));
        assert!(result.stdout.contains("PUBLISHER"));
        assert!(result.stdout.contains("SOURCE"));
        assert!(result.stdout.contains("SCHEMA_PROFILE"));
        assert!(!result.stdout.contains('\t'));
        assert!(!result.stdout.lines().next().unwrap().contains("STATE"));
        assert_eq!(result.stdout.matches(" · current\n").count(), 1);
    }
}
