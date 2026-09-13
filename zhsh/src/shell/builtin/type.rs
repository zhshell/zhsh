//! `type`：说明名称会解析为别名、函数、内建命令还是外部程序。
//!
//! 用法：`type [-atpP] 名称 [名称 ...]`

use super::super::{command::resolver, SessionState};
use super::BuiltinResult;

/// 按别名、函数、内建命令和 PATH 的解析顺序说明一个或多个名称。
///
/// `builtin_names` 来自唯一命令注册表，避免本模块维护第二份名称列表。
pub(crate) fn execute(
    shell: &mut SessionState,
    args: &[String],
    builtin_names: &[&str],
) -> BuiltinResult {
    execute_with_functions(shell, args, builtin_names, true)
}

pub(crate) fn execute_native(
    shell: &mut SessionState,
    args: &[String],
    builtin_names: &[&str],
) -> BuiltinResult {
    execute_with_functions(shell, args, builtin_names, false)
}

fn execute_with_functions(
    shell: &mut SessionState,
    args: &[String],
    builtin_names: &[&str],
    include_functions: bool,
) -> BuiltinResult {
    let mut all = false;
    let mut kind_only = false;
    let mut path_only = false;
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        if argument == "--" {
            index += 1;
            break;
        }
        let Some(options) = argument
            .strip_prefix('-')
            .filter(|options| !options.is_empty())
        else {
            break;
        };
        for option in options.chars() {
            match option {
                'a' => all = true,
                't' => kind_only = true,
                'p' | 'P' => path_only = true,
                _ => return BuiltinResult::error("type: 用法: type [-atpP] 名称 [名称 ...]\n"),
            }
        }
        index += 1;
    }
    let names = &args[index..];
    if names.is_empty() {
        return BuiltinResult::error("type: 用法: type [-atpP] 名称 [名称 ...]\n");
    }

    let mut output = String::new();
    let mut errors = String::new();
    for name in names {
        let mut found = false;
        if !path_only {
            if let Some(value) = shell.aliases.get(name) {
                found = true;
                if kind_only {
                    output.push_str("alias\n");
                } else {
                    output.push_str(&format!("{name} 是 `{value}` 的别名\n"));
                }
                if !all {
                    continue;
                }
            }
            if include_functions && shell.functions.contains_key(name) {
                found = true;
                if kind_only {
                    output.push_str("function\n");
                } else {
                    output.push_str(&format!("{name} 是 shell 函数\n"));
                }
                if !all {
                    continue;
                }
            }
            if builtin_names.contains(&name.as_str()) {
                found = true;
                if kind_only {
                    output.push_str("builtin\n");
                } else {
                    output.push_str(&format!("{name} 是 zhsh 内建命令\n"));
                }
                if !all {
                    continue;
                }
            }
        }
        for path in resolver::executable_paths(shell, name) {
            found = true;
            if kind_only {
                output.push_str("file\n");
            } else {
                output.push_str(&format!("{}\n", path.display()));
            }
            if !all {
                break;
            }
        }
        if !found {
            errors.push_str(&format!("type: {name}: 未找到\n"));
        }
    }

    BuiltinResult {
        stdout: output,
        stderr: errors.clone(),
        code: i32::from(!errors.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_aliases_builtins_and_missing_commands() {
        let mut shell = SessionState::test();
        shell.aliases.insert("ll".into(), "ls -l".into());
        let builtins = ["cd", "type"];
        assert!(execute(&mut shell, &["ll".into()], &builtins)
            .stdout
            .contains("别名"));
        assert!(execute(&mut shell, &["cd".into()], &builtins)
            .stdout
            .contains("内建"));
        assert_eq!(
            execute(
                &mut shell,
                &["certainly-not-a-zhsh-command".into()],
                &builtins
            )
            .code,
            1
        );
    }
}
