//! 面向终端和管道的无边框纯文本表格。

use unicode_width::UnicodeWidthStr;

/// 按 Unicode 显示宽度渲染固定列数的表格。
///
/// 单元格先被压平成一行并转义控制字符；列之间固定使用两个 ASCII 空格，行尾不补空格。
pub(super) fn render<const N: usize>(headers: [&str; N], rows: Vec<[String; N]>) -> String {
    let headers = headers.map(sanitize_cell);
    let rows: Vec<_> = rows
        .into_iter()
        .map(|row| row.map(|value| sanitize_cell(&value)))
        .collect();
    let mut widths = std::array::from_fn(|index| UnicodeWidthStr::width(headers[index].as_str()));
    for row in &rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(UnicodeWidthStr::width(value.as_str()));
        }
    }

    let mut output = String::new();
    append_row(&mut output, &headers, &widths);
    for row in &rows {
        append_row(&mut output, row, &widths);
    }
    output
}

fn append_row<const N: usize>(output: &mut String, values: &[String; N], widths: &[usize; N]) {
    for (index, value) in values.iter().enumerate() {
        output.push_str(value);
        if index + 1 != N {
            let padding = widths[index].saturating_sub(UnicodeWidthStr::width(value.as_str())) + 2;
            output.extend(std::iter::repeat_n(' ', padding));
        }
    }
    output.push('\n');
}

fn sanitize_cell(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if is_terminal_format_control(character) => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{{{:x}}}", character as u32);
            }
            character => output.push(character),
        }
    }
    output
}

fn is_terminal_format_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character as u32,
            0x061c | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2069 | 0xfeff
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_ascii_and_unicode_without_tabs_or_trailing_padding() {
        let output = render(
            ["FORMAT", "SOURCE", "状态"],
            vec![
                ["a@1".into(), "official".into(), "active".into()],
                ["very-long@2".into(), "用户".into(), "inactive".into()],
            ],
        );
        assert_eq!(
            output,
            "FORMAT       SOURCE    状态\n\
a@1          official  active\n\
very-long@2  用户      inactive\n"
        );
        assert!(!output.contains('\t'));
        assert!(output.lines().all(|line| !line.ends_with(' ')));
    }
}
