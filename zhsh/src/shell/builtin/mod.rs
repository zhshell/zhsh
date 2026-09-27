//! 按用户命令名组织的内建命令入口与使用说明。
//!
//! 顶层命令各占一个同名文件；`.` 共用 source.rs，`type` 使用 r#type 模块标识符。
//! zh 子命令及其辅助代码位于 zh/，共享结果与参数辅助位于 support/。
//! 命令注册和路由保持在 command 层；目录组织不合并默认委托模式与 Native 的执行行为。

mod support;
pub(super) use support::job_control::{error as job_error, JobContext};
pub(crate) use support::result::BuiltinResult;

pub(crate) mod alias;
pub(crate) mod cd;
pub(crate) mod dirs;
pub(crate) mod exit;
pub(crate) mod export;
pub(crate) mod fg;
pub(crate) mod help;
pub(crate) mod history;
pub(crate) mod popd;
pub(crate) mod pushd;
pub(crate) mod pwd;
pub(crate) mod source;
pub(crate) mod r#type;
pub(crate) mod umask;
pub(crate) mod unalias;
pub(crate) mod unset;
pub(crate) mod zh;

pub(crate) mod bg;
pub(crate) mod disown;
pub(crate) mod jobs;
pub(crate) mod kill;
pub(crate) mod set;
pub(crate) mod shopt;
pub(crate) mod suspend;
pub(crate) mod wait;
