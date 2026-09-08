//! 内建命令实现。
//!
//! 命令元数据、参数解析和路由位于 `command` 层；本模块只保存各命令行为。

pub(crate) mod alias;
pub(crate) mod cd;
pub(crate) mod codec;
pub(crate) mod dirs;
pub(crate) mod exit;
pub(crate) mod export;
pub(crate) mod fg;
pub(crate) mod help;
pub(crate) mod history;
pub(crate) mod llm_config;
mod plugin_path;
pub(crate) mod popd;
pub(crate) mod pushd;
pub(crate) mod pwd;
pub(crate) mod safety;
pub(crate) mod source;
mod table;
pub(crate) mod trust;
pub(crate) mod r#type;
pub(crate) mod umask;
pub(crate) mod unalias;
pub(crate) mod unset;
pub(crate) mod zh;

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
