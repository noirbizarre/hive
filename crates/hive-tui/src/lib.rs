//! Full-screen ACPv2 client; the daemon remains the sole session authority.

mod state;
mod theme;
mod ui;

#[cfg(test)]
mod interaction_tests;

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, Error, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use hive_protocol::HttpClient;
use tokio::sync::{mpsc, oneshot};

use ui::{App, Page, SessionSnapshot};

#[derive(Args, Clone, Debug)]
pub struct Options {
    #[arg(long, default_value = "127.0.0.1:8765")]
    pub address: String,
    #[arg(long, default_value = "ws", value_parser = ["ws", "http"])]
    pub transport: String,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub session: Option<String>,
}

enum Interaction {
    Permission(
        v2::RequestPermissionRequest,
        oneshot::Sender<v2::RequestPermissionResponse>,
    ),
    Elicitation(
        v2::CreateElicitationRequest,
        oneshot::Sender<v2::CreateElicitationResponse>,
        FormDraft,
    ),
}

#[derive(Default)]
struct FormDraft {
    field: usize,
    choice: usize,
    values: BTreeMap<String, String>,
}

impl FormDraft {
    fn choices(field: &serde_json::Value) -> Vec<String> {
        if field["type"] == "boolean" {
            return vec!["true".into(), "false".into()];
        }
        let field = if field["type"] == "array" {
            &field["items"]
        } else {
            field
        };
        field["enum"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            })
            .or_else(|| {
                field
                    .get("oneOf")
                    .or_else(|| field.get("anyOf"))
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value["const"].as_str().map(str::to_owned))
                            .collect()
                    })
            })
            .unwrap_or_default()
    }
    fn fields(request: &v2::CreateElicitationRequest) -> Vec<(String, serde_json::Value)> {
        match &request.mode {
            v2::ElicitationMode::Form(form) => form
                .requested_schema
                .properties
                .iter()
                .map(|(name, field)| {
                    (
                        name.clone(),
                        serde_json::to_value(field).expect("ACP field serializes"),
                    )
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn save(&mut self, request: &v2::CreateElicitationRequest, text: &str) {
        if let Some((name, schema)) = Self::fields(request).get(self.field) {
            let required = matches!(&request.mode, v2::ElicitationMode::Form(form) if form.requested_schema.required.as_ref().is_some_and(|fields| fields.contains(name)));
            if text.is_empty()
                && !(required && matches!(schema["type"].as_str(), Some("string" | "array")))
                && !self.values.get(name).is_some_and(String::is_empty)
            {
                self.values.remove(name);
            } else {
                self.values.insert(name.clone(), text.into());
            }
        }
    }

    fn response(
        &self,
        request: &v2::CreateElicitationRequest,
    ) -> Result<v2::CreateElicitationResponse, String> {
        let mut content = BTreeMap::new();
        for (name, schema) in Self::fields(request) {
            if let Some(text) = self.values.get(&name) {
                content.insert(
                    name,
                    hive_protocol::interactions::parse_field(&schema, text)
                        .map_err(|error| error.to_string())?,
                );
            }
        }
        let response = v2::CreateElicitationResponse::new(v2::ElicitationAction::Accept(
            v2::ElicitationAcceptAction::new().content(content),
        ));
        hive_protocol::interactions::validate_elicitation(request, &response)
            .map_err(|error| error.to_string())?;
        Ok(response)
    }

    fn show(&self, app: &mut App, request: &v2::CreateElicitationRequest) {
        app.dialog_scroll = 0;
        if let Some((name, schema)) = Self::fields(request).get(self.field) {
            app.dialog = Some(format!(
                "{}\nField {}/{}: {} ({}, {})\n{}\nChoices: {}\nConstraints: {}\nTab/Shift-Tab: fields · arrows: choices · Space: multiselect · Ctrl-Space: empty · Enter: submit · Esc: decline",
                request.message,
                self.field + 1,
                Self::fields(request).len(),
                schema["title"].as_str().unwrap_or(name),
                schema["type"].as_str().unwrap_or("unknown"),
                if matches!(&request.mode, v2::ElicitationMode::Form(form) if form.requested_schema.required.as_ref().is_some_and(|fields| fields.contains(name)))
                {
                    "required"
                } else {
                    "optional"
                },
                schema["description"].as_str().unwrap_or(""),
                schema
                    .get("enum")
                    .or_else(|| schema.get("oneOf"))
                    .or_else(|| schema.get("items"))
                    .map_or_else(
                        || "free input (arrays separated by |)".into(),
                        ToString::to_string
                    ),
                schema
            ));
            app.input = self.values.get(name).cloned().unwrap_or_else(|| {
                schema.get("default").map_or_else(String::new, |value| {
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned)
                })
            });
        }
    }
}

struct TerminalGuard;
impl Interaction {
    fn url_id(&self) -> Option<String> {
        match self {
            Self::Elicitation(request, _, _) => match &request.mode {
                v2::ElicitationMode::Url(url) => Some(url.elicitation_id.to_string()),
                _ => None,
            },
            _ => None,
        }
    }
    fn request_scoped(&self) -> bool {
        matches!(self, Self::Elicitation(request, _, _) if matches!(request.scope(), v2::ElicitationScope::Request(_)))
    }
    fn expired(&self) -> bool {
        match self {
            Self::Permission(_, tx) => tx.is_closed(),
            Self::Elicitation(_, tx, _) => tx.is_closed(),
        }
    }
}
impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(std::io::stdout(), EnterAlternateScreen) {
            // An alternate-screen failure must not leave the user's shell in raw mode.
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn hive_meta(mode: &str, owner: Option<&v2::Meta>) -> v2::Meta {
    let mut meta = if mode == "relinquish" {
        owner.cloned().unwrap_or_default()
    } else {
        v2::Meta::new()
    };
    let mut hive = meta
        .get("hive")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if mode != "pilot" {
        hive["mode"] = serde_json::json!(mode);
    }
    meta.insert("hive".into(), hive);
    meta
}

async fn relinquish_active(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
) -> agent_client_protocol::Result<()> {
    if let Some(previous) = app.active.as_ref()
        && app.pilot()
    {
        let cwd = app
            .sessions
            .iter()
            .find(|s| &s.session_id == previous)
            .map_or_else(|| app.cwd.clone(), |s| s.cwd.clone());
        // A switched-away conversation must not retain an invisible pilot on this connection.
        cx.send_request(
            v2::ResumeSessionRequest::new(v2::SessionId::new(previous.clone()), cwd)
                .meta(hive_meta("relinquish", app.owner.as_ref())),
        )
        .block_task()
        .await?;
        app.owner = None;
    }
    Ok(())
}

async fn resume(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
    id: String,
    cwd: std::path::PathBuf,
    mode: &str,
) -> agent_client_protocol::Result<()> {
    if let Some(previous) = app.active.as_ref()
        && previous != &id
    {
        relinquish_active(cx, app).await?;
    }
    let seq = app.conversations.get(&id).map_or(0, |c| c.seq);
    let replay = if seq == 0 {
        v2::ReplayFrom::Start(v2::ReplayFromStart::new())
    } else {
        v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
            "_hive_seq",
            BTreeMap::from([("seq".into(), serde_json::json!(seq + 1))]),
        ))
    };
    let mut request = v2::ResumeSessionRequest::new(v2::SessionId::new(id.clone()), cwd.clone())
        .meta(hive_meta(mode, app.owner.as_ref()));
    if !app.skip_replay {
        request = request.replay_from(replay);
    }
    let response = match cx.send_request(request).block_task().await {
        Ok(response) => response,
        Err(error) if format!("{error:?}").contains("replay cursor unavailable") => {
            app.status = "History expired: press Ctrl-R to attach without earlier events".into();
            if app.active.as_deref() != Some(&id) {
                app.owner = None;
                app.select_session(id);
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let hive = response
        .meta
        .as_ref()
        .and_then(|m| m.get("hive"))
        .ok_or_else(Error::internal_error)?;
    let conversation = app.conversations.entry(id.clone()).or_default();
    conversation.generation = hive["generation"].as_u64().unwrap_or(0);
    conversation.running = hive["running"].as_bool().unwrap_or(false);
    conversation.pilot_present = hive["pilot"].as_bool().unwrap_or(false);
    conversation.config_snapshot(
        response.config_options,
        hive["configSeq"].as_u64().unwrap_or(0),
    );
    conversation.snapshot(&hive["activitySnapshot"]);
    conversation.usage_snapshot(
        &hive["usageSnapshot"],
        hive["usageSeq"].as_u64().unwrap_or(0),
    );
    conversation.gap = false;
    let next_seq = hive["nextSeq"].as_u64().unwrap_or(1);
    app.owner = response.meta;
    if app.skip_replay {
        // A live-only attachment must not treat future event sequences as an expired replay gap.
        conversation.seq = next_seq.saturating_sub(1);
        conversation.push_notice("[Earlier history unavailable]".into());
    }
    app.skip_replay = false;
    if app.active.as_deref() != Some(&id) {
        app.select_session(id);
    } else {
        app.page = Page::Chat;
    }
    Ok(())
}

async fn refresh(address: &str, app: &mut App) -> Result<(), String> {
    let response = reqwest::get(format!("http://{address}/v1/sessions"))
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    let snapshot: SessionSnapshot = response.json().await.map_err(|error| error.to_string())?;
    let sessions = snapshot
        .sessions
        .into_iter()
        .filter(|s| app.all || s.cwd == app.cwd)
        .collect();
    app.update_sessions(sessions);
    Ok(())
}

fn resume_failure(error: &agent_client_protocol::Error) -> String {
    if format!("{error:?}").contains("replay cursor unavailable") {
        "History expired: press Ctrl-R to attach without earlier events".into()
    } else {
        format!("Could not attach: {error}")
    }
}

fn show_interaction(app: &mut App, interaction: &Interaction) {
    app.dialog_scroll = 0;
    app.dialog = Some(match interaction {
        Interaction::Permission(req, _) => format!(
            "{}\n{}\n{}\n{}\nEnter an option number; Esc cancels",
            req.title,
            req.description.as_deref().unwrap_or(""),
            req.subject
                .as_ref()
                .map(|subject| serde_json::to_string_pretty(subject).unwrap_or_default())
                .unwrap_or_default(),
            req.options
                .iter()
                .enumerate()
                .map(|(i, o)| format!(
                    "{}: {} ({})",
                    i + 1,
                    o.name,
                    serde_json::to_value(&o.kind).expect("ACP option serializes")
                ))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        Interaction::Elicitation(req, _, _) => match &req.mode {
            v2::ElicitationMode::Form(_) => {
                format!("{}\nEnter submits the form; Esc declines", req.message)
            }
            v2::ElicitationMode::Url(url) => format!(
                "{}\nOpen {}. Enter consents to continue; completion is reported by the agent. Esc declines",
                req.message, url.url
            ),
            _ => "Unsupported agent input (Esc to decline)".into(),
        },
    });
    app.input.clear();
    if let Interaction::Elicitation(req, _, draft) = interaction {
        draft.show(app, req);
    }
}

fn send_prompt(
    cx: &V2ConnectionTo<Agent>,
    app: &App,
    acks: mpsc::UnboundedSender<(String, Result<(), String>)>,
) {
    let (Some(id), Some(owner)) = (&app.active, &app.owner) else {
        return;
    };
    let Some((key, content)) = app.pending.get(id) else {
        return;
    };
    let mut meta = owner.clone();
    let mut hive = meta.get("hive").cloned().unwrap_or_default();
    hive["key"] = serde_json::json!(key);
    meta.insert("hive".into(), hive);
    let cx = cx.clone();
    let id = id.clone();
    let content = content.clone();
    tokio::spawn(async move {
        let result = cx
            .send_request(
                v2::PromptRequest::new(v2::SessionId::new(id.clone()), vec![content.into()])
                    .meta(meta),
            )
            .block_task()
            .await;
        let _ = acks.send((id, result.map(|_| ()).map_err(|e| e.to_string())));
    });
}

async fn handle_key(
    cx: &V2ConnectionTo<Agent>,
    address: &str,
    app: &mut App,
    key: event::KeyEvent,
    interaction: &mut Option<Interaction>,
    acks: mpsc::UnboundedSender<(String, Result<(), String>)>,
    created: mpsc::UnboundedSender<Result<v2::NewSessionResponse, String>>,
) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if interaction.is_some() && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
        app.dialog_scroll = if key.code == KeyCode::PageUp {
            app.dialog_scroll.saturating_sub(4)
        } else {
            app.dialog_scroll.saturating_add(4)
        };
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let (Some(id), Some(owner)) = (&app.active, &app.owner)
            && app.pilot()
        {
            let _ = cx.send_notification(
                v2::CancelSessionNotification::new(v2::SessionId::new(id.clone()))
                    .meta(owner.clone()),
            );
            *interaction = None;
            app.dialog = None;
            app.input.clear();
        }
        return false;
    }
    if let Some(active) = interaction.take() {
        let mut keep = true;
        match active {
            Interaction::Permission(req, tx) => {
                let mut tx = Some(tx);
                if key.code == KeyCode::Esc {
                    let _ = tx.take().unwrap().send(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Cancelled,
                    ));
                    keep = false;
                } else if key.code == KeyCode::Enter {
                    if let Some(option) = app
                        .input
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|i| req.options.get(i))
                    {
                        let _ = tx.take().unwrap().send(v2::RequestPermissionResponse::new(
                            v2::RequestPermissionOutcome::Selected(
                                v2::SelectedPermissionOutcome::new(option.option_id.clone()),
                            ),
                        ));
                        keep = false;
                    } else {
                        app.status = "Select a listed permission option".into();
                    }
                } else if key.code == KeyCode::Backspace {
                    app.input.pop();
                } else if let KeyCode::Char(digit) = key.code
                    && digit.is_ascii_digit()
                {
                    app.input.push(digit);
                }
                if keep {
                    *interaction = Some(Interaction::Permission(req, tx.unwrap()));
                }
            }
            Interaction::Elicitation(req, tx, mut draft) => {
                let mut tx = Some(tx);
                match key.code {
                    KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some((name, field)) = FormDraft::fields(&req).get(draft.field)
                            && matches!(field["type"].as_str(), Some("string" | "array"))
                        {
                            draft.values.insert(name.clone(), String::new());
                            app.input.clear();
                        }
                    }
                    KeyCode::Esc => {
                        let _ = tx.take().unwrap().send(v2::CreateElicitationResponse::new(
                            v2::ElicitationAction::Decline,
                        ));
                        keep = false;
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        draft.save(&req, &app.input);
                        let count = FormDraft::fields(&req).len();
                        if count > 0 {
                            draft.field = (draft.field
                                + if key.code == KeyCode::BackTab {
                                    count - 1
                                } else {
                                    1
                                })
                                % count;
                            draft.choice = 0;
                            draft.show(app, &req);
                        }
                    }
                    KeyCode::Up | KeyCode::Down => {
                        if let Some((_, field)) = FormDraft::fields(&req).get(draft.field) {
                            let choices = FormDraft::choices(field);
                            if !choices.is_empty() {
                                if field["type"] == "array" {
                                    draft.choice = (draft.choice
                                        + if key.code == KeyCode::Up {
                                            choices.len() - 1
                                        } else {
                                            1
                                        })
                                        % choices.len();
                                    app.status = format!("Space toggles {}", choices[draft.choice]);
                                } else {
                                    let index = choices
                                        .iter()
                                        .position(|value| value == &app.input)
                                        .map_or(0, |index| {
                                            (index
                                                + if key.code == KeyCode::Up {
                                                    choices.len() - 1
                                                } else {
                                                    1
                                                })
                                                % choices.len()
                                        });
                                    app.input = choices[index].clone();
                                }
                            }
                        }
                    }
                    KeyCode::Enter => {
                        draft.save(&req, &app.input);
                        let result = match &req.mode {
                            v2::ElicitationMode::Url(_) => Ok(v2::CreateElicitationResponse::new(
                                v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new()),
                            )),
                            _ => draft.response(&req),
                        };
                        match result {
                            Ok(response) => {
                                let _ = tx.take().unwrap().send(response);
                                keep = false;
                            }
                            Err(message) => app.status = message,
                        }
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(' ')
                        if FormDraft::fields(&req).get(draft.field).is_some_and(
                            |(_, field)| {
                                field["type"] == "array" && !FormDraft::choices(field).is_empty()
                            },
                        ) =>
                    {
                        let fields = FormDraft::fields(&req);
                        let field = &fields[draft.field].1;
                        let choice = &FormDraft::choices(field)[draft.choice];
                        match hive_protocol::interactions::parse_field(field, &app.input) {
                            Ok(v2::ElicitationContentValue::StringArray(mut values)) => {
                                if values.contains(choice) {
                                    values.retain(|value| value != choice);
                                } else {
                                    values.push(choice.clone());
                                }
                                app.input =
                                    serde_json::to_string(&values).expect("strings serialize");
                            }
                            _ => {
                                app.status =
                                    "Enter a JSON string array before toggling choices".into()
                            }
                        }
                    }
                    KeyCode::Char(ch) => app.input.push(ch),
                    _ => {}
                }
                if keep {
                    *interaction = Some(Interaction::Elicitation(req, tx.unwrap(), draft));
                }
            }
        }
        if !keep {
            app.dialog = None;
            app.input.clear();
        }
        return false;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (key.code, ctrl) {
        // Ctrl-M aliases Enter in terminals; Ctrl-P remains distinguishable for the model picker.
        (KeyCode::Char('p'), true) => {
            app.page = if app.page == Page::Configuration {
                Page::Chat
            } else {
                Page::Configuration
            };
            app.selected = 0;
        }
        (KeyCode::Esc, _) | (KeyCode::Char('q'), true) => return true,
        (KeyCode::Tab, _) => {
            if app.toggle_sessions()
                && let Err(error) = refresh(address, app).await
            {
                app.status = error;
            }
        }
        (KeyCode::Char('b'), true) if app.page == Page::Chat => {
            if let Some(info) = app.previous_session()
                && let Err(error) = resume(cx, app, info.session_id, info.cwd, "pilot").await
            {
                app.status = resume_failure(&error);
            }
        }
        (KeyCode::Char('n'), true) => {
            app.page = Page::Profiles;
            app.selected = 0;
        }
        (KeyCode::Char('r'), true) => {
            if let Some(id) = app.active.clone() {
                app.skip_replay = true;
                let cwd = app
                    .sessions
                    .iter()
                    .find(|s| s.session_id == id)
                    .map_or_else(|| app.cwd.clone(), |s| s.cwd.clone());
                let mode = if app
                    .owner
                    .as_ref()
                    .is_some_and(|m| m["hive"]["role"] == "observer")
                {
                    "observe"
                } else {
                    "pilot"
                };
                if let Err(error) = resume(cx, app, id, cwd, mode).await {
                    app.skip_replay = false;
                    app.status = resume_failure(&error);
                }
            }
        }
        (KeyCode::Char('y'), true) => {
            if let Some(id) = app.active.clone() {
                let cwd = app
                    .sessions
                    .iter()
                    .find(|session| session.session_id == id)
                    .map_or_else(|| app.cwd.clone(), |session| session.cwd.clone());
                let mut meta = v2::Meta::new();
                meta.insert(
                    "hive".into(),
                    serde_json::json!({ "recoveryAction": "acknowledge" }),
                );
                match cx
                    .send_request(
                        v2::ResumeSessionRequest::new(v2::SessionId::new(id.clone()), cwd.clone())
                            .meta(meta),
                    )
                    .block_task()
                    .await
                {
                    Ok(response) => {
                        app.owner = response.meta;
                        app.skip_replay = true;
                        if let Err(error) = resume(cx, app, id, cwd, "pilot").await {
                            app.status = resume_failure(&error);
                        } else {
                            app.status = "Interrupted work acknowledged; verify agent state before sending a new prompt".into();
                        }
                    }
                    Err(error) => app.status = resume_failure(&error),
                }
            }
        }
        (KeyCode::Char('c'), true) => {
            if let (Some(id), Some(owner)) = (&app.active, &app.owner)
                && app.pilot()
            {
                let _ = cx.send_notification(
                    v2::CancelSessionNotification::new(v2::SessionId::new(id.clone()))
                        .meta(owner.clone()),
                );
            }
        }
        (KeyCode::Char('t'), true) | (KeyCode::Char('h'), true) => {
            if let Some(id) = app.active.clone() {
                let mode = if key.code == KeyCode::Char('h') {
                    "relinquish"
                } else {
                    "pilot"
                };
                let cwd = app
                    .sessions
                    .iter()
                    .find(|s| s.session_id == id)
                    .map_or_else(|| app.cwd.clone(), |s| s.cwd.clone());
                if let Err(error) = resume(cx, app, id, cwd, mode).await {
                    app.status = resume_failure(&error);
                }
            }
        }
        (KeyCode::Up, _) => app.selected = app.selected.saturating_sub(1),
        (KeyCode::Down, _) => {
            app.selected = app.selected.saturating_add(1).min(
                match app.page {
                    Page::Sessions => app.sessions.len(),
                    Page::Profiles => app.profiles.len(),
                    Page::Configuration => app.config_choices().len(),
                    Page::Chat => 0,
                }
                .saturating_sub(1),
            )
        }
        (KeyCode::Char('a'), _) if app.page == Page::Sessions => {
            app.all = !app.all;
            if let Err(error) = refresh(address, app).await {
                app.status = error;
            }
        }
        (KeyCode::Char('o'), _) if app.page == Page::Sessions => {
            if let Some(info) = app.sessions.get(app.selected).cloned()
                && let Err(error) = resume(cx, app, info.session_id, info.cwd, "observe").await
            {
                app.status = resume_failure(&error);
            }
        }
        (KeyCode::Enter, _) => match app.page {
            Page::Configuration => {
                if !app.connected
                    || !app.pilot()
                    || app.conversation().is_some_and(|state| state.running)
                    || app.configuration.is_some()
                    || app
                        .active
                        .as_ref()
                        .is_some_and(|id| app.pending.contains_key(id))
                {
                    app.status = "Take control of an idle session before changing its model".into();
                } else if let (Some(id), Some((config_id, value, _)), Some(owner)) = (
                    app.active.clone(),
                    app.config_choices().get(app.selected).cloned(),
                    app.owner.clone(),
                ) {
                    let request = v2::SetSessionConfigOptionRequest::new(
                        v2::SessionId::new(id.clone()),
                        v2::SessionConfigId::new(config_id),
                        v2::SessionConfigOptionValue::Id {
                            value: v2::SessionConfigValueId::new(value),
                        },
                    )
                    .meta(owner);
                    let request = cx.send_request(request).block_task();
                    let (sent, received) = tokio::sync::oneshot::channel();
                    app.configuration = Some(received);
                    app.status = "Updating session configuration…".into();
                    if let Err(error) = cx.spawn(async move {
                        let _ = sent.send((
                            id,
                            request.await.map_err(|error| {
                                error
                                    .data
                                    .as_ref()
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("Agent configuration failed; check the agent logs")
                                    .to_owned()
                            }),
                        ));
                        Ok(())
                    }) {
                        app.configuration = None;
                        app.status = error.to_string();
                    }
                }
            }
            Page::Sessions => {
                if let Some(info) = app.sessions.get(app.selected).cloned()
                    && let Err(error) = resume(cx, app, info.session_id, info.cwd, "pilot").await
                {
                    app.status = resume_failure(&error);
                }
            }
            Page::Profiles => {
                if let Some(profile) = app.profiles.get(app.selected).cloned() {
                    let mut meta = v2::Meta::new();
                    meta.insert("hive".into(), serde_json::json!({"agent": profile}));
                    if let Err(error) = relinquish_active(cx, app).await {
                        app.status = format!("Could not leave the previous session: {error}");
                        return false;
                    }
                    let request = cx
                        .send_request(v2::NewSessionRequest::new(app.cwd.clone()).meta(meta))
                        .block_task();
                    cx.spawn(async move {
                        let _ = created.send(request.await.map_err(|error| error.to_string()));
                        Ok(())
                    })
                    .unwrap_or_else(|error| app.status = error.to_string());
                    app.page = Page::Chat;
                    app.status = "Creating session…".into();
                }
            }
            Page::Chat => {
                if !app.input.trim().is_empty()
                    && app.pilot()
                    && app.configuration.is_none()
                    && !app.conversation().is_some_and(|c| c.running)
                    && app
                        .active
                        .as_ref()
                        .is_some_and(|id| !app.pending.contains_key(id))
                {
                    let content = std::mem::take(&mut app.input);
                    let key = uuid::Uuid::new_v4().to_string();
                    let id = app.active.clone().unwrap_or_default();
                    // Only sequenced daemon messages enter history; the admission echo would duplicate this draft.
                    app.pending.insert(id, (key, content));
                    send_prompt(cx, app, acks);
                } else {
                    app.status = "Pilot control and an idle session are required to send".into();
                }
            }
        },
        (KeyCode::Backspace, _) if app.page == Page::Chat => {
            app.input.pop();
        }
        (KeyCode::Char(ch), _) if app.page == Page::Chat => app.input.push(ch),
        _ => {}
    }
    false
}

async fn interaction_client<R>(
    updates: mpsc::UnboundedSender<v2::UpdateSessionNotification>,
    interactions: mpsc::UnboundedSender<Interaction>,
    completions: mpsc::UnboundedSender<v2::ElicitationId>,
    transport: impl agent_client_protocol::ConnectTo<Client>,
    main: impl AsyncFnOnce(V2ConnectionTo<Agent>) -> Result<R, Error>,
) -> Result<R, Error> {
    Client
        .v2()
        .on_receive_notification(
            async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                let _ = updates.send(update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let interactions = interactions.clone();
                async move |request: v2::RequestPermissionRequest,
                            responder: agent_client_protocol::Responder<
                    v2::RequestPermissionResponse,
                >,
                            cx: V2ConnectionTo<Agent>| {
                    let (tx, rx) = oneshot::channel();
                    let _ = interactions.send(Interaction::Permission(request, tx));
                    let cancellation = responder.cancellation();
                    cx.spawn(async move {
                        let result = cancellation
                            .run_until_cancelled(async {
                                Ok(rx.await.unwrap_or_else(|_| {
                                    v2::RequestPermissionResponse::new(
                                        v2::RequestPermissionOutcome::Cancelled,
                                    )
                                }))
                            })
                            .await;
                        let _ = responder.respond_with_result(result);
                        Ok(())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: v2::CreateElicitationRequest,
                        responder: agent_client_protocol::Responder<
                v2::CreateElicitationResponse,
            >,
                        cx: V2ConnectionTo<Agent>| {
                let supported = match &request.mode {
                    v2::ElicitationMode::Form(form) => {
                        hive_protocol::interactions::schema_supported(
                            &serde_json::to_value(&form.requested_schema)
                                .map_err(Error::into_internal_error)?,
                        )
                    }
                    v2::ElicitationMode::Url(url) => {
                        hive_protocol::interactions::safe_url(&url.url)
                    }
                    _ => false,
                };
                if !supported {
                    return responder.respond(v2::CreateElicitationResponse::new(
                        v2::ElicitationAction::Cancel,
                    ));
                }
                let (tx, rx) = oneshot::channel();
                let _ =
                    interactions.send(Interaction::Elicitation(request, tx, FormDraft::default()));
                let cancellation = responder.cancellation();
                cx.spawn(async move {
                    let result = cancellation
                        .run_until_cancelled(async {
                            Ok(rx.await.unwrap_or_else(|_| {
                                v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel)
                            }))
                        })
                        .await;
                    let _ = responder.respond_with_result(result);
                    Ok(())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: v2::CompleteElicitationNotification,
                        _: V2ConnectionTo<Agent>| {
                let _ = completions.send(notification.elicitation_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, main)
        .await
}

pub async fn run(options: Options) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut app = App {
        cwd: std::env::current_dir()?,
        all: true,
        ..App::default()
    };
    let providers: serde_json::Value =
        reqwest::get(format!("http://{}/v1/providers", options.address))
            .await?
            .json()
            .await?;
    app.profiles = providers["agents"]
        .as_array()
        .map(|agents| {
            agents
                .iter()
                .filter_map(|agent| agent["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if let Some(agent) = &options.agent {
        if options.session.is_none() {
            app.page = Page::Profiles;
        }
        app.selected = app.profiles.iter().position(|id| id == agent).unwrap_or(0);
    }
    let _guard = TerminalGuard::enter()?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let (keys_tx, mut keys) = mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        loop {
            if event::poll(Duration::from_millis(80)).unwrap_or(false)
                && let Ok(event) = event::read()
                && keys_tx.send(event).is_err()
            {
                break;
            }
        }
    });
    let mut quit = false;
    while !quit {
        let url = format!("{}://{}", options.transport, options.address);
        let (updates_tx, mut updates) = mpsc::unbounded_channel();
        let (interaction_tx, mut interactions) = mpsc::unbounded_channel();
        let (completion_tx, mut completions) = mpsc::unbounded_channel();
        let result = interaction_client(updates_tx, interaction_tx, completion_tx, HttpClient::new(url)?, async |cx| {
                cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2,
                    v2::Implementation::new("hive-tui", env!("CARGO_PKG_VERSION")))
                    .capabilities(v2::ClientCapabilities::new().elicitation(v2::ElicitationCapabilities::new()
                        .form(v2::ElicitationFormCapabilities::new()).url(v2::ElicitationUrlCapabilities::new()))))
                    .block_task().await?;
                app.connected = true;
                 if let Err(error) = refresh(&options.address, &mut app).await { app.status = error; }
                if app.page == Page::Profiles && app.active.is_none() {
                    app.selected = options.agent.as_ref().and_then(|agent| app.profiles.iter().position(|id| id == agent)).unwrap_or(0);
                }
                let initial = app.active.clone().or_else(|| options.session.clone());
                if let Some(id) = initial {
                     let cwd = app.sessions.iter().find(|s| s.session_id == id).map_or_else(|| app.cwd.clone(), |s| s.cwd.clone());
                    let mode = if app.owner.as_ref().is_some_and(|m| m["hive"]["role"] == "observer") { "observe" } else { "pilot" };
                     if let Err(error) = resume(&cx, &mut app, id, cwd, mode).await { app.status = resume_failure(&error); }
                }
                  let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<(String, Result<(), String>)>();
                  let (created_tx, mut created_rx) = mpsc::unbounded_channel::<Result<v2::NewSessionResponse, String>>();
                // The previous acknowledgement may have been lost; the original key is safe to retry.
                 if app.active.as_ref().is_some_and(|id| app.pending.contains_key(id)) && app.pilot() { send_prompt(&cx, &app, ack_tx.clone()); }
                  let mut interaction: Option<Interaction> = None;
                  let mut queued = VecDeque::<Interaction>::new();
                  let mut url_flows = BTreeMap::<String, (bool, bool, tokio::time::Instant)>::new();
                  let mut interaction_tick = tokio::time::interval(Duration::from_millis(50));
                 let mut catalog_tick = tokio::time::interval(Duration::from_secs(2));
                 loop {
                    if let Some(receiver) = app.configuration.as_mut() {
                        match receiver.try_recv() {
                            Ok((id, result)) => {
                                app.configuration = None;
                                match result {
                                    Ok(response) => {
                                        let seq = response.meta.as_ref().and_then(|meta| meta.get("hive")).and_then(|hive| hive["configSeq"].as_u64()).unwrap_or(0);
                                        app.conversations.entry(id).or_default().config_snapshot(response.config_options, seq);
                                        app.status = "Session configuration updated".into();
                                    }
                                    Err(error) => app.status = error,
                                }
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => { app.configuration = None; app.status = "Configuration interrupted; reopen the session to inspect its current model".into(); }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                        }
                    }
                    terminal.draw(|frame| app.show(frame)).map_err(Error::into_internal_error)?;
                     tokio::select! {
                         _ = interaction_tick.tick() => {
                             url_flows.retain(|_, (known, complete, expires)| {
                                 if *expires <= tokio::time::Instant::now() {
                                     if *known && !*complete { app.status = "Completion not received before timeout".into(); }
                                     false
                                 } else { true }
                             });
                             queued.retain(|request| !request.expired());
                             if interaction.as_ref().is_some_and(Interaction::expired) {
                                 if let Some(id) = interaction.as_ref().and_then(Interaction::url_id) { url_flows.remove(&id); }
                                 interaction = None; app.dialog = None; app.input.clear();
                             }
                             if interaction.is_none() && let Some(index) = queued.iter().position(|request| app.pilot() || request.request_scoped()) && let Some(next) = queued.remove(index) { show_interaction(&mut app, &next); interaction = Some(next); }
                         }
                         Some(id) = completions.recv() => {
                             if url_flows.len() >= 128 { url_flows.clear(); }
                             let flow = url_flows.entry(id.to_string()).or_insert_with(|| (false, false, tokio::time::Instant::now() + Duration::from_secs(120)));
                             flow.1 = true;
                             if flow.0 { app.status = "External elicitation completed".into(); }
                         }
                         Some(result) = created_rx.recv() => match result {
                             Ok(created) => {
                                 let id = created.session_id.to_string();
                                 let state = app.conversations.entry(id.clone()).or_default();
                                 state.generation = 1;
                                  state.pilot_present = true;
                                  let seq = created.meta.as_ref().and_then(|meta| meta.get("hive")).and_then(|hive| hive["configSeq"].as_u64()).unwrap_or(0);
                                  state.config_snapshot(created.config_options, seq);
                                 app.owner = created.meta;
                                 app.select_session(id);
                                 app.status = "Session created".into();
                             }
                             Err(error) => { app.status = format!("Could not start session: {error}"); app.page = Page::Profiles; }
                         },
                         _ = cx.incoming_closed() => break,
                         _ = catalog_tick.tick() => {
                             if let Err(error) = refresh(&options.address, &mut app).await { app.status = format!("Session list unavailable: {error}"); }
                         }
                         Some(update) = updates.recv() => {
                            let id = update.session_id.to_string();
                            let current = app.active.as_ref() == Some(&id);
                            let state = app.conversations.entry(id).or_default();
                             let applied = state.apply(update);
                             let missing = state.gap;
                             if missing && current {
                                 app.status = "Activity gap detected; reconnect to reconcile history".into();
                                 // The next connection replays from the last committed sequence.
                                 break;
                             }
                             if applied && current && !app.pilot() {
                                app.status = "Observer — take control to interact".into();
                                queued.retain(Interaction::request_scoped);
                                if !interaction.as_ref().is_some_and(Interaction::request_scoped) {
                                    interaction = None;
                                    app.dialog = None;
                                    app.input.clear();
                                }
                            }
                        }
                         Some(next) = interactions.recv() => {
                            if let Interaction::Elicitation(request, _, _) = &next && let v2::ElicitationMode::Url(url) = &request.mode {
                                if url_flows.len() >= 128 { url_flows.clear(); }
                                let flow = url_flows.entry(url.elicitation_id.to_string()).or_insert_with(|| (false, false, tokio::time::Instant::now() + Duration::from_secs(120)));
                                flow.0 = true;
                                app.status = if flow.1 { "External elicitation completed" } else { "Waiting for external completion" }.into();
                            }
                            if interaction.is_none() && (app.pilot() || next.request_scoped()) { show_interaction(&mut app, &next); interaction = Some(next); }
                             else if queued.len() < 128 { queued.push_back(next); }
                             // Dropping an overflow request safely cancels it at the daemon boundary.
                        }
                         Some((id, result)) = ack_rx.recv() => {
                             match result { Ok(()) => { app.pending.remove(&id); }, Err(error) => {
                                 app.status = format!("Prompt acknowledgement uncertain: {error}; retry with the same key after reconnect");
                                 if error.contains("stale") && app.active.as_deref() == Some(&id) { app.owner = None; }
                             }}
                         }
                         Some(Event::Key(key)) = keys.recv() => {
                             let before = app.active.clone();
                             let declined_url = if key.code == KeyCode::Esc { interaction.as_ref().and_then(Interaction::url_id) } else { None };
                             if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) { url_flows.clear(); }
                             if handle_key(&cx, &options.address, &mut app, key, &mut interaction, ack_tx.clone(), created_tx.clone()).await { quit = true; break; }
                             if let Some(id) = declined_url { url_flows.remove(&id); app.status = "External elicitation declined".into(); }
                             if before != app.active {
                                 interaction = None;
                                 app.dialog = None;
                             }
                         },
                    }
                }
                Ok(())
            }).await;
        app.connected = false;
        // A disconnected form draft must never reappear as an ordinary chat prompt.
        if app.dialog.take().is_some() {
            app.input.clear();
        }
        if quit {
            break;
        }
        app.status = format!("Disconnected ({result:?}); reconnecting to the same Hive session…");
        terminal.draw(|frame| app.show(frame))?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_tui_adapter_keeps_concurrent_permissions_forms_and_url_completion_responsive() {
        let released = std::sync::Arc::new(tokio::sync::Notify::new());
        let agent_released = released.clone();
        let agent = Agent.v2().on_receive_request(async |request: v2::InitializeRequest, responder: agent_client_protocol::Responder<v2::InitializeResponse>, _: V2ConnectionTo<Client>| {
            responder.respond(v2::InitializeResponse::new(request.protocol_version, v2::Implementation::new("scripted", "1")).capabilities(v2::AgentCapabilities::new().session(v2::SessionCapabilities::new())))
        }, agent_client_protocol::on_receive_request!()).on_receive_request(
            async move |request: v2::PromptRequest, responder: agent_client_protocol::Responder<v2::PromptResponse>, cx: V2ConnectionTo<Client>| {
                let peer = cx.clone();
                let released = agent_released.clone();
                cx.spawn(async move {
                    let permission = |title| v2::RequestPermissionRequest::new(request.session_id.clone(), title, vec![v2::PermissionOption::new("allow", "Allow once", v2::PermissionOptionKind::AllowOnce)]);
                    let (first, second) = tokio::join!(peer.send_request(permission("first")).block_task(), peer.send_request(permission("second")).block_task());
                    assert!(matches!(first?.outcome, v2::RequestPermissionOutcome::Cancelled));
                    assert!(matches!(second?.outcome, v2::RequestPermissionOutcome::Selected(_)));
                    let form: v2::CreateElicitationRequest = serde_json::from_value(serde_json::json!({"mode":"form","sessionId":"session","message":"Approve?","requestedSchema":{"properties":{"approved":{"type":"boolean"}},"required":["approved"]}})).unwrap();
                    let result = peer.send_request(form).block_task().await?;
                    assert_eq!(serde_json::to_value(result).unwrap()["content"]["approved"], false);
                    let url = v2::CreateElicitationRequest::new(v2::ElicitationUrlMode::new(v2::ElicitationSessionScope::new(request.session_id.clone()), "flow", "https://example.com"), "Authenticate");
                    assert!(matches!(peer.send_request(url).block_task().await?.action, v2::ElicitationAction::Accept(_)));
                    peer.send_notification(v2::CompleteElicitationNotification::new("flow"))?;
                    let prepared = peer.prepare_request(permission("withdrawn"));
                    let cancellation = prepared.cancellation_handle();
                    let pending = prepared.block_task();
                    released.notified().await;
                    cancellation.cancel()?;
                    assert!(pending.await.is_err());
                    responder.respond(v2::PromptResponse::new(v2::MessageId::new("ack")))
                })
            }, agent_client_protocol::on_receive_request!());
        let (updates, _) = mpsc::unbounded_channel();
        let (interactions, mut pending) = mpsc::unbounded_channel();
        let (completions, mut done) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            interaction_client(updates, interactions, completions, agent, async move |cx| {
                cx.send_request(v2::InitializeRequest::new(
                    ProtocolVersion::V2,
                    v2::Implementation::new("tui-test", "1"),
                ))
                .block_task()
                .await?;
                let ack = cx
                    .send_request(v2::PromptRequest::new(
                        v2::SessionId::new("session"),
                        vec!["go".into()],
                    ))
                    .block_task();
                let first = pending.recv().await.unwrap();
                let second = pending.recv().await.unwrap();
                let Interaction::Permission(first_request, first_tx) = first else {
                    panic!("permission expected")
                };
                let Interaction::Permission(second_request, second_tx) = second else {
                    panic!("permission expected")
                };
                assert_eq!(first_request.title, "first");
                assert_eq!(second_request.title, "second");
                second_tx
                    .send(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Selected(v2::SelectedPermissionOutcome::new(
                            "allow",
                        )),
                    ))
                    .unwrap();
                first_tx
                    .send(v2::RequestPermissionResponse::new(
                        v2::RequestPermissionOutcome::Cancelled,
                    ))
                    .unwrap();
                let Interaction::Elicitation(request, tx, mut draft) =
                    pending.recv().await.unwrap()
                else {
                    panic!("form expected")
                };
                draft.save(&request, "not a boolean");
                assert!(draft.response(&request).is_err());
                draft.save(&request, "false");
                tx.send(draft.response(&request).unwrap()).unwrap();
                let Interaction::Elicitation(_, tx, _) = pending.recv().await.unwrap() else {
                    panic!("URL expected")
                };
                tx.send(v2::CreateElicitationResponse::new(
                    v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new()),
                ))
                .unwrap();
                assert_eq!(done.recv().await.unwrap().to_string(), "flow");
                let Interaction::Permission(_, mut tx) = pending.recv().await.unwrap() else {
                    panic!("permission expected")
                };
                released.notify_one();
                tx.closed().await;
                assert!(tx.is_closed());
                ack.await?;
                Ok(())
            }),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[test]
    fn required_form_fields_are_validated_before_the_agent_receives_a_response() {
        let mut schema = v2::ElicitationSchema::new();
        schema.properties.insert(
            "approved".into(),
            serde_json::from_value(serde_json::json!({"type":"boolean"})).unwrap(),
        );
        schema.required = Some(vec!["approved".into()]);
        let request = v2::CreateElicitationRequest::new(
            v2::ElicitationFormMode::new(v2::ElicitationSessionScope::new("hive-test"), schema),
            "Confirm",
        );
        let mut draft = FormDraft::default();
        assert!(draft.response(&request).is_err());
        draft.save(&request, "maybe");
        assert!(draft.response(&request).is_err());
        draft.save(&request, "true");
        let response = draft.response(&request).unwrap();
        let v2::ElicitationAction::Accept(answer) = response.action else {
            panic!("form not accepted");
        };
        assert_eq!(
            answer.content.unwrap()["approved"],
            v2::ElicitationContentValue::Boolean(true)
        );
    }

    #[test]
    fn observers_never_receive_relinquish_credentials_in_resume_metadata() {
        let mut owner = v2::Meta::new();
        owner.insert(
            "hive".into(),
            serde_json::json!({"token":"private", "generation": 3}),
        );
        assert_eq!(
            hive_meta("observe", Some(&owner))["hive"],
            serde_json::json!({"mode":"observe"})
        );
        assert_eq!(
            hive_meta("relinquish", Some(&owner))["hive"]["generation"],
            3
        );
    }
}
