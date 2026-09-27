//! `source` / `.`：在当前会话加载脚本，各模式保留独立执行设施。
//!
//! # 用法
//! 默认模式：`source 文件 [参数 ...]` 或 `. 文件 [参数 ...]`。
//! Native：`source 文件` 或 `. 文件`，仅一个文件参数。
//!
//! # 参数与选项
//! 文件按现有 cwd/PATH 和路径规则定位，支持本命令的 ~ 路径。
//! 默认模式将其余参数交给 Bash；Native 不支持位置参数，保留 1 MiB 文件与 16 层嵌套限制。
//!
//! # 模式与上下文
//! 本文件的 execute 只供默认模式调用 Bash source loader。Native 经 shell/native/source.rs 逐行执行当前支持的 Native 命令，不能调用该 loader。
//!
//! # 示例
//! 先准备可读的 example.zh，内容为一行 `export EXAMPLE=value`：
//! ```sh
//! source ./example.zh
//! ```
//! 两种模式成功后，后续命令均可读取导出的变量。
//!
//! # 输出与退出状态
//! 默认模式脚本通过执行设施输出，返回脚本状态；定位/同步失败为 1，诊断写 stderr。
//! Native 保留各行输出和最终状态；准备失败停止读取，取消、exit 或输出上限按 Native 执行层返回，不能将所有失败归为 1。
//!
//! # 状态影响
//! 默认模式取得有效快照后同步目录、变量、别名、函数与提示符；Native 逐行提交，后续失败不回滚先前效果。
//! 默认 Agent 不直接 source；Native 按整体命令确认并保留捕获/取消边界。委托模式管道 source 在 Bash 子环境处理。

use super::super::{executor::source_loader, SessionState};
use super::BuiltinResult;
use std::path::PathBuf;

fn locate(session: &SessionState, name: &str) -> Option<PathBuf> {
    if name == "~" || name.starts_with("~/") {
        let home = PathBuf::from(session.env.get("HOME")?);
        let path = if name == "~" {
            home
        } else {
            home.join(&name[2..])
        };
        return path.is_file().then_some(path);
    }
    if name.contains('/') {
        let path = PathBuf::from(name);
        let path = if path.is_absolute() {
            path
        } else {
            session.cwd.join(path)
        };
        return path.is_file().then_some(path);
    }
    for directory in session
        .env
        .get("PATH")
        .map(String::as_str)
        .unwrap_or("")
        .split(':')
    {
        let directory = if directory.is_empty() {
            session.cwd.clone()
        } else {
            PathBuf::from(directory)
        };
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let local = session.cwd.join(name);
    local.is_file().then_some(local)
}

/// 定位并执行脚本，在完整快照成功后提交可持续会话状态。
///
/// `session` 在快照读取/校验失败时保持不变；脚本非零退出仍可提交脚本已经产生的有效
/// 状态，并把脚本状态码返回给调用方。
pub(crate) fn execute(session: &mut SessionState, args: &[String]) -> BuiltinResult {
    let Some((name, source_args)) = args.split_first() else {
        return BuiltinResult::error("source: 用法: source 文件 [参数 ...]\n");
    };
    let Some(path) = locate(session, name) else {
        return BuiltinResult::error(format!("source: {name}: 没有那个文件或目录\n"));
    };
    let (mut snapshot, status) = match source_loader::load(session, &path, source_args) {
        Ok(result) => result,
        Err(error) => return BuiltinResult::error(format!("source: {error}\n")),
    };

    // Bash 子进程自身会改变这两个值；source 不应因此污染父会话。
    for name in ["SHLVL", "_"] {
        if let Some(value) = session.env.get(name) {
            snapshot.env.insert(name.into(), value.clone());
        } else {
            snapshot.env.remove(name);
        }
    }
    if let Err(error) = session.replace_environment(snapshot.env) {
        return BuiltinResult::error(format!("source: 无法同步环境变量: {error}\n"));
    }
    session.cwd = snapshot.cwd;
    session.aliases = snapshot.aliases;
    session.functions = snapshot.functions;
    session.variables = snapshot.variables;
    session.prompt_variables = snapshot.prompt_variables;
    session.normalize_prompt_variables();
    BuiltinResult {
        stdout: String::new(),
        stderr: String::new(),
        code: status,
    }
}
