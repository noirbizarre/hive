//! Render only the visible transcript entries, retaining scroll and expansion per session.

use std::collections::HashSet;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};
use serde_json::Value;

use crate::{
    state::{Conversation, ConversationLine, LineKind},
    theme::Theme,
};

#[derive(Default)]
pub struct Transcript {
    // None follows live output; an absolute top row stays put when new messages arrive.
    pub top: Option<usize>,
    pub max_top: usize,
    pub area: Rect,
    expanded: HashSet<usize>,
    selected: Option<usize>,
    tool_rows: Vec<(usize, usize)>,
    cache: Vec<RenderedEntry>,
    width: u16,
}

struct RenderedEntry {
    revision: u64,
    expanded: bool,
    text: Text<'static>,
    rows: usize,
}

impl Transcript {
    pub fn scroll(&mut self, amount: isize) {
        let top = self
            .top
            .unwrap_or(self.max_top)
            .saturating_add_signed(amount)
            .min(self.max_top);
        self.top = (top < self.max_top).then_some(top);
    }

    pub fn boundary(&mut self, end: bool) {
        self.top = if end { None } else { Some(0) };
    }

    pub fn page(&mut self, down: bool) {
        let step = self.area.height.saturating_sub(1).max(1) as isize;
        self.scroll(if down { step } else { -step });
    }

    pub fn select_tool(&mut self, next: bool) {
        let current = self
            .selected
            .and_then(|id| self.tool_rows.iter().position(|(index, _)| *index == id));
        let top = self.top.unwrap_or(self.max_top);
        let position = match current {
            Some(index) if next => (index + 1).min(self.tool_rows.len().saturating_sub(1)),
            Some(index) => index.saturating_sub(1),
            None if next => self
                .tool_rows
                .iter()
                .position(|(_, row)| *row >= top)
                .unwrap_or(self.tool_rows.len().saturating_sub(1)),
            None => self
                .tool_rows
                .iter()
                .rposition(|(_, row)| *row < top + self.area.height as usize)
                .unwrap_or(0),
        };
        if let Some(&(index, row)) = self.tool_rows.get(position) {
            self.selected = Some(index);
            self.top = Some(row.min(self.max_top));
        }
    }

    pub fn toggle_selected(&mut self) {
        if self.selected.is_none() {
            self.select_tool(false);
        }
        if let Some(index) = self.selected {
            self.toggle(index);
        }
    }

