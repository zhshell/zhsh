//! `zh llm` 交互式向导。
//!
//! 本模块只收集和呈现数据；文件写入与会话切换由应用服务负责。

use crate::application::{
    ConfigRecord, LlmConfigAction, LlmConfigDecision, LlmConfigUi, LlmConfigUiError, SaveMode,
};
use crate::llm::{
    self, normalize_base_url, validate_model, validate_name, JsonSchemaMode, LlmConfig, LlmProfile,
    LlmProfileDraft, LlmProfileIssue, LlmProfileReadiness, ModelTier, ModelTiers, PluginSummary,
};
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

type WizardResult<T> = Result<T, LlmConfigUiError>;

const KEY_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(10);

/// REPL 对应用层 LLM 配置界面端口的终端实现。
pub(crate) struct TerminalLlmConfigUi;

impl LlmConfigUi for TerminalLlmConfigUi {
    fn collect(
        &self,
        action: &LlmConfigAction,
        records: &[ConfigRecord],
        plugins: &[PluginSummary],
    ) -> Result<LlmConfigDecision, LlmConfigUiError> {
        run(action, records, plugins)
    }

    fn confirm_repair(
        &self,
        name: &str,
        issues: &[LlmProfileIssue],
    ) -> Result<bool, LlmConfigUiError> {
        let mut wizard = Wizard::new()?;
        eprintln!("配置 {name} 不完整：");
        for issue in issues {
            eprintln!("- {}: {}", issue.field, issue.message);
        }
        Ok(wizard.select("是否进入修复流程", &["修复", "暂不修复"], 0)? == 0)
    }
}

struct Wizard {
    _raw_terminal: RawTerminal,
    pending_input: Vec<u8>,
}

