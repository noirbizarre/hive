//! Logical offsets and display cells share one layout, so wide text cannot displace the caret.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub offset: usize,
    pub end: usize,
    pub width: usize,
    pub style: Style,
}

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub start: usize,
    pub end: usize,
}

impl Row {
    pub fn hit(&self, column: usize) -> usize {
        let mut x = 0;
        for cell in &self.cells {
            if column < x + cell.width {
                return cell.offset;
            }
            x += cell.width;
        }
        self.end
    }

    pub fn line(&self, selection: Option<Range<usize>>) -> Line<'static> {
        Line::from(
            self.cells
                .iter()
                .map(|cell| {
                    let selected = selection
                        .as_ref()
                        .is_some_and(|range| cell.offset < range.end && cell.end > range.start);
                    Span::styled(
                        cell.text.clone(),
                        if selected {
                            cell.style.patch(crate::theme::Theme.selected())
                        } else {
                            cell.style
                        },
                    )
                })
                .collect::<Vec<_>>(),
        )
    }
}

/// Lay out styled logical lines once; rendering and pointer hit tests consume these exact rows.
pub fn layout(text: &Text<'_>, width: u16) -> (String, Vec<Row>) {
    let width = usize::from(width.max(1));
    let mut plain = String::new();
    let mut rows = Vec::new();
    for (index, line) in text.lines.iter().enumerate() {
        if index > 0 {
            plain.push('\n');
        }
        let mut cells = Vec::new();
        let logical = line.to_string();
        let base = plain.len();
        plain.push_str(&logical);
        let mut span_start = 0;
        let mut styles = Vec::new();
        for span in &line.spans {
            styles.push((span_start, span_start + span.content.len(), span.style));
            span_start += span.content.len();
        }
        // Markdown may split a grapheme across styled spans; segment the complete logical line.
        for (offset, grapheme) in logical.grapheme_indices(true) {
            let measured = if grapheme == "\t" {
                4
            } else {
                grapheme.width()
            };
            let displayed = if grapheme == "\t" {
                " ".repeat(measured.min(width))
            } else if measured == 0 {
                format!("◌{grapheme}")
            } else if measured > width {
                "�".into()
            } else {
                grapheme.into()
            };
            let style = styles
                .iter()
                .find(|&&(start, end, _)| offset >= start && offset < end)
                .map_or(Style::default(), |&(_, _, style)| style);
            cells.push(Cell {
                text: displayed,
                offset: base + offset,
                end: base + offset + grapheme.len(),
                width: measured.max(1).min(width),
                style: text.style.patch(line.style).patch(style),
            });
        }
        let mut row = Row {
            start: cells.first().map_or(plain.len(), |cell| cell.offset),
            ..Row::default()
        };
        let mut used = 0;
        let mut cursor = 0;
        while cursor < cells.len() {
            // Move whole words when possible, splitting only words wider than the viewport.
            let whitespace = cells[cursor].text.chars().all(char::is_whitespace);
            let end = (cursor + 1..cells.len())
                .find(|&i| cells[i].text.chars().all(char::is_whitespace) != whitespace)
                .unwrap_or(cells.len());
            let word_width: usize = cells[cursor..end].iter().map(|cell| cell.width).sum();
            if !whitespace && used > 0 && used + word_width > width && word_width <= width {
                row.end = cells[cursor].offset;
                rows.push(row);
                row = Row {
                    start: cells[cursor].offset,
                    ..Row::default()
                };
                used = 0;
            }
            for cell in &cells[cursor..end] {
                if used + cell.width > width && used > 0 {
                    row.end = cell.offset;
                    rows.push(row);
                    row = Row {
                        start: cell.offset,
                        ..Row::default()
                    };
                    used = 0;
                }
                used += cell.width;
                row.end = cell.end;
                row.cells.push(cell.clone());
            }
            cursor = end;
        }
        if row.cells.is_empty() {
            row.end = plain.len();
        }
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(Row::default());
    }
    (plain, rows)
}

#[derive(Clone, Default)]
pub struct PromptEditor {
    text: String,
    layout_text: String,
    pub caret: usize,
    anchor: Option<usize>,
    preferred: Option<usize>,
    pub top: usize,
    pub area: Rect,
    pub rows: Vec<Row>,
    dragging: bool,
}

impl PromptEditor {
    pub fn sync(&mut self, text: &str) {
        if self.text != text {
            self.text = text.into();
            self.caret = text.len();
            self.anchor = None;
            self.preferred = None;
        }
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        self.anchor
            .filter(|&anchor| anchor != self.caret)
            .map(|anchor| anchor.min(self.caret)..anchor.max(self.caret))
    }

