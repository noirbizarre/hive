//! Render only the visible transcript entries, retaining scroll and expansion per session.

use std::{collections::HashSet, sync::Arc};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};
use serde_json::Value;

use crate::{
    images::{EncodedImage, ImageBounds, ImageFrame},
    state::{Conversation, ConversationLine, LineKind, MessagePart},
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
    epoch: u64,
    selection: Option<((usize, usize), (usize, usize))>,
    image_selection: Option<(usize, usize, Arc<EncodedImage>)>,
    dragging: bool,
    pressed_tool: Option<usize>,
    dragged: bool,
    edge: Option<(usize, isize)>,
    starts: Vec<usize>,
    total_rows: usize,
    seen_revision: u64,
    layout_dirty: bool,
    image_layout: (bool, u16),
    pub selection_invalidated: bool,
}

struct RenderedEntry {
    width: u16,
    revision: u64,
    expanded: bool,
    plain: String,
    layout: Vec<crate::editor::Row>,
    rows: usize,
    image_layout: Option<(bool, u16)>,
    dirty: bool,
    images: Vec<InlineImage>,
}

struct InlineImage {
    caption_row: usize,
    row: usize,
    height: u16,
    image: Arc<EncodedImage>,
    ordinal: usize,
}

fn append_layout(
    plain: &mut String,
    rows: &mut Vec<crate::editor::Row>,
    text: Text<'_>,
    width: u16,
) {
    if !rows.is_empty() {
        plain.push('\n');
    }
    let base = plain.len();
    let (fragment, mut layout) = crate::editor::layout(&text, width);
    // Every fragment uses the entry's logical offsets, so selection crosses images naturally.
    for row in &mut layout {
        row.start += base;
        row.end += base;
        for cell in &mut row.cells {
            cell.offset += base;
            cell.end += base;
        }
    }
    plain.push_str(&fragment);
    rows.extend(layout);
}

fn entry_layout(
    entry: &ConversationLine,
    expanded: bool,
    width: u16,
    mode: (bool, u16),
) -> (String, Vec<crate::editor::Row>, Vec<InlineImage>) {
    if !entry
        .content
        .iter()
        .any(|part| matches!(part, MessagePart::Image(_)))
    {
        let (plain, rows) = crate::editor::layout(&entry_text(entry, expanded, width), width);
        return (plain, rows, Vec::new());
    }
    let mut plain = String::new();
    let mut rows = Vec::new();
    let mut images = Vec::new();
    let header = match entry.kind {
        LineKind::User => Line::styled("▏ You", Theme.accent()),
        LineKind::Thought => Line::styled(" Thinking:", Theme.muted()),
        _ => Line::styled("Agent", Theme.muted()),
    };
    append_layout(&mut plain, &mut rows, Text::from(header), width);
    let mut ordinal = 0;
    for part in &entry.content {
        match part {
            MessagePart::Text(text) => {
                let lines = if entry.kind == LineKind::Thought {
                    text.lines()
                        .map(|line| Line::styled(line.to_owned(), Theme.muted()))
                        .collect()
                } else {
                    markdown(text, width)
                };
                if !lines.is_empty() {
                    append_layout(&mut plain, &mut rows, Text::from(lines), width);
                }
            }
            MessagePart::Image(image) => {
                ordinal += 1;
                let caption_row = rows.len();
                append_layout(
                    &mut plain,
                    &mut rows,
                    Text::from(Line::styled(image.caption(ordinal), Theme.muted())),
                    width,
                );
                let caption_height = rows.len() - caption_row;
                // A reservation must fit with its wrapped caption so scrolling can reveal it fully.
                let height = usize::from(mode.1)
                    .saturating_sub(caption_height)
                    .min(usize::from(ImageBounds::INLINE.rows)) as u16;
                let height =
                    if mode.0 && height > 0 && image.dimensions().is_ok() && !image.failed() {
                        height
                    } else {
                        0
                    };
                // Semantic hit targets exist even when graphics are unavailable or decoding failed.
                images.push(InlineImage {
                    caption_row,
                    row: rows.len(),
                    height,
                    image: image.clone(),
                    ordinal,
                });
                // Pixel reservations have no logical characters and add nothing to copied text.
                rows.extend((0..height).map(|_| crate::editor::Row {
                    start: plain.len(),
                    end: plain.len(),
                    ..Default::default()
                }));
            }
        }
    }
    (plain, rows, images)
}

