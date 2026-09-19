//! Native 的内核直接进程执行。所有入口均不接收 SessionState。

use super::super::native::{NativeExecutionError, NativeNotStartedReason};
use super::{
    captured, interactive, BashExecutor, CapturedExecution, OutputMode, AGENT_COMMAND_OUTPUT_LIMIT,
};
use crate::common::{AppError, CancellationToken};
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

pub(in super::super) struct ExternalEnvironment<'a> {
    pub(in super::super) cwd: &'a Path,
    pub(in super::super) exported: &'a HashMap<String, String>,
}

#[cfg(target_os = "linux")]
struct ExecPayload {
    path: CString,
    _arguments: Vec<CString>,
    _environment: Vec<CString>,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
}
// SAFETY: pointer tables only reference the owned CString heap allocations. After construction
// neither strings nor tables are mutated. Moving the payload does not move those allocations.
// The pre_exec callback borrows it, and Command owns it for the entire spawn handshake.
#[cfg(target_os = "linux")]
unsafe impl Send for ExecPayload {}
#[cfg(target_os = "linux")]
unsafe impl Sync for ExecPayload {}

#[cfg(target_os = "linux")]
impl ExecPayload {
    fn execute(&self) -> io::Result<()> {
        // SAFETY: all strings and null-terminated pointer tables were built before fork and remain
        // alive. execve is async-signal-safe; failure returns errno without allocation or logging.
        unsafe {
            libc::execve(self.path.as_ptr(), self.argv.as_ptr(), self.envp.as_ptr());
        }
        Err(io::Error::last_os_error())
    }
}

fn command(
    environment: ExternalEnvironment<'_>,
    path: &Path,
    program: &str,
    arguments: &[OsString],
    mode: OutputMode,
) -> Result<Command, NativeExecutionError> {
    #[cfg(not(target_os = "linux"))]
    return Err(NativeExecutionError::not_started(
        NativeNotStartedReason::UnsupportedFormat,
        "当前平台尚不支持 Native 执行",
    ));
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::{ffi::OsStrExt, process::CommandExt};
        let invalid = || {
            NativeExecutionError::not_started(
                NativeNotStartedReason::InvalidRequest,
                "Native 启动参数或环境无效",
            )
        };
        if !path.is_absolute() {
            return Err(invalid());
        }
        let path_c = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid())?;
        let args = std::iter::once(OsStr::new(program))
            .chain(arguments.iter().map(OsString::as_os_str))
            .map(|arg| CString::new(arg.as_bytes()).map_err(|_| invalid()))
            .collect::<Result<Vec<_>, _>>()?;
        let env = environment
            .exported
            .iter()
            .map(|(key, value)| {
                if key.is_empty() || key.contains('=') {
                    return Err(invalid());
                }
                CString::new(format!("{key}={value}")).map_err(|_| invalid())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let argv = args
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        let envp = env
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        let payload = ExecPayload {
            path: path_c,
            _arguments: args,
            _environment: env,
            argv,
            envp,
        };
        let mut command = Command::new(path);
        command
            .args(arguments)
            .arg0(program)
            .current_dir(environment.cwd)
            .env_clear()
            .envs(environment.exported);
        match mode {
            OutputMode::Inherit => {
                command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
            OutputMode::Capture => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            }
            OutputMode::ForegroundCapture => {
                command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            }
        }
        // SAFETY: this callback performs only the preallocated payload's execve/errno operations.
        // It never returns Ok, so the standard library cannot proceed to an execvp fallback.
        unsafe {
            command.pre_exec(move || payload.execute());
        }
        Ok(command)
    }
}

fn spawn_error(error: io::Error) -> NativeExecutionError {
    if error.raw_os_error() == Some(libc::ENOENT) {
        return NativeExecutionError::not_started(
            NativeNotStartedReason::ExecutableNotFound,
            format!("Native 程序或其加载解释器不存在: {error}"),
        );
    }
    if matches!(
        error.raw_os_error(),
        Some(
            libc::ENOENT
                | libc::ENOEXEC
                | libc::EACCES
                | libc::EPERM
                | libc::ENOTDIR
                | libc::ELOOP
                | libc::ETXTBSY
                | libc::E2BIG
                | libc::EINVAL
        )
    ) {
        NativeExecutionError::not_started(
            NativeNotStartedReason::SpawnFailed,
            format!("无法启动 Native 外部命令: {error}"),
        )
    } else {
        NativeExecutionError::Execution(AppError::io(format!("Native 启动通道失败: {error}")))
    }
}

