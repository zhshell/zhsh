//! `zh safety`：查看、校验、安装和显式重载进程内 Safety 规则。

use super::super::{
    SafetyInstallOutcomeView, SafetyInstallPlan, SafetyInstallReport, SafetyInstallRequest,
    SafetyManagementPort, SafetyManagementUi, SafetyOperationReport, SafetyOverwriteDecision,
    SafetyOverwritePrompt, SafetyRuleRowView, SessionState,
};
use super::BuiltinResult;
use crate::common::{terminal_safe_path, ActiveCancellation, CancellationToken};
use std::sync::Arc;

const HELP: &str = "Safety：管理 Agent 命令的声明式分类规则。规则是本地策略，不是安全证明。\n\n用法：\n  zh safety\n  zh safety -t [SOURCE]\n  zh safety assess '<COMMAND>'\n  zh safety install [--overwrite] <FILE.zhse.json>...\n  zh safety install [--overwrite] <BUNDLE.zhse.json.gz>\n  zh safety reload\n\n命令：\n  zh safety  列出当前进程内存中的 builtin/local 规则和加载顺序\n  -t         校验磁盘候选或一个外部输入，不安装、不激活\n  assess     解释命令的语义规则、实际目标、绑定等级和最终决策，不执行命令\n  install    按 JSON name 规范化安装；多个文件构成一个批次\n  reload     完整校验本地目录，并仅替换当前进程的快照\n\n选项：\n  --overwrite  允许覆盖内容不同的同名本地规则，跳过覆盖确认\n\nlocal 文件按 mtime 升序加载；mtime 相同时按文件名字典序升序加载；后匹配规则覆盖前匹配规则。\n详细说明：man zhsh\n";
const INSTALL_HELP: &str = "zh safety install：安装一个或一批本地 Safety 规则。\n\n用法：\n  zh safety install [--overwrite] <FILE.zhse.json>...\n  zh safety install [--overwrite] <BUNDLE.zhse.json.gz>\n\n外部文件名不作为规则身份；安装目标固定为 <name>.zhse.json。同一批次使用相同 mtime，批次内按规范文件名字典序决定覆盖优先级。内容不同的已有目标默认汇总确认一次；--overwrite 跳过该确认。安装不改变当前 generation，需显式 reload。\n详细说明：man zhsh\n";

pub(crate) fn execute(
    shell: &SessionState,
    args: &[String],
    ui: Option<&dyn SafetyManagementUi>,
    port: Option<&dyn SafetyManagementPort>,
) -> BuiltinResult {
    if matches!(args, [value] if matches!(value.as_str(), "help" | "-h" | "--help")) {
        return BuiltinResult::stdout(HELP);
    }
    if matches!(args, [command, value] if command == "install" && matches!(value.as_str(), "help" | "-h" | "--help"))
    {
        return BuiltinResult::stdout(INSTALL_HELP);
    }
    let Some(port) = port else {
        return BuiltinResult::error("zh safety: Safety 运行时不可用\n");
    };
    match args {
        [] => list(port),
        [value] if value == "-t" => report(port.test_candidate(), false),
        [flag, source] if flag == "-t" => {
            let source = match super::plugin_path::resolve_source(shell, source) {
                Ok(source) => source,
                Err(error) => return BuiltinResult::error(format!("zh safety -t: {error}\n")),
            };
            report(port.test_source(source), false)
        }
        [value] if value == "reload" => report(port.reload(), true),
        [command, value] if command == "assess" => assess(shell, value, port),
        [command, rest @ ..] if command == "install" => install(shell, rest, ui, port),
        _ => BuiltinResult::error("zh safety: 参数无效\n运行 `zh safety -h` 查看用法。\n"),
    }
}

