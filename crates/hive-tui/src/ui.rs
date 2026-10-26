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
    pub refresh_requested: bool,
    pub active: Option<String>,
    pub recent: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub conversations: HashMap<String, Conversation>,
    pub transcripts: HashMap<String, Transcript>,
    pub owner: Option<v2::Meta>,
    pub input: String,
    pub editor: crate::editor::PromptEditor,
    pub draft_editors: HashMap<String, crate::editor::PromptEditor>,
    pub transcript_focused: bool,
    // Questions have their own input lifetime; their answers must never become chat drafts.
    pub suspended_draft: Option<String>,
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
        self.choices_for(
            self.picker
                .as_ref()
                .map_or(crate::picker::Kind::Model, |p| p.kind),
        )
    }

    pub fn choices_for(&self, kind: crate::picker::Kind) -> Vec<(String, String, String)> {
        let mut choices = Vec::new();
        for option in self
            .conversation()
            .into_iter()
            .flat_map(|state| &state.config_options)
        {
            let value = serde_json::to_value(option).unwrap_or_default();
            if (value["category"] == "model") != (kind == crate::picker::Kind::Model) {
                continue;
            }
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

    fn config_summary(&self) -> String {
        self.conversation()
            .into_iter()
            .flat_map(|state| &state.config_options)
            .filter_map(|option| {
                let value = serde_json::to_value(option).ok()?;
                if value["type"] != "select" {
                    return None;
                }
                let current = value["currentValue"].as_str()?;
                let label = value["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|choice| {
                        choice["options"]
                            .as_array()
                            .map_or_else(|| vec![choice], |items| items.iter().collect())
                    })
                    .find(|choice| choice["value"] == current)
                    .and_then(|choice| choice["name"].as_str())
                    .unwrap_or(current);
                Some(format!("{}: {}", option.name, label))
            })
            .collect::<Vec<_>>()
            .join(" · ")
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
            self.editor.cancel_drag();
            self.editor.sync(&self.input);
            self.draft_editors
                .insert(previous.clone(), std::mem::take(&mut self.editor));
            self.drafts
                .insert(previous, std::mem::take(&mut self.input));
        }
        self.input = self.drafts.remove(&id).unwrap_or_default();
        self.editor = self.draft_editors.remove(&id).unwrap_or_default();
        self.transcript_focused = false;
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
        let history_label = self.history_label();
        // Keep coverage warnings separate from the transient connection/submission status.
        let history_height = history_label.as_ref().map_or(0, |label| {
            (label.chars().count() as u16 / main.width.max(1) + 1).min(if compact { 1 } else { 4 })
        });
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(if compact { 1 } else { 2 } + history_height),
                Constraint::Min(0),
                Constraint::Length(if compact {
                    2
                } else if self.dialog.is_some() {
                    (main.height / 2).max(8).min(main.height.saturating_sub(4))
                } else {
                    // Grow short drafts without allowing the composer to consume the conversation.
                    let text = Text::from(
                        self.input
                            .split('\n')
                            .map(|line| Line::from(line.to_owned()))
                            .collect::<Vec<_>>(),
                    );
                    (crate::editor::layout(&text, main.width.saturating_sub(5))
                        .1
                        .len() as u16
                        + 1)
                    .clamp(4, (main.height / 3).clamp(4, 8))
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
        if self.picker.is_none() {
            return;
        }
        let choices = self.config_choices();
        let settings = self
            .picker
            .as_ref()
            .is_some_and(|p| p.kind == crate::picker::Kind::Settings);
        // An advertised selector still supports selection when its catalog is empty.
        let supports_selection = self
            .conversation()
            .into_iter()
            .flat_map(|state| &state.config_options)
            .any(|option| {
                let value = serde_json::to_value(option).unwrap_or_default();
                value["type"] == "select" && (value["category"] == "model") != settings
            });
        let empty_message = if settings && !supports_selection {
            "This backend does not expose session settings."
        } else if settings && choices.is_empty() {
            "No setting choices available."
        } else if settings {
            "No matching settings."
        } else if !supports_selection {
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
            .title(if settings {
                " Session settings "
            } else {
                " Provider / Model "
            })
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
        let mut lines = vec![
            Line::from(vec![
                Span::styled("  ▪ hive  ", theme.accent()),
                Span::styled(title, theme.text()),
            ]),
            Line::styled(
                format!(
                    "  {} · Ctrl-P model · Ctrl-G settings",
                    self.config_summary()
                ),
                theme.muted(),
            ),
        ];
        if let Some(label) = self.history_label() {
            // Compact layouts can hide the model line, but must keep history coverage visible.
            lines.insert(1, Line::styled(label, theme.warning()));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn history_label(&self) -> Option<String> {
        (self.page == Page::Chat)
            .then(|| {
                self.active
                    .as_ref()
                    .and_then(|id| self.conversations.get(id))
                    .map(|conversation| conversation.transcript.label())
            })
            .flatten()
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
            if let Some(view) = self.transcripts.get_mut(id)
                && view.selection_invalidated
            {
                self.status = "Selected content changed; select it again to copy".into();
                view.selection_invalidated = false;
            }
        }
    }

    fn render_composer(&mut self, frame: &mut Frame, area: Rect, theme: Theme) {
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
            if self.picker.is_none() && sections[1].width > 2 && sections[1].height > 0 {
                use unicode_width::UnicodeWidthStr;
                let x = self.input.width().min(usize::from(sections[1].width - 3)) as u16;
                frame.set_cursor_position((sections[1].x + 2 + x, sections[1].y));
            }
            return;
        }
        if self.page == Page::Chat {
            let block = Block::default()
                .title(Line::styled(
                    " Message · Alt-Enter newline · Ctrl-X Ctrl-E editor",
                    theme.muted(),
                ))
                .borders(Borders::LEFT)
                .border_style(theme.accent())
                .padding(Padding::horizontal(2));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            self.editor.prepare(&self.input, inner);
            if self.input.is_empty() {
                frame.render_widget(
                    Paragraph::new("Type a message…").style(theme.muted()),
                    inner,
                );
            } else {
                let selection = self.editor.selection();
                let lines = self
                    .editor
                    .rows
                    .iter()
                    .skip(self.editor.top)
                    .take(usize::from(inner.height))
                    .map(|row| row.line(selection.clone()))
                    .collect::<Vec<_>>();
                frame.render_widget(Paragraph::new(lines).style(theme.text()), inner);
            }
            if self.picker.is_none() && !inner.is_empty() {
                let (row, column) = self.editor.position();
                frame.set_cursor_position((
                    inner.x + (column as u16).min(inner.width - 1),
                    inner.y
                        + row
                            .saturating_sub(self.editor.top)
                            .min(usize::from(inner.height - 1)) as u16,
                ));
            }
            return;
        }
        let prompt = if self.page == Page::Sessions && self.sessions.is_empty() {
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
        let style = if self.input.is_empty() {
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
                        .title(Line::styled(" Tab sessions · Ctrl-N new", theme.muted()))
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
                "Enter send  Tab sessions  PgUp/PgDn scroll  End live  Alt-C/V/X clipboard  Ctrl-X Ctrl-E editor  Ctrl-C cancel  Esc quit"
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
            for (label, field) in [("Tools", "tools"), ("Tasks", "tasks")] {
                let status = activity["support"][field].as_str().unwrap_or("unknown");
                lines.push(Line::styled(format!("{label} · {status}"), theme.text()));
                if let Some(items) = activity[field].as_object() {
                    for (id, item) in items {
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
        if let Some(conversation) = self.conversation() {
            let snapshot = &conversation.plan_snapshot;
            // Canonical native content wins over the filtered activity compatibility view.
            let mut plans = conversation.activity["plans"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            if conversation.plans_authoritative {
                // Old activity can survive a restart; only explicit v1 adaptations are a fallback.
                plans.retain(|_, plan| plan["source"] == "acp-v1");
            }
            if let Some(native) = snapshot["plans"].as_object() {
                for (id, update) in native {
                    plans.insert(id.clone(), update["plan"].clone());
                }
            }
            let support = snapshot["support"].as_str().unwrap_or("unknown");
            let observation = if snapshot["observation"] == "observed" {
                "observed"
            } else {
                "not yet observed"
            };
            lines.push(Line::styled(
                format!("Plans · {support} · {observation}"),
                theme.text(),
            ));
            for (id, plan) in plans {
                lines.push(Line::styled(id, theme.muted()));
                if plan["type"] == "items" || (plan["type"].is_null() && plan["entries"].is_array())
                {
                    let entries = plan["entries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>();
                    if entries.is_empty() {
                        lines.push(Line::styled("  Empty plan", theme.muted()));
                    }
                    for entry in entries {
                        lines.push(Line::styled(
                            format!(
                                "  {} · {} · {}",
                                entry["status"].as_str().unwrap_or("?"),
                                entry["priority"].as_str().unwrap_or("?"),
                                entry["content"].as_str().unwrap_or("?")
                            ),
                            theme.text(),
                        ));
                    }
                } else {
                    lines.push(Line::styled(
                        format!(
                            "  Unsupported plan content type: {} · {plan}",
                            plan["type"].as_str().unwrap_or("unknown")
                        ),
                        theme.text(),
                    ));
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
    fn partial_history_warnings_remain_visible_when_transient_status_changes() {
        let mut app = App {
            page: Page::Chat,
            active: Some("session".into()),
            ..App::default()
        };
        let conversation = app.conversations.entry("session".into()).or_default();
        conversation
            .transcript
            .partial(hive_protocol::history::WarningCode::Truncated);
        conversation.push_notice("Retained answer".into());
        let mut terminal = Terminal::new(TestBackend::new(120, 25)).unwrap();
        for status in ["Connected", "Prompt sent"] {
            app.status = status.into();
            terminal.draw(|frame| app.show(frame)).unwrap();
            let rendered = screen(&terminal);
            assert!(rendered.contains("History: partial"));
            assert!(rendered.contains("bounded portion"));
            assert!(rendered.contains("Retained answer"));
            assert!(rendered.contains(status));
        }
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
                "configId": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "0",
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
    fn current_settings_remain_visible_when_the_agent_removes_the_selected_choice() {
        let mut app = App::default();
        app.select_session("session".into());
        app.conversations.entry("session".into()).or_default().config_options = serde_json::from_value(serde_json::json!([
            {"configId":"model","name":"Model","category":"model","type":"select","currentValue":"fast","options":[{"value":"fast","name":"Fast"},{"value":"other","name":"Other model"}]},
            {"configId":"mode","name":"Mode","category":"mode","type":"select","currentValue":"removed-mode","options":[]}
        ])).unwrap();
        assert_eq!(app.config_summary(), "Model: Fast · Mode: removed-mode");
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| app.show(frame)).unwrap();
        assert!(screen(&terminal).contains("Mode: removed-mode"));
        assert!(!screen(&terminal).contains("Other model"));
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
                "configId": "model", "name": "Model", "category": "model", "type": "select",
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
            "configId": "model", "name": "Model", "category": "model", "type": "select",
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
    fn the_sidebar_shows_tool_task_and_plan_status_from_the_shared_snapshot() {
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
        assert!(rendered.contains("in_progress · ? · Review"));
        assert!(rendered.contains("Plans · unknown"));
        assert!(rendered.contains("Tokens  120"));
        assert!(rendered.contains("estimated 0.0005 USD"));
        app.conversations.get_mut("hive-one").unwrap().apply(
            v2::UpdateSessionNotification::new(
                "hive-one",
                v2::SessionUpdate::PlanUpdate(v2::PlanUpdate::new(v2::PlanUpdateContent::items(
                    "native",
                    vec![v2::PlanEntry::new(
                        "Native",
                        v2::PlanEntryPriority::High,
                        v2::PlanEntryStatus::Pending,
                    )],
                ))),
            )
            .meta(v2::Meta::from_iter([(
                "hive".into(),
                serde_json::json!({"seq":1}),
            )])),
        );
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(
            rendered.contains("Review"),
            "one native update must not hide other older-daemon plans"
        );
        assert!(rendered.contains("Native"));
        let conversation = app.conversations.get_mut("hive-one").unwrap();
        conversation.activity["plans"]["v1-plan"] = serde_json::json!({"source":"acp-v1","entries":[{"content":"V1","status":"pending","priority":"low"}]});
        conversation.plans_snapshot(&serde_json::json!({"version":1,"seq":6,"support":"supported","observation":"not_observed","plans":{}}));
        terminal.draw(|frame| app.show(frame)).unwrap();
        let rendered = screen(&terminal);
        assert!(
            !rendered.contains("Review"),
            "canonical recovery must not resurrect a stale native plan"
        );
        assert!(rendered.contains("pending · low · V1"));
        assert!(rendered.contains("Plans · supported"));
        // The narrow sidebar wraps this label between words; layout must not decide observation.
        assert!(
            ["not", "yet", "observed"]
                .iter()
                .all(|word| rendered.contains(word)),
            "adapted v1 plans do not establish native v2 observation"
        );
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
