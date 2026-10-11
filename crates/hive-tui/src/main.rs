//! Full-screen ACPv2 client; the daemon remains the sole session authority.

mod state;
mod theme;
mod ui;

use std::{collections::BTreeMap, time::Duration};

use agent_client_protocol::{
    Agent, Client, Error, V2ConnectionTo,
    schema::{ProtocolVersion, v2},
};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use hive_protocol::HttpClient;
use tokio::sync::{mpsc, oneshot};

use ui::{App, Page};

#[derive(Parser)]
struct Options {
    #[arg(long, default_value = "127.0.0.1:8765")]
    address: String,
    #[arg(long, default_value = "ws", value_parser = ["ws", "http"])]
    transport: String,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    session: Option<String>,
}

enum Interaction {
    Permission(
        v2::RequestPermissionRequest,
        oneshot::Sender<v2::RequestPermissionResponse>,
    ),
    Elicitation(
        v2::CreateElicitationRequest,
        oneshot::Sender<v2::CreateElicitationResponse>,
    ),
}

struct TerminalGuard;
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

async fn resume(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
    id: String,
    cwd: std::path::PathBuf,
    mode: &str,
) -> agent_client_protocol::Result<()> {
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
            app.active = Some(id);
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
    app.owner = response.meta;
    if app.skip_replay {
        conversation.push_notice("[Earlier history unavailable]".into());
    }
    app.skip_replay = false;
    app.active = Some(id);
    app.page = Page::Chat;
    Ok(())
}

async fn refresh(cx: &V2ConnectionTo<Agent>, app: &mut App) -> agent_client_protocol::Result<()> {
    let mut request = v2::ListSessionsRequest::new();
    if !app.all {
        request = request.cwd(app.cwd.clone());
    }
    app.sessions = cx.send_request(request).block_task().await?.sessions;
    app.selected = app.selected.min(app.sessions.len().saturating_sub(1));
    Ok(())
}

fn show_interaction(app: &mut App, interaction: &Interaction) {
    app.dialog = Some(match interaction {
        Interaction::Permission(req, _) => format!(
            "{}\n{}\n{}\nEnter an option number; Esc declines",
            req.title,
            req.description.as_deref().unwrap_or(""),
            req.options
                .iter()
                .enumerate()
                .map(|(i, o)| format!("{}: {}", i + 1, o.name))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        Interaction::Elicitation(req, _) => match &req.mode {
            v2::ElicitationMode::Form(form) => format!(
                "{}\nFields: {}. Enter key=value pairs separated by commas; Esc declines",
                req.message,
                form.requested_schema
                    .properties
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            v2::ElicitationMode::Url(url) => format!(
                "{}\nOpen {} and press Enter when finished; Esc declines",
                req.message, url.url
            ),
            _ => "Unsupported agent input (Esc to decline)".into(),
        },
    });
    app.input.clear();
}

fn form_response(
    req: &v2::CreateElicitationRequest,
    input: &str,
) -> Result<v2::CreateElicitationResponse, String> {
    match &req.mode {
        v2::ElicitationMode::Url(_) => Ok(v2::CreateElicitationResponse::new(
            v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new()),
        )),
        v2::ElicitationMode::Form(form) => {
            let mut values = BTreeMap::new();
            for item in input.split(',').filter(|s| !s.trim().is_empty()) {
                let (key, raw) = item
                    .split_once('=')
                    .ok_or("Use key=value, separated by commas")?;
                let key = key.trim();
                let schema = form
                    .requested_schema
                    .properties
                    .get(key)
                    .ok_or_else(|| format!("Unknown field: {key}"))?;
                let value = serde_json::to_value(schema).map_err(|e| e.to_string())?;
                let parsed = match value["type"].as_str() {
                    Some("integer") => v2::ElicitationContentValue::Integer(
                        raw.trim()
                            .parse()
                            .map_err(|_| format!("{key} needs an integer"))?,
                    ),
                    Some("number") => v2::ElicitationContentValue::Number(
                        raw.trim()
                            .parse()
                            .map_err(|_| format!("{key} needs a number"))?,
                    ),
                    Some("boolean") => v2::ElicitationContentValue::Boolean(
                        raw.trim()
                            .parse()
                            .map_err(|_| format!("{key} needs true or false"))?,
                    ),
                    Some("array") => v2::ElicitationContentValue::StringArray(
                        raw.trim().split('|').map(str::to_owned).collect(),
                    ),
                    _ => v2::ElicitationContentValue::String(raw.trim().into()),
                };
                values.insert(key.to_string(), parsed);
            }
            for key in form
                .requested_schema
                .required
                .as_deref()
                .unwrap_or_default()
            {
                if !values.contains_key(key) {
                    return Err(format!("Required field: {key}"));
                }
            }
            Ok(v2::CreateElicitationResponse::new(
                v2::ElicitationAction::Accept(v2::ElicitationAcceptAction::new().content(values)),
            ))
        }
        _ => Err("Unsupported elicitation mode".into()),
    }
}

fn send_prompt(
    cx: &V2ConnectionTo<Agent>,
    app: &App,
    acks: mpsc::UnboundedSender<Result<(), String>>,
) {
    let (Some(id), Some((key, content)), Some(owner)) = (&app.active, &app.pending, &app.owner)
    else {
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
                v2::PromptRequest::new(v2::SessionId::new(id), vec![content.into()]).meta(meta),
            )
            .block_task()
            .await;
        let _ = acks.send(result.map(|_| ()).map_err(|e| e.to_string()));
    });
}