    fn toggle(&mut self, index: usize) {
        self.selected = Some(index);
        let row = self
            .tool_rows
            .iter()
            .find(|(id, _)| *id == index)
            .map_or(0, |(_, row)| *row);
        // Anchor at the header when opening so following the bottom cannot hide the new details.
        if self.expanded.insert(index) {
            self.top = Some(row);
        } else {
            self.expanded.remove(&index);
            self.top = Some(self.top.unwrap_or(self.max_top).min(row));
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) {
        if !self.area.contains(Position::new(event.column, event.row)) {
            return;
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(-3),
            MouseEventKind::ScrollDown => self.scroll(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = self.top.unwrap_or(self.max_top) + (event.row - self.area.y) as usize;
                if let Some(&(index, _)) = self.tool_rows.iter().find(|(_, header)| *header == row)
                {
                    self.toggle(index);
                }
            }
            _ => {}
        }
    }

    pub fn render(&mut self, conversation: &Conversation, frame: &mut Frame, area: Rect) {
        self.area = area;
        if self.width != area.width {
            // Width changes affect Markdown tables and wrapped hit targets, not just line counts.
            self.cache.clear();
            self.width = area.width;
        }
        if area.is_empty() {
            return;
        }
        self.tool_rows.clear();
        let mut layout = Vec::new();
        let mut rows = 0;
        for (index, entry) in conversation.lines.iter().enumerate() {
            let expanded = self.expanded.contains(&index);
            if self.cache.get(index).is_none_or(|cached| {
                cached.revision != entry.revision || cached.expanded != expanded
            }) {
                let text = entry_text(entry, expanded, area.width);
                // Use the widget's word wrapping, not character-count estimates, for exact hit testing.
                let count = Paragraph::new(text.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(area.width);
                let cached = RenderedEntry {
                    revision: entry.revision,
                    expanded,
                    text,
                    rows: count,
                };
                if index == self.cache.len() {
                    self.cache.push(cached);
                } else {
                    self.cache[index] = cached;
                }
            }
            if index > 0
                && !(entry.kind == LineKind::Tool
                    && conversation.lines[index - 1].kind == LineKind::Tool)
            {
                rows += 1;
            }
            if entry.kind == LineKind::Tool {
                self.tool_rows.push((index, rows));
            }
            layout.push((index, rows));
            rows += self.cache[index].rows;
        }
        self.max_top = rows.saturating_sub(area.height as usize);
        if let Some(top) = &mut self.top {
            *top = (*top).min(self.max_top);
        }
        let top = self.top.unwrap_or(self.max_top);
        let bottom = top + area.height as usize;
        for (index, start) in layout {
            let cached = &self.cache[index];
            let end = start + cached.rows;
            if end <= top || start >= bottom {
                continue;
            }
            let visible_start = start.max(top);
            let visible_end = end.min(bottom);
            let target = Rect::new(
                area.x,
                area.y + (visible_start - top) as u16,
                area.width,
                (visible_end - visible_start) as u16,
            );
            let mut text = cached.text.clone();
            if self.selected == Some(index)
                && let Some(header) = text.lines.first_mut()
            {
                header.style = Theme.selected();
            }
            frame.render_widget(
                Paragraph::new(text)
                    .style(Theme.text())
                    .wrap(Wrap { trim: false })
                    .scroll(((visible_start - start).min(u16::MAX as usize) as u16, 0)),
                target,
            );
        }
    }
}

#[derive(Clone)]
struct MarkdownStyle;

impl tui_markdown::StyleSheet for MarkdownStyle {
    fn heading(&self, _: u8) -> Style {
        Theme.accent().add_modifier(Modifier::BOLD)
    }
    fn heading_marker(&self, _: u8) -> &str {
        ""
    }
    fn code_block_fence(&self) -> &str {
        ""
    }
    fn code(&self) -> Style {
        Theme.accent()
    }
    fn link(&self) -> Style {
        Theme.accent().add_modifier(Modifier::UNDERLINED)
    }
    fn blockquote(&self) -> Style {
        Theme.muted()
    }
    fn table_header(&self) -> Style {
        Theme.accent().add_modifier(Modifier::BOLD)
    }
    fn table_border(&self) -> Style {
        Theme.muted()
    }
}

fn markdown(input: &str, width: u16) -> Vec<Line<'static>> {
    let options = tui_markdown::Options::new(MarkdownStyle).table_width(width);
    tui_markdown::from_str_with_options(input, &options)
        .lines
        .into_iter()
        .map(|line| {
            let spans = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect::<Vec<_>>();
            Line::from(spans).style(line.style)
        })
        .collect()
}

fn entry_text(entry: &ConversationLine, expanded: bool, width: u16) -> Text<'static> {
    if let Some(tool) = &entry.tool {
        let mut lines = vec![Line::styled(
            short(
                &format!(
                    "{} {}",
                    if expanded { "▾" } else { "▸" },
                    tool_summary(tool)
                ),
                width,
            ),
            if tool["status"] == "failed" {
                Theme.warning()
            } else {
                Theme.accent()
            },
        )];
        if expanded {
            tool_details(tool, width, &mut lines);
        }
        return Text::from(lines);
    }
    match entry.kind {
        LineKind::Agent | LineKind::User => {
            let (label, prefix, style) = if entry.kind == LineKind::User {
                ("▏ You", "You: ", Theme.accent())
            } else {
                ("Agent", "Agent: ", Theme.muted())
            };
            let mut lines = vec![Line::styled(label, style)];
            lines.extend(markdown(
                entry.text.strip_prefix(prefix).unwrap_or(&entry.text),
                width,
            ));
            Text::from(lines)
        }
        _ => Text::from(
            entry
                .text
                .lines()
                .map(|line| Line::styled(line.to_owned(), Theme.muted()))
                .collect::<Vec<_>>(),
        ),
    }
}

fn short(input: &str, width: u16) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if Line::raw(&normalized).width() <= width as usize {
        return normalized;
    }
    let mut output = String::new();
    for ch in normalized.chars() {
        if Line::raw(format!("{output}{ch}…")).width() > width as usize {
            break;
        }
        output.push(ch);
    }
    if width > 0 {
        output.push('…');
    }
    output
}