pub(in super::super) fn requires_terminal(program: &str, arguments: &[OsString]) -> bool {
    terminal_mode(program, arguments).requires_terminal()
}

fn terminal_mode(program: &str, arguments: &[OsString]) -> interactive::TerminalMode {
    let words = std::iter::once(program.to_owned())
        .chain(arguments.iter().map(|s| s.to_string_lossy().into_owned()))
        .collect::<Vec<_>>();
    interactive::terminal_mode_words(&words)
}

impl BashExecutor {
    pub(in super::super) fn run_native_user(
        &mut self,
        environment: ExternalEnvironment<'_>,
        path: &Path,
        program: &str,
        arguments: &[OsString],
    ) -> Result<i32, NativeExecutionError> {
        let mut command = command(environment, path, program, arguments, OutputMode::Inherit)?;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().map_err(spawn_error)?;
        #[cfg(unix)]
        {
            self.foreground
                .wait(child, program.to_owned())
                .map_err(|e| NativeExecutionError::Execution(AppError::io(e.to_string())))
        }
        #[cfg(not(unix))]
        {
            let mut child = child;
            child
                .wait()
                .map(|s| s.code().unwrap_or(1))
                .map_err(|e| NativeExecutionError::Execution(AppError::io(e.to_string())))
        }
    }

    pub(in super::super) fn run_native_agent(
        &self,
        environment: ExternalEnvironment<'_>,
        path: &Path,
        program: &str,
        arguments: &[OsString],
        cancellation: &CancellationToken,
    ) -> Result<Option<CapturedExecution>, NativeExecutionError> {
        let mode = terminal_mode(program, arguments);
        let output = match mode {
            interactive::TerminalMode::None => OutputMode::Capture,
            interactive::TerminalMode::Captured => OutputMode::ForegroundCapture,
            interactive::TerminalMode::Opaque => OutputMode::Inherit,
        };
        let mut command = command(environment, path, program, arguments, output)?;
        let Some(mut child) = cancellation.spawn(&mut command).map_err(spawn_error)? else {
            return Ok(None);
        };
        #[cfg(unix)]
        if mode.requires_terminal() {
            let pgid = child.id();
            let terminal = interactive::ForegroundTerminal::give_to(pgid).map_err(|e| {
                super::abort_foreground_spawn(&mut child, pgid, cancellation);
                NativeExecutionError::Execution(AppError::io(e.to_string()))
            })?;
            let result = if mode == interactive::TerminalMode::Captured {
                captured::wait_foreground_captured(child, cancellation)
            } else {
                Ok(captured::wait_interactive(child, cancellation))
            };
            drop(terminal);
            return result.map(Some).map_err(NativeExecutionError::Execution);
        }
        captured::wait(child, cancellation, AGENT_COMMAND_OUTPUT_LIMIT)
            .map(Some)
            .map_err(NativeExecutionError::Execution)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn native_spawn_never_falls_back_to_shell() {
        let root = std::env::temp_dir().join(format!(
            "zhsh-native-spawn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("probe");
        let env = HashMap::new();
        for bytes in [
            b"printf unsafe > sentinel\n".as_slice(),
            b"\x7fELFbroken".as_slice(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            // The actual spawn primitive must not interpret ENOEXEC text as Shell input.
            let error = command(
                ExternalEnvironment {
                    cwd: &root,
                    exported: &env,
                },
                &path,
                "probe",
                &[],
                OutputMode::Capture,
            )
            .unwrap()
            .spawn()
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::ENOEXEC));
            assert!(matches!(
                spawn_error(error),
                NativeExecutionError::NotStarted { .. }
            ));
            assert!(!root.join("sentinel").exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_execve_preserves_literal_arguments_and_wait_protocol() {
        let env = HashMap::new();
        let args: Vec<OsString> = ["[%s]", "a b", "", "$HOME"]
            .into_iter()
            .map(Into::into)
            .collect();
        let output = command(
            ExternalEnvironment {
                cwd: Path::new("/"),
                exported: &env,
            },
            Path::new("/usr/bin/printf"),
            "printf",
            &args,
            OutputMode::Capture,
        )
        .unwrap()
        .output()
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"[a b][][$HOME]");
        assert!(matches!(
            spawn_error(io::Error::from_raw_os_error(libc::EIO)),
            NativeExecutionError::Execution(_)
        ));
    }
}
