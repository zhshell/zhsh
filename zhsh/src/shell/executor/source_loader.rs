//! Bash `source` 的子进程执行和状态快照解码。
//!
//! 脚本在隔离的 Bash 中执行，并通过权限为 `0600` 的临时文件写出 NUL 分隔快照。
//! 只有快照完整解析后，调用方才会把 cwd、环境、别名、函数、普通变量和 PS0–PS4
//! 提交到会话。

use super::super::SessionState;
use crate::common::{AppError, AppResult};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TemporaryFile {
    path: PathBuf,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn temporary_file(label: &str) -> AppResult<(TemporaryFile, File)> {
    for _ in 0..100 {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zhsh-{label}-{}-{sequence}", std::process::id()));
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((TemporaryFile { path }, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(AppError::io(error.to_string())),
        }
    }
    Err("无法创建临时状态文件".into())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 一次 `source` 执行结束后的完整、待提交会话候选值。
pub(crate) struct Snapshot {
    /// 脚本结束时的绝对工作目录。
    pub(crate) cwd: PathBuf,
    /// 脚本结束时的导出环境。
    pub(crate) env: HashMap<String, String>,
    /// Bash 别名集合。
    pub(crate) aliases: HashMap<String, String>,
    /// Bash 函数的 `declare -f` 定义。
    pub(crate) functions: HashMap<String, String>,
    /// 函数依赖的可重放普通变量声明。
    pub(crate) variables: HashMap<String, String>,
    /// Bash 完成赋值和展开后的 PS0–PS4 字符串值。
    pub(crate) prompt_variables: HashMap<String, String>,
}

fn parse_snapshot(bytes: &[u8]) -> AppResult<Snapshot> {
    let fields: Vec<_> = bytes.split(|byte| *byte == 0).collect();
    let mut index = 0;
    let mut next = || {
        let field = fields
            .get(index)
            .copied()
            .ok_or_else(|| AppError::protocol("状态数据不完整"))?;
        index += 1;
        std::str::from_utf8(field).map_err(|_| AppError::protocol("状态数据不是 UTF-8"))
    };
    if next()? != "ZHSH_SOURCE_4" {
        return Err("脚本没有生成可识别的会话状态".into());
    }
    let cwd = PathBuf::from(next()?);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("脚本结束时的当前目录无效".into());
    }

    let mut env = HashMap::new();
    loop {
        let entry = next()?;
        if entry.is_empty() {
            break;
        }
        let Some((name, value)) = entry.split_once('=') else {
            return Err("脚本生成了无效环境变量".into());
        };
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(AppError::protocol(format!(
                "脚本生成了无效环境变量名: {name}"
            )));
        }
        env.insert(name.to_string(), value.to_string());
    }

    let mut aliases = HashMap::new();
    loop {
        let name = next()?;
        if name.is_empty() {
            break;
        }
        aliases.insert(name.to_string(), next()?.to_string());
    }
    let mut functions = HashMap::new();
    loop {
        let name = next()?;
        if name.is_empty() {
            break;
        }
        functions.insert(name.to_string(), next()?.to_string());
    }
    let mut variables = HashMap::new();
    loop {
        let name = next()?;
        if name.is_empty() {
            break;
        }
        variables.insert(name.to_string(), next()?.to_string());
    }
    let mut prompt_variables = HashMap::new();
    loop {
        let name = next()?;
        if name.is_empty() {
            break;
        }
        if !SessionState::is_prompt_variable(name) {
            return Err(AppError::protocol(format!(
                "脚本生成了无效提示符变量名: {name}"
            )));
        }
        prompt_variables.insert(name.to_string(), next()?.to_string());
    }
    Ok(Snapshot {
        cwd,
        env,
        aliases,
        functions,
        variables,
        prompt_variables,
    })
}

fn write_initial_state(file: &mut File, session: &SessionState) -> AppResult<()> {
    writeln!(file, "shopt -s expand_aliases").map_err(|error| AppError::io(error.to_string()))?;
    for declaration in session.variables.values() {
        writeln!(file, "{declaration}").map_err(|error| AppError::io(error.to_string()))?;
    }
    for definition in session.functions.values() {
        writeln!(file, "{definition}").map_err(|error| AppError::io(error.to_string()))?;
    }
    for (name, value) in &session.aliases {
        writeln!(file, "alias -- {}", shell_quote(&format!("{name}={value}")))
            .map_err(|error| AppError::io(error.to_string()))?;
    }
    file.sync_all()
        .map_err(|error| AppError::io(error.to_string()))
}

const SCRIPT: &str = r#"
if [[ $3 == 1 ]]; then export BASH_ENV=$4; else unset BASH_ENV; fi
source "$1" "${@:5}"
source_status=$?
{
    printf 'ZHSH_SOURCE_4\0%s\0' "$PWD"
    env -0
    printf '\0'
    for name in "${!BASH_ALIASES[@]}"; do
        printf '%s\0%s\0' "$name" "${BASH_ALIASES[$name]}"
    done
    printf '\0'
    while read -r _ _ name; do
        printf '%s\0%s\0' "$name" "$(declare -f "$name")"
    done < <(declare -F)
    printf '\0'
    for __zhsh_name in $(compgen -A variable); do
        case "$__zhsh_name" in
            BASH*|EPOCH*|FUNCNAME|GROUPS|DIRSTACK|PIPESTATUS|RANDOM|SECONDS|LINENO|PPID|EUID|UID|SHELLOPTS|_|__zhsh_*) continue ;;
        esac
        __zhsh_declaration=$(declare -p "$__zhsh_name" 2>/dev/null) || continue
        __zhsh_attributes=${__zhsh_declaration#declare -}
        __zhsh_attributes=${__zhsh_attributes%% *}
        if [[ "$__zhsh_attributes" == *r* || "$__zhsh_attributes" == *x* ]]; then
            continue
        fi
        printf '%s\0%s\0' "$__zhsh_name" "$__zhsh_declaration"
    done
    printf '\0'
    for __zhsh_name in PS0 PS1 PS2 PS3 PS4; do
        if [[ -v "$__zhsh_name" ]]; then
            printf '%s\0%s\0' "$__zhsh_name" "${!__zhsh_name}"
        fi
    done
    printf '\0'
} >| "$2"
exit "$source_status"
"#;

/// 在 Bash 中读取脚本并返回完整状态快照。
///
/// # Arguments
///
/// - `session`：脚本执行前的 cwd、环境及可重放 Bash 状态。
/// - `path`：已经由内建命令定位的脚本路径。
/// - `arguments`：作为脚本位置参数传入的字面参数。
///
/// # Returns
///
/// 返回解析完成的候选 [`Snapshot`] 和脚本自身的退出状态。非零状态不会阻止快照返回，
/// 与 Bash `source` 的“副作用已经发生”语义保持一致。
///
/// # Errors
///
/// 临时文件、Bash 启动、状态读取失败，或快照协议不完整/非法时返回结构化错误。
pub(crate) fn load(
    session: &SessionState,
    path: &Path,
    arguments: &[String],
) -> AppResult<(Snapshot, i32)> {
    let (state_temp, state_file) = temporary_file("source-state")?;
    drop(state_file);
    let (init_temp, mut init_file) = temporary_file("source-init")?;
    write_initial_state(&mut init_file, session)
        .map_err(|error| AppError::io(format!("无法准备初始状态: {error}")))?;
    drop(init_file);

    let original_bash_env = session.env.get("BASH_ENV").cloned();
    let mut command = Command::new("bash");
    command
        .args(["--noprofile", "--norc", "-c", SCRIPT, "zhsh-source"])
        .arg(path)
        .arg(&state_temp.path)
        .arg(if original_bash_env.is_some() {
            "1"
        } else {
            "0"
        })
        .arg(original_bash_env.as_deref().unwrap_or(""))
        .args(arguments)
        .current_dir(&session.cwd)
        .env_clear()
        .envs(&session.env)
        .env("BASH_ENV", &init_temp.path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let status = run_source_command(&mut command)?;
    let bytes = std::fs::read(&state_temp.path)
        .map_err(|error| AppError::io(format!("无法读取会话状态: {error}")))?;
    parse_snapshot(&bytes).map(|snapshot| (snapshot, status))
}

#[cfg(unix)]
fn run_source_command(command: &mut Command) -> AppResult<i32> {
    let mut child = command
        .spawn()
        .map_err(|error| AppError::io(format!("无法启动 Bash: {error}")))?;
    let process_group = child.id();
    let terminal = match super::interactive::ForegroundTerminal::give_to(process_group) {
        Ok(terminal) => Some(terminal),
        Err(error) if super::foreground::terminal_control_is_unavailable(&error) => None,
        Err(error) => {
            super::foreground::terminate_process_group(&mut child, process_group);
            return Err(AppError::io(format!("无法交出前台终端: {error}")));
        }
    };
    let state = super::foreground::wait_child(&child);
    drop(terminal);
    match state.map_err(|error| AppError::io(format!("等待 Bash 失败: {error}")))? {
        super::foreground::ChildState::Exited { code, .. } => Ok(code),
        super::foreground::ChildState::Stopped(_) => {
            super::foreground::terminate_process_group(&mut child, process_group);
            Err(AppError::input(
                "source 执行被暂停；状态型 builtin 不支持恢复，进程组已终止",
            ))
        }
        super::foreground::ChildState::Running => {
            Err(AppError::internal("阻塞等待意外返回运行状态"))
        }
    }
}

#[cfg(not(unix))]
fn run_source_command(command: &mut Command) -> AppResult<i32> {
    command
        .status()
        .map_err(|error| AppError::io(format!("无法启动 Bash: {error}")))
        .map(|status| status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nul_delimited_state_without_losing_spaces() {
        let snapshot = parse_snapshot(
            b"ZHSH_SOURCE_4\0/tmp\0A=value with space\0EMPTY=\0\0ll\0ls -l\0\0dev\0dev () {\n printf ok\n}\0\0LOCAL\0declare -- LOCAL=\"value\"\0\0PS1\0\\u:\\w\\$ \0PS4\0+ \0\0",
        )
        .unwrap();
        assert_eq!(snapshot.cwd, Path::new("/tmp"));
        assert_eq!(
            snapshot.env.get("A").map(String::as_str),
            Some("value with space")
        );
        assert_eq!(
            snapshot.aliases.get("ll").map(String::as_str),
            Some("ls -l")
        );
        assert!(snapshot.functions.get("dev").unwrap().contains("printf ok"));
        assert_eq!(
            snapshot.variables.get("LOCAL").map(String::as_str),
            Some("declare -- LOCAL=\"value\"")
        );
        assert_eq!(
            snapshot.prompt_variables.get("PS1").map(String::as_str),
            Some(r"\u:\w\$ ")
        );
    }
}