impl Wizard {
    fn new() -> WizardResult<Self> {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(LlmConfigUiError::Terminal(
                "LLM 配置只能从交互式终端读取".into(),
            ));
        }
        Ok(Self {
            _raw_terminal: RawTerminal::enter()
                .ok_or_else(|| LlmConfigUiError::Terminal("无法切换配置向导终端模式".into()))?,
            pending_input: Vec::new(),
        })
    }

    fn next_menu_key(&mut self) -> WizardResult<WizardKey> {
        let key = read_wizard_key(&mut self.pending_input)?.ok_or(LlmConfigUiError::Cancelled)?;
        if key != WizardKey::Escape {
            return Ok(key);
        }
        let next = read_wizard_key(&mut self.pending_input)?.ok_or(LlmConfigUiError::Cancelled)?;
        if next == WizardKey::Character(':') {
            self.vim_menu_command()
        } else {
            Ok(next)
        }
    }

    fn vim_menu_command(&mut self) -> WizardResult<WizardKey> {
        let mut command = String::new();
        redraw_vim_command(&command)?;
        loop {
            let key =
                read_wizard_key(&mut self.pending_input)?.ok_or(LlmConfigUiError::Cancelled)?;
            match key {
                WizardKey::Enter => {
                    clear_vim_command()?;
                    return match command.trim() {
                        "q" => Err(LlmConfigUiError::Cancelled),
                        "wq" => Ok(WizardKey::WriteQuit),
                        _ => Ok(WizardKey::UnknownCommand),
                    };
                }
                WizardKey::Escape => {
                    clear_vim_command()?;
                    return Ok(WizardKey::Other);
                }
                WizardKey::CtrlC | WizardKey::CtrlD => {
                    clear_vim_command()?;
                    return Err(LlmConfigUiError::Cancelled);
                }
                WizardKey::Backspace | WizardKey::Delete => {
                    command.pop();
                }
                WizardKey::CtrlU => command.clear(),
                WizardKey::Character(character) if !character.is_control() => {
                    command.push(character);
                }
                _ => continue,
            }
            redraw_vim_command(&command)?;
        }
    }

    fn select(&mut self, label: &str, items: &[&str], default: usize) -> WizardResult<usize> {
        self.select_inner(label, items, default, false)
            .map(|value| value.expect(":wq disabled"))
    }

    fn final_select(
        &mut self,
        label: &str,
        items: &[&str],
        default: usize,
    ) -> WizardResult<Option<usize>> {
        self.select_inner(label, items, default, true)
    }

    fn select_inner(
        &mut self,
        label: &str,
        items: &[&str],
        default: usize,
        allow_write_quit: bool,
    ) -> WizardResult<Option<usize>> {
        if items.is_empty() {
            return Err(LlmConfigUiError::Terminal(format!("选择项为空: {label}")));
        }
        let mut cursor = default.min(items.len() - 1);
        render_select(label, items, cursor)?;
        loop {
            let key = match self.next_menu_key() {
                Ok(key) => key,
                Err(error) => {
                    clear_select(items.len())?;
                    eprintln!("? {label}");
                    return Err(error);
                }
            };
            match key {
                WizardKey::Up => cursor = cursor.saturating_sub(1),
                WizardKey::Down => cursor = (cursor + 1).min(items.len() - 1),
                WizardKey::Home => cursor = 0,
                WizardKey::End => cursor = items.len() - 1,
                WizardKey::Enter => {
                    clear_select(items.len())?;
                    eprintln!("? {label}: {}", items[cursor]);
                    return Ok(Some(cursor));
                }
                WizardKey::CtrlC | WizardKey::CtrlD => {
                    clear_select(items.len())?;
                    eprintln!("? {label}");
                    return Err(LlmConfigUiError::Cancelled);
                }
                WizardKey::WriteQuit if allow_write_quit => {
                    clear_select(items.len())?;
                    eprintln!("? {label}: 保存（:wq）");
                    return Ok(None);
                }
                WizardKey::WriteQuit => {
                    clear_select(items.len())?;
                    eprintln!("! 当前选择界面不能保存；进入配置编辑器后可在任意阶段使用 :wq");
                    render_select(label, items, cursor)?;
                    continue;
                }
                WizardKey::UnknownCommand => {
                    clear_select(items.len())?;
                    eprintln!("! 未知向导命令；仅支持 :q 和 :wq");
                    render_select(label, items, cursor)?;
                    continue;
                }
                _ => continue,
            }
            clear_select(items.len())?;
            render_select(label, items, cursor)?;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileField {
    Name,
    Format,
    JsonSchema,
    Url,
    AccessToken,
    Flash,
    Standard,
    Max,
    Tier,
}

impl ProfileField {
    const ALL: [Self; 9] = [
        Self::Name,
        Self::Format,
        Self::JsonSchema,
        Self::Url,
        Self::AccessToken,
        Self::Flash,
        Self::Standard,
        Self::Max,
        Self::Tier,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::Name => "NAME",
            Self::Format => "FORMAT",
            Self::JsonSchema => "JSON_SCHEMA",
            Self::Url => "URL",
            Self::AccessToken => "ACCESS_TOKEN",
            Self::Flash => "FLASH",
            Self::Standard => "STANDARD",
            Self::Max => "MAX",
            Self::Tier => "TIER",
        }
    }

    fn value(self, draft: &LlmProfileDraft) -> &str {
        match self {
            Self::Name => &draft.name,
            Self::Format => &draft.request_format,
            Self::JsonSchema => &draft.json_schema,
            Self::Url => &draft.url,
            Self::AccessToken => &draft.access_token,
            Self::Flash => &draft.flash,
            Self::Standard => &draft.standard,
            Self::Max => &draft.max,
            Self::Tier => &draft.tier,
        }
    }

    fn value_mut(self, draft: &mut LlmProfileDraft) -> &mut String {
        match self {
            Self::Name => &mut draft.name,
            Self::Format => &mut draft.request_format,
            Self::JsonSchema => &mut draft.json_schema,
            Self::Url => &mut draft.url,
            Self::AccessToken => &mut draft.access_token,
            Self::Flash => &mut draft.flash,
            Self::Standard => &mut draft.standard,
            Self::Max => &mut draft.max,
            Self::Tier => &mut draft.tier,
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .expect("field belongs to ALL")
    }

    fn from_issue(issue: &LlmProfileIssue) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|field| field.key() == issue.field)
    }

    fn input_kind(self) -> FieldInputKind {
        match self {
            Self::Format | Self::JsonSchema | Self::Tier => FieldInputKind::Choice,
            Self::AccessToken => FieldInputKind::Secret,
            _ => FieldInputKind::Text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldInputKind {
    Text,
    Secret,
    Choice,
}

struct ChoiceOption {
    value: String,
    detail: Option<&'static str>,
}

enum EditorMode {
    Insert { cursor: usize },
    Normal { column: usize },
    Command { input: String },
}

enum EditorOutcome {
    SaveDraft(LlmProfileDraft),
    Complete(LlmProfile),
}

struct FormRenderer {
    drawn: bool,
    cursor_row: usize,
    line_count: usize,
}

impl FormRenderer {
    fn new() -> Self {
        Self {
            drawn: false,
            cursor_row: 0,
            line_count: 0,
        }
    }

    fn clear(&mut self) -> WizardResult<()> {
        if self.drawn {
            let below = self.line_count.saturating_sub(self.cursor_row + 1);
            if below > 0 {
                eprint!("\x1b[{below}B");
            }
            eprint!("\r\x1b[2K");
            if self.line_count > 1 {
                eprint!("\x1b[{}A", self.line_count - 1);
            }
            eprint!("\r\x1b[J");
        }
        self.drawn = false;
        io::stderr()
            .flush()
            .map_err(|error| LlmConfigUiError::Terminal(error.to_string()))
    }

    fn render(
        &mut self,
        draft: &LlmProfileDraft,
        field: ProfileField,
        mode: &EditorMode,
        notice: &str,
        name_locked: bool,
        plugins: &[PluginSummary],
    ) -> WizardResult<()> {
        self.clear()?;
        // 保留最后一列，避免恰好写满终端宽度时触发自动换行，破坏块重绘的行数计算。
        let width = terminal_columns().saturating_sub(1).max(1);
        let mut lines = vec![fit_to_width(editor_header(mode, field), width)];
        let mut target_row = 0;
        let mut target_column = 0;
        for candidate in ProfileField::ALL {
            let marker = if candidate == field { "› " } else { "  " };
            let lock = if candidate == ProfileField::Name && name_locked {
                "  [fixed]"
            } else {
                ""
            };

            if candidate == field {
                match mode {
                    EditorMode::Insert { cursor } => {
                        let (line, column) =
                            render_active_field(candidate, draft, *cursor, width, marker, lock);
                        target_row = lines.len();
                        target_column = column;
                        lines.push(line);
                        continue;
                    }
                    EditorMode::Normal { column } => {
                        let key_len = candidate.key().chars().count();
                        let value_cursor = column.saturating_sub(key_len + 1);
                        let (line, value_column) = render_active_field(
                            candidate,
                            draft,
                            value_cursor,
                            width,
                            marker,
                            lock,
                        );
                        target_row = lines.len();
                        target_column = if *column <= key_len {
                            UnicodeWidthStr::width(marker)
                                + UnicodeWidthStr::width(
                                    candidate
                                        .key()
                                        .chars()
                                        .take(*column)
                                        .collect::<String>()
                                        .as_str(),
                                )
                        } else {
                            value_column
                        };
                        lines.push(line);
                        continue;
                    }
                    EditorMode::Command { .. } => {}
                }
            }

            let value = displayed_value(candidate, draft);
            lines.push(fit_to_width(
                format!("{marker}{}={value}{lock}", candidate.key()),
                width,
            ));
        }

        match mode {
            EditorMode::Command { input } => {
                let command = fit_to_width(format!(":{input}"), width);
                target_row = lines.len();
                target_column = UnicodeWidthStr::width(command.as_str()).min(width);
                lines.push(command);
            }
            _ if field.input_kind() == FieldInputKind::Choice => {
                lines.push(fit_to_width("Options:".into(), width));
                let options = matching_choices(field, draft, plugins);
                if options.is_empty() {
                    lines.push(fit_to_width("  (no matches)".into(), width));
                } else {
                    lines.extend(options.into_iter().map(|option| {
                        let detail = option
                            .detail
                            .map(|detail| format!(" · {detail}"))
                            .unwrap_or_default();
                        fit_to_width(format!("  {}{detail}", option.value), width)
                    }));
                }
            }
            _ => {}
        }
        if !notice.is_empty() {
            lines.push(fit_to_width(notice.into(), width));
        }

        target_column = target_column.min(width);
        eprint!("{}", lines.join("\n"));
        let below = lines.len().saturating_sub(target_row + 1);
        if below > 0 {
            eprint!("\x1b[{below}A");
        }
        eprint!("\r");
        if target_column > 0 {
            eprint!("\x1b[{target_column}C");
        }
        io::stderr()
            .flush()
            .map_err(|error| LlmConfigUiError::Terminal(error.to_string()))?;
        self.drawn = true;
        self.cursor_row = target_row;
        self.line_count = lines.len();
        Ok(())
    }
}

fn editor_header(mode: &EditorMode, field: ProfileField) -> String {
    match mode {
        EditorMode::Insert { .. } if field.input_kind() == FieldInputKind::Choice => {
            "LLM 配置 · INSERT · Esc: normal · Enter: confirm · Tab: complete".into()
        }
        EditorMode::Insert { .. } => "LLM 配置 · INSERT · Esc: normal · Enter: next line".into(),
        EditorMode::Normal { .. } => {
            "LLM 配置 · NORMAL · arrows: move · i: insert · :q quit · :wq write and quit".into()
        }
        EditorMode::Command { .. } => "LLM 配置 · COMMAND · Esc: normal · Enter: execute".into(),
    }
}

fn displayed_value(field: ProfileField, draft: &LlmProfileDraft) -> String {
    match field.input_kind() {
        FieldInputKind::Secret => masked_token(field.value(draft)),
        FieldInputKind::Text | FieldInputKind::Choice => terminal_safe(field.value(draft)),
    }
}

fn render_active_field(
    field: ProfileField,
    draft: &LlmProfileDraft,
    cursor: usize,
    width: usize,
    marker: &str,
    suffix: &str,
) -> (String, usize) {
    let prefix = format!("{marker}{}=", field.key());
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    if prefix_width >= width {
        return (fit_to_width(prefix, width), width);
    }
    let visible_value = displayed_value(field, draft);
    let (viewport, viewport_cursor) = text_viewport(
        &visible_value,
        cursor.min(visible_value.chars().count()),
        width - prefix_width,
    );
    (
        fit_to_width(format!("{prefix}{viewport}{suffix}"), width),
        (prefix_width + viewport_cursor).min(width),
    )
}

fn text_viewport(value: &str, cursor: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let characters: Vec<char> = value.chars().collect();
    let cursor = cursor.min(characters.len());
    let widths: Vec<usize> = characters
        .iter()
        .map(|character| UnicodeWidthStr::width(character.to_string().as_str()))
        .collect();
    let total_width: usize = widths.iter().sum();
    if total_width <= width {
        return (
            value.into(),
            widths.iter().take(cursor).copied().sum::<usize>(),
        );
    }

    let mut start = cursor;
    let mut before_cursor = 0;
    let before_limit = width.saturating_sub(1);
    while start > 0 {
        let next = widths[start - 1];
        if before_cursor + next > before_limit {
            break;
        }
        start -= 1;
        before_cursor += next;
    }
    let leading_ellipsis = usize::from(start > 0);
    let mut used = leading_ellipsis;
    let mut end = start;
    while end < characters.len() && used + widths[end] <= width {
        used += widths[end];
        end += 1;
    }
    let mut trailing_ellipsis = false;
    if end < characters.len() {
        while end > cursor && used + 1 > width {
            end -= 1;
            used -= widths[end];
        }
        trailing_ellipsis = used < width;
    }

    let mut output = String::new();
    if leading_ellipsis > 0 {
        output.push('…');
    }
    output.extend(characters[start..end].iter());
    if trailing_ellipsis {
        output.push('…');
    }
    (output, leading_ellipsis + before_cursor)
}

fn fit_to_width(value: String, width: usize) -> String {
    if UnicodeWidthStr::width(value.as_str()) <= width {
        return value;
    }
    if width == 0 {
        return String::new();
    }
    let content_width = width.saturating_sub(1);
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthStr::width(character.to_string().as_str());
        if used + character_width > content_width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

#[cfg(unix)]
fn terminal_columns() -> usize {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0 {
        let columns = unsafe { size.assume_init() }.ws_col as usize;
        if columns > 0 {
            return columns;
        }
    }
    80
}

#[cfg(not(unix))]
fn terminal_columns() -> usize {
    80
}

fn masked_token(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        "•".repeat(value.chars().count())
    }
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn field_line_len(field: ProfileField, draft: &LlmProfileDraft) -> usize {
    field.key().chars().count() + 1 + field.value(draft).chars().count()
}

fn completions(field: ProfileField, plugins: &[PluginSummary]) -> Vec<String> {
    choice_options(field, plugins)
        .into_iter()
        .map(|option| option.value)
        .collect()
}

fn choice_options(field: ProfileField, plugins: &[PluginSummary]) -> Vec<ChoiceOption> {
    match field {
        ProfileField::Format => plugins
            .iter()
            .map(|plugin| ChoiceOption {
                value: plugin.label(),
                detail: Some(if plugin.official { "official" } else { "user" }),
            })
            .collect(),
        ProfileField::JsonSchema => ["off", "on"]
            .into_iter()
            .map(|value| ChoiceOption {
                value: value.into(),
                detail: None,
            })
            .collect(),
        ProfileField::Tier => ["flash", "standard", "max"]
            .into_iter()
            .map(|value| ChoiceOption {
                value: value.into(),
                detail: None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn matching_choices(
    field: ProfileField,
    draft: &LlmProfileDraft,
    plugins: &[PluginSummary],
) -> Vec<ChoiceOption> {
    let query = field.value(draft).trim();
    choice_options(field, plugins)
        .into_iter()
        .filter(|option| query.is_empty() || option.value.starts_with(query))
        .collect()
}

fn confirm_choice(
    draft: &mut LlmProfileDraft,
    field: ProfileField,
    plugins: &[PluginSummary],
) -> Result<(), String> {
    let query = field.value(draft).trim();
    let options = choice_options(field, plugins);
    if let Some(option) = options.iter().find(|option| option.value == query) {
        *field.value_mut(draft) = option.value.clone();
        return Ok(());
    }
    let mut matches = options
        .iter()
        .filter(|option| option.value.starts_with(query));
    let Some(first) = matches.next() else {
        return Err(format!("{} 没有匹配的选项", field.key()));
    };
    if matches.next().is_some() {
        return Err(format!(
            "{} 匹配多个选项，请继续输入或按 Tab 补全",
            field.key()
        ));
    }
    *field.value_mut(draft) = first.value.clone();
    Ok(())
}

fn advance_default(draft: &mut LlmProfileDraft, field: ProfileField) {
    match field {
        ProfileField::Flash if draft.standard.trim().is_empty() => {
            draft.standard = draft.flash.trim().into();
        }
        ProfileField::Standard if draft.max.trim().is_empty() => {
            draft.max = draft.standard.trim().into();
        }
        _ => {}
    }
}

fn edit_profile(
    wizard: &mut Wizard,
    mut draft: LlmProfileDraft,
    plugins: &[PluginSummary],
    name_locked: bool,
) -> WizardResult<EditorOutcome> {
    if draft.request_format.is_empty() {
        if let Some(plugin) = plugins.first() {
            draft.request_format = plugin.label();
        }
    }
    let initial_profile = assess_editor_profile(draft.clone(), plugins);
    let mut field = initial_profile
        .issues()
        .first()
        .and_then(ProfileField::from_issue)
        .unwrap_or(ProfileField::Format);
    if field == ProfileField::Name && name_locked {
        field = ProfileField::Format;
    }
    let mut mode = EditorMode::Insert {
        cursor: field.value(&draft).chars().count(),
    };
    let mut notice = String::new();
    let mut renderer = FormRenderer::new();

    loop {
        let display_notice = if notice.is_empty() {
            field_hint(field)
        } else {
            format!("! {notice}")
        };
        renderer.render(&draft, field, &mode, &display_notice, name_locked, plugins)?;
        notice.clear();
        let key = read_wizard_key(&mut wizard.pending_input)?.ok_or(LlmConfigUiError::Cancelled)?;
        match &mut mode {
            EditorMode::Insert { cursor } => match key {
                WizardKey::Escape => {
                    mode = EditorMode::Normal {
                        column: field.key().chars().count() + 1 + *cursor,
                    };
                }
                WizardKey::CtrlC | WizardKey::CtrlD => {
                    renderer.clear()?;
                    return Err(LlmConfigUiError::Cancelled);
                }
                WizardKey::Left => *cursor = cursor.saturating_sub(1),
                WizardKey::Right => {
                    *cursor = (*cursor + 1).min(field.value(&draft).chars().count())
                }
                WizardKey::Home => *cursor = 0,
                WizardKey::End => *cursor = field.value(&draft).chars().count(),
                WizardKey::Backspace if *cursor > 0 => {
                    *cursor -= 1;
                    let index = char_byte_index(field.value(&draft), *cursor);
                    field.value_mut(&mut draft).remove(index);
                }
                WizardKey::Delete if *cursor < field.value(&draft).chars().count() => {
                    let index = char_byte_index(field.value(&draft), *cursor);
                    field.value_mut(&mut draft).remove(index);
                }
                WizardKey::CtrlU => {
                    field.value_mut(&mut draft).clear();
                    *cursor = 0;
                }
                WizardKey::Tab => {
                    complete_text(
                        field.value_mut(&mut draft),
                        cursor,
                        &completions(field, plugins),
                    );
                }
                WizardKey::Character(character) if !character.is_control() => {
                    if field == ProfileField::Name && name_locked {
                        notice = "NAME 由目标文件固定，不能在修改流程中重命名".into();
                    } else {
                        let index = char_byte_index(field.value(&draft), *cursor);
                        field.value_mut(&mut draft).insert(index, character);
                        *cursor += 1;
                    }
                }
                WizardKey::Enter => {
                    if field.input_kind() == FieldInputKind::Choice {
                        if let Err(error) = confirm_choice(&mut draft, field, plugins) {
                            notice = error;
                            *cursor = field.value(&draft).chars().count();
                            continue;
                        }
                        *cursor = field.value(&draft).chars().count();
                    }
                    advance_default(&mut draft, field);
                    if field.index() + 1 < ProfileField::ALL.len() {
                        field = ProfileField::ALL[field.index() + 1];
                        *cursor = field.value(&draft).chars().count();
                    } else {
                        let profile = assess_editor_profile(draft.clone(), plugins);
                        if profile.config().is_some() {
                            renderer.clear()?;
                            eprintln!();
                            return Ok(EditorOutcome::Complete(profile));
                        }
                        let issues = profile.issues();
                        notice = format!("配置尚不完整：{}", issue_text(issues));
                        field = issues
                            .first()
                            .and_then(ProfileField::from_issue)
                            .unwrap_or(ProfileField::Format);
                        *cursor = field.value(&draft).chars().count();
                    }
                }
                _ => {}
            },
            EditorMode::Normal { column } => match key {
                WizardKey::Up => {
                    field = ProfileField::ALL[field.index().saturating_sub(1)];
                    *column = (*column).min(field_line_len(field, &draft));
                }
                WizardKey::Down => {
                    field = ProfileField::ALL[(field.index() + 1).min(ProfileField::ALL.len() - 1)];
                    *column = (*column).min(field_line_len(field, &draft));
                }
                WizardKey::Left => *column = column.saturating_sub(1),
                WizardKey::Right => {
                    *column = (*column + 1).min(field_line_len(field, &draft));
                }
                WizardKey::Home => *column = 0,
                WizardKey::End => *column = field_line_len(field, &draft),
                WizardKey::Character('i') | WizardKey::Enter => {
                    if field == ProfileField::Name && name_locked {
                        notice = "NAME 由目标文件固定，不能在修改流程中重命名".into();
                    } else {
                        let cursor = insert_cursor(field, &draft, *column);
                        mode = EditorMode::Insert { cursor };
                    }
                }
                WizardKey::Character(':') => {
                    mode = EditorMode::Command {
                        input: String::new(),
                    };
                }
                WizardKey::CtrlC | WizardKey::CtrlD => {
                    renderer.clear()?;
                    return Err(LlmConfigUiError::Cancelled);
                }
                _ => {}
            },
            EditorMode::Command { input } => match key {
                WizardKey::Escape => {
                    mode = EditorMode::Normal {
                        column: field_line_len(field, &draft),
                    };
                }
                WizardKey::CtrlC | WizardKey::CtrlD => {
                    renderer.clear()?;
                    return Err(LlmConfigUiError::Cancelled);
                }
                WizardKey::Backspace | WizardKey::Delete => {
                    input.pop();
                }
                WizardKey::CtrlU => input.clear(),
                WizardKey::Character(character) if !character.is_control() => {
                    input.push(character);
                }
                WizardKey::Enter => match input.trim() {
                    "q" => {
                        renderer.clear()?;
                        return Err(LlmConfigUiError::Cancelled);
                    }
                    "wq" => match validate_name(draft.name.trim()) {
                        Ok(()) => {
                            draft.name = draft.name.trim().into();
                            renderer.clear()?;
                            eprintln!();
                            return Ok(EditorOutcome::SaveDraft(draft));
                        }
                        Err(error) => {
                            notice = format!("保存前需要有效 NAME：{error}");
                            field = ProfileField::Name;
                            mode = EditorMode::Normal { column: 0 };
                        }
                    },
                    _ => {
                        notice = "未知向导命令；仅支持 :q 和 :wq".into();
                        mode = EditorMode::Normal {
                            column: field_line_len(field, &draft),
                        };
                    }
                },
                _ => {}
            },
        }
    }
}

fn field_hint(field: ProfileField) -> String {
    match field {
        ProfileField::AccessToken => {
            "ACCESS_TOKEN may be empty; Ctrl-U clears the current value".into()
        }
        _ => String::new(),
    }
}

fn char_byte_index(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(index, _)| index)
}

fn insert_cursor(field: ProfileField, draft: &LlmProfileDraft, normal_column: usize) -> usize {
    let key_end = field.key().chars().count();
    if normal_column <= key_end {
        field.value(draft).chars().count()
    } else {
        (normal_column - key_end - 1).min(field.value(draft).chars().count())
    }
}

fn issue_text(issues: &[LlmProfileIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("{}: {}", issue.field, issue.message))
        .collect::<Vec<_>>()
        .join("；")
}

fn assess_editor_profile(mut draft: LlmProfileDraft, plugins: &[PluginSummary]) -> LlmProfile {
    let mut issues = Vec::new();
    if let Err(error) = validate_name(draft.name.trim()) {
        push_editor_issue(&mut issues, "NAME", error);
    }
    let url = match normalize_base_url(draft.url.trim()) {
        Ok(url) => Some(url),
        Err(error) => {
            push_editor_issue(&mut issues, "URL", error);
            None
        }
    };
    let plugin = plugins
        .iter()
        .find(|plugin| plugin.label() == draft.request_format.trim());
    if draft.request_format.trim().is_empty() {
        push_editor_issue(&mut issues, "FORMAT", "缺少 FORMAT".into());
    } else if plugin.is_none() {
        push_editor_issue(&mut issues, "FORMAT", "当前未加载该 Codec".into());
    }
    let requested_schema = match JsonSchemaMode::parse(draft.json_schema.trim()) {
        Some(mode) => Some(mode),
        None => {
            push_editor_issue(
                &mut issues,
                "JSON_SCHEMA",
                "JSON_SCHEMA 必须是 off 或 on".into(),
            );
            None
        }
    };
    let schema = plugin.and_then(|plugin| {
        requested_schema.and_then(|mode| match plugin.resolve_json_schema(mode) {
            Ok(resolution) => Some(resolution),
            Err(error) => {
                push_editor_issue(&mut issues, "FORMAT", error.to_string());
                None
            }
        })
    });
    for (field, value) in [
        ("FLASH", draft.flash.as_str()),
        ("STANDARD", draft.standard.as_str()),
        ("MAX", draft.max.as_str()),
    ] {
        if let Err(error) = validate_model(value.trim()) {
            push_editor_issue(&mut issues, field, error);
        }
    }
    let tier = match ModelTier::parse(draft.tier.trim()) {
        Some(tier) => Some(tier),
        None => {
            push_editor_issue(
                &mut issues,
                "TIER",
                "TIER 必须是 flash、standard 或 max".into(),
            );
            None
        }
    };
    let readiness = if issues.is_empty() {
        draft.name = draft.name.trim().into();
        draft.url = url.expect("URL was validated");
        draft.request_format = draft.request_format.trim().into();
        draft.json_schema = requested_schema
            .expect("JSON_SCHEMA was validated")
            .as_str()
            .into();
        draft.flash = draft.flash.trim().into();
        draft.standard = draft.standard.trim().into();
        draft.max = draft.max.trim().into();
        draft.tier = tier.expect("TIER was validated").as_str().into();
        LlmProfileReadiness::Ready(LlmConfig {
            name: draft.name.clone(),
            url: draft.url.clone(),
            request_format: draft.request_format.clone(),
            json_schema: schema.expect("Codec capability was resolved"),
            access_token: draft.access_token.clone(),
            models: ModelTiers {
                flash: draft.flash.clone(),
                standard: draft.standard.clone(),
                max: draft.max.clone(),
            },
            tier: tier.expect("TIER was validated"),
        })
    } else {
        LlmProfileReadiness::Incomplete(issues)
    };
    LlmProfile { draft, readiness }
}

fn push_editor_issue(issues: &mut Vec<LlmProfileIssue>, field: &'static str, message: String) {
    issues.push(LlmProfileIssue { field, message });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardKey {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    Enter,
    Tab,
    CtrlC,
    CtrlD,
    CtrlU,
    Escape,
    WriteQuit,
    UnknownCommand,
    Character(char),
    Other,
}

#[cfg(unix)]
struct RawTerminal {
    original: libc::termios,
}

#[cfg(unix)]
impl RawTerminal {
    fn enter() -> Option<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return None;
        }
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { original })
    }
}

#[cfg(unix)]
impl Drop for RawTerminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(not(unix))]
struct RawTerminal;

#[cfg(not(unix))]
impl RawTerminal {
    fn enter() -> Option<Self> {
        None
    }
}

fn redraw_vim_command(command: &str) -> WizardResult<()> {
    eprint!("\r\x1b[2K:{command}");
    io::stderr()
        .flush()
        .map_err(|error| LlmConfigUiError::Terminal(error.to_string()))
}

fn clear_vim_command() -> WizardResult<()> {
    eprint!("\r\x1b[2K");
    io::stderr()
        .flush()
        .map_err(|error| LlmConfigUiError::Terminal(error.to_string()))
}

fn complete_text(value: &mut String, cursor: &mut usize, completions: &[String]) {
    let prefix = value.trim();
    let matches: Vec<_> = completions
        .iter()
        .filter(|candidate| candidate.starts_with(prefix))
        .collect();
    let replacement = match matches.as_slice() {
        [] => return,
        [only] => (*only).clone(),
        [first, rest @ ..] => {
            let mut common = (*first).clone();
            for candidate in rest {
                while !candidate.starts_with(&common) {
                    if common.pop().is_none() {
                        return;
                    }
                }
            }
            common
        }
    };
    *value = replacement;
    *cursor = value.chars().count();
}

fn render_select(label: &str, items: &[&str], cursor: usize) -> WizardResult<()> {
    eprintln!("? {label}");
    for (index, item) in items.iter().enumerate() {
        let line = format!("{} {item}", if index == cursor { "›" } else { " " });
        if index + 1 == items.len() {
            eprint!("{line}");
        } else {
            eprintln!("{line}");
        }
    }
    io::stderr()
        .flush()
        .map_err(|error| LlmConfigUiError::Terminal(error.to_string()))
}

fn clear_select(item_count: usize) -> WizardResult<()> {
    eprint!("\r\x1b[2K\x1b[{item_count}A\r\x1b[J");
    io::stderr()
        .flush()
        .map_err(|error| LlmConfigUiError::Terminal(error.to_string()))
}

#[cfg(unix)]
fn read_wizard_key(pending: &mut Vec<u8>) -> WizardResult<Option<WizardKey>> {
    if pending.is_empty() && !read_pending_input(pending, None)? {
        return Ok(None);
    }

    let first = pending[0];
    let key_len = if first == b'\x1b' {
        while pending.len() < 3 {
            if !read_pending_input(pending, Some(KEY_SEQUENCE_TIMEOUT))? {
                break;
            }
        }
        if pending.starts_with(b"\x1b[") {
            while pending.len() < 8
                && !pending
                    .last()
                    .is_some_and(|byte| matches!(byte, b'A'..=b'Z' | b'~'))
            {
                if !read_pending_input(pending, Some(KEY_SEQUENCE_TIMEOUT))? {
                    break;
                }
            }
            pending
                .iter()
                .position(|byte| matches!(byte, b'A'..=b'Z' | b'~'))
                .map_or(1, |index| index + 1)
        } else {
            1
        }
    } else if first.is_ascii() {
        1
    } else {
        let sequence_len = utf8_sequence_len(first);
        while pending.len() < sequence_len {
            if !read_pending_input(pending, None)? {
                break;
            }
        }
        if pending.len() >= sequence_len && std::str::from_utf8(&pending[..sequence_len]).is_ok() {
            sequence_len
        } else {
            1
        }
    };
    let bytes: Vec<_> = pending.drain(..key_len).collect();
    Ok(Some(match bytes.as_slice() {
        b"\x1b[A" => WizardKey::Up,
        b"\x1b[B" => WizardKey::Down,
        b"\x1b[C" => WizardKey::Right,
        b"\x1b[D" => WizardKey::Left,
        b"\x1b[H" | b"\x1b[1~" => WizardKey::Home,
        b"\x1b[F" | b"\x1b[4~" => WizardKey::End,
        b"\x1b[3~" => WizardKey::Delete,
        b"\r" | b"\n" => WizardKey::Enter,
        b"\t" => WizardKey::Tab,
        [3] => WizardKey::CtrlC,
        [4] => WizardKey::CtrlD,
        [27] => WizardKey::Escape,
        [8] | [127] => WizardKey::Backspace,
        [21] => WizardKey::CtrlU,
        bytes => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.chars().next())
            .map_or(WizardKey::Other, WizardKey::Character),
    }))
}

#[cfg(unix)]
fn read_pending_input(pending: &mut Vec<u8>, timeout: Option<Duration>) -> WizardResult<bool> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        let poll_timeout = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(false);
                }
                deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .min(i32::MAX as u128) as i32
            }
            None => -1,
        };
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, poll_timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(LlmConfigUiError::Terminal(error.to_string()));
        }
        if ready == 0 {
            return Ok(false);
        }
        let mut bytes = [0u8; 256];
        let count = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                bytes.as_mut_ptr().cast::<libc::c_void>(),
                bytes.len(),
            )
        };
        if count > 0 {
            pending.extend_from_slice(&bytes[..count as usize]);
            return Ok(true);
        }
        if count == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(LlmConfigUiError::Terminal(error.to_string()));
        }
    }
}