async fn handle_key(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
    key: event::KeyEvent,
    interaction: &mut Option<Interaction>,
    acks: mpsc::UnboundedSender<Result<(), String>>,
) -> bool {
    if key.kind != KeyEventKind::Press {
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
            Interaction::Elicitation(req, tx) => {
                let mut tx = Some(tx);
                match key.code {
                    KeyCode::Esc => {
                        let _ = tx.take().unwrap().send(v2::CreateElicitationResponse::new(
                            v2::ElicitationAction::Decline,
                        ));
                        keep = false;
                    }
                    KeyCode::Enter => match form_response(&req, &app.input) {
                        Ok(response) => {
                            let _ = tx.take().unwrap().send(response);
                            keep = false;
                        }
                        Err(message) => app.status = message,
                    },
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(ch) => app.input.push(ch),
                    _ => {}
                }
                if keep {
                    *interaction = Some(Interaction::Elicitation(req, tx.unwrap()));
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
        (KeyCode::Esc, _) | (KeyCode::Char('q'), true) => return true,
        (KeyCode::Tab, _) => {
            app.page = Page::Sessions;
            let _ = refresh(cx, app).await;
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
                    .find(|s| s.session_id.to_string() == id)
                    .map_or_else(|| app.cwd.clone(), |s| s.cwd.0.clone());
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
                    app.status = format!("Could not attach: {error}");
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
                    .find(|s| s.session_id.to_string() == id)
                    .map_or_else(|| app.cwd.clone(), |s| s.cwd.0.clone());
                if let Err(error) = resume(cx, app, id, cwd, mode).await {
                    app.status = format!("Control change failed: {error}");
                }
            }
        }
        (KeyCode::Up, _) => app.selected = app.selected.saturating_sub(1),
        (KeyCode::Down, _) => {
            app.selected = app.selected.saturating_add(1).min(
                match app.page {
                    Page::Sessions => app.sessions.len(),
                    Page::Profiles => app.profiles.len(),
                    Page::Chat => 0,
                }
                .saturating_sub(1),
            )
        }
        (KeyCode::Char('a'), _) if app.page == Page::Sessions => {
            app.all = !app.all;
            let _ = refresh(cx, app).await;
        }
        (KeyCode::Char('o'), _) if app.page == Page::Sessions => {
            if let Some(info) = app.sessions.get(app.selected).cloned()
                && let Err(error) =
                    resume(cx, app, info.session_id.to_string(), info.cwd.0, "observe").await
            {
                app.status = error.to_string();
            }
        }
        (KeyCode::Enter, _) => match app.page {
            Page::Sessions => {
                if let Some(info) = app.sessions.get(app.selected).cloned()
                    && let Err(error) =
                        resume(cx, app, info.session_id.to_string(), info.cwd.0, "pilot").await
                {
                    app.status = error.to_string();
                }
            }
            Page::Profiles => {
                if let Some(profile) = app.profiles.get(app.selected) {
                    let mut meta = v2::Meta::new();
                    meta.insert("hive".into(), serde_json::json!({"agent": profile}));
                    match cx
                        .send_request(v2::NewSessionRequest::new(app.cwd.clone()).meta(meta))
                        .block_task()
                        .await
                    {
                        Ok(created) => {
                            let id = created.session_id.to_string();
                            let state = app.conversations.entry(id.clone()).or_default();
                            state.generation = 1;
                            state.pilot_present = true;
                            app.owner = created.meta;
                            app.active = Some(id);
                            app.page = Page::Chat;
                            let _ = refresh(cx, app).await;
                        }
                        Err(error) => app.status = format!("Could not start session: {error}"),
                    }
                }
            }
            Page::Chat => {
                if !app.input.trim().is_empty()
                    && app.pilot()
                    && !app.conversation().is_some_and(|c| c.running)
                    && app.pending.is_none()
                {
                    let content = std::mem::take(&mut app.input);
                    let key = uuid::Uuid::new_v4().to_string();
                    let id = app.active.clone().unwrap_or_default();
                    app.conversations
                        .entry(id.clone())
                        .or_default()
                        .push_user(content.clone());
                    app.pending = Some((key.clone(), content.clone()));
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = Options::parse();
    let mut app = App {
        cwd: std::env::current_dir()?,
        ..App::default()
    };
    let providers: serde_json::Value =
        reqwest::get(format!("http://{}/hive/v1/providers", options.address))
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
        let result = Client.v2()
            .on_receive_notification(async move |update: v2::UpdateSessionNotification, _: V2ConnectionTo<Agent>| {
                updates_tx.send(update).map_err(Error::into_internal_error)
            }, agent_client_protocol::on_receive_notification!())
            .on_receive_request({
                let interactions = interaction_tx.clone();
                async move |request: v2::RequestPermissionRequest, responder: agent_client_protocol::Responder<v2::RequestPermissionResponse>, _: V2ConnectionTo<Agent>| {
                    let (tx, rx) = oneshot::channel();
                    interactions.send(Interaction::Permission(request, tx)).map_err(Error::into_internal_error)?;
                    responder.respond(rx.await.unwrap_or_else(|_| v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Cancelled)))
                }
            }, agent_client_protocol::on_receive_request!())
            .on_receive_request(async move |request: v2::CreateElicitationRequest, responder: agent_client_protocol::Responder<v2::CreateElicitationResponse>, _: V2ConnectionTo<Agent>| {
                let (tx, rx) = oneshot::channel();
                interaction_tx.send(Interaction::Elicitation(request, tx)).map_err(Error::into_internal_error)?;
                responder.respond(rx.await.unwrap_or_else(|_| v2::CreateElicitationResponse::new(v2::ElicitationAction::Cancel)))
            }, agent_client_protocol::on_receive_request!())
            .connect_with(HttpClient::new(url)?, async |cx| {
                cx.send_request(v2::InitializeRequest::new(ProtocolVersion::V2,
                    v2::Implementation::new("hive-tui", env!("CARGO_PKG_VERSION")))
                    .capabilities(v2::ClientCapabilities::new().elicitation(v2::ElicitationCapabilities::new()
                        .form(v2::ElicitationFormCapabilities::new()).url(v2::ElicitationUrlCapabilities::new()))))
                    .block_task().await?;
                app.connected = true;
                refresh(&cx, &mut app).await?;
                if app.page == Page::Profiles && app.active.is_none() {
                    app.selected = options.agent.as_ref().and_then(|agent| app.profiles.iter().position(|id| id == agent)).unwrap_or(0);
                }
                let initial = app.active.clone().or_else(|| options.session.clone());
                if let Some(id) = initial {
                    let cwd = app.sessions.iter().find(|s| s.session_id.to_string() == id).map_or_else(|| app.cwd.clone(), |s| s.cwd.0.clone());
                    let mode = if app.owner.as_ref().is_some_and(|m| m["hive"]["role"] == "observer") { "observe" } else { "pilot" };
                    if let Err(error) = resume(&cx, &mut app, id, cwd, mode).await { app.status = format!("Resume failed: {error}"); }
                }
                let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<Result<(), String>>();
                // The previous acknowledgement may have been lost; the original key is safe to retry.
                if app.pending.is_some() && app.pilot() { send_prompt(&cx, &app, ack_tx.clone()); }
                let mut interaction: Option<Interaction> = None;
                loop {
                    terminal.draw(|frame| app.show(frame)).map_err(Error::into_internal_error)?;
                    tokio::select! {
                        _ = cx.incoming_closed() => break,
                        Some(update) = updates.recv() => {
                            let id = update.session_id.to_string();
                            let current = app.active.as_ref() == Some(&id);
                            let state = app.conversations.entry(id).or_default();
                            if state.apply(update) && current && !app.pilot() {
                                app.status = "Observer — take control to interact".into();
                                interaction = None;
                                app.dialog = None;
                            }
                        }
                        Some(next) = interactions.recv() => {
                            if interaction.is_none() && app.pilot() { show_interaction(&mut app, &next); interaction = Some(next); }
                            // Dropping an undeliverable request cancels it at the daemon boundary.
                        }
                        Some(result) = ack_rx.recv() => {
                            match result { Ok(()) => app.pending = None, Err(error) => {
                                app.status = format!("Prompt acknowledgement uncertain: {error}; retry with the same key after reconnect");
                                if error.contains("stale") { app.owner = None; }
                            }}
                        }
                        Some(Event::Key(key)) = keys.recv() => {
                            if handle_key(&cx, &mut app, key, &mut interaction, ack_tx.clone()).await { quit = true; break; }
                        },
                    }
                }
                Ok(())
            }).await;
        app.connected = false;
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
        assert!(form_response(&request, "").is_err());
        assert!(form_response(&request, "approved=maybe").is_err());
        let response = form_response(&request, "approved=true").unwrap();
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