fn assess(shell: &SessionState, command: &str, port: &dyn SafetyManagementPort) -> BuiltinResult {
    if command.trim().is_empty() {
        return BuiltinResult::error("zh safety assess: COMMAND 不能为空\n");
    }
    let view = port.assess(
        super::super::AgentCommandPlan::prepare(shell, command),
        shell.agent_trust(),
    );
    let mut output = String::new();
    if view.targets.is_empty() {
        output.push_str("PROGRAM: -\nTARGET: -\n");
    } else {
        for target in view.targets {
            output.push_str(&format!("PROGRAM: {}\n", target.program));
            output.push_str(&format!(
                "TARGET: {}\n",
                target
                    .target
                    .as_deref()
                    .map(terminal_safe_path)
                    .unwrap_or_else(|| "-".into())
            ));
        }
    }
    output.push_str(&format!(
        "SEMANTIC: {}\nRULE: {}\nBINDING: {}\nDECISION: {}\nREASON: {}\n",
        view.semantic, view.rule, view.binding, view.decision, view.reason
    ));
    BuiltinResult::stdout(output)
}

fn install(
    shell: &SessionState,
    args: &[String],
    ui: Option<&dyn SafetyManagementUi>,
    port: &dyn SafetyManagementPort,
) -> BuiltinResult {
    let mut overwrite = false;
    let mut positional = Vec::new();
    let mut options = true;
    for argument in args {
        if options && argument == "--" {
            options = false;
        } else if options && argument == "--overwrite" {
            if overwrite {
                return BuiltinResult::error(
                    "zh safety install: --overwrite 只能指定一次\n运行 `zh safety install -h` 查看用法。\n",
                );
            }
            overwrite = true;
        } else if options && argument.starts_with('-') {
            return BuiltinResult::error(format!(
                "zh safety install: 未知选项 {argument}\n运行 `zh safety install -h` 查看用法。\n"
            ));
        } else {
            positional.push(argument);
        }
    }
    if positional.is_empty() {
        return BuiltinResult::error(
            "zh safety install: 至少需要一个规则文件\n运行 `zh safety install -h` 查看用法。\n",
        );
    }
    let mut sources = Vec::with_capacity(positional.len());
    for source in positional {
        match super::plugin_path::resolve_source(shell, source) {
            Ok(source) => sources.push(source),
            Err(error) => return BuiltinResult::error(format!("zh safety install: {error}\n")),
        }
    }

    let plan = port.plan_install(sources.clone());
    if !plan.success {
        return plan_error(plan);
    }
    let conflict_count = plan
        .entries
        .iter()
        .filter(|entry| entry.state == super::super::SafetyInstallStateView::Conflict)
        .count();
    if conflict_count != 0 && !overwrite {
        let Some(ui) = ui else {
            return BuiltinResult::error(
                "zh safety install: 当前输入不可交互，使用 --overwrite 显式授权覆盖\n",
            );
        };
        let cancellation = Arc::new(CancellationToken::default());
        let _active = ActiveCancellation::register(Arc::clone(&cancellation));
        let decision = ui.confirm_overwrite(
            &SafetyOverwritePrompt {
                new_rules: plan
                    .entries
                    .iter()
                    .filter(|entry| entry.state == super::super::SafetyInstallStateView::New)
                    .count(),
                conflicts: conflict_count,
            },
            &cancellation,
        );
        match decision {
            SafetyOverwriteDecision::Confirm => overwrite = true,
            SafetyOverwriteDecision::Decline => {
                return BuiltinResult::stdout("Safety 安装已取消 · 用户拒绝 · 未写入文件\n")
            }
            SafetyOverwriteDecision::Cancelled => {
                return BuiltinResult::stdout("Safety 安装已取消 · 用户中断 · 未写入文件\n")
            }
            SafetyOverwriteDecision::TimedOut => {
                return BuiltinResult::stdout("Safety 安装已取消 · 确认超时 · 未写入文件\n")
            }
            SafetyOverwriteDecision::Unavailable => {
                return BuiltinResult::error("zh safety install: 无法读取覆盖确认\n")
            }
        }
    }
    install_report(port.install(SafetyInstallRequest {
        sources,
        overwrite,
        expected_plan_id: plan.plan_id,
    }))
}

fn plan_error(plan: SafetyInstallPlan) -> BuiltinResult {
    let mut stderr = String::new();
    for warning in plan.warnings {
        stderr.push_str("! ");
        stderr.push_str(&warning);
        stderr.push('\n');
    }
    for error in plan.errors {
        stderr.push_str("! ");
        stderr.push_str(&error);
        stderr.push('\n');
    }
    BuiltinResult {
        stdout: String::new(),
        stderr,
        code: 1,
    }
}