fn tool_summary(tool: &Value) -> String {
    let name = tool["name"]
        .as_str()
        .or_else(|| tool["kind"].as_str().filter(|kind| *kind != "other"))
        .unwrap_or("tool");
    let title = tool["title"]
        .as_str()
        .filter(|title| !title.is_empty() && *title != name);
    let input = &tool["rawInput"];
    let action = title
        .or_else(|| {
            [
                "description",
                "command",
                "filePath",
                "file_path",
                "path",
                "pattern",
                "query",
                "url",
            ]
            .iter()
            .find_map(|key| input[key].as_str())
        })
        .or_else(|| tool["locations"][0]["path"].as_str())
        .unwrap_or("Tool action");
    let status = tool["status"].as_str().unwrap_or("working");
    format!("{name} [{status}] · {}", short(action, 100))
}

fn literal(input: &str, lines: &mut Vec<Line<'static>>) {
    // Raw input and diffs are literal code; treating them as Markdown would eat indentation or '#'.
    lines.extend(
        input
            .lines()
            .map(|line| Line::styled(format!("  {line}"), Theme.text())),
    );
}

fn section(label: &str, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::styled(
        format!("  {label}"),
        Theme.muted().add_modifier(Modifier::BOLD),
    ));
}

fn output(value: &Value, width: u16, lines: &mut Vec<Line<'static>>) {
    match value {
        Value::String(text) => {
            // Some agents return JSON as text; decode it before rendering nested output strings.
            if let Ok(value @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str(text) {
                output(&value, width, lines);
            } else {
                lines.extend(markdown(text, width));
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                section(key, lines);
                output(value, width, lines);
            }
        }
        Value::Array(values) => {
            for value in values {
                output(value, width, lines);
            }
        }
        Value::Null => {}
        _ => literal(&value.to_string(), lines),
    }
}

fn tool_details(tool: &Value, width: u16, lines: &mut Vec<Line<'static>>) {
    if !tool["rawInput"].is_null() {
        section("Input", lines);
        literal(
            &serde_json::to_string_pretty(&tool["rawInput"]).unwrap_or_default(),
            lines,
        );
    }
    if let Some(content) = tool["content"]
        .as_array()
        .filter(|content| !content.is_empty())
    {
        section("Output", lines);
        for item in content {
            match item["type"].as_str() {
                Some("content") => {
                    let block = &item["content"];
                    if let Some(text) = block["text"]
                        .as_str()
                        .or(block["resource"]["text"].as_str())
                    {
                        output(&Value::String(text.to_owned()), width, lines);
                    } else {
                        literal(
                            &format!(
                                "[{}] {}",
                                block["type"].as_str().unwrap_or("content"),
                                block["uri"].as_str().unwrap_or_default()
                            ),
                            lines,
                        );
                    }
                }
                Some("diff") => {
                    if let Some(patch) = item["patch"]["text"].as_str() {
                        literal(patch, lines);
                    } else {
                        if let Some(path) = item["path"].as_str() {
                            section(path, lines);
                        }
                        if let Some(old) = item["oldText"].as_str() {
                            literal(&format!("--- Before\n{old}"), lines);
                        }
                        if let Some(new) = item["newText"].as_str() {
                            literal(&format!("+++ After\n{new}"), lines);
                        }
                        if item["changes"].is_array() {
                            literal(
                                &serde_json::to_string_pretty(&item["changes"]).unwrap_or_default(),
                                lines,
                            );
                        }
                    }
                }
                Some("terminal") => literal(
                    &format!(
                        "Terminal: {}",
                        item["terminalId"].as_str().unwrap_or("agent-owned")
                    ),
                    lines,
                ),
                _ => literal(
                    &serde_json::to_string_pretty(item).unwrap_or_default(),
                    lines,
                ),
            }
        }
    } else if !tool["rawOutput"].is_null() {
        section("Output", lines);
        output(&tool["rawOutput"], width, lines);
    }
    if lines.len() == 1 {
        section("No details reported yet", lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v2;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn draw(
        view: &mut Transcript,
        conversation: &Conversation,
        terminal: &mut Terminal<TestBackend>,
    ) {
        terminal
            .draw(|frame| view.render(conversation, frame, frame.area()))
            .unwrap();
    }

    fn update(conversation: &mut Conversation, value: Value) {
        let seq = conversation.seq + 1;
        let notification =
            v2::UpdateSessionNotification::new("session", serde_json::from_value(value).unwrap())
                .meta(
                    [("hive".into(), json!({"seq":seq}))]
                        .into_iter()
                        .collect::<v2::Meta>(),
                );
        assert!(conversation.apply(notification));
    }

    fn mouse(kind: MouseEventKind, row: u16, column: u16) -> MouseEvent {
        MouseEvent {
            kind,
            row,
            column,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn markdown_renders_headings_emphasis_lists_tables_and_literal_code() {
        let source = "## Heading\n\n**bold** and *italic* with `code`\n\n- item\n\n| Name | Value |\n| --- | --- |\n| a | b |\n\n```rust\n  let x = 1;\n```";
        let lines = markdown(source, 60);
        let plain = Text::from(lines.clone()).to_string();
        assert!(!plain.contains("##"));
        assert!(!plain.contains("**"));
        assert!(!plain.contains("```"));
        assert!(plain.contains("Heading"));
        assert!(plain.contains("let x = 1;"));
        assert!(plain.contains('│'));
        assert!(lines.iter().any(|line| {
            line.style.add_modifier.contains(Modifier::BOLD)
                || line
                    .spans
                    .iter()
                    .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        }));
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn tools_are_one_line_until_clicked_and_details_decode_text_instead_of_dumping_events() {
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"tool_call_update", "toolCallId":"call", "name":"read",
            "title":"Read the project guide", "status":"completed", "rawInput":{"path":"README.md"},
            "rawOutput":{"output":"# Result\n\n**Readable answer**", "metadata":{"count":2}}}),
        );
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        assert_eq!(view.cache[0].rows, 1);
        let rendered = screen(&terminal);
        assert!(rendered.contains("▸ read [completed] · Read the project guide"));
        assert!(!rendered.contains("Readable answer"));
        assert!(!rendered.contains("rawOutput"));
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 5));
        draw(&mut view, &conversation, &mut terminal);
        let rendered = screen(&terminal);
        assert!(rendered.contains("▾ read"));
        assert!(rendered.contains("README.md"));
        assert!(rendered.contains("Readable answer"));
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains("\\n"));
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 5));
        draw(&mut view, &conversation, &mut terminal);
        assert!(!screen(&terminal).contains("Readable answer"));
    }

    #[test]
    fn scrolling_stays_put_during_streaming_and_end_resumes_following() {
        let mut conversation = Conversation::default();
        for i in 0..40 {
            conversation.push_notice(format!("Message {i}"));
        }
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        assert!(screen(&terminal).contains("Message 39"));
        view.page(false);
        draw(&mut view, &conversation, &mut terminal);
        let top = view.top.unwrap();
        let before = screen(&terminal);
        conversation.push_notice("New output".into());
        draw(&mut view, &conversation, &mut terminal);
        assert_eq!(view.top, Some(top));
        assert_eq!(screen(&terminal), before);
        view.mouse(mouse(MouseEventKind::ScrollUp, 5, 5));
        assert_eq!(view.top, Some(top - 3));
        view.boundary(true);
        draw(&mut view, &conversation, &mut terminal);
        assert!(view.top.is_none());
        assert!(screen(&terminal).contains("New output"));
        view.boundary(false);
        draw(&mut view, &conversation, &mut terminal);
        assert!(screen(&terminal).contains("Message 0"));
    }

    #[test]
    fn wrapped_markdown_and_resizing_keep_tool_click_targets_on_the_header() {
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"m", "content":{"type":"text", "text":"**A heading** with some long words and 日本語 that must wrap onto several rows."}}),
        );
        update(
            &mut conversation,
            json!({"sessionUpdate":"tool_call_update", "toolCallId":"tool", "name":"bash", "rawInput":{"command":"cargo test"}, "rawOutput":"tests passed"}),
        );
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(22, 20)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        terminal.resize(Rect::new(0, 0, 18, 20)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        let row = view.tool_rows[0].1 as u16;
        assert!(row > 3);
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), row - 1, 1));
        assert!(view.expanded.is_empty());
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), row, 1));
        assert!(view.expanded.contains(&1));
        draw(&mut view, &conversation, &mut terminal);
        assert!(screen(&terminal).contains("tests passed"));
    }

    #[test]
    fn histories_larger_than_a_u16_scroll_offset_can_reach_both_ends() {
        let mut conversation = Conversation::default();
        for i in 0..33_000 {
            conversation.push_notice(format!("Entry {i}"));
        }
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        assert!(view.max_top > u16::MAX as usize);
        assert!(screen(&terminal).contains("Entry 32999"));
        view.boundary(false);
        draw(&mut view, &conversation, &mut terminal);
        assert!(screen(&terminal).contains("Entry 0"));
    }
}
