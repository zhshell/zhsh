//! 内建命令共享的结构化结果；保持 stdout/stderr、状态及捕获拼接约定。

/// 内建命令返回给用户执行门面的结构化输出。
pub(crate) struct BuiltinResult {
    /// 写入标准输出的完整文本。
    pub(crate) stdout: String,
    /// 写入标准错误的完整文本。
    pub(crate) stderr: String,
    /// Bash 风格退出状态，成功通常为 `0`。
    pub(crate) code: i32,
}

impl BuiltinResult {
    /// 创建没有输出的成功结果。
    pub(crate) fn ok() -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            code: 0,
        }
    }

    /// 创建只包含标准输出的成功结果。
    pub(crate) fn stdout(output: impl Into<String>) -> Self {
        Self {
            stdout: output.into(),
            stderr: String::new(),
            code: 0,
        }
    }

    /// 创建状态为 `1`、只包含标准错误的失败结果。
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: message.into(),
            code: 1,
        }
    }

    /// 按 stdout、stderr 的目标流输出结果。
    pub(crate) fn emit(&self) {
        print!("{}", self.stdout);
        eprint!("{}", self.stderr);
    }

    /// 按 stdout 后接 stderr 的顺序合并捕获文本。
    ///
    /// Agent 捕获内建命令时使用；不保留两个流的实时交错顺序。
    pub(crate) fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}
