//! `alias`：定义、查询或列出当前会话的命令别名。
//!
//! 用法：`alias [名称[=命令] ...]`

use super::super::SessionState;
use super::BuiltinResult;

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn render(name: &str, value: &str) -> String {
    format!("alias {name}={}\n", quote(value))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|character| character.is_whitespace() || "=/$`'\";&|()<>".contains(character))
}

/// 查询、列出或提交当前会话别名。
///
/// `shell` 提供别名状态，`args` 是路由层解析后的字面参数。所有赋值先验证后逐项提交；
/// 查询不存在的名称返回非零状态。
pub(crate) fn execute(shell: &mut SessionState, args: &[String]) -> BuiltinResult {
    if args.is_empty() || args == ["-p"] {
        let mut aliases: Vec<_> = shell.aliases.iter().collect();
        aliases.sort_by_key(|(name, _)| *name);
        return BuiltinResult::stdout(
            aliases
                .into_iter()
                .map(|(name, value)| render(name, value))
                .collect::<String>(),
        );
    }

    let operands = if args.first().is_some_and(|argument| argument == "--") {
        &args[1..]
    } else {
        args
    };
    let mut output = String::new();
    let mut errors = String::new();
    for operand in operands {
        if let Some((name, value)) = operand.split_once('=') {
            if valid_name(name) {
                shell.aliases.insert(name.to_string(), value.to_string());
            } else {
                errors.push_str(&format!("alias: `{name}`: 无效的别名名称\n"));
            }
        } else if let Some(value) = shell.aliases.get(operand) {
            output.push_str(&render(operand, value));
        } else {
            errors.push_str(&format!("alias: {operand}: 未找到\n"));
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
    fn creates_queries_and_quotes_aliases() {
        let mut shell = SessionState::test();
        assert_eq!(execute(&mut shell, &["ll=ls -l".into()]).code, 0);
        let result = execute(&mut shell, &["ll".into()]);
        assert_eq!(result.stdout, "alias ll='ls -l'\n");
        assert_eq!(execute(&mut shell, &["missing".into()]).code, 1);
    }
}