#[cfg(not(unix))]
fn read_wizard_key(_: &mut Vec<u8>) -> WizardResult<Option<WizardKey>> {
    Ok(None)
}

fn utf8_sequence_len(first: u8) -> usize {
    match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}

fn config_box(config: &LlmConfig) -> String {
    let transport = llm::transport_status(&config.url).unwrap_or("无效");
    let lines = vec![
        format!("配置: {}", config.name),
        format!("FORMAT: {}", config.request_format),
        format!("JSON Schema: {}", config.json_schema.status()),
        format!("URL: {}", config.url),
        format!("传输: {transport}"),
        format!("档位: {}", config.tier.as_str()),
        format!("token: {}", llm::mask_auth(Some(&config.access_token))),
        format!("flash: {}", config.models.flash),
        format!("standard: {}", config.models.standard),
        format!("max: {}", config.models.max),
    ];
    let width = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()))
        .max()
        .unwrap_or(0);
    let mut output = format!("┌{}┐\n", "─".repeat(width + 2));
    for line in lines {
        let padding = width - UnicodeWidthStr::width(line.as_str());
        output.push_str(&format!("│ {line}{} │\n", " ".repeat(padding)));
    }
    output.push_str(&format!("└{}┘", "─".repeat(width + 2)));
    output
}

