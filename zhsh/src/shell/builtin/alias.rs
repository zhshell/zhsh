//! `alias`：列出、查询或定义当前会话别名。
//!
//! # 用法
//! `alias [--] [名称[=命令] ...]`；`alias -p`。无参数列出全部别名。
//!
//! # 参数与选项
//! 仅给名称时查询；`名称=命令` 定义或覆盖该别名，含空格的命令需加引号。
//! 单独的 `-p` 与无参数相同；开头 `--` 后按操作数处理。
//!
//! # 模式与上下文
//! 默认 Bash 委托模式与 Native 均通过各自路由调用；此处列出内建接受的字面参数。
//!
//! # 示例
//! ```sh
//! alias ll='ls -l'
//! alias ll
//! ```
//! 定义 ll 后查询，会输出带引号的别名声明。
//!
//! # 输出与退出状态
//! 列表和查询写 stdout；定义成功不输出。成功为 0，无效名称或未找到的查询为 1，诊断写 stderr。
//!
//! # 状态影响
//! 定义立即影响本会话后续别名解析；多个操作数按顺序处理，失败不回滚先前定义。
//! 默认 Agent 仅允许不含赋值的查询；Native 保留授权。委托模式查询可作管道源，赋值形式走 Bash 子环境。

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
