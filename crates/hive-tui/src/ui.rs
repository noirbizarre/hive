use std::collections::HashMap;

use agent_client_protocol::schema::v2;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use serde::Deserialize;

use crate::{picker::Picker, state::Conversation, theme::Theme, transcript::Transcript};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
    pub agent: String,
    pub title: Option<String>,
    pub lifecycle: String,
    pub recovery: Option<String>,
    pub recovery_reason: Option<String>,
    pub activity: String,
    pub last_activity_at: Option<u64>,
    pub control_owner: Option<String>,
    pub generation: u64,
}

#[derive(Deserialize)]
pub struct SessionSnapshot {
    pub sessions: Vec<SessionSummary>,
}

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
    pub sessions: Vec<SessionSummary>,
    pub profiles: Vec<String>,
    // Catalog refreshes reconcile only the session cursor, never another picker's selection.
    pub selected: usize,
    pub profile_selected: usize,
    // List widgets update offsets while drawing; retaining them prevents viewport jumps on redraw.
    pub session_list: ListState,
    pub profile_list: ListState,
    pub picker: Option<Picker>,
    pub all: bool,
    pub active: Option<String>,
    pub recent: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub conversations: HashMap<String, Conversation>,
    pub transcripts: HashMap<String, Transcript>,
    pub owner: Option<v2::Meta>,
    pub input: String,
    pub status: String,
    pub connected: bool,
    pub pending: HashMap<String, (String, String)>,
    pub drafts: HashMap<String, String>,
    pub dialog: Option<String>,
    pub dialog_scroll: u16,
    pub skip_replay: bool,
    pub configuration: Option<
        tokio::sync::oneshot::Receiver<(
            String,
            Result<v2::SetSessionConfigOptionResponse, String>,
        )>,
    >,
}

impl App {
    pub fn config_choices(&self) -> Vec<(String, String, String)> {
        let mut choices = Vec::new();
        for option in self
            .conversation()
            .into_iter()
            .flat_map(|state| &state.config_options)
        {
            let value = serde_json::to_value(option).unwrap_or_default();
            if value["type"] != "select" {
                continue;
            }
            if let Some(options) = value["options"].as_array() {
                for choice in options {
                    let grouped = choice["options"].as_array();
                    for item in grouped.map_or_else(|| vec![choice], |items| items.iter().collect())
                    {
                        let Some(id) = item["value"].as_str() else {
                            continue;
                        };
                        let group = choice["name"]
                            .as_str()
                            .or(choice["group"].as_str())
                            .unwrap_or_default();
                        let label = item["name"].as_str().unwrap_or(id);
                        let current = if value["currentValue"] == id {
                            " *"
                        } else {
                            ""
                        };
                        choices.push((
                            option.config_id.to_string(),
                            id.into(),
                            format!(
                                "{} · {}{}{}{}",
                                option.name,
                                if grouped.is_some() { group } else { "" },
                                if grouped.is_some() { " / " } else { "" },
                                label,
                                current
                            ),
                        ));
                    }
                }
            }
        }
        choices
    }

    pub fn toggle_sessions(&mut self) -> bool {
        if self.page == Page::Sessions && self.active.is_some() {
            self.page = Page::Chat;
            return false;
        }
        self.page = Page::Sessions;
        if let Some(id) = &self.active
            && let Some(index) = self.sessions.iter().position(|s| &s.session_id == id)
        {
            self.selected = index;
        }
        true
    }

    pub fn previous_session(&self) -> Option<SessionSummary> {
        self.recent
            .iter()
            .skip(1)
            .find_map(|id| {
                self.sessions
                    .iter()
                    .find(|session| &session.session_id == id)
            })
            .cloned()
    }

    pub fn select_session(&mut self, id: String) {
        self.picker = None;
        if let Some(previous) = self.active.take() {
            self.drafts
                .insert(previous, std::mem::take(&mut self.input));
        }
        self.input = self.drafts.remove(&id).unwrap_or_default();
        self.recent.retain(|recent| recent != &id);
        self.recent.insert(0, id.clone());
        self.active = Some(id);
        self.page = Page::Chat;
    }

