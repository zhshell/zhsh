//! 用户插件安装参数的有限路径解析。

use super::super::SessionState;
use crate::common::{AppError, AppResult};
use std::path::{Path, PathBuf};

pub(super) fn resolve_source(shell: &SessionState, argument: &str) -> AppResult<PathBuf> {
    if argument.is_empty() || argument.contains('\0') {
        return Err(AppError::input("插件源路径为空或包含 NUL"));
    }
    let path = if argument == "~" {
        shell
            .user_home()
            .ok_or_else(|| AppError::input("用户 HOME 不可用"))?
            .to_path_buf()
    } else if let Some(relative) = argument.strip_prefix("~/") {
        shell
            .user_home()
            .ok_or_else(|| AppError::input("用户 HOME 不可用"))?
            .join(relative)
    } else {
        let path = Path::new(argument);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            shell.cwd.join(path)
        }
    };
    if !path.is_absolute() {
        return Err(AppError::input("插件源路径无法解析为绝对路径"));
    }
    Ok(path)
}