fn install_report(report: SafetyInstallReport) -> BuiltinResult {
    let mut stderr = String::new();
    for warning in &report.warnings {
        stderr.push_str("! ");
        stderr.push_str(warning);
        stderr.push('\n');
    }
    for error in &report.errors {
        stderr.push_str("! ");
        stderr.push_str(error);
        stderr.push('\n');
    }
    if !report.success {
        return BuiltinResult {
            stdout: String::new(),
            stderr,
            code: 1,
        };
    }
    let rows: Vec<[String; 3]> = report
        .entries
        .iter()
        .map(|entry| {
            [
                entry.name.clone(),
                match entry.outcome {
                    SafetyInstallOutcomeView::Created => "created",
                    SafetyInstallOutcomeView::Replaced => "replaced",
                    SafetyInstallOutcomeView::Identical => "unchanged",
                }
                .into(),
                terminal_safe_path(&entry.destination),
            ]
        })
        .collect();
    let mut stdout = super::table::render(["RULE", "STATE", "DESTINATION"], rows);
    if report.candidate_reloadable {
        stdout.push_str(&format!(
            "\ngeneration {} unchanged · run `zh safety reload` to activate\n",
            report.generation
        ));
    } else {
        stdout.push_str(&format!(
            "\ngeneration {} unchanged · local candidate is invalid\n",
            report.generation
        ));
    }
    BuiltinResult {
        stdout,
        stderr,
        code: 0,
    }
}

fn list(port: &dyn SafetyManagementPort) -> BuiltinResult {
    let catalog = port.catalog();
    let rows: Vec<[String; 8]> = catalog.rows.iter().map(table_row).collect();
    let mut output = super::table::render(
        [
            "ORDER", "SOURCE", "RULE SET", "PROGRAM", "RULES", "STATUS", "MTIME", "FILE",
        ],
        rows,
    );
    output.push_str(&format!("\ngeneration {}\n", catalog.generation));
    BuiltinResult::stdout(output)
}

fn table_row(row: &SafetyRuleRowView) -> [String; 8] {
    [
        row.order.to_string(),
        row.source.as_str().into(),
        row.name.clone(),
        row.program.clone(),
        if row.builtin {
            "builtin".into()
        } else {
            row.rule_count
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".into())
        },
        row.status.as_str().into(),
        row.modified
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| format!("{}.{:09}", value.as_secs(), value.subsec_nanos()))
            .unwrap_or_else(|| "-".into()),
        row.location_or_diagnostic.clone(),
    ]
}

fn report(report: SafetyOperationReport, reloaded: bool) -> BuiltinResult {
    let mut stderr = String::new();
    for warning in &report.warnings {
        stderr.push_str("! ");
        stderr.push_str(warning);
        stderr.push('\n');
    }
    for error in &report.errors {
        stderr.push_str("! ");
        stderr.push_str(error);
        stderr.push('\n');
    }
    if report.success {
        let verb = if reloaded { "reloaded" } else { "valid" };
        let source = report.source.map(|source| {
            format!(
                "type: {}\nversion: {}\nrules: {}\nprograms: {}\nSHA-256: {}\n",
                source.kind, source.version, source.rules, source.programs, source.sha256
            )
        });
        BuiltinResult {
            stdout: source.unwrap_or_else(|| {
                format!(
                    "Safety rules: {verb}\nlocal: {}\nshadowed builtin programs: {}\ngeneration: {}\n",
                    report.local_rules, report.shadowed_builtin_programs, report.generation
                )
            }),
            stderr,
            code: 0,
        }
    } else {
        stderr.push_str(&format!(
            "Safety {} failed: {} error(s) · generation {} unchanged\n",
            if reloaded { "reload" } else { "validation" },
            report.errors.len(),
            report.generation
        ));
        BuiltinResult {
            stdout: String::new(),
            stderr,
            code: 1,
        }
    }
}
