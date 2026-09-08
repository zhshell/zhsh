//! 各职责包共享的最小运行时基础设施。
//!
//! 本包提供结构化错误和单次任务取消原语，不依赖 LLM、Shell、Agent 或 REPL。
//! 只有内建命令或 REPL 边界负责把 [`AppError`] 格式化为终端文本。

mod cancellation;
mod private_file;

pub(crate) use cancellation::{cancel_active, ActiveCancellation, CancellationToken};
pub(crate) use private_file::{
    ensure_private_tree, persist_private_file, read_file_snapshot, rollback_created_private_file,
    terminal_safe_path, PersistOutcome, PersistPolicy, PersistReceipt,
};

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// 用户输入或配置值无效。
    Input,
    /// 文件系统、终端或子进程 I/O 失败。
    Io,
    /// HTTP 传输失败或服务端返回失败状态。
    Http,
    /// 外部数据不符合约定的响应或持久化协议。
    Protocol,
    /// 当前操作因用户取消而停止。
    Cancelled,
    /// 不属于上述边界的内部不变量错误。
    Internal,
}

/// 带稳定类别和可展示消息的应用错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppError {
    kind: ErrorKind,
    message: String,
}

impl AppError {
    /// 使用指定类别和消息构造错误。
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// 构造用户输入错误。
    pub(crate) fn input(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Input, message)
    }

    /// 构造 I/O 边界错误。
    pub(crate) fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }

    /// 构造 HTTP 错误。
    pub(crate) fn http(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Http, message)
    }

    /// 构造外部协议错误。
    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }

    /// 构造使用统一中文消息的取消错误。
    pub(crate) fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "已取消")
    }

    /// 构造携带具体交互原因的取消错误。
    pub(crate) fn cancelled_with(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Cancelled, message)
    }

    /// 构造内部错误。
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// 返回错误的稳定分类。
    pub(crate) fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(message: String) -> Self {
        Self::internal(message)
    }
}

impl From<&str> for AppError {
    fn from(message: &str) -> Self {
        Self::internal(message)
    }
}

/// 跨层操作统一使用的结果类型。
pub(crate) type AppResult<T> = Result<T, AppError>;