    pub fn selected(&self) -> Option<String> {
        self.selection().map(|range| self.text[range].to_owned())
    }

    pub fn insert(&mut self, text: &mut String, value: &str) {
        self.sync(text);
        let range = self.selection().unwrap_or(self.caret..self.caret);
        let end = range.start + value.len();
        text.replace_range(range, value);
        self.text.clone_from(text);
        // Insertions can join a following combining mark or ZWJ sequence into a new grapheme.
        self.caret = text
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .chain([text.len()])
            .find(|&offset| offset >= end)
            .unwrap_or(text.len());
        self.anchor = None;
        self.preferred = None;
    }

    fn move_to(&mut self, offset: usize, selecting: bool) {
        if selecting {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = offset;
    }

    pub fn position(&self) -> (usize, usize) {
        let row = self
            .rows
            .iter()
            .rposition(|row| row.start <= self.caret)
            .unwrap_or(0);
        let column = self.rows.get(row).map_or(0, |row| {
            row.cells
                .iter()
                .take_while(|cell| cell.offset < self.caret)
                .map(|cell| cell.width)
                .sum()
        });
        (row, column)
    }

    pub fn prepare(&mut self, text: &str, area: Rect) {
        self.sync(text);
        let reflow =
            self.layout_text != text || self.area.width != area.width || self.rows.is_empty();
        self.area = area;
        if reflow {
            self.layout_text = text.into();
            // Text::from(String) drops a trailing newline; an editor must keep that final empty line.
            self.rows = layout(
                &Text::from(
                    text.split('\n')
                        .map(|line| Line::from(line.to_owned()))
                        .collect::<Vec<_>>(),
                ),
                area.width,
            )
            .1;
            // Full logical lines need a cursor cell before their newline (or end of buffer).
            for index in (0..self.rows.len()).rev() {
                let row = &self.rows[index];
                if row.cells.iter().map(|cell| cell.width).sum::<usize>()
                    >= usize::from(area.width.max(1))
                    && (index + 1 == self.rows.len() || self.rows[index + 1].start > row.end)
                {
                    let end = row.end;
                    self.rows.insert(
                        index + 1,
                        Row {
                            start: end,
                            end,
                            cells: Vec::new(),
                        },
                    );
                }
            }
        }
        let (row, _) = self.position();
        self.top = self.top.min(row);
        if row >= self.top + usize::from(area.height) {
            self.top = row.saturating_sub(usize::from(area.height).saturating_sub(1));
        }
    }

    pub fn mouse(&mut self, event: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        let inside = self.area.contains((event.column, event.row).into());
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if inside => {
                self.anchor = None;
                self.dragging = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                self.anchor.get_or_insert(self.caret);
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                return true;
            }
            _ => return false,
        }
        let row = self.top + usize::from(event.row.saturating_sub(self.area.y));
        self.caret = self.rows.get(row).map_or(self.text.len(), |row| {
            row.hit(usize::from(event.column.saturating_sub(self.area.x)))
        });
        true
    }

    pub fn cancel_drag(&mut self) {
        self.dragging = false;
    }

