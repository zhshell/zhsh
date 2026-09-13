//! Native 的 ELF 验证与直接进程执行。所有入口均不接收 SessionState。

use super::super::native::{NativeExecutionError, NativeNotStartedReason, NativePreparationError};
use super::{
    captured, interactive, BashExecutor, CapturedExecution, OutputMode, AGENT_COMMAND_OUTPUT_LIMIT,
};
use crate::common::{AppError, CancellationToken};
use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};

pub(in super::super) struct ExternalEnvironment<'a> {
    pub(in super::super) cwd: &'a Path,
    pub(in super::super) exported: &'a HashMap<String, String>,
}

pub(in super::super) fn validate_native_binary(path: &Path) -> Result<(), NativePreparationError> {
    #[cfg(not(target_os = "linux"))]
    return Err(NativePreparationError::UnsupportedFormat(
        "当前平台尚不支持 Native 二进制执行".into(),
    ));
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| NativePreparationError::ReadFailed(e.to_string()))?;
        if !file
            .metadata()
            .map_err(|e| NativePreparationError::ReadFailed(e.to_string()))?
            .is_file()
        {
            return Err(NativePreparationError::UnsupportedFormat(
                "Native 目标不是普通文件".into(),
            ));
        }
        let mut bytes = Vec::with_capacity(64);
        file.by_ref()
            .take(64)
            .read_to_end(&mut bytes)
            .map_err(|e| NativePreparationError::ReadFailed(e.to_string()))?;
        if !valid_elf_header(&bytes) {
            return Err(NativePreparationError::UnsupportedFormat(
                "Native 本期仅执行 ELF 二进制，不支持脚本或该文件格式".into(),
            ));
        }
        Ok(())
    }
}

fn valid_elf_header(b: &[u8]) -> bool {
    if b.len() < 16 || &b[..4] != b"\x7fELF" || !matches!(b[5], 1 | 2) || b[6] != 1 {
        return false;
    }
    let (length, offset) = match b[4] {
        1 => (52, 40),
        2 => (64, 52),
        _ => return false,
    };
    if b.len() < length {
        return false;
    }
    let u16_at = |i| {
        if b[5] == 1 {
            u16::from_le_bytes([b[i], b[i + 1]])
        } else {
            u16::from_be_bytes([b[i], b[i + 1]])
        }
    };
    let version = [b[20], b[21], b[22], b[23]];
    matches!(u16_at(16), 2 | 3)
        && u16_at(offset) == length as u16
        && (if b[5] == 1 {
            u32::from_le_bytes(version)
        } else {
            u32::from_be_bytes(version)
        }) == 1
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
            assert!(validate_native_binary(&path).is_err());
            // Bypass the format guard to prove the actual spawn primitive does not invoke sh.
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

    #[test]
    fn native_elf_header_is_bounded_and_checks_class_endianness_type_and_size() {
        for class in [1, 2] {
            for endian in [1, 2] {
                let (length, offset) = if class == 1 { (52, 40) } else { (64, 52) };
                let mut b = vec![0; length];
                b[..4].copy_from_slice(b"\x7fELF");
                b[4] = class;
                b[5] = endian;
                b[6] = 1;
                let ty = if endian == 1 {
                    3_u16.to_le_bytes()
                } else {
                    3_u16.to_be_bytes()
                };
                let version = if endian == 1 {
                    1_u32.to_le_bytes()
                } else {
                    1_u32.to_be_bytes()
                };
                let size = if endian == 1 {
                    (length as u16).to_le_bytes()
                } else {
                    (length as u16).to_be_bytes()
                };
                b[16..18].copy_from_slice(&ty);
                b[20..24].copy_from_slice(&version);
                b[offset..offset + 2].copy_from_slice(&size);
                assert!(valid_elf_header(&b));
                assert!(!valid_elf_header(&b[..length - 1]));
                b[16..18].fill(0);
                assert!(!valid_elf_header(&b));
            }
        }
    }
}
