//! 当前 Native 字面命令的逐行 source；不执行 Bash 状态导入。
use super::*;
use std::io::Read;

const SOURCE_LIMIT: u64 = 1024 * 1024;
const SOURCE_DEPTH: usize = 16;

impl Shell {
    pub(super) fn run_native_source(
        &mut self,
        arguments: &[String],
        history: &[String],
        cancellation: Option<&CancellationToken>,
        depth: usize,
    ) -> Result<CapturedExecution, NativeExecutionError> {
        let capture = cancellation.is_some();
        let failure =
            |message: String| builtin_execution(BuiltinResult::error(message), capture, false);
        if arguments.len() != 1 {
            return Ok(failure(
                "source: Native 用法: source 文件；位置参数尚未实现\n".into(),
            ));
        }
        if depth >= SOURCE_DEPTH {
            return Ok(failure("source: Native 嵌套超过 16 层\n".into()));
        }
        let name = &arguments[0];
        let expanded = if name == "~" || name.starts_with("~/") {
            match self.state.env.get("HOME") {
                Some(home) => PathBuf::from(home).join(name.strip_prefix("~/").unwrap_or("")),
                None => return Ok(failure("source: HOME 未设置\n".into())),
            }
        } else {
            PathBuf::from(name)
        };
        let path = if expanded.is_absolute() {
            expanded
        } else if name.contains('/') {
            self.state.cwd.join(expanded)
        } else {
            self.state
                .env
                .get("PATH")
                .map(String::as_str)
                .unwrap_or("")
                .split(':')
                .map(|entry| {
                    let dir = Path::new(entry);
                    if dir.is_absolute() {
                        dir.join(name)
                    } else {
                        self.state.cwd.join(dir).join(name)
                    }
                })
                .find(|path| path.is_file())
                .unwrap_or_else(|| self.state.cwd.join(name))
        };
        let read = (|| -> std::io::Result<String> {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let file = options.open(&path)?;
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("不是普通文件"));
            }
            let mut text = String::new();
            file.take(SOURCE_LIMIT + 1).read_to_string(&mut text)?;
            if text.len() as u64 > SOURCE_LIMIT {
                return Err(std::io::Error::other("Native source 文件超过 1 MiB"));
            }
            Ok(text)
        })();
        let text = match read {
            Ok(text) => text,
            Err(error) => {
                return Ok(failure(format!(
                    "source: {}\n",
                    safe_diagnostic(&error.to_string())
                )))
            }
        };
        let mut result = builtin_execution(BuiltinResult::ok(), capture, false);
        for (index, line) in text.lines().enumerate() {
            if self.state.should_exit {
                break;
            }
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                result.exit_code = 130;
                result.termination = CommandTermination::Interrupted;
                result.output_evidence = OutputEvidence::Partial;
                break;
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let plan = match self.prepare_native_agent_command(line) {
                Ok(plan) => plan,
                Err(error) => {
                    let message = format!(
                        "source: 第 {} 行: {}\n",
                        index + 1,
                        safe_diagnostic(&error.to_string())
                    );
                    if capture {
                        result.output.push_str(&message);
                    } else {
                        eprint!("{message}");
                    }
                    result.exit_code = error.code();
                    break;
                }
            };
            let next = match self.execute_native_plan(plan, history, cancellation, depth + 1) {
                Ok(Some(next)) => next,
                Ok(None) => {
                    result.exit_code = 130;
                    result.termination = CommandTermination::Interrupted;
                    result.output_evidence = OutputEvidence::Partial;
                    break;
                }
                Err(error) => {
                    let message = format!(
                        "source: 第 {} 行: {}\n",
                        index + 1,
                        safe_diagnostic(&error.to_string())
                    );
                    if capture {
                        result.output.push_str(&message);
                    } else {
                        eprint!("{message}");
                    }
                    result.exit_code = error.code();
                    break;
                }
            };
            result.exit_code = next.exit_code;
            result.total_output_bytes = result
                .total_output_bytes
                .saturating_add(next.total_output_bytes);
            result.output.push_str(&next.output);
            if next.output_evidence != OutputEvidence::Complete {
                result.output_evidence = next.output_evidence;
            }
            if result.output.len() > SOURCE_LIMIT as usize {
                let mut end = SOURCE_LIMIT as usize;
                while !result.output.is_char_boundary(end) {
                    end -= 1;
                }
                result.output.truncate(end);
                result.output_evidence = OutputEvidence::Truncated;
                result.termination = CommandTermination::OutputLimit;
                result.exit_code = 1;
                break;
            }
            if next.termination != CommandTermination::Exited {
                result.termination = next.termination;
                break;
            }
        }
        result.total_output_bytes = result.total_output_bytes.max(result.output.len());
        Ok(result)
    }
}
