//! 进程启动时固定的用户状态根。
//!
//! `HOME` 只在 REPL 组合边界读取一次。无效值不会向 cwd、passwd 数据库或其他环境变量
//! 回退；较低 package 只接收已经验证的绝对 [`Path`]。

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// 已验证、UTF-8 可表示的绝对 HOME。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UserHome(PathBuf);

/// HOME 无法作为 zhsh 用户状态根的稳定原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InvalidHomeReason {
    Missing,
    Empty,
    NotAbsolute,
    NonUtf8,
}

impl fmt::Display for InvalidHomeReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "HOME 未设置",
            Self::Empty => "HOME 为空",
            Self::NotAbsolute => "HOME 不是绝对路径",
            Self::NonUtf8 => "HOME 不是有效 UTF-8 路径",
        })
    }
}

/// 启动时 HOME 的非致命解析结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum UserHomeState {
    Available(UserHome),
    Unavailable(InvalidHomeReason),
}

impl UserHomeState {
    /// 读取进程 HOME，并且不对缺失或非法值作隐式回退。
    pub(super) fn from_process() -> Self {
        Self::from_value(std::env::var_os("HOME"))
    }

    /// 校验调用方提供的原始环境值。
    fn from_value(value: Option<OsString>) -> Self {
        let Some(value) = value else {
            return Self::Unavailable(InvalidHomeReason::Missing);
        };
        if value.is_empty() {
            return Self::Unavailable(InvalidHomeReason::Empty);
        }
        if value.to_str().is_none() {
            return Self::Unavailable(InvalidHomeReason::NonUtf8);
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Self::Unavailable(InvalidHomeReason::NotAbsolute);
        }
        Self::Available(UserHome(path))
    }

    /// 返回可传入较低 package 的固定绝对路径。
    pub(super) fn path(&self) -> Option<&Path> {
        match self {
            Self::Available(home) => Some(&home.0),
            Self::Unavailable(_) => None,
        }
    }

    /// 返回非致命降级原因。
    pub(super) fn reason(&self) -> Option<InvalidHomeReason> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable(reason) => Some(*reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_empty_and_relative_values() {
        assert_eq!(
            UserHomeState::from_value(None),
            UserHomeState::Unavailable(InvalidHomeReason::Missing)
        );
        assert_eq!(
            UserHomeState::from_value(Some(OsString::new())),
            UserHomeState::Unavailable(InvalidHomeReason::Empty)
        );
        assert_eq!(
            UserHomeState::from_value(Some(OsString::from("relative/home"))),
            UserHomeState::Unavailable(InvalidHomeReason::NotAbsolute)
        );
    }

    #[test]
    fn accepts_an_absolute_utf8_path() {
        let state = UserHomeState::from_value(Some(OsString::from("/tmp/zhsh-home")));
        assert_eq!(state.path(), Some(Path::new("/tmp/zhsh-home")));
        assert_eq!(state.reason(), None);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt;

        let value = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
        assert_eq!(
            UserHomeState::from_value(Some(value)),
            UserHomeState::Unavailable(InvalidHomeReason::NonUtf8)
        );
    }
}
