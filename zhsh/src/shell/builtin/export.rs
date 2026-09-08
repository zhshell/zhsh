//! `export`：设置、取消导出或列出当前会话的环境变量。
//!
//! 用法：`export [-n] [名称[=值] ...]`

use super::super::SessionState;
use super::BuiltinResult;

/// 判断名称是否符合可导出 Shell 变量的 ASCII 标识符语法。
pub(crate) fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn declaration(name: &str, value: &str) -> String {
    format!("declare -- {name}='{}'", value.replace('\'', "'\\''"))
}

fn mark_exported(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("declare -- ") {
        return format!("declare -x {rest}");
    }
    let Some(rest) = value.strip_prefix("declare -") else {
        return value.to_string();
    };
    let Some((attributes, assignment)) = rest.split_once(' ') else {
        return value.to_string();
    };
    if attributes.contains('x') {
        value.to_string()
    } else {
        format!("declare -{attributes}x {assignment}")
    }
}

fn render(shell: &SessionState) -> String {
    let mut variables: Vec<_> = shell.env.iter().collect();
    variables.sort_by_key(|(key, _)| *key);
    variables
        .into_iter()
        .map(|(key, value)| format!("declare -x {key}={}\n", quote(value)))
        .collect()
}

/// 列出、设置或使用 `-n` 取消导出会话环境变量。
///
/// `args` 必须已经由命令层解析为字面参数；含展开的表达式会整体交给 Bash，而不会到达
/// 本函数。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    if args.is_empty() || args == ["-p"] {
        return BuiltinResult::stdout(render(shell));
    }

    let mut remove_export = false;
    let mut operands = args;
    if let Some(option) = args.first().filter(|argument| argument.starts_with('-')) {
        match option.as_str() {
            "--" => operands = &args[1..],
            "-n" => {
                remove_export = true;
                operands = &args[1..];
            }
            _ => return BuiltinResult::error("export: 用法: export [-n] [名称[=值] ...]\n"),
        }
    }

    let mut errors = String::new();
    for operand in operands {
        let (name, value) = operand
            .split_once('=')
            .map_or((operand.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        if !valid_name(name) || value.is_some_and(|value| value.contains('\0')) {
            errors.push_str(&format!("export: `{operand}`: 不是有效的标识符\n"));
            continue;
        }
        if remove_export {
            if let Some(value) = value
                .map(str::to_string)
                .or_else(|| shell.env.get(name).cloned())
            {
                shell
                    .variables
                    .insert(name.to_string(), declaration(name, &value));
                if SessionState::is_prompt_variable(name) {
                    shell.prompt_variables.insert(name.to_string(), value);
                }
            }
            shell.remove_env(name);
        } else if let Some(value) = value {
            if let Err(error) = shell.set_env(name, value) {
                errors.push_str(&format!("export: `{operand}`: {error}\n"));
            } else if SessionState::is_prompt_variable(name) {
                if let Err(error) = shell.set_prompt_variable(name, value) {
                    errors.push_str(&format!("export: `{operand}`: {error}\n"));
                }
            } else {
                shell.variables.remove(name);
            }
        } else if SessionState::is_prompt_variable(name) {
            let value = shell.prompt_variable(name).unwrap_or_default().to_string();
            if let Err(error) = shell.set_env(name, &value) {
                errors.push_str(&format!("export: `{operand}`: {error}\n"));
            } else if let Err(error) = shell.set_prompt_variable(name, &value) {
                errors.push_str(&format!("export: `{operand}`: {error}\n"));
            }
        } else if let Some(declaration) = shell.variables.get_mut(name) {
            *declaration = mark_exported(declaration);
        } else {
            let value = shell.env.get(name).cloned().unwrap_or_default();
            if let Err(error) = shell.set_env(name, &value) {
                errors.push_str(&format!("export: `{operand}`: {error}\n"));
            }
        }
    }

    if errors.is_empty() {
        BuiltinResult::ok()
    } else {
        BuiltinResult::error(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_multiple_quoted_assignments() {
        let mut shell = SessionState::test();
        let result = execute(
            &mut shell,
            &["FIRST=value with space".into(), "SECOND=two".into()],
        );
        assert_eq!(result.code, 0);
        assert_eq!(
            shell.env.get("FIRST").map(String::as_str),
            Some("value with space")
        );
        assert_eq!(shell.env.get("SECOND").map(String::as_str), Some("two"));
    }

    #[test]
    fn invalid_names_never_reach_process_environment() {
        let mut shell = SessionState::test();
        for invalid in [
            "=value",
            "BAD-NAME=value",
            "1BAD=value",
            "BAD NAME=value",
            "BAD\0=value",
        ] {
            let result = execute(&mut shell, &[invalid.into()]);
            assert_ne!(result.code, 0, "{invalid:?}");
        }
    }

    #[test]
    fn supports_unexporting_values() {
        let mut shell = SessionState::test();
        shell.set_env("ZHSH_EXPORT_TEST", "yes").unwrap();
        assert_eq!(
            execute(&mut shell, &["-n".into(), "ZHSH_EXPORT_TEST".into()]).code,
            0
        );
        assert!(!shell.env.contains_key("ZHSH_EXPORT_TEST"));
    }

    #[test]
    fn preserves_prompt_value_while_changing_its_export_attribute() {
        let mut shell = SessionState::test();
        shell.set_prompt_variable("PS1", r"\u:\w\$ ").unwrap();

        assert_eq!(execute(&mut shell, &["PS1".into()]).code, 0);
        assert_eq!(shell.env.get("PS1").map(String::as_str), Some(r"\u:\w\$ "));
        assert!(shell
            .variables
            .get("PS1")
            .is_some_and(|declaration| declaration.starts_with("declare -x PS1=")));

        assert_eq!(execute(&mut shell, &["-n".into(), "PS1".into()]).code, 0);
        assert!(!shell.env.contains_key("PS1"));
        assert_eq!(shell.prompt_variable("PS1"), Some(r"\u:\w\$ "));
        assert!(shell
            .variables
            .get("PS1")
            .is_some_and(|declaration| declaration.starts_with("declare -- PS1=")));
    }
}
