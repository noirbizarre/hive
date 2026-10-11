use std::collections::HashMap;

use agent_client_protocol::schema::v2;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use crate::state::Conversation;

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
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(2),
                Constraint::Length(if self.dialog.is_some() { 8 } else { 3 }),
                Constraint::Length(3),
            ])
            .split(frame.area());
        let role = if self.pilot() {
            "PILOT"
        } else if self.conversation().is_some_and(|c| c.pilot_present) {
            "OBSERVER — another pilot"
        } else {
            "OBSERVER — no pilot"
        };
        let work = if self.conversation().is_some_and(|c| c.running) {
            "RUNNING"
        } else {
            "IDLE"
        };
        let link = if self.connected {
            "connected"
        } else {
            "disconnected"
        };
        frame.render_widget(
            Paragraph::new(format!(
                "Hive  {link}  {role}  {work}  {}",
                self.active.as_deref().unwrap_or("Select a session")
            ))
            .block(Block::default().borders(Borders::ALL)),
            sections[0],
        );
        match self.page {
            Page::Sessions => {
                let entries = self.sessions.iter().enumerate().map(|(i, info)| {
                    let agent = info
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("hive"))
                        .and_then(|h| h.get("agent"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    ListItem::new(format!(
                        "{} {}  {agent}  {}  {}",
                        if i == self.selected { ">" } else { " " },
                        info.session_id,
                        info.cwd.0.display(),
                        if info
                            .meta
                            .as_ref()
                            .is_some_and(|m| m["hive"]["running"] == true)
                        {
                            "running"
                        } else {
                            "idle"
                        }
                    ))
                });
                frame.render_widget(
                    List::new(entries).block(
                        Block::default()
                            .title("Sessions — n new / a all / Enter pilot / o observe")
                            .borders(Borders::ALL),
                    ),
                    sections[1],
                );
            }
            Page::Profiles => {
                let entries = self.profiles.iter().enumerate().map(|(i, profile)| {
                    ListItem::new(format!(
                        "{} {profile}",
                        if i == self.selected { ">" } else { " " }
                    ))
                });
                frame.render_widget(
                    List::new(entries).block(
                        Block::default()
                            .title("New session — select an agent profile")
                            .borders(Borders::ALL),
                    ),
                    sections[1],
                );
            }
            Page::Chat => {
                let lines = self
                    .conversation()
                    .map(|c| c.lines.join("\n"))
                    .unwrap_or_default();
                let height = sections[1].height.saturating_sub(2) as usize;
                let rows: Vec<_> = lines.lines().collect();
                let recent = rows
                    .iter()
                    .skip(rows.len().saturating_sub(height))
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n");
                frame.render_widget(
                    Paragraph::new(recent)
                        .wrap(Wrap { trim: false })
                        .block(Block::default().title("Conversation").borders(Borders::ALL)),
                    sections[1],
                );
            }
        }
        let prompt = if let Some(dialog) = &self.dialog {
            format!("{dialog}\n> {}", self.input)
        } else {
            self.input.clone()
        };
        frame.render_widget(
            Paragraph::new(prompt).wrap(Wrap { trim: false }).block(
                Block::default()
                    .title(if self.dialog.is_some() {
                        "Agent request"
                    } else {
                        "Input"
                    })
                    .borders(Borders::ALL),
            ),
            sections[2],
        );
        frame.render_widget(Paragraph::new(format!("{}  |  Tab sessions  Ctrl-N new  Ctrl-T pilot  Ctrl-H handoff  Ctrl-R live only  Ctrl-C cancel  Esc quit", self.status))
            .style(Style::default().fg(Color::Yellow)).block(Block::default().borders(Borders::ALL)), sections[3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

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
        assert!(rendered.contains("OBSERVER"));
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
}