    pub fn key(&mut self, text: &mut String, key: KeyEvent) -> bool {
        self.sync(text);
        let selecting = key.modifiers.contains(KeyModifiers::SHIFT);
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('a') {
            self.anchor = Some(0);
            self.caret = text.len();
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            if key.code == KeyCode::Enter {
                self.insert(text, "\n");
                return true;
            }
            return false;
        }
        let boundaries = text
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .chain([text.len()])
            .collect::<Vec<_>>();
        match key.code {
            KeyCode::Left | KeyCode::Right => {
                let offset = if !selecting && self.selection().is_some() {
                    let range = self.selection().unwrap();
                    if key.code == KeyCode::Left {
                        range.start
                    } else {
                        range.end
                    }
                } else if key.code == KeyCode::Left {
                    boundaries
                        .iter()
                        .copied()
                        .rfind(|&offset| offset < self.caret)
                        .unwrap_or(0)
                } else {
                    boundaries
                        .iter()
                        .copied()
                        .find(|&offset| offset > self.caret)
                        .unwrap_or(text.len())
                };
                self.move_to(offset, selecting);
                self.preferred = None;
            }
            KeyCode::Up | KeyCode::Down => {
                self.prepare(text, self.area);
                let (row, column) = self.position();
                let column = *self.preferred.get_or_insert(column);
                let target = if key.code == KeyCode::Up {
                    row.saturating_sub(1)
                } else {
                    (row + 1).min(self.rows.len().saturating_sub(1))
                };
                let mut offset = self
                    .rows
                    .get(target)
                    .map_or(self.caret, |row| row.hit(column));
                // A soft-wrap boundary belongs to the next row; do not let Up land back there.
                if self
                    .rows
                    .get(target + 1)
                    .is_some_and(|next| next.start == offset)
                    && let Some(cell) = self.rows[target].cells.last()
                {
                    offset = cell.offset;
                }
                self.move_to(offset, selecting);
            }
            KeyCode::Backspace | KeyCode::Delete => {
                if self.selection().is_none() {
                    self.anchor = Some(if key.code == KeyCode::Backspace {
                        boundaries
                            .iter()
                            .copied()
                            .rfind(|&offset| offset < self.caret)
                            .unwrap_or(self.caret)
                    } else {
                        boundaries
                            .iter()
                            .copied()
                            .find(|&offset| offset > self.caret)
                            .unwrap_or(self.caret)
                    });
                }
                self.insert(text, "");
            }
            KeyCode::Char(ch) => self.insert(text, &ch.to_string()),
            _ => return false,
        }
        true
    }
}

pub fn normalize(text: &str) -> String {
    // Pasted escape/control bytes are data, never terminal commands or synthetic key events.
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

pub const MAX_IMPORT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("Prompt import exceeds 8 MiB; shorten the pasted text or edited file and retry")]
pub struct ImportTooLarge;

pub fn import(text: &str) -> Result<String, ImportTooLarge> {
    if text.len() > MAX_IMPORT_BYTES {
        return Err(ImportTooLarge);
    }
    Ok(normalize(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_a_grapheme_preserves_neighboring_unicode_text() {
        let mut text = "界e\u{301}👩‍💻".to_owned();
        let mut editor = PromptEditor::default();
        editor.key(
            &mut text,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(text, "界e\u{301}");
        editor.key(
            &mut text,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(text, "界");
    }

    #[test]
    fn replacing_a_selection_keeps_the_caret_on_a_grapheme_boundary() {
        let mut text = "hello".to_owned();
        let mut editor = PromptEditor::default();
        editor.key(
            &mut text,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        editor.insert(&mut text, "e\u{301}");
        assert_eq!(text, "e\u{301}");
        assert_eq!(editor.caret, text.len());
    }

    #[test]
    fn a_full_last_row_and_trailing_newline_have_a_visible_cursor_cell() {
        let mut editor = PromptEditor::default();
        editor.prepare("界ab", Rect::new(3, 2, 4, 1));
        assert_eq!(editor.position(), (1, 0));
        assert_eq!(editor.top, 1);
        editor.prepare("abc\n", Rect::new(3, 2, 4, 1));
        assert_eq!(editor.position(), (1, 0));
    }

    #[test]
    fn a_full_logical_line_has_a_cursor_cell_before_its_newline() {
        let mut text = "abcd\nx".to_owned();
        let mut editor = PromptEditor::default();
        editor.prepare(&text, Rect::new(0, 0, 4, 4));
        editor.key(&mut text, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        editor.key(&mut text, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(editor.caret, 4);
        assert_eq!(editor.position(), (1, 0));
    }

    #[test]
    fn styled_graphemes_and_tiny_cells_have_the_same_display_and_hit_test_width() {
        let text = Text::from(Line::from(vec![
            Span::raw("e"),
            Span::styled("\u{301}界\t", Style::default()),
        ]));
        let (plain, rows) = layout(&text, 1);
        assert_eq!(plain, "e\u{301}界\t");
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(
                row.line(None).width(),
                row.cells.iter().map(|cell| cell.width).sum::<usize>()
            );
        }
        assert_eq!(rows[0].hit(0), 0);
        assert_eq!(rows[1].hit(0), "e\u{301}".len());
    }

    #[test]
    fn moving_up_does_not_fall_back_onto_the_next_soft_wrapped_row() {
        let mut text = "abc defgh".to_owned();
        let mut editor = PromptEditor::default();
        editor.prepare(&text, Rect::new(0, 0, 6, 4));
        editor.key(&mut text, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(editor.position().0, 0);
    }

    #[test]
    fn oversized_imports_are_rejected_before_modifying_the_draft() {
        assert!(import(&"x".repeat(MAX_IMPORT_BYTES + 1)).is_err());
    }
}
