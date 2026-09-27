//! `umask`：显示或设置文件创建掩码。
//!
//! # 用法
//! `umask`、`umask -S` 或 `umask 八进制掩码`。
//!
//! # 参数与选项
//! 无参数显示四位八进制；-S 显示允许权限的符号形式。
//! 设置值为 000–777，可带 0o 前缀；不接受符号赋值或 -S 与数值组合。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。 Unix 使用进程级掩码，非 Unix 明确返回不支持。
//!
//! # 示例
//! ```sh
//! umask -S
//! ```
//! 显示当前创建权限约束，不修改掩码。
//!
//! # 输出与退出状态
//! 查询写 stdout，设置成功无输出。成功 0，参数、范围或平台错误为 1 并写 stderr。
//!
//! # 状态影响
//! 设置影响当前进程及后续子进程，不仅是 SessionState 字段。
//! 默认 Agent 仅允许查询；Native 保留授权。委托模式查询作管道源，设置在 Bash 子环境处理。

use super::super::SessionState;
use super::BuiltinResult;

#[cfg(unix)]
fn current() -> u32 {
    // SAFETY: POSIX umask 接受任意 mode_t 且不访问指针。先设为 0 取得旧值后立即恢复；
    // 调用期间持有的是进程全局状态，因此本函数只在单线程命令处理路径使用。
    let previous = unsafe { libc::umask(0) };
    // SAFETY: `previous` 是前一次 umask 返回的有效掩码，调用不访问内存。
    unsafe { libc::umask(previous) };
    previous as u32 & 0o777
}

#[cfg(unix)]
fn symbolic(mask: u32) -> String {
    fn permissions(bits: u32) -> String {
        let mut value = String::new();
        if bits & 4 != 0 {
            value.push('r');
        }
        if bits & 2 != 0 {
            value.push('w');
        }
        if bits & 1 != 0 {
            value.push('x');
        }
        value
    }
    let allowed = !mask & 0o777;
    format!(
        "u={},g={},o={}\n",
        permissions((allowed >> 6) & 7),
        permissions((allowed >> 3) & 7),
        permissions(allowed & 7)
    )
}

/// 显示或设置进程级文件创建掩码。
///
/// 与其他状态型内建不同，umask 是后续子进程继承的进程全局状态，不能只保存在
/// [`SessionState`] 中。非 Unix 平台明确返回不支持。
pub(crate) fn execute(_: &mut SessionState, args: &[String]) -> BuiltinResult {
    #[cfg(not(unix))]
    {
        let _ = args;
        return BuiltinResult::error("umask: 当前平台不支持\n");
    }
    #[cfg(unix)]
    {
        match args {
            [] => BuiltinResult::stdout(format!("{:04o}\n", current())),
            [option] if option == "-S" => BuiltinResult::stdout(symbolic(current())),
            [value] => {
                let value = value.strip_prefix("0o").unwrap_or(value);
                let Ok(mask) = u32::from_str_radix(value, 8) else {
                    return BuiltinResult::error(format!("umask: {value}: 八进制数超出范围\n"));
                };
                if mask > 0o777 {
                    return BuiltinResult::error(format!("umask: {value}: 八进制数超出范围\n"));
                }
                // SAFETY: 已验证 `mask <= 0o777`，POSIX umask 不访问指针。
                unsafe { libc::umask(mask as libc::mode_t) };
                BuiltinResult::ok()
            }
            _ => BuiltinResult::error("umask: 用法: umask [-S] [八进制掩码]\n"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn renders_and_sets_octal_masks() {
        let mut shell = SessionState::test();
        let old = current();
        assert_eq!(execute(&mut shell, &["027".into()]).code, 0);
        assert_eq!(execute(&mut shell, &[]).stdout, "0027\n");
        assert_eq!(
            execute(&mut shell, &["-S".into()]).stdout,
            "u=rwx,g=rx,o=\n"
        );
        // SAFETY: `old` 来自 `current()` 返回的有效进程掩码；测试结束时恢复全局状态。
        unsafe { libc::umask(old as libc::mode_t) };
    }
}