/// 运行 `zh llm` 配置收集流程。
///
/// # Arguments
///
/// - `action`：命令行已经确定的创建或修改动作、可选目标和当前配置名；
/// - `records`：应用服务预先读取的配置清单；用于名称补全、修改和损坏文件处理。
/// - `plugins`：已经完整验证、可供精确选择的 Codec 清单。
///
/// # Returns
///
/// 成功时只返回候选配置和保存模式。本函数不写入文件、不更新活动标记，也不修改会话。
///
/// # Errors
///
/// 用户在任意阶段取消或输入流结束时返回 [`LlmConfigUiError::Cancelled`]；其他终端错误映射为
/// [`LlmConfigUiError::Terminal`]。编辑器不设置无操作超时；`:wq` 可在任意编辑阶段返回草稿。
pub(crate) fn run(
    action: &LlmConfigAction,
    records: &[ConfigRecord],
    plugins: &[PluginSummary],
) -> WizardResult<LlmConfigDecision> {
    let mut wizard = Wizard::new()?;
    let (mut draft, name_locked, modifying_current) = match action {
        LlmConfigAction::Create { name } => {
            let (name, locked) = match name {
                Some(name) => {
                    validate_name(name).map_err(LlmConfigUiError::Terminal)?;
                    (name.clone(), true)
                }
                None => (String::new(), false),
            };
            if !name.is_empty() && records.iter().any(|record| record.name == name) {
                return Err(LlmConfigUiError::Terminal(format!(
                    "配置 {name} 已存在；使用 `zh llm -m {name}` 修改"
                )));
            }
            (LlmProfileDraft::empty(name), locked, false)
        }
        LlmConfigAction::Modify { name, current_name } => {
            if records.is_empty() {
                return Err(LlmConfigUiError::Terminal(
                    "没有可修改的配置；使用 `zh llm` 创建".into(),
                ));
            }
            let name = match name {
                Some(name) => {
                    validate_name(name).map_err(LlmConfigUiError::Terminal)?;
                    name.clone()
                }
                None => {
                    let labels: Vec<_> =
                        records.iter().map(|record| record.name.as_str()).collect();
                    let default = current_name
                        .as_deref()
                        .and_then(|current| labels.iter().position(|name| *name == current))
                        .unwrap_or(0);
                    labels[wizard.select("修改配置", &labels, default)?].to_string()
                }
            };
            let record = records
                .iter()
                .find(|record| record.name == name)
                .ok_or_else(|| {
                    LlmConfigUiError::Terminal(format!(
                        "配置 {name} 不存在；使用 `zh llm {name}` 创建"
                    ))
                })?;
            let draft = match &record.profile {
                Ok(profile) => profile.draft.clone(),
                Err(error) => {
                    eprintln!("! 配置 {name} 无法读取: {error}");
                    match wizard.select("如何处理损坏的配置", &["取消", "覆盖并重新填写"], 0)?
                    {
                        0 => return Err(LlmConfigUiError::Cancelled),
                        _ => LlmProfileDraft::empty(name.clone()),
                    }
                }
            };
            let modifying_current = current_name.as_deref() == Some(name.as_str());
            (draft, true, modifying_current)
        }
    };
    loop {
        match edit_profile(&mut wizard, draft, plugins, name_locked)? {
            EditorOutcome::SaveDraft(draft) => {
                return Ok(LlmConfigDecision {
                    draft,
                    mode: SaveMode::Save,
                });
            }
            EditorOutcome::Complete(profile) => {
                let config = profile.config().expect("complete editor outcome");
                eprintln!("{}", config_box(config));
                let default = usize::from(modifying_current);
                match wizard.final_select("下一步", &["保存", "保存并启用", "取消"], default)?
                {
                    None | Some(0) => {
                        return Ok(LlmConfigDecision {
                            draft: profile.draft,
                            mode: SaveMode::Save,
                        });
                    }
                    Some(1) => {
                        return Ok(LlmConfigDecision {
                            draft: profile.draft,
                            mode: SaveMode::SaveAndActivate,
                        });
                    }
                    Some(_) => {
                        let confirmation =
                            wizard.select("确认取消？", &["返回配置", "放弃配置并退出"], 0)?;
                        if confirmation == 1 {
                            return Err(LlmConfigUiError::Cancelled);
                        }
                        draft = profile.draft;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_an_aligned_configuration_box() {
        let config = LlmConfig {
            name: "测试".into(),
            url: "https://example.com".into(),
            request_format: "openai@0.3.0".into(),
            json_schema: crate::llm::JsonSchemaResolution::Off,
            access_token: "abcdefghijklmnop".into(),
            models: ModelTiers {
                flash: "fast".into(),
                standard: "normal".into(),
                max: "best".into(),
            },
            tier: ModelTier::Flash,
        };
        let box_text = config_box(&config);
        let widths: Vec<_> = box_text.lines().map(UnicodeWidthStr::width).collect();
        assert!(widths.iter().all(|width| *width == widths[0]));
        assert!(box_text.contains("token: abcd*****mnop"));
        assert!(box_text.contains("传输: HTTPS"));
    }

    #[test]
    fn advancing_model_fields_fills_only_missing_defaults() {
        let mut draft = LlmProfileDraft::empty("test");
        draft.flash = "fast".into();
        advance_default(&mut draft, ProfileField::Flash);
        assert_eq!(draft.standard, "fast");
        draft.max = "best".into();
        advance_default(&mut draft, ProfileField::Standard);
        assert_eq!(draft.max, "best");
    }

    #[test]
    fn normal_key_region_enters_insert_at_value_end() {
        let mut draft = LlmProfileDraft::empty("test");
        draft.url = "https://例.example/path".into();
        assert_eq!(
            insert_cursor(ProfileField::Url, &draft, 0),
            draft.url.chars().count()
        );
        assert_eq!(
            insert_cursor(
                ProfileField::Url,
                &draft,
                ProfileField::Url.key().len() + 1 + 3
            ),
            3
        );
    }
}
