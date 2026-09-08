//! `source`/`.`：用 Bash 执行脚本，并将可持续的会话状态（包括 PS0–PS4）同步回 zhsh。
//!
//! 用法：`source 文件 [参数 ...]` 或 `. 文件 [参数 ...]`

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
