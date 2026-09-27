//! export、unset 与 Native wait 共用的 ASCII 变量名校验。

/// 判断名称是否符合可导出 Shell 变量的 ASCII 标识符语法。
pub(crate) fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}
