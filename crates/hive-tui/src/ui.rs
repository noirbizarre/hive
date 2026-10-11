use std::collections::HashMap;

use agent_client_protocol::schema::v2;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

use crate::{
    state::{Conversation, LineKind},
    theme::Theme,
};

#[derive(Default, PartialEq)]
pub enum Page {
    #[default]
    Sessions,
    Profiles,
    Chat,
}

#[derive(Default)]
pub struct App {
    pub page: Page,
    pub sessions: Vec<v2::SessionInfo>,
    pub profiles: Vec<String>,
    pub selected: usize,
    pub all: bool,
    pub active: Option<String>,
    pub cwd: std::path::PathBuf,
    pub conversations: HashMap<String, Conversation>,
    pub owner: Option<v2::Meta>,
    pub input: String,
    pub status: String,
    pub connected: bool,
    pub pending: Option<(String, String)>,
    pub dialog: Option<String>,
    pub skip_replay: bool,
}

impl App {
    pub fn conversation(&self) -> Option<&Conversation> {
        self.active
            .as_ref()
            .and_then(|id| self.conversations.get(id))
    }

    pub fn pilot(&self) -> bool {
        let generation = self.conversation().map_or(0, |c| c.generation);
        self.owner
            .as_ref()
            .and_then(|m| m.get("hive"))
            .is_some_and(|h| {
                h["role"] == "pilot" && h["generation"] == generation && h["token"].is_string()
            })
    }

