//! `cd`：切换当前会话的工作目录。
//!
//! # 用法
//! `cd [-L|-P] [--] [目录]`；省略目录进入 HOME。
//!
//! # 参数与选项
//! `-L` 保留逻辑路径（默认），`-P` 解析符号链接，重复时后者生效。
//! 目录 `-` 使用 OLDPWD；支持本命令内的 `~`/`~/`，不提供通用展开。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! cd /tmp
//! pwd
//! ```
//! 在 /tmp 存在且可访问时切换目录，后续 pwd 显示新的会话目录。
//!
//! # 输出与退出状态
//! 通常无 stdout；`cd -` 成功时打印目标目录。成功 0；目录、环境或参数错误为 1，写 stderr。
//!
//! # 状态影响
//! 成功更新 cwd、PWD、OLDPWD，供后续命令使用；目标验证失败不切换 cwd。
//! 默认模式 Agent 不直接执行该状态操作；Native Agent 保留现有 Safety 与确认流程。 委托模式管道中的 cd 由 Bash 子环境处理。

use super::super::SessionState;
use super::BuiltinResult;
use std::path::{Component, Path, PathBuf};

fn logical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !path.is_absolute() {
                    normalized.push(component);
                }
            }
            _ => normalized.push(component),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        normalized
    }
}

fn diagnostic(target: &Path, error: &std::io::Error) -> String {
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "没有那个文件或目录".to_string(),
        std::io::ErrorKind::PermissionDenied => "权限不够".to_string(),
        _ => error.to_string(),
    };
    format!("cd: {}: {reason}\n", target.display())
}

/// 验证并提交一次工作目录切换。
///
/// # Arguments
///
/// - `shell`：接收 cwd、PWD 和 OLDPWD 更新的会话。
/// - `target`：已完成参数解析、尚未展开 `~` 的目标。
/// - `physical`：为 `true` 时解析符号链接，否则保留逻辑路径。
///
/// 目标无效时不修改 cwd；环境更新和 cwd 提交由本函数统一完成。
pub(crate) fn change_directory(
    shell: &mut SessionState,
    target: &str,
    physical: bool,
) -> BuiltinResult {
    let expanded = if target == "~" {
        let Some(home) = shell.env.get("HOME") else {
            return BuiltinResult::error("cd: HOME 未设置\n");
        };
        PathBuf::from(home)
    } else if let Some(rest) = target.strip_prefix("~/") {
        let Some(home) = shell.env.get("HOME") else {
            return BuiltinResult::error("cd: HOME 未设置\n");
        };
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(target)
    };
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        shell.cwd.join(expanded)
    };

    let metadata = match std::fs::metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) => return BuiltinResult::error(diagnostic(&candidate, &error)),
    };
    if !metadata.is_dir() {
        return BuiltinResult::error(format!("cd: {}: 不是目录\n", candidate.display()));
    }

    let destination = if physical {
        match std::fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(error) => return BuiltinResult::error(diagnostic(&candidate, &error)),
        }
    } else {
        logical_path(&candidate)
    };

    // SessionState 是 cwd 和 PWD/OLDPWD 的唯一状态源；子进程执行器显式使用它。
    let old = shell.cwd.clone();
    if let Err(error) = shell.set_env("OLDPWD", &old.to_string_lossy()) {
        return BuiltinResult::error(format!("cd: 无法更新 OLDPWD: {error}\n"));
    }
    if let Err(error) = shell.set_env("PWD", &destination.to_string_lossy()) {
        let _ = shell.set_env("OLDPWD", &old.to_string_lossy());
        return BuiltinResult::error(format!("cd: 无法更新 PWD: {error}\n"));
    }
    shell.cwd = destination;
    BuiltinResult::ok()
}

/// 解析 `cd` 选项和操作数，并调用 [`change_directory`] 提交状态。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    let mut physical = false;
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        match argument.as_str() {
            "-L" => physical = false,
            "-P" => physical = true,
            "--" => {
                index += 1;
                break;
            }
            _ => break,
        }
        index += 1;
    }
    let operands = &args[index..];
    if operands.len() > 1 {
        return BuiltinResult::error("cd: 参数过多\n");
    }

    let (target, print_destination) = match operands.first().map(String::as_str) {
        None => {
            let Some(home) = shell.env.get("HOME").cloned() else {
                return BuiltinResult::error("cd: HOME 未设置\n");
            };
            (home, false)
        }
        Some("-") => {
            let Some(oldpwd) = shell.env.get("OLDPWD").cloned() else {
                return BuiltinResult::error("cd: OLDPWD 未设置\n");
            };
            (oldpwd, true)
        }
        Some(target) => (target.to_string(), false),
    };

    let result = change_directory(shell, &target, physical);
    if result.code == 0 && print_destination {
        BuiltinResult::stdout(format!("{}\n", shell.cwd.display()))
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_oldpwd_and_extra_arguments_without_state_changes() {
        let mut shell = SessionState::test();
        shell.env.remove("OLDPWD");
        let original = shell.cwd.clone();
        assert_eq!(execute(&mut shell, &["-".into()]).code, 1);
        assert_eq!(execute(&mut shell, &["/".into(), "/tmp".into()]).code, 1);
        assert_eq!(shell.cwd, original);
    }

    #[test]
    fn reports_regular_files_as_not_directories() {
        let mut shell = SessionState::test();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let result = execute(&mut shell, &[source.to_string_lossy().into_owned()]);
        assert_eq!(result.code, 1);
        assert!(result.stderr.contains("不是目录"));
    }
}