impl Transcript {
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.image_selection = None;
        self.dragging = false;
        self.pressed_tool = None;
        self.edge = None;
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        if start == end {
            return None;
        }
        let mut text = String::new();
        for index in start.0..=end.0 {
            let entry = self.cache.get(index)?;
            if index > start.0 {
                text.push('\n');
                if self.starts[index] > self.starts[index - 1] + self.cache[index - 1].rows {
                    text.push('\n');
                }
            }
            let from = if index == start.0 { start.1 } else { 0 };
            let to = if index == end.0 {
                end.1
            } else {
                entry.plain.len()
            };
            text.push_str(entry.plain.get(from..to)?);
        }
        Some(text)
    }

    pub fn selected_image(&self) -> Option<Arc<EncodedImage>> {
        self.image_selection
            .as_ref()
            .map(|(_, _, image)| image.clone())
    }

    fn hit(&self, row: usize, column: usize) -> Option<(usize, usize)> {
        let index = self
            .starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1);
        let entry = self.cache.get(index)?;
        Some((
            index,
            entry
                .layout
                .get(row.saturating_sub(self.starts[index]))
                .map_or(entry.plain.len(), |row| row.hit(column)),
        ))
    }

    pub fn autoscroll(&mut self) -> bool {
        let Some((column, direction)) = self.edge else {
            return false;
        };
        let before = self.top.unwrap_or(self.max_top);
        self.scroll(direction);
        let top = self.top.unwrap_or(self.max_top);
        self.top = Some(top);
        let row = if direction < 0 {
            top
        } else {
            top + usize::from(self.area.height.saturating_sub(1))
        };
        if let Some(hit) = self.hit(row, column)
            && let Some((_, end)) = &mut self.selection
        {
            *end = hit;
        }
        top != before
    }

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
        self.layout_dirty = true;
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
        let inside = self.area.contains(Position::new(event.column, event.row));
        if let MouseEventKind::Up(MouseButton::Left) = event.kind {
            if self.dragging
                && !self.dragged
                && inside
                && self.tool_rows.iter().any(|&(index, row)| {
                    Some(index) == self.pressed_tool
                        && row
                            == self.top.unwrap_or(self.max_top)
                                + usize::from(event.row - self.area.y)
                })
                && let Some(index) = self.pressed_tool.take()
            {
                self.toggle(index);
            }
            self.dragging = false;
            self.pressed_tool = None;
            self.edge = None;
            return;
        }
        if !inside && !self.dragging {
            return;
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(-3),
            MouseEventKind::ScrollDown => self.scroll(3),
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let row = self.top.unwrap_or(self.max_top) + (event.row - self.area.y) as usize;
                self.image_selection = self.starts.iter().enumerate().find_map(|(index, start)| {
                    self.cache[index]
                        .images
                        .iter()
                        .find(|image| {
                            row >= start + image.caption_row
                                && row < start + image.row + usize::from(image.height)
                        })
                        .map(|image| (index, image.ordinal, image.image.clone()))
                });
                self.top = Some(self.top.unwrap_or(self.max_top));
                self.selection = self
                    .hit(row, usize::from(event.column.saturating_sub(self.area.x)))
                    .map(|hit| (hit, hit));
                self.dragging = true;
                self.dragged = false;
                self.edge = None;
                self.pressed_tool = self
                    .tool_rows
                    .iter()
                    .find(|(_, header)| *header == row)
                    .map(|&(index, _)| index);
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                self.image_selection = None;
                self.dragged = true;
                // The controller ticks edge scrolling, so event frequency cannot accelerate it.
                self.edge = if event.row < self.area.y {
                    Some((usize::from(event.column.saturating_sub(self.area.x)), -1))
                } else if event.row >= self.area.bottom() {
                    Some((usize::from(event.column.saturating_sub(self.area.x)), 1))
                } else {
                    None
                };
                let row = self.top.unwrap_or(self.max_top)
                    + usize::from(
                        event
                            .row
                            .clamp(self.area.y, self.area.bottom().saturating_sub(1))
                            .saturating_sub(self.area.y),
                    );
                if let Some(hit) =
                    self.hit(row, usize::from(event.column.saturating_sub(self.area.x)))
                    && let Some((_, end)) = &mut self.selection
                {
                    *end = hit;
                }
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub fn render(
        &mut self,
        conversation: &Conversation,
        frame: &mut Frame,
        area: Rect,
        live: bool,
        waiting_for_input: bool,
    ) {
        let mut images = ImageFrame::new(&crate::capabilities::TerminalCapabilities::default());
        self.render_with_images(
            conversation,
            frame,
            area,
            live,
            waiting_for_input,
            &mut images,
        );
    }

    pub fn render_with_images(
        &mut self,
        conversation: &Conversation,
        frame: &mut Frame,
        area: Rect,
        live: bool,
        waiting_for_input: bool,
        images: &mut ImageFrame,
    ) {
        self.area = area;
        if self.epoch != conversation.transcript_epoch {
            *self = Self {
                area,
                epoch: conversation.transcript_epoch,
                ..Self::default()
            };
        }
        let width_changed = self.width != area.width;
        let image_layout = (images.enabled(), area.height);
        if self.image_layout != image_layout {
            self.image_layout = image_layout;
            self.layout_dirty = true;
        }
        if width_changed {
            // Width changes affect Markdown tables and wrapped hit targets, not just line counts.
            self.width = area.width;
            self.layout_dirty = true;
        }
        if area.is_empty() {
            self.clear_selection();
            return;
        }
        // Scrolling and caret-only redraws must not scan every entry in a large retained history.
        if self.layout_dirty
            || self.seen_revision != conversation.lines_revision
            || self.cache.len() != conversation.lines.len()
        {
            self.layout_dirty = false;
            self.seen_revision = conversation.lines_revision;
            self.tool_rows.clear();
            let mut first_changed = self.cache.len().min(conversation.lines.len());
            for (index, entry) in conversation.lines.iter().enumerate() {
                let expanded = self.expanded.contains(&index);
                // Composer growth changes height without changing text wrapping in retained history.
                let entry_image_layout = entry
                    .content
                    .iter()
                    .any(|part| matches!(part, MessagePart::Image(_)))
                    .then_some(image_layout);
                if self.cache.get(index).is_none_or(|cached| {
                    cached.width != area.width
                        || cached.image_layout != entry_image_layout
                        || cached.dirty
                        || cached.revision != entry.revision
                        || cached.expanded != expanded
                }) {
                    let (plain, layout, entry_images) =
                        entry_layout(entry, expanded, area.width, image_layout);
                    let count = layout.len();
                    if self
                        .selection
                        .is_some_and(|(a, b)| index >= a.0.min(b.0) && index <= a.0.max(b.0))
                        && self
                            .cache
                            .get(index)
                            .is_some_and(|cached| cached.plain != plain)
                    {
                        self.clear_selection();
                        self.selection_invalidated = true;
                    }
                    let cached = RenderedEntry {
                        width: area.width,
                        revision: entry.revision,
                        expanded,
                        plain,
                        layout,
                        rows: count,
                        image_layout: entry_image_layout,
                        dirty: false,
                        images: entry_images,
                    };
                    first_changed = first_changed.min(index);
                    if index == self.cache.len() {
                        self.cache.push(cached);
                    } else {
                        self.cache[index] = cached;
                    }
                }
            }
            self.cache.truncate(conversation.lines.len());
            if self
                .image_selection
                .as_ref()
                .is_some_and(|(index, ordinal, image)| {
                    self.cache.get(*index).is_none_or(|entry| {
                        !entry.images.iter().any(|candidate| {
                            candidate.ordinal == *ordinal && Arc::ptr_eq(&candidate.image, image)
                        })
                    })
                })
            {
                self.image_selection = None;
                self.selection_invalidated = true;
            }
            if first_changed < self.starts.len() {
                self.starts.truncate(first_changed);
            }
            let mut rows = self
                .starts
                .last()
                .map_or(0, |&start| start + self.cache[self.starts.len() - 1].rows);
            for (index, entry) in conversation
                .lines
                .iter()
                .enumerate()
                .skip(self.starts.len())
            {
                if index > 0
                    && !(entry.kind == LineKind::Tool
                        && conversation.lines[index - 1].kind == LineKind::Tool)
                {
                    rows += 1;
                }
                self.starts.push(rows);
                rows += self.cache[index].rows;
            }
            self.total_rows = rows;
            for (index, entry) in conversation.lines.iter().enumerate() {
                if entry.kind == LineKind::Tool {
                    self.tool_rows.push((index, self.starts[index]));
                }
            }
        }
        self.max_top = self.total_rows.saturating_sub(area.height as usize);
        if let Some(top) = &mut self.top {
            *top = (*top).min(self.max_top);
        }
        let top = self.top.unwrap_or(self.max_top);
        let bottom = top + area.height as usize;
        let first = self
            .starts
            .partition_point(|&start| start <= top)
            .saturating_sub(1);
        let mut failed_entries = Vec::new();
        for (index, &start) in self.starts.iter().enumerate().skip(first) {
            let cached = &self.cache[index];
            let end = start + cached.rows;
            if end <= top || start >= bottom {
                if start >= bottom {
                    break;
                }
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
            let range = self.selection.and_then(|(a, b)| {
                let (a, b) = if a <= b { (a, b) } else { (b, a) };
                (index >= a.0 && index <= b.0).then_some(
                    if index == a.0 { a.1 } else { 0 }..if index == b.0 {
                        b.1
                    } else {
                        cached.plain.len()
                    },
                )
            });
            let mut text = Text::from(
                cached
                    .layout
                    .iter()
                    .skip(visible_start - start)
                    .take(visible_end - visible_start)
                    .map(|row| row.line(range.clone()))
                    .collect::<Vec<_>>(),
            );
            if let Some((selected, ordinal, _)) = &self.image_selection
                && *selected == index
                && let Some(image) = cached.images.iter().find(|image| image.ordinal == *ordinal)
            {
                for row in image.caption_row..image.row {
                    if let Some(line) = text
                        .lines
                        .get_mut((start + row).saturating_sub(visible_start))
                        && start + row >= visible_start
                    {
                        line.style = Theme.selected();
                    }
                }
            }
            if self.selected == Some(index)
                && visible_start == start
                && let Some(header) = text.lines.first_mut()
            {
                header.style = Theme.selected();
            }
            if visible_start == start
                && let Some(header) = text.lines.first_mut()
            {
                let entry = &conversation.lines[index];
                let active = live
                    && (entry
                        .tool
                        .as_ref()
                        .is_some_and(|tool| tool["status"] == "in_progress")
                        || (entry.kind == LineKind::Thought
                            && conversation.active_thought == Some(index)
                            && conversation.foreground != "requires_action"
                            && !waiting_for_input));
                if matches!(entry.kind, LineKind::Tool | LineKind::Thought) {
                    // Replace only the reserved cell, leaving cached text and selection offsets stable.
                    if let Some(span) = header.spans.first_mut() {
                        span.content = span.content.chars().skip(1).collect::<String>().into();
                    }
                    header.spans.insert(0, crate::activity::marker(active));
                }
            }
            frame.render_widget(Paragraph::new(text).style(Theme.text()), target);
            for image in &cached.images {
                if image.height == 0 {
                    continue;
                }
                let image_start = start + image.row;
                let image_end = image_start + usize::from(image.height);
                if start + image.caption_row >= top && image_end <= bottom {
                    let pixels = Rect::new(
                        area.x,
                        area.y + (image_start - top) as u16,
                        area.width.min(ImageBounds::INLINE.columns),
                        image.height,
                    );
                    frame.render_widget(ratatui::widgets::Clear, pixels);
                    match image.image.source() {
                        Ok(source) => images.place(source, pixels),
                        Err(_) => {
                            // Show decode failures immediately; the next layout drops their pixel rows.
                            frame.render_widget(
                                Paragraph::new(image.image.caption(image.ordinal))
                                    .wrap(ratatui::widgets::Wrap { trim: false })
                                    .style(Theme.muted()),
                                pixels,
                            );
                            failed_entries.push(index);
                        }
                    }
                } else if image_start < bottom && image_end > top {
                    // Do not scale a partial reservation: that would make images jump while scrolling.
                    frame.render_widget(
                        Paragraph::new("[image partially visible; scroll to view]")
                            .style(Theme.muted()),
                        Rect::new(
                            area.x,
                            area.y + (image_start.max(top) - top) as u16,
                            area.width,
                            1,
                        ),
                    );
                }
            }
        }
        for index in failed_entries {
            self.cache[index].dirty = true;
            self.layout_dirty = true;
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
            format!(
                " {}",
                short(
                    &format!(
                        "{} {}",
                        if expanded { "▾" } else { "▸" },
                        tool_summary(tool)
                    ),
                    width.saturating_sub(1),
                )
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
        LineKind::Thought => Text::from(
            entry
                .text
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    Line::styled(
                        if index == 0 {
                            format!(" {line}")
                        } else {
                            line.to_owned()
                        },
                        Theme.muted(),
                    )
                })
                .collect::<Vec<_>>(),
        ),
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
            .draw(|frame| view.render(conversation, frame, frame.area(), true, false))
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

    fn image_conversation() -> Conversation {
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message","messageId":"images","content":[
                {"type":"text","text":"before"}, crate::images::fixture_block(),
                {"type":"text","text":"between"}, crate::images::second_fixture_block(),
                {"type":"text","text":"after"}
            ]}),
        );
        conversation
    }

    #[test]
    fn clicking_an_image_selects_its_data_even_without_graphics_and_dragging_remains_text() {
        let mut conversation = image_conversation();
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 60)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        let row = view.cache[0].images[0].caption_row as u16;
        let event = |kind, row| MouseEvent {
            kind,
            row,
            column: 1,
            modifiers: KeyModifiers::NONE,
        };
        view.mouse(event(MouseEventKind::Down(MouseButton::Left), row));
        view.mouse(event(MouseEventKind::Up(MouseButton::Left), row));
        assert!(view.selected_image().is_some());
        assert!(view.selected_text().is_none());
        view.mouse(event(MouseEventKind::Down(MouseButton::Left), row));
        view.mouse(event(MouseEventKind::Drag(MouseButton::Left), row + 2));
        view.mouse(event(MouseEventKind::Up(MouseButton::Left), row + 2));
        assert!(view.selected_image().is_none());
        assert!(view.selected_text().is_some());
        view.mouse(event(MouseEventKind::Down(MouseButton::Left), row));
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message","messageId":"images","content":[{"type":"text","text":"replacement"}]}),
        );
        draw(&mut view, &conversation, &mut terminal);
        assert!(view.selected_image().is_none());
    }

    fn draw_images(
        view: &mut Transcript,
        conversation: &Conversation,
        terminal: &mut Terminal<TestBackend>,
        support: crate::capabilities::Support,
    ) -> ImageFrame {
        let mut capabilities = crate::capabilities::TerminalCapabilities::default();
        capabilities.kitty_graphics.effective = support;
        let mut images = ImageFrame::new(&capabilities);
        terminal
            .draw(|frame| {
                view.render_with_images(conversation, frame, frame.area(), true, false, &mut images)
            })
            .unwrap();
        images
    }

    #[test]
    fn acp_images_reserve_ordered_rows_and_copy_captions_without_pixel_padding() {
        use crate::capabilities::Support;
        let conversation = image_conversation();
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 60)).unwrap();
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert_eq!(
            images.areas(),
            vec![Rect::new(0, 3, 80, 20), Rect::new(0, 25, 80, 20)]
        );
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 1)].symbol(), "b");
        assert_eq!(buffer[(0, 23)].symbol(), "b");
        assert_eq!(buffer[(0, 45)].symbol(), "a");
        assert_eq!(
            view.cache[0].images[0].image.source().unwrap().dimensions(),
            (8, 8)
        );
        assert_eq!(
            view.cache[0].images[1].image.source().unwrap().dimensions(),
            (3, 2)
        );
        let event = |kind, row, column| MouseEvent {
            kind,
            row,
            column,
            modifiers: KeyModifiers::NONE,
        };
        view.mouse(event(MouseEventKind::Down(MouseButton::Left), 0, 0));
        view.mouse(event(MouseEventKind::Drag(MouseButton::Left), 45, 5));
        view.mouse(event(MouseEventKind::Up(MouseButton::Left), 45, 5));
        assert!(!view.autoscroll());
        assert_eq!(
            view.selected_text().unwrap(),
            "Agent\nbefore\n[image 1: image/png] 8×8\nbetween\n[image 2: image/png] 3×2\nafter"
        );
        assert_eq!(
            view.hit(10, 50),
            Some((0, "Agent\nbefore\n[image 1: image/png] 8×8".len()))
        );
    }

    #[test]
    fn fallback_and_offscreen_images_do_not_prepare_pixels() {
        use crate::capabilities::Support;
        use base64::Engine as _;
        let pixels = image::RgbImage::from_pixel(3, 2, image::Rgb([20, 80, 150]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        let block = json!({"type":"image","mimeType":"image/jpeg","data":base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())});
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message","messageId":"images","content":[
                {"type":"text","text":"before"}, block, {"type":"text","text":"between"}, block
            ]}),
        );
        let prepared = || {
            conversation.lines[0]
                .content
                .iter()
                .filter_map(|part| match part {
                    MessagePart::Image(image) => Some(image.is_prepared()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        for support in [Support::Unknown, Support::Unsupported] {
            draw_images(&mut view, &conversation, &mut terminal, support);
            assert_eq!(prepared(), [false, false]);
            assert!(screen(&terminal).contains("[image 1: image/jpeg] 3×2"));
        }
        view.top = Some(0);
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert_eq!(images.areas().len(), 1);
        assert_eq!(prepared(), [true, false]);
        view.boundary(true);
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert_eq!(images.areas().len(), 1);
        assert_eq!(prepared(), [true, true]);
    }

    #[test]
    fn height_and_graphics_changes_reuse_text_only_entry_layouts() {
        use crate::capabilities::Support;
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message","messageId":"text","content":[
                {"type":"text","text":"# Heading\n\nA **formatted** paragraph."}
            ]}),
        );
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        draw_images(&mut view, &conversation, &mut terminal, Support::Unknown);
        let layout = view.cache[0].layout.as_ptr();
        let plain = view.cache[0].plain.as_ptr();
        for (height, support) in [(15, Support::Supported), (30, Support::Unknown)] {
            terminal.backend_mut().resize(100, height);
            terminal.resize(Rect::new(0, 0, 100, height)).unwrap();
            draw_images(&mut view, &conversation, &mut terminal, support);
            assert_eq!(view.cache[0].layout.as_ptr(), layout);
            assert_eq!(view.cache[0].plain.as_ptr(), plain);
        }
    }

    #[test]
    fn a_visible_conversion_failure_becomes_a_cached_caption_without_pixel_rows() {
        use crate::capabilities::Support;
        use base64::Engine as _;
        let pixels = image::RgbImage::from_pixel(3, 2, image::Rgb([20, 80, 150]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut bytes, image::ImageFormat::WebP)
            .unwrap();
        let mut bytes = bytes.into_inner();
        let header = bytes
            .windows(4)
            .position(|marker| marker == b"VP8L")
            .unwrap();
        // Keep the lossless header and chunk length intact, but corrupt the pixel bitstream.
        bytes[header + 8 + 5..].fill(0xff);
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message","messageId":"bad","content":[
            {"type":"image","mimeType":"image/webp","data":base64::engine::general_purpose::STANDARD.encode(bytes)}
            ]}),
        );
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert!(images.areas().is_empty());
        assert!(screen(&terminal).contains("could not decode or convert image"));
        assert!(view.cache[0].dirty);
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert!(images.areas().is_empty());
        assert!(view.cache[0].images.iter().all(|image| image.height == 0));
        assert!(!view.cache[0].dirty);
        assert!(
            view.cache[0]
                .plain
                .contains("could not decode or convert image")
        );
    }

    #[test]
    fn disabled_unknown_and_invalid_images_use_labels_without_graphics() {
        use crate::capabilities::Support;
        let mut conversation = image_conversation();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message","messageId":"bad","content":[
                {"type":"image","mimeType":"image/gif","data":"private-data"},
                {"type":"image","mimeType":"image/png","data":"invalid-base64"}
            ]}),
        );
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 60)).unwrap();
        for support in [
            Support::Unsupported,
            Support::Unknown,
            Support::Supported,
            Support::Unknown,
        ] {
            let images = draw_images(&mut view, &conversation, &mut terminal, support);
            assert_eq!(
                images.areas().len(),
                if support == Support::Supported { 2 } else { 0 }
            );
            let text = screen(&terminal);
            assert!(text.contains("[image 1: image/png] 8×8"));
            assert!(text.contains("unsupported image format image/gif"));
            assert!(text.contains("invalid base64 image data"));
            assert!(!text.contains("private-data"));
        }
    }

    #[test]
    fn scrolling_clips_images_without_overlapping_text_and_resize_reflows_reservations() {
        use crate::capabilities::Support;
        let conversation = image_conversation();
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        view.top = Some(0);
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert_eq!(images.areas(), vec![Rect::new(0, 3, 80, 20)]);
        view.top = Some(4);
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert!(images.areas().is_empty());
        assert!(screen(&terminal).contains("partially visible"));
        view.boundary(true);
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert_eq!(images.areas().len(), 1);
        assert_eq!(terminal.backend().buffer()[(0, 24)].symbol(), "a");
        for (width, height) in [(12, 8), (1, 1), (0, 0), (100, 60)] {
            terminal.backend_mut().resize(width, height);
            terminal.resize(Rect::new(0, 0, width, height)).unwrap();
            let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
            assert!(
                images
                    .areas()
                    .iter()
                    .all(|area| area.right() <= width && area.bottom() <= height)
            );
        }
        assert_eq!(view.cache[0].images.len(), 2);
    }

    #[test]
    fn acp_redraws_and_appended_text_reuse_uploads_and_replacement_frees_them() {
        use crate::{capabilities::Support, images::KittyRenderer};
        let mut conversation = image_conversation();
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 60)).unwrap();
        let mut renderer = KittyRenderer::default();
        let mut bytes = Vec::new();
        renderer
            .finish(
                &mut bytes,
                draw_images(&mut view, &conversation, &mut terminal, Support::Supported),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes).matches("a=t,").count(), 2);
        bytes.clear();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message_chunk","messageId":"images","content":{"type":"text","text":"!"}}),
        );
        renderer.begin(&mut bytes).unwrap();
        renderer
            .finish(
                &mut bytes,
                draw_images(&mut view, &conversation, &mut terminal, Support::Supported),
            )
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("a=t,"));
        assert!(screen(&terminal).contains("after!"));
        bytes.clear();
        update(
            &mut conversation,
            json!({"sessionUpdate":"agent_message","messageId":"images","content":null}),
        );
        renderer
            .finish(
                &mut bytes,
                draw_images(&mut view, &conversation, &mut terminal, Support::Supported),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&bytes).matches("a=d,d=I").count(),
            2
        );
        assert!(view.cache[0].images.is_empty());
        conversation.begin_transcript(conversation.seq);
        let images = draw_images(&mut view, &conversation, &mut terminal, Support::Supported);
        assert!(images.areas().is_empty());
        assert!(view.cache.is_empty());
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
    fn spinner_frames_leave_cached_layout_selection_and_revisions_unchanged() {
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"tool_call_update","toolCallId":"call","title":"Compile","status":"in_progress"}),
        );
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        let mut render = |view: &mut Transcript, phase| {
            terminal
                .draw(|frame| {
                    view.render(&conversation, frame, frame.area(), true, false);
                    assert!(crate::activity::finish(frame, phase, true));
                })
                .unwrap();
            screen(&terminal)
        };
        let first = render(&mut view, 0);
        view.selection = Some(((0, 0), (0, view.cache[0].plain.len())));
        let copied = view.selected_text();
        let plain = view.cache[0].plain.clone();
        let rows = view.starts.clone();
        let revision = conversation.lines_revision;
        let second = render(&mut view, 1);
        assert_ne!(first, second);
        assert!(first.contains('⠋'));
        assert!(second.contains('⠙'));
        assert_eq!(view.selected_text(), copied);
        assert_eq!(view.cache[0].plain, plain);
        assert_eq!(view.starts, rows);
        assert_eq!(conversation.lines_revision, revision);
        assert!(!view.selection_invalidated);
    }

    #[test]
    fn null_message_patches_invalidate_cached_text_for_every_speaker() {
        let mut conversation = Conversation::default();
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        for kind in ["user_message", "agent_message", "agent_thought"] {
            update(
                &mut conversation,
                json!({"sessionUpdate":kind,"messageId":"same","content":[{"type":"text","text":"Obsolete text"}]}),
            );
            draw(&mut view, &conversation, &mut terminal);
            assert!(screen(&terminal).contains("Obsolete text"));
            update(
                &mut conversation,
                json!({"sessionUpdate":kind,"messageId":"same","content":null}),
            );
            draw(&mut view, &conversation, &mut terminal);
            assert!(!screen(&terminal).contains("Obsolete text"));
        }
        assert_eq!(conversation.lines.len(), 3);
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
        view.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 5));
        draw(&mut view, &conversation, &mut terminal);
        let rendered = screen(&terminal);
        assert!(rendered.contains("▾ read"));
        assert!(rendered.contains("README.md"));
        assert!(rendered.contains("Readable answer"));
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains("\\n"));
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 5));
        view.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 5));
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
        view.mouse(mouse(MouseEventKind::Up(MouseButton::Left), row, 1));
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

    #[test]
    fn selecting_wrapped_wide_text_copies_logical_lines_without_screen_padding() {
        let mut conversation = Conversation::default();
        conversation.push_notice("hello 界 world and more words".into());
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(12, 10)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        let expected = view.cache[0].plain.clone();
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        let last = view.cache[0].rows as u16 - 1;
        view.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), last, 11));
        view.mouse(mouse(MouseEventKind::Up(MouseButton::Left), last, 11));
        assert_eq!(view.selected_text().as_deref(), Some(expected.as_str()));
        terminal.resize(Rect::new(0, 0, 18, 10)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        assert_eq!(view.selected_text().as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn dragging_a_tool_header_selects_instead_of_expanding_and_release_outside_ends_the_drag() {
        let mut conversation = Conversation::default();
        update(
            &mut conversation,
            json!({"sessionUpdate":"tool_call_update", "toolCallId":"tool", "name":"bash", "rawInput":{"command":"cargo test"}}),
        );
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        view.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 0, 8));
        view.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 15, 8));
        assert!(view.expanded.is_empty());
        assert!(!view.dragging);
        assert!(view.selected_text().is_some());
    }

    #[test]
    fn snapshot_replacement_cannot_reuse_cached_text_or_selection() {
        let mut conversation = Conversation::default();
        conversation.push_notice("old text".into());
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        view.selection = Some(((0, 0), (0, 3)));
        conversation.begin_transcript(1);
        conversation.push_notice("replacement".into());
        draw(&mut view, &conversation, &mut terminal);
        assert!(screen(&terminal).contains("replacement"));
        assert!(!screen(&terminal).contains("old text"));
        assert!(view.selected_text().is_none());
    }

    #[test]
    fn resizing_an_active_drag_to_an_empty_viewport_cancels_it_safely() {
        let mut conversation = Conversation::default();
        conversation.push_notice("some text".into());
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        terminal
            .draw(|frame| view.render(&conversation, frame, Rect::new(0, 4, 10, 0), true, false))
            .unwrap();
        view.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 2, 0));
        assert!(!view.dragging);
        terminal
            .draw(|frame| view.render(&conversation, frame, Rect::new(0, 0, 10, 8), true, false))
            .unwrap();
        assert_eq!(view.cache[0].width, 10);
    }

    #[test]
    fn edge_scrolling_extends_selection_across_messages_and_stops_on_release() {
        let mut conversation = Conversation::default();
        for index in 0..10 {
            conversation.push_notice(format!("Entry {index}"));
        }
        let mut view = Transcript::default();
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        draw(&mut view, &conversation, &mut terminal);
        view.boundary(false);
        draw(&mut view, &conversation, &mut terminal);
        view.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        view.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 6, 19));
        assert_eq!(view.top, Some(0));
        assert!(view.autoscroll());
        assert!(view.autoscroll());
        assert_eq!(view.top, Some(2));
        let selected = view.selected_text().unwrap();
        assert!(selected.starts_with("Entry 0\n\nEntry 1"), "{selected:?}");
        view.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 6, 19));
        assert!(!view.autoscroll());
    }
}