    pub fn show(&self, frame: &mut Frame) {
        let theme = Theme;
        let area = frame.area();
        frame.render_widget(Block::default().style(theme.text()), area);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(if area.width >= 90 {
                [Constraint::Min(40), Constraint::Length(31)]
            } else {
                [Constraint::Min(0), Constraint::Length(0)]
            })
            .split(area);
        let main = columns[0];
        let compact = main.height < 9;
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(if compact { 1 } else { 2 }),
                Constraint::Min(0),
                Constraint::Length(if compact {
                    2
                } else if self.dialog.is_some() {
                    8
                } else {
                    4
                }),
                Constraint::Length(if compact { 1 } else { 2 }),
            ])
            .split(main);
        self.render_heading(frame, sections[0], theme);
        match self.page {
            Page::Sessions => self.render_sessions(frame, sections[1], theme),
            Page::Profiles => self.render_profiles(frame, sections[1], theme),
            Page::Chat => self.render_conversation(frame, sections[1], theme),
        }
        self.render_composer(frame, sections[2], theme);
        self.render_footer(frame, sections[3], theme, columns[1].width == 0);
        if columns[1].width > 0 {
            self.render_sidebar(frame, columns[1], theme);
        }
    }

    fn render_heading(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        let title = match self.page {
            Page::Chat => self.active.as_deref().unwrap_or("Select a session"),
            Page::Sessions => "Sessions",
            Page::Profiles => "New session · Select an agent",
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  ▪ hive  ", theme.accent()),
                Span::styled(title, theme.text()),
            ])),
            area,
        );
    }

    fn render_sessions(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        let entries = self.sessions.iter().map(|info| {
            let agent = info
                .meta
                .as_ref()
                .and_then(|m| m.get("hive"))
                .and_then(|h| h.get("agent"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let running = info
                .meta
                .as_ref()
                .is_some_and(|m| m["hive"]["running"] == true);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{}  ", info.session_id), theme.text()),
                Span::styled(format!("{agent}  "), theme.muted()),
                Span::styled(if running { "running  " } else { "idle  " }, theme.accent()),
                Span::styled(info.cwd.0.display().to_string(), theme.muted()),
            ]))
        });
        let list = List::new(entries)
            .block(Block::default().padding(Padding::horizontal(3)))
            .highlight_symbol("▸ ")
            .highlight_style(theme.selected());
        let mut selected = ListState::default()
            .with_selected(self.sessions.get(self.selected).map(|_| self.selected));
        frame.render_stateful_widget(list, area, &mut selected);
    }

    fn render_profiles(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        let list = List::new(
            self.profiles
                .iter()
                .map(|profile| ListItem::new(Line::styled(profile.as_str(), theme.text()))),
        )
        .block(Block::default().padding(Padding::horizontal(3)))
        .highlight_symbol("▸ ")
        .highlight_style(theme.selected());
        let mut selected = ListState::default()
            .with_selected(self.profiles.get(self.selected).map(|_| self.selected));
        frame.render_stateful_widget(list, area, &mut selected);
    }

    fn render_conversation(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        if self.active.is_none() {
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::styled("Choose a session to continue", theme.text()),
                    Line::styled("Tab  sessions     Ctrl-N  new session", theme.muted()),
                ]))
                .block(Block::default().padding(Padding::horizontal(3))),
                area,
            );
            return;
        }
        let lines = self
            .conversation()
            .into_iter()
            .flat_map(|conversation| conversation.lines.iter())
            .enumerate()
            .flat_map(|(position, entry)| {
                let style = match entry.kind {
                    LineKind::User => theme.accent(),
                    LineKind::Tool | LineKind::Notice => theme.muted(),
                    LineKind::Agent => theme.text(),
                };
                // Spacing separates turns without the heavy borders of the old conversation pane.
                let gap = (position > 0).then(Line::default);
                // Split embedded newlines so wrapping and bottom anchoring count every row.
                gap.into_iter()
                    .chain(entry.text.lines().enumerate().map(move |(index, line)| {
                        if entry.kind == LineKind::User {
                            Line::styled(
                                format!("{} {line}", if index == 0 { "▏" } else { " " }),
                                style,
                            )
                        } else {
                            Line::styled(line.to_owned(), style)
                        }
                    }))
            })
            .collect::<Vec<_>>();
        let body = Block::default().padding(Padding::horizontal(3));
        let width = area.width.saturating_sub(6).max(1) as usize;
        // Account for wrapped rows so the latest response remains in view while it grows.
        let rows: usize = lines
            .iter()
            .map(|line| line.width().max(1).div_ceil(width))
            .sum();
        let paragraph = Paragraph::new(Text::from(lines))
            .style(theme.text())
            .wrap(Wrap { trim: false })
            .block(body);
        let scroll = rows
            .saturating_sub(area.height as usize)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(paragraph.scroll((scroll, 0)), area);
    }

    fn render_composer(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        let label = if self.dialog.is_some() {
            " Agent request"
        } else if self.page == Page::Chat {
            " Message"
        } else {
            " Tab sessions · Ctrl-N new"
        };
        let prompt = if let Some(dialog) = &self.dialog {
            format!("{dialog}\n> {}", self.input)
        } else if self.page == Page::Chat {
            if self.input.is_empty() {
                "Type a message…".to_owned()
            } else {
                self.input.clone()
            }
        } else {
            "Select an item with ↑/↓ and Enter".to_owned()
        };
        let style = if self.input.is_empty() && self.dialog.is_none() {
            theme.muted()
        } else {
            theme.text()
        };
        frame.render_widget(
            Paragraph::new(prompt)
                .style(style)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(Line::styled(label, theme.muted()))
                        .borders(Borders::LEFT)
                        .border_style(theme.accent())
                        .padding(Padding::horizontal(2)),
                ),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, theme: Theme, compact: bool) {
        let hint = match self.page {
            Page::Sessions => "↑↓ select  Enter pilot  o observe  a all  Ctrl-N new  Esc quit",
            Page::Profiles => "↑↓ select  Enter create  Tab sessions  Esc quit",
            Page::Chat if self.dialog.is_some() => "Enter confirm  Esc decline",
            Page::Chat => {
                "Enter send  Tab sessions  Ctrl-N new  Ctrl-T pilot  Ctrl-H handoff  Ctrl-R live  Ctrl-C cancel  Esc quit"
            }
        };
        let mut lines = vec![Line::from(vec![
            Span::styled("  ▪ ", theme.accent()),
            Span::styled(
                if self.status.is_empty() {
                    hint
                } else {
                    &self.status
                },
                if self.status.is_empty() {
                    theme.muted()
                } else {
                    theme.warning()
                },
            ),
        ])];
        if compact && area.height > 1 {
            lines.push(Line::styled(
                format!(
                    "  {} · {} · {}",
                    if self.connected {
                        "connected"
                    } else {
                        "disconnected"
                    },
                    if self.pilot() { "pilot" } else { "observer" },
                    if self.conversation().is_some_and(|c| c.running) {
                        "running"
                    } else {
                        "idle"
                    }
                ),
                theme.muted(),
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_sidebar(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        let role = if self.pilot() {
            "Pilot"
        } else if self.conversation().is_some_and(|c| c.pilot_present) {
            "Observer · another pilot"
        } else {
            "Observer · no pilot"
        };
        let active = self.active.as_ref().and_then(|id| {
            self.sessions
                .iter()
                .find(|s| s.session_id.to_string() == *id)
        });
        let agent = active
            .and_then(|s| s.meta.as_ref())
            .and_then(|m| m.get("hive"))
            .and_then(|h| h.get("agent"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("—");
        let mut lines = vec![
            Line::styled("Hive", theme.accent()),
            Line::default(),
            Line::styled("Session", theme.muted()),
            Line::styled(self.active.as_deref().unwrap_or("None"), theme.text()),
            Line::styled(format!("Agent  {agent}"), theme.text()),
            Line::default(),
            Line::styled("Control", theme.muted()),
            Line::styled(role, theme.text()),
            Line::styled(
                if self.conversation().is_some_and(|c| c.running) {
                    "Running"
                } else {
                    "Idle"
                },
                theme.text(),
            ),
            Line::styled(
                if self.connected {
                    "Connected"
                } else {
                    "Disconnected"
                },
                if self.connected {
                    theme.accent()
                } else {
                    theme.warning()
                },
            ),
            Line::default(),
            Line::styled("Directory", theme.muted()),
        ];
        lines.push(Line::styled(
            active.map_or_else(
                || self.cwd.display().to_string(),
                |s| s.cwd.0.display().to_string(),
            ),
            theme.text(),
        ));
        frame.render_widget(
            Paragraph::new(lines)
                .style(theme.text())
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(theme.muted())
                        .padding(Padding::horizontal(2)),
                ),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let area = terminal.backend().buffer().area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_status_bar_fences_stale_pilot_metadata() {
        let mut app = App {
            active: Some("hive-one".into()),
            connected: true,
            page: Page::Chat,
            ..App::default()
        };
        let mut meta = v2::Meta::new();
        meta.insert(
            "hive".into(),
            serde_json::json!({"generation": 1, "role": "pilot", "token": "opaque"}),
        );
        app.owner = Some(meta);
        app.conversations
            .entry("hive-one".into())
            .or_default()
            .generation = 1;
        assert!(app.pilot());
        app.conversations.get_mut("hive-one").unwrap().generation = 2;
        assert!(!app.pilot());
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("observer"));
    }

    #[test]
    fn the_session_picker_shows_profile_and_work_state() {
        let mut app = App::default();
        let mut meta = v2::Meta::new();
        meta.insert(
            "hive".into(),
            serde_json::json!({"agent": "pi", "running": true}),
        );
        app.sessions.push(
            v2::SessionInfo::new(
                v2::SessionId::new("hive-one"),
                std::env::current_dir().unwrap(),
            )
            .meta(meta),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 14)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("hive-one"));
        assert!(rendered.contains("pi"));
        assert!(rendered.contains("running"));
    }

    #[test]
    fn the_transcript_and_sidebar_use_the_terminal_palette() {
        let mut app = App {
            page: Page::Chat,
            active: Some("hive-one".into()),
            connected: true,
            input: "next step".into(),
            ..App::default()
        };
        let conversation = app.conversations.entry("hive-one".into()).or_default();
        conversation.push_user("hello".into());
        conversation.push_notice("Turn ended".into());
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("▏ You: hello"));
        assert!(rendered.contains("Session"));
        assert!(rendered.contains("Connected"));
        assert!(rendered.contains("next step"));
        let buffer = terminal.backend().buffer();
        // No fixed background should override terminal transparency or a light theme.
        assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
        assert!(buffer.content().iter().any(|cell| cell.fg == Color::Cyan));
    }

    #[test]
    fn selected_profiles_are_visible_on_the_terminal_background() {
        let app = App {
            page: Page::Profiles,
            profiles: vec!["pi".into(), "opencode".into()],
            selected: 1,
            ..App::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("▸ opencode"));
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn narrow_and_tiny_terminals_keep_dialogs_and_control_visible() {
        let app = App {
            page: Page::Chat,
            dialog: Some("Allow access?\n1: yes".into()),
            active: Some("hive-one".into()),
            ..App::default()
        };
        let mut narrow = Terminal::new(TestBackend::new(44, 16)).unwrap();
        narrow.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&narrow);
        assert!(rendered.contains("Allow access?"));
        assert!(rendered.contains("observer"));
        assert!(!rendered.contains("Directory"));
        let mut tiny = Terminal::new(TestBackend::new(18, 5)).unwrap();
        tiny.draw(|frame| app.show(frame)).unwrap();
    }

    #[test]
    fn long_conversations_keep_the_latest_message_in_view() {
        let mut app = App {
            page: Page::Chat,
            active: Some("hive-one".into()),
            ..App::default()
        };
        let conversation = app.conversations.entry("hive-one".into()).or_default();
        for number in 0..25 {
            conversation.push_notice(format!("Earlier {number}"));
        }
        conversation.push_notice("Latest answer".into());
        let mut terminal = Terminal::new(TestBackend::new(70, 12)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("Latest answer"));
        assert!(!screen(&terminal).contains("Earlier 0"));
    }
}
