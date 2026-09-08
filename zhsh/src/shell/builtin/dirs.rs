//! `dirs`：显示或清空 `pushd`/`popd` 使用的目录栈。
//!
//! 用法：`dirs [-clpv]`

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