    pub fn update_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let selected_id = self
            .sessions
            .get(self.selected)
            .map(|s| s.session_id.clone());
        self.sessions = sessions;
        self.recent.retain(|id| {
            self.active.as_ref() == Some(id) || self.sessions.iter().any(|s| &s.session_id == id)
        });
        if self.page != Page::Profiles {
            // The provider picker uses this index for a different list.
            self.selected = selected_id
                .and_then(|id| self.sessions.iter().position(|s| s.session_id == id))
                .unwrap_or_else(|| self.selected.min(self.sessions.len().saturating_sub(1)));
        }
    }

    pub fn conversation(&self) -> Option<&Conversation> {
        self.active
            .as_ref()
            .and_then(|id| self.conversations.get(id))
    }

    pub fn transcript(&mut self) -> Option<&mut Transcript> {
        self.active
            .clone()
            .map(|id| self.transcripts.entry(id).or_default())
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

    pub fn show(&mut self, frame: &mut Frame) {
        let theme = Theme;
        let area = frame.area();
        frame.render_widget(Block::default().style(theme.text()), area);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(if area.width >= 90 && self.page == Page::Chat {
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
                    (main.height / 2).max(8).min(main.height.saturating_sub(4))
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
        self.render_picker(frame, theme);
    }

    fn render_picker(&mut self, frame: &mut Frame, theme: Theme) {
        let choices = self.config_choices();
        // An advertised selector still supports selection when its catalog is empty.
        let supports_selection = self
            .conversation()
            .into_iter()
            .flat_map(|state| &state.config_options)
            .any(|option| serde_json::to_value(option).unwrap_or_default()["type"] == "select");
        let empty_message = if !supports_selection {
            "This backend does not expose model selection."
        } else if choices.is_empty() {
            "No models available."
        } else {
            "No matching models."
        };
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        picker.refresh(choices);
        let screen = frame.area();
        let width = screen
            .width
            .saturating_sub(4)
            .max(screen.width.min(4))
            .min(90);
        let height = screen
            .height
            .saturating_sub(4)
            .max(screen.height.min(4))
            .min(24);
        let area = Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Provider / Model ")
            .style(theme.text());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let sections = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(inner);
        // Tail-scroll the query so typing remains visible on narrow terminals.
        let query = format!("Search: {}", picker.query);
        let scroll = query
            .chars()
            .count()
            .saturating_sub(sections[0].width.saturating_sub(1) as usize)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(
            Paragraph::new(query)
                .scroll((0, scroll))
                .style(theme.accent()),
            sections[0],
        );
        if picker.choices.is_empty() {
            frame.render_widget(Paragraph::new(empty_message), sections[1]);
        } else {
            let list = List::new(picker.choices.iter().map(|c| ListItem::new(c.2.as_str())))
                .highlight_symbol("▸ ")
                .highlight_style(theme.selected());
            frame.render_stateful_widget(list, sections[1], &mut picker.list);
        }
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(&picker.message, theme.warning()),
                Line::styled(
                    "↑↓ select · Enter apply · Esc close · Ctrl-Q quit",
                    theme.muted(),
                ),
            ]),
            sections[2],
        );
    }

    fn render_heading(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        let title = match self.page {
            Page::Chat => self
                .active
                .as_ref()
                .and_then(|id| self.sessions.iter().find(|s| &s.session_id == id))
                .and_then(|s| s.title.as_deref())
                .or(self.active.as_deref())
                .unwrap_or("Select a session"),
            Page::Sessions => "Sessions",
            Page::Profiles => "New session · Select an agent",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("  ▪ hive  ", theme.accent()),
                    Span::styled(title, theme.text()),
                ]),
                Line::styled(
                    self.config_choices()
                        .iter()
                        .filter(|(_, _, label)| label.ends_with(" *"))
                        .map(|(_, _, label)| {
                            format!("  {} · Ctrl-P configure", label.trim_end_matches(" *"))
                        })
                        .collect::<Vec<_>>()
                        .join(" · "),
                    theme.muted(),
                ),
            ]),
            area,
        );
    }

    fn render_sessions(&mut self, frame: &mut Frame, area: Rect, theme: Theme) {
        if self.sessions.is_empty() {
            let (title, help) = if self.all {
                ("No sessions yet", "Ctrl-N  create a session")
            } else {
                (
                    "No sessions in this directory",
                    "a  show all directories · Ctrl-N  create a session",
                )
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(title, theme.text()),
                    Line::styled(help, theme.muted()),
                ])
                .wrap(Wrap { trim: false })
                .block(Block::default().padding(Padding::horizontal(3))),
                area,
            );
            return;
        }
        let entries = self.sessions.iter().map(|info| {
            let owner = if info.control_owner.is_none() {
                "unclaimed"
            } else if self.active.as_deref() == Some(&info.session_id)
                && self.pilot()
                && self
                    .conversation()
                    .is_some_and(|c| c.generation == info.generation)
            {
                "pilot here"
            } else {
                "another pilot"
            };
            let age = info
                .last_activity_at
                .map_or("no activity".to_owned(), |at| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |time| time.as_millis() as u64);
                    let minutes = now.saturating_sub(at) / 60_000;
                    if minutes < 1 {
                        "just now".into()
                    } else {
                        format!("{minutes}m ago")
                    }
                });
            let attention =
                if self.active.as_deref() == Some(&info.session_id) && self.dialog.is_some() {
                    " · needs attention"
                } else {
                    ""
                };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{}  ", info.title.as_deref().unwrap_or(&info.session_id)),
                    theme.text(),
                ),
                Span::styled(format!("{}  ", info.agent), theme.muted()),
                Span::styled(
                    format!(
                        "{} / {}{}{}  ",
                        info.lifecycle,
                        info.activity,
                        info.recovery_reason
                            .as_ref()
                            .map_or(String::new(), |reason| format!(" · {reason}")),
                        attention
                    ),
                    if info.activity == "running"
                        || !attention.is_empty()
                        || info.recovery.as_deref() == Some("recovery_required")
                    {
                        theme.warning()
                    } else {
                        theme.accent()
                    },
                ),
                Span::styled(
                    format!("{owner} · {age} · {}", info.cwd.display()),
                    theme.muted(),
                ),
            ]))
        });
        let list = List::new(entries)
            .block(Block::default().padding(Padding::horizontal(3)))
            .highlight_symbol("▸ ")
            .highlight_style(theme.selected());
        self.session_list
            .select(self.sessions.get(self.selected).map(|_| self.selected));
        frame.render_stateful_widget(list, area, &mut self.session_list);
    }

    fn render_profiles(&mut self, frame: &mut Frame, area: Rect, theme: Theme) {
        if self.profiles.is_empty() {
            // Profiles are loaded at startup, so restarting only the daemon leaves this picker empty.
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled("No agents configured", theme.text()),
                    Line::styled(
                        "Add an [agents.<id>] profile to the daemon configuration, then run hive daemon reload.",
                        theme.muted(),
                    ),
                ])
                .wrap(Wrap { trim: false })
                .block(Block::default().padding(Padding::horizontal(3))),
                area,
            );
            return;
        }
        let list = List::new(
            self.profiles
                .iter()
                .map(|profile| ListItem::new(Line::styled(profile.as_str(), theme.text()))),
        )
        .block(Block::default().padding(Padding::horizontal(3)))
        .highlight_symbol("▸ ")
        .highlight_style(theme.selected());
        self.profile_list.select(
            self.profiles
                .get(self.profile_selected)
                .map(|_| self.profile_selected),
        );
        frame.render_stateful_widget(list, area, &mut self.profile_list);
    }

    fn render_conversation(&mut self, frame: &mut Frame, area: Rect, theme: Theme) {
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
        let body = Block::default().padding(Padding::horizontal(3));
        if let Some(id) = self.active.as_ref()
            && let Some(conversation) = self.conversations.get(id)
        {
            self.transcripts.entry(id.clone()).or_default().render(
                conversation,
                frame,
                body.inner(area),
            );
        }
    }

    fn render_composer(&self, frame: &mut Frame, area: Rect, theme: Theme) {
        if let Some(dialog) = &self.dialog {
            let block = Block::default()
                .title(" Agent request · PgUp/PgDn details")
                .borders(Borders::LEFT)
                .border_style(theme.accent())
                .padding(Padding::horizontal(2));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            let sections =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
            frame.render_widget(
                Paragraph::new(dialog.as_str())
                    .style(theme.text())
                    .wrap(Wrap { trim: false })
                    .scroll((self.dialog_scroll, 0)),
                sections[0],
            );
            // Keep the editable value visible even when the subject/schema needs scrolling.
            frame.render_widget(
                Paragraph::new(format!("> {}", self.input)).style(theme.text()),
                sections[1],
            );
            return;
        }
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
        } else if self.page == Page::Sessions && self.sessions.is_empty() {
            if self.all {
                "Ctrl-N to create a session".to_owned()
            } else {
                "a to show all directories · Ctrl-N to create a session".to_owned()
            }
        } else if self.page == Page::Profiles && self.profiles.is_empty() {
            "Configure an agent, then restart the daemon and TUI".to_owned()
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
        let history = self.page == Page::Chat
            && self.dialog.is_none()
            && self
                .active
                .as_ref()
                .and_then(|id| self.transcripts.get(id))
                .is_some_and(|view| view.top.is_some());
        let hint = match self.page {
            Page::Sessions if self.sessions.is_empty() => {
                if self.active.is_some() {
                    "Tab chat  a all  Ctrl-N new  Esc quit"
                } else {
                    "a all  Ctrl-N new  Esc quit"
                }
            }
            Page::Sessions => {
                "↑↓ select  Enter pilot  o observe  Tab chat  a all  Ctrl-N new  Esc quit"
            }
            Page::Profiles if self.profiles.is_empty() => "Tab sessions  Esc quit",
            Page::Profiles => "↑↓ select  Enter create  Tab sessions  Esc quit",
            Page::Chat if self.dialog.is_some() => "Enter confirm  Esc decline",
            Page::Chat if history => {
                "History · PgUp/PgDn or wheel scroll · End live · Ctrl-↑↓ tool · Ctrl-O details"
            }
            Page::Chat => {
                "Enter send  Tab sessions  PgUp/PgDn scroll  End live  Ctrl-↑↓ tool  Ctrl-O details  Ctrl-P model  Ctrl-N new  Ctrl-C cancel  Esc quit"
            }
        };
        // Browsing older messages must not hide a new connection or submission error.
        let history_status = format!("History · End live · {}", self.status);
        let mut lines = vec![Line::from(vec![
            Span::styled("  ▪ ", theme.accent()),
            Span::styled(
                if history && !self.status.is_empty() {
                    &history_status
                } else if self.status.is_empty() || history {
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
        let active = self
            .active
            .as_ref()
            .and_then(|id| self.sessions.iter().find(|s| s.session_id == *id));
        let agent = active.map_or("—", |s| s.agent.as_str());
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
                |s| s.cwd.display().to_string(),
            ),
            theme.text(),
        ));
        if let Some(usage) = self.conversation().map(|c| &c.usage) {
            let tokens = usage["tokens"]["totalTokens"]
                .as_u64()
                .map_or("unknown".into(), |count| count.to_string());
            let actual = &usage["actualSessionCost"];
            let estimated = &usage["estimatedCost"];
            let price = if let (Some(amount), Some(currency)) =
                (actual["amount"].as_f64(), actual["currency"].as_str())
            {
                format!("actual {amount:.4} {currency}")
            } else if let (Some(amount), Some(currency)) =
                (estimated["amount"].as_f64(), estimated["currency"].as_str())
            {
                format!("estimated {amount:.4} {currency}")
            } else {
                "unknown".into()
            };
            lines.push(Line::default());
            lines.push(Line::styled("Usage", theme.muted()));
            lines.push(Line::styled(format!("Tokens  {tokens}"), theme.text()));
            lines.push(Line::styled(format!("Cost  {price}"), theme.text()));
        }
        if let Some(activity) = self.conversation().map(|c| &c.activity) {
            lines.push(Line::default());
            lines.push(Line::styled("Activity", theme.muted()));
            for (label, field) in [("Tools", "tools"), ("Tasks", "tasks"), ("Todos", "plans")] {
                let status = activity["support"][field].as_str().unwrap_or("unknown");
                lines.push(Line::styled(format!("{label} · {status}"), theme.text()));
                if let Some(items) = activity[field].as_object() {
                    for (id, item) in items {
                        if field == "plans" {
                            for entry in item["entries"].as_array().into_iter().flatten() {
                                lines.push(Line::styled(
                                    format!(
                                        "  {} · {}",
                                        entry["status"].as_str().unwrap_or("?"),
                                        entry["content"].as_str().unwrap_or("?")
                                    ),
                                    theme.text(),
                                ));
                            }
                        } else {
                            lines.push(Line::styled(
                                format!(
                                    "  {} · {}",
                                    item["status"].as_str().unwrap_or("unknown"),
                                    item["title"].as_str().unwrap_or(id)
                                ),
                                theme.text(),
                            ));
                        }
                    }
                }
            }
        }
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
    use agent_client_protocol::schema::v2;
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
    fn an_empty_session_overview_explains_how_to_create_a_session() {
        let mut app = App {
            all: true,
            ..App::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("No sessions yet"));
        assert!(rendered.contains("Ctrl-N  create a session"));
        assert!(!rendered.contains("Select an item"));
        assert!(!rendered.contains("Enter pilot"));
        assert!(!rendered.contains("o observe"));
    }

    #[test]
    fn an_empty_directory_overview_explains_how_to_broaden_the_scope() {
        let mut app = App::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("No sessions in this directory"));
        assert!(rendered.contains("a  show all directories"));
        app.all = true;
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("No sessions yet"));
        assert!(!rendered.contains("No sessions in this directory"));
    }

    #[test]
    fn an_empty_agent_picker_explains_configuration_and_restart_requirements() {
        let mut app = App {
            page: Page::Profiles,
            ..App::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(120, 14)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("No agents configured"));
        assert!(rendered.contains("[agents.<id>]"));
        assert!(rendered.contains("restart the daemon and TUI"));
        assert!(!rendered.contains("Select an item"));
        assert!(!rendered.contains("Enter create"));

        app.status = "Connection lost".into();
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("Connection lost"));
        assert!(screen(&terminal).contains("No agents configured"));
    }

    #[test]
    fn empty_state_guidance_tracks_catalog_changes() {
        let mut app = App {
            page: Page::Profiles,
            all: true,
            ..App::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("No agents configured"));
        app.profiles.push("pi".into());
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("▸ pi"));
        assert!(rendered.contains("Select an item"));
        assert!(rendered.contains("Enter create"));
        assert!(!rendered.contains("No agents configured"));
        app.profiles.clear();
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("No agents configured"));

        app.page = Page::Sessions;
        app.update_sessions(vec![SessionSummary {
            session_id: "hive-one".into(),
            cwd: "/tmp".into(),
            agent: "pi".into(),
            title: None,
            lifecycle: "active".into(),
            recovery: None,
            recovery_reason: None,
            activity: "idle".into(),
            last_activity_at: None,
            control_owner: None,
            generation: 1,
        }]);
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("hive-one"));
        assert!(rendered.contains("Enter pilot"));
        assert!(!rendered.contains("No sessions yet"));
        app.update_sessions(vec![]);
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("No sessions yet"));
    }

    #[test]
    fn narrow_terminals_wrap_empty_state_guidance_and_tiny_terminals_do_not_panic() {
        for (page, all, title, help) in [
            (Page::Sessions, true, "No sessions yet", "create a session"),
            (
                Page::Sessions,
                false,
                "No sessions in this directory",
                "create a session",
            ),
            (
                Page::Profiles,
                false,
                "No agents configured",
                "daemon reload.",
            ),
        ] {
            let mut app = App {
                page,
                all,
                ..App::default()
            };
            let mut narrow = Terminal::new(TestBackend::new(44, 18)).unwrap();
            narrow.draw(|frame| app.show(frame)).unwrap();
            let rendered = screen(&narrow);
            assert!(rendered.contains(title));
            assert!(rendered.contains(help));
            let mut tiny = Terminal::new(TestBackend::new(18, 5)).unwrap();
            tiny.draw(|frame| app.show(frame)).unwrap();
        }
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
        app.sessions.push(SessionSummary {
            session_id: "hive-one".into(),
            cwd: std::env::current_dir().unwrap(),
            agent: "pi".into(),
            title: None,
            lifecycle: "active".into(),
            recovery: None,
            recovery_reason: None,
            activity: "running".into(),
            last_activity_at: None,
            control_owner: None,
            generation: 1,
        });
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
    fn catalog_updates_keep_the_highlighted_identity_and_remove_expired_recents() {
        let summary = |id: &str| SessionSummary {
            session_id: id.into(),
            cwd: std::env::current_dir().unwrap(),
            agent: "pi".into(),
            title: Some(format!("Task {id}")),
            lifecycle: "active".into(),
            recovery: None,
            recovery_reason: None,
            activity: "unknown".into(),
            last_activity_at: None,
            control_owner: None,
            generation: 1,
        };
        let mut app = App::default();
        app.update_sessions(vec![summary("one"), summary("two")]);
        app.selected = 1;
        app.select_session("two".into());
        app.input = "draft for two".into();
        app.select_session("one".into());
        app.update_sessions(vec![summary("two"), summary("one")]);
        assert_eq!(app.sessions[app.selected].session_id, "two");
        assert_eq!(app.recent, ["one", "two"]);
        assert_eq!(app.previous_session().unwrap().session_id, "two");
        assert!(app.toggle_sessions());
        assert_eq!(app.sessions[app.selected].session_id, "one");
        assert!(!app.toggle_sessions());
        assert!(app.page == Page::Chat);
        app.select_session("two".into());
        assert_eq!(app.input, "draft for two");
        app.update_sessions(vec![summary("two")]);
        assert_eq!(app.recent, ["two"]);
        assert_eq!(app.active.as_deref(), Some("two"));
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        app.page = Page::Sessions;
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("Task two"));
        assert!(screen(&terminal).contains("unknown"));
    }

    #[test]
    fn catalog_refresh_and_redraws_preserve_picker_identity_and_viewport() {
        let mut app = App::default();
        app.select_session("session".into());
        app.input = "draft underneath".into();
        app.conversations.entry("session".into()).or_default().config_options =
            serde_json::from_value(serde_json::json!([{
                "configId": "model", "name": "Model", "type": "select", "currentValue": "0",
                "options": (0..60).map(|i| serde_json::json!({"value": i.to_string(), "name": format!("Model {i}")})).collect::<Vec<_>>()
            }])).unwrap();
        app.picker = Some(Picker::new("session".into(), app.config_choices()));
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        app.picker.as_mut().unwrap().navigate(35);
        terminal.draw(|frame| app.show(frame)).unwrap();
        let offset = app.picker.as_ref().unwrap().list.offset();
        assert!(offset > 0);
        app.profile_selected = 12;
        app.update_sessions(vec![]);
        assert_eq!(app.profile_selected, 12);
        app.picker.as_mut().unwrap().navigate(-1);
        for _ in 0..3 {
            terminal.draw(|frame| app.show(frame)).unwrap();
            assert_eq!(app.picker.as_ref().unwrap().list.offset(), offset);
            assert_eq!(app.picker.as_ref().unwrap().selected().unwrap().1, "34");
        }
        assert!(screen(&terminal).contains("Provider / Model"));
        assert!(screen(&terminal).contains("draft underneath"));
    }

    #[test]
    fn an_advertised_empty_model_selector_reports_no_models_available() {
        for options in [
            serde_json::json!([]),
            serde_json::json!([{ "groupId": "provider", "name": "Provider", "options": [] }]),
        ] {
            let mut app = App::default();
            app.select_session("session".into());
            app.conversations
                .entry("session".into())
                .or_default()
                .config_options = serde_json::from_value(serde_json::json!([{
                "configId": "model", "name": "Model", "type": "select",
                "currentValue": "", "options": options
            }]))
            .unwrap();
            app.picker = Some(Picker::new("session".into(), app.config_choices()));
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
            terminal.draw(|frame| app.show(frame)).unwrap();
            let rendered = screen(&terminal);
            assert!(rendered.contains("No models available."));
            assert!(!rendered.contains("does not expose model selection"));
            assert!(!rendered.contains("No matching models."));
        }
    }

    #[test]
    fn a_search_without_matching_models_reports_no_matches() {
        let mut app = App::default();
        app.select_session("session".into());
        app.conversations
            .entry("session".into())
            .or_default()
            .config_options = serde_json::from_value(serde_json::json!([{
            "configId": "model", "name": "Model", "type": "select",
            "currentValue": "fast", "options": [{ "value": "fast", "name": "Fast" }]
        }]))
        .unwrap();
        let mut picker = Picker::new("session".into(), app.config_choices());
        picker.query = "zzzzzz".into();
        app.picker = Some(picker);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("No matching models."));
        assert!(!rendered.contains("No models available."));
        assert!(!rendered.contains("does not expose model selection"));
    }

    #[test]
    fn the_configuration_modal_handles_empty_results_and_tiny_terminals() {
        let mut app = App::default();
        app.select_session("session".into());
        app.picker = Some(Picker::new("session".into(), vec![]));
        for (width, height) in [(80, 20), (18, 5), (3, 2), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.show(frame)).unwrap();
            if width == 80 {
                assert!(screen(&terminal).contains("does not expose model selection"));
            }
        }
    }

    #[test]
    fn hiding_the_overview_keeps_the_active_conversation() {
        let mut app = App {
            page: Page::Chat,
            active: Some("one".into()),
            ..App::default()
        };
        app.conversations
            .entry("one".into())
            .or_default()
            .push_notice("Still here".into());
        let mut terminal = Terminal::new(TestBackend::new(50, 12)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("Still here"));
        assert!(screen(&terminal).contains("Tab sessions"));
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
        assert!(rendered.contains("▏ You"));
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("Session"));
        assert!(rendered.contains("Connected"));
        assert!(rendered.contains("next step"));
        let buffer = terminal.backend().buffer();
        // No fixed background should override terminal transparency or a light theme.
        assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
        assert!(buffer.content().iter().any(|cell| cell.fg == Color::Cyan));
    }

    #[test]
    fn the_sidebar_shows_tool_task_and_todo_status_from_the_shared_snapshot() {
        let mut app = App {
            page: Page::Chat,
            active: Some("hive-one".into()),
            cwd: "/tmp".into(),
            ..App::default()
        };
        app.conversations
            .entry("hive-one".into())
            .or_default()
            .snapshot(&serde_json::json!({
                "version": 1, "seq": 4, "foreground": "idle", "session": "active",
                "support": {"tools": "observed", "plans": "observed", "tasks": "observed"},
                "tools": {"tool": {"title": "Compile", "status": "failed"}},
                "tasks": {"job": {"title": "Index", "status": "completed"}},
                "plans": {"plan": {"entries": [{"content": "Review", "status": "in_progress"}]}},
            }));
        app.conversations
            .get_mut("hive-one")
            .unwrap()
            .usage_snapshot(
                &serde_json::json!({"turns": 1, "tokens": {"totalTokens": 120},
                "estimatedCost": {"amount": 0.0005, "currency": "USD"}}),
                5,
            );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(rendered.contains("failed · Compile"));
        assert!(rendered.contains("completed · Index"));
        assert!(rendered.contains("in_progress · Review"));
        assert!(rendered.contains("Tokens  120"));
        assert!(rendered.contains("estimated 0.0005 USD"));
    }

    #[test]
    fn selected_profiles_are_visible_on_the_terminal_background() {
        let mut app = App {
            page: Page::Profiles,
            profiles: vec!["pi".into(), "opencode".into()],
            profile_selected: 1,
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
        let mut app = App {
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
