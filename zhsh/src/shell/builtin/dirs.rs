//! `dirs`：显示当前目录与目录栈，或清空目录栈。
//!
//! # 用法
//! `dirs [-clpv]`；无参数按一行显示目录。
//!
//! # 参数与选项
//! `-c` 清空栈但保留 cwd；`-l` 不把 HOME 缩写为 ~；`-p` 逐行；`-v` 逐行带序号。
//! 短选项可组合；接受 `--`，不接受目录操作数。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! dirs -v
//! ```
//! 每行显示序号与目录，第 0 项为当前目录。
//!
//! # 输出与退出状态
//! 显示结果写 stdout；清空成功不输出。成功 0，无效参数为 1 并写 stderr。
//!
//! # 状态影响
//! 查询不变更目录；`-c` 删除栈记录。默认 Agent 仅允许查询选项，Native 保留授权。
//! 委托模式查询可作为管道源；清空操作不能作 builtin 管道源。

use super::super::SessionState;
use super::BuiltinResult;
use std::path::Path;

fn display(path: &Path, home: Option<&str>, long: bool) -> String {
    let value = path.to_string_lossy();
    if !long {
        if let Some(home) = home.filter(|home| !home.is_empty()) {
            if value == home {
                return "~".into();
            }
            if let Some(rest) = value
                .strip_prefix(home)
                .filter(|rest| rest.starts_with('/'))
            {
                return format!("~{rest}");
            }
        }
    }
    value.into_owned()
}

/// 按 Bash `dirs` 的显示选项渲染当前目录和目录栈。
///
/// `one_per_line` 对应 `-p`，`numbered` 对应 `-v`，`long` 对应 `-l`。
pub(crate) fn render(
    shell: &SessionState,
    one_per_line: bool,
    numbered: bool,
    long: bool,
) -> String {
    let home = shell.env.get("HOME").map(String::as_str);
    let paths = std::iter::once(&shell.cwd).chain(shell.directory_stack.iter().rev());
    let values: Vec<_> = paths.map(|path| display(path, home, long)).collect();
    if numbered {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| format!("{index}  {value}\n"))
            .collect()
    } else if one_per_line {
        format!("{}\n", values.join("\n"))
    } else {
        format!("{}\n", values.join(" "))
    }
}

/// 显示目录栈，或在 `-c` 时清空栈但保留当前目录。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    let mut clear = false;
    let mut one_per_line = false;
    let mut numbered = false;
    let mut long = false;
    for argument in args {
        if argument == "--" {
            continue;
        }
        let Some(options) = argument.strip_prefix('-') else {
            return BuiltinResult::error("dirs: 用法: dirs [-clpv]\n");
        };
        for option in options.chars() {
            match option {
                'c' => clear = true,
                'p' => one_per_line = true,
                'v' => numbered = true,
                'l' => long = true,
                _ => return BuiltinResult::error("dirs: 用法: dirs [-clpv]\n"),
            }
        }
    }
    if clear {
        shell.directory_stack.clear();
        return BuiltinResult::ok();
    }
    BuiltinResult::stdout(render(shell, one_per_line, numbered, long))
}
