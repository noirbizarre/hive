//! Full-screen ACPv2 client; the daemon remains the sole session authority.

mod activity;
pub mod capabilities;
mod clipboard;
mod draft;
mod editor;
mod external_editor;
pub mod images;
mod input;
mod osc5522;
mod picker;
mod program_status;
mod state;
mod theme;
mod transcript;
mod ui;

#[cfg(test)]
mod interaction_tests;

#[cfg(test)]
mod federation_tests;

#[cfg(all(test, unix))]
mod terminal_tests;

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
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use tokio::sync::{mpsc, oneshot};

use ui::{App, Page, SessionSnapshot};

#[derive(Args, Clone, Debug)]
pub struct Options {
    #[command(flatten)]
    pub terminal: capabilities::TerminalOptions,
    /// Explicitly use a directory instead of a profile's managed workspace policy.
    #[arg(long, conflicts_with_all = ["workspace", "workspace_id", "project", "base_ref", "branch", "revision", "session"])]
    pub cwd: Option<std::path::PathBuf>,
    #[arg(long, conflicts_with_all = ["workspace_id", "session"])]
    pub project: Option<String>,
    #[arg(long, conflicts_with_all = ["workspace_id", "session"])]
    pub workspace: Option<String>,
    #[arg(long, conflicts_with_all = ["session", "base_ref", "branch", "revision"])]
    pub workspace_id: Option<String>,
    #[arg(long, conflicts_with = "session")]
    pub base_ref: Option<String>,
    /// JJ revision/change expression for a new managed workspace.
    #[arg(long, conflicts_with_all = ["base_ref", "branch", "session"])]
    pub revision: Option<String>,
    #[arg(long, conflicts_with = "session")]
    pub branch: Option<String>,
    #[arg(long, default_value = "127.0.0.1:8765")]
    pub address: String,
    #[arg(long, default_value = "ws", value_parser = ["ws", "http", "wss", "https"])]
    pub transport: String,
    #[arg(long)]
    pub agent: Option<String>,
    /// Named execution profile for new sessions.
    #[arg(long, conflicts_with = "session")]
    pub profile: Option<String>,
    #[arg(long)]
    pub session: Option<String>,
    /// Execution owner NodeId; with --session, the session ID belongs to this owner.
    #[arg(long, value_parser = parse_owner)]
    pub owner: Option<hive_core::identity::NodeId>,
}

fn parse_owner(value: &str) -> Result<hive_core::identity::NodeId, String> {
    hive_core::identity::NodeId::try_from(value.to_owned()).map_err(|error| error.to_string())
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

struct TerminalGuard {
    active: bool,
    // Retry cleanup only for a mode we actually used; disabled/unavailable transports emit none.
    rich_mode_used: bool,
    images: images::KittyRenderer,
    program_status: program_status::Reporter,
}
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
    fn available(&self, app: &App) -> bool {
        if self.request_scoped() {
            return true;
        }
        if !app.pilot() {
            return false;
        }
        let session = match self {
            Self::Permission(request, _) => request.session_id.to_string(),
            Self::Elicitation(request, _, _) => match request.scope() {
                v2::ElicitationScope::Session(scope) => scope.session_id.to_string(),
                _ => return false,
            },
        };
        app.active.as_deref() == Some(session.as_str())
    }
    fn expired(&self) -> bool {
        match self {
            Self::Permission(_, tx) => tx.is_closed(),
            Self::Elicitation(_, tx, _) => tx.is_closed(),
        }
    }
}

fn reconcile_interactions(
    app: &mut App,
    interaction: &mut Option<Interaction>,
    queued: &mut VecDeque<Interaction>,
) {
    // Destination questions can precede the resume response; keep them after installing that session.
    queued.retain(|request| request.available(app));
    if !interaction
        .as_ref()
        .is_some_and(|request| request.available(app))
    {
        *interaction = None;
        if app.dialog.take().is_some() {
            app.input.clear();
            restore_draft(app);
        }
    }
}
impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        let mut guard = Self {
            active: false,
            rich_mode_used: false,
            images: images::KittyRenderer::default(),
            program_status: program_status::Reporter::default(),
        };
        guard.resume()?;
        Ok(guard)
    }
    fn resume(&mut self) -> std::io::Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode()?;
        if let Err(error) = execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            // An alternate-screen failure must not leave the user's shell in raw mode.
            let _ = disable_raw_mode();
            let _ = execute!(
                std::io::stdout(),
                DisableMouseCapture,
                DisableBracketedPaste,
                crossterm::style::Print(if self.rich_mode_used {
                    "\x1b[?5522l"
                } else {
                    ""
                }),
                LeaveAlternateScreen
            );
            return Err(error);
        }
        self.active = true;
        self.images.invalidate();
        Ok(())
    }

    fn draw<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<W>>,
        capabilities: &capabilities::TerminalCapabilities,
        render: impl FnOnce(&mut ratatui::Frame, &mut images::ImageFrame),
    ) -> std::io::Result<()> {
        self.images.resize(terminal.size()?);
        self.images.begin(terminal.backend_mut())?;
        let mut desired = images::ImageFrame::new(capabilities);
        let area = terminal.draw(|frame| render(frame, &mut desired))?.area;
        // draw() also autoresizes: a resize racing the initial size query can clear
        // resident image data, so trust the geometry actually used by the frame.
        self.images
            .resize(ratatui::layout::Size::new(area.width, area.height));
        self.images.finish(terminal.backend_mut(), desired)
    }

    fn clear<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<W>>,
    ) -> std::io::Result<()> {
        self.images.invalidate();
        self.images.begin(terminal.backend_mut())?;
        // Fullscreen resize resets the back buffer without Crossterm's cursor-position reader.
        // A cursor query here could buffer keystrokes outside our single input owner.
        let size = terminal.size()?;
        terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))
    }

    fn suspend(&mut self) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // Images belong to this alternate screen, never to the shell or editor.
        let images = self.images.cleanup(&mut std::io::stdout());
        let status = self.program_status.clear(&mut std::io::stdout());
        self.images.invalidate();
        // Attempt all restorations even if terminal output fails.
        let screen = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::style::Print(if self.rich_mode_used {
                "\x1b[?5522l"
            } else {
                ""
            }),
            crossterm::cursor::Show,
            LeaveAlternateScreen
        );
        let raw = disable_raw_mode();
        self.active = false;
        images.and(status).and(screen).and(raw)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Retry all cleanup even after a partially failed suspend or resume.
        let _ = self.images.cleanup(&mut std::io::stdout());
        let _ = self.program_status.clear(&mut std::io::stdout());
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::style::Print(if self.rich_mode_used {
                "\x1b[?5522l"
            } else {
                ""
            }),
            crossterm::cursor::Show,
            LeaveAlternateScreen
        );
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
        Err(error)
            if format!("{error:?}").contains("replay cursor unavailable")
                || format!("{error:?}").contains("hive::transcript::unsupported")
                || format!("{error:?}").contains("hive::transcript::history_missing") =>
        {
            app.status = resume_failure(&error);
            if app.active.as_deref() != Some(&id) {
                app.owner = None;
                app.select_session(id);
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    apply_resume(app, id, response)
}

fn apply_resume(
    app: &mut App,
    id: String,
    response: v2::ResumeSessionResponse,
) -> agent_client_protocol::Result<()> {
    let hive = response
        .meta
        .as_ref()
        .and_then(|m| m.get("hive"))
        .ok_or_else(Error::internal_error)?;
    if let Ok(reference) = serde_json::from_value(hive["session"].clone()) {
        app.references.insert(id.clone(), reference);
    }
    let conversation = app.conversations.entry(id.clone()).or_default();
    if let Some(through) = hive["snapshotThroughSeq"].as_u64() {
        // Notifications are drained after this request; marked snapshot entries bypass cursor checks.
        conversation.begin_transcript(through);
    }
    conversation.generation = hive["generation"].as_u64().unwrap_or(0);
    conversation.running = hive["running"].as_bool().unwrap_or(false);
    conversation.pilot_present = hive["pilot"].as_bool().unwrap_or(false);
    conversation.config_snapshot(
        response.config_options,
        hive["configSeq"].as_u64().unwrap_or(0),
    );
    conversation.snapshot(&hive["activitySnapshot"]);
    conversation.plans_snapshot(&hive["planSnapshot"]);
    conversation.usage_snapshot(
        &hive["usageSnapshot"],
        hive["usageSeq"].as_u64().unwrap_or(0),
    );
    conversation.gap = false;
    conversation.transcript_snapshot(
        &hive["transcript"],
        hive["transcriptSeq"].as_u64().unwrap_or(0),
    );
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

pub struct SessionOperation {
    label: &'static str,
    result: oneshot::Receiver<SessionOperationResult>,
}

struct SessionOperationResult {
    relinquished: bool,
    creation_uncertain: Option<bool>,
    outcome: SessionOutcome,
}

enum SessionOutcome {
    Resume {
        id: String,
        response: agent_client_protocol::Result<v2::ResumeSessionResponse>,
    },
    Create(Result<v2::NewSessionResponse, String>),
}

fn previous_pilot(app: &App, target: Option<&str>) -> Option<v2::ResumeSessionRequest> {
    let previous = app.active.as_ref()?;
    if !app.pilot() || target == Some(previous.as_str()) {
        return None;
    }
    let cwd = app
        .sessions
        .iter()
        .find(|s| &s.session_id == previous)
        .map_or_else(|| app.cwd.clone(), |s| s.cwd.clone());
    Some(
        v2::ResumeSessionRequest::new(v2::SessionId::new(previous.clone()), cwd)
            .meta(hive_meta("relinquish", app.owner.as_ref())),
    )
}

fn start_resume(
    cx: &V2ConnectionTo<Agent>,
    app: &mut App,
    id: String,
    cwd: std::path::PathBuf,
    mode: &str,
    acknowledge: bool,
) {
    if app.session_operation.is_some() {
        return;
    }
    let previous = previous_pilot(app, Some(&id));
    let seq = app.conversations.get(&id).map_or(0, |c| c.seq);
    let replay = if seq == 0 {
        v2::ReplayFrom::Start(v2::ReplayFromStart::new())
    } else {
        v2::ReplayFrom::Other(v2::OtherReplayFrom::new(
            "_hive_seq",
            BTreeMap::from([("seq".into(), serde_json::json!(seq + 1))]),
        ))
    };
    let mut meta = hive_meta(mode, app.owner.as_ref());
    if acknowledge {
        meta.insert(
            "hive".into(),
            serde_json::json!({"recoveryAction":"acknowledge"}),
        );
    }
    let mut request =
        v2::ResumeSessionRequest::new(v2::SessionId::new(id.clone()), cwd.clone()).meta(meta);
    if !app.skip_replay && !acknowledge {
        request = request.replay_from(replay);
    }
    let (sent, result) = oneshot::channel();
    let label = if acknowledge {
        "Acknowledging recovery"
    } else {
        match mode {
            "pilot" => "Taking control",
            "relinquish" => "Releasing control",
            _ => "Opening conversation",
        }
    };
    app.session_operation = Some(SessionOperation { label, result });
    app.status = format!("{label}…");
    let peer = cx.clone();
    if let Err(error) = cx.spawn(async move {
        let mut relinquished = false;
        let response = async {
            if let Some(previous) = previous {
                peer.send_request(previous).block_task().await?;
                relinquished = true;
            }
            peer.send_request(request).block_task().await
        }
        .await;
        let _ = sent.send(SessionOperationResult {
            relinquished,
            creation_uncertain: None,
            outcome: SessionOutcome::Resume { id, response },
        });
        Ok(())
    }) {
        app.session_operation = None;
        app.skip_replay = false;
        app.status = error.to_string();
    }
}

fn start_creation(cx: &V2ConnectionTo<Agent>, app: &mut App, profile: String) {
    if app.session_operation.is_some() {
        return;
    }
    if app.creation_uncertain {
        app.status =
            "Creation outcome uncertain; inspect the owner catalog/workspaces, then press u to acknowledge before another launch"
                .into();
        return;
    }
    let execution_profile = app.execution_profile.clone().or_else(|| {
        app.launch_owner
            .as_ref()
            .and_then(|_| app.providers["defaultProfile"].as_str().map(str::to_owned))
    });
    let execution = execution_profile.as_ref().and_then(|id| {
        app.execution_profiles
            .iter()
            .find(|profile| &profile.id == id)
    });
    let managed_profile = execution.is_some_and(|profile| !profile.workspaces.is_empty());
    let selected_agent = if profile.is_empty() {
        execution.map_or_else(
            || app.providers["defaultAgent"].as_str(),
            |execution| {
                execution
                    .default_agent
                    .as_deref()
                    .or_else(|| {
                        app.providers["defaultAgent"].as_str().filter(|default| {
                            execution
                                .agents
                                .iter()
                                .any(|agent| agent.as_str() == *default)
                        })
                    })
                    .or_else(|| (execution.agents.len() == 1).then(|| execution.agents[0].as_str()))
            },
        )
    } else {
        Some(profile.as_str())
    };
    // Automatic owner provisioning can come from the agent rather than a named execution profile.
    let managed_agent = selected_agent.is_some_and(|id| {
        app.providers["agents"].as_array().is_some_and(|agents| {
            agents
                .iter()
                .any(|agent| agent["id"] == id && agent["launch"]["workspace"] == "configured")
        })
    });
    if app.launch_owner.is_some()
        && !app.explicit_cwd
        && !managed_profile
        && !managed_agent
        && app.workspace_launch.is_none()
    {
        app.status =
            "Remote launch needs --cwd with an absolute owner path, or a managed execution profile"
                .into();
        return;
    }
    if app.launch_owner.is_some()
        && app.explicit_cwd
        && !hive_protocol::remote::owner_path_is_absolute(&app.cwd)
    {
        app.status = "Remote working directory must be absolute on the owner".into();
        return;
    }
    let previous = previous_pilot(app, None);
    app.creation_target = Some((profile.clone(), app.cwd.clone()));
    let mut meta = v2::Meta::new();
    let mut hive = serde_json::json!({});
    if let Some(owner) = &app.launch_owner {
        hive["peer"] = owner.as_str().into();
    }
    if !profile.is_empty() {
        hive["agent"] = profile.into();
    }
    if let Some(profile) = &execution_profile {
        hive["profile"] = profile.clone().into();
    }
    meta.insert("hive".into(), hive);
    if let Some(launch) = &app.workspace_launch {
        meta.get_mut("hive").expect("Hive metadata exists")["workspace"] =
            serde_json::to_value(launch).expect("launch serializes");
    }
    // Omission delegates to daemon defaults; an empty Hive selector is deliberately invalid.
    if meta["hive"] == serde_json::json!({}) {
        meta.remove("hive");
    }
    let request = v2::NewSessionRequest::new(app.cwd.clone()).meta(meta);
    let (sent, result) = oneshot::channel();
    app.session_operation = Some(SessionOperation {
        label: "Creating session",
        result,
    });
    // A dropped creation response is not proof that the owner never launched an agent.
    app.creation_uncertain = true;
    app.page = Page::Chat;
    app.status = "Creating session…".into();
    let peer = cx.clone();
    if let Err(error) = cx.spawn(async move {
        let mut relinquished = false;
        let mut uncertain = false;
        let response = async {
            if let Some(previous) = previous {
                peer.send_request(previous)
                    .block_task()
                    .await
                    .map_err(|error| resume_failure(&error))?;
                relinquished = true;
            }
            uncertain = true;
            peer.send_request(request)
                .block_task()
                .await
                .map_err(|error| {
                    let diagnostic = serde_json::to_value(&error).unwrap_or_default();
                    let code = diagnostic["data"]["code"].as_str();
                    // Only an explicit pre-create rejection proves another launch is safe.
                    if code.is_some_and(|code| {
                        code.starts_with("hive::remote::")
                            && !matches!(
                                code,
                                "hive::remote::launch_uncertain"
                                    | "hive::remote::provisioning_uncertain"
                            )
                    }) || diagnostic["code"] == -32602
                    {
                        uncertain = false;
                    }
                    format!(
                        "{error}{}",
                        diagnostic["data"]["help"]
                            .as_str()
                            .map_or(String::new(), |help| format!(" — {help}"))
                    )
                })
        }
        .await;
        let _ = sent.send(SessionOperationResult {
            relinquished,
            creation_uncertain: Some(uncertain),
            outcome: SessionOutcome::Create(response),
        });
        Ok(())
    }) {
        app.session_operation = None;
        app.status = error.to_string();
    }
}

async fn operation_completion(
    operation: &mut Option<SessionOperation>,
) -> Result<SessionOperationResult, oneshot::error::RecvError> {
    match operation {
        Some(operation) => (&mut operation.result).await,
        None => std::future::pending().await,
    }
}

fn apply_session_result(
    app: &mut App,
    result: SessionOperationResult,
    label: &str,
) -> Option<Result<v2::NewSessionResponse, String>> {
    if let Some(uncertain) = result.creation_uncertain {
        app.creation_uncertain = uncertain;
    }
    if result.relinquished {
        app.owner = None;
    }
    match result.outcome {
        SessionOutcome::Resume { id, response } => {
            let mut acknowledged = false;
            let acknowledged_id = id.clone();
            let previous_gap = app
                .conversations
                .get(&id)
                .is_some_and(|conversation| conversation.gap);
            match response {
                Ok(response) => match apply_resume(app, id, response) {
                    Ok(()) => {
                        acknowledged = label == "Acknowledging recovery";
                        app.status = format!("{label} complete");
                    }
                    Err(error) => {
                        app.skip_replay = false;
                        app.status = resume_failure(&error);
                    }
                },
                Err(error) => {
                    app.skip_replay = false;
                    // Ctrl-R must target the failed attachment, not the conversation we just left.
                    if history_unavailable(&error) && app.active.as_deref() != Some(&id) {
                        app.owner = None;
                        app.select_session(id);
                    }
                    app.status = resume_failure(&error);
                }
            }
            if acknowledged {
                app.submitting.remove(&acknowledged_id);
                if let Some((key, _)) = app.pending.remove(&acknowledged_id) {
                    app.conversations
                        .entry(acknowledged_id.clone())
                        .or_default()
                        .push_notice(format!(
                            "[Recovery acknowledged for {key}; outcome remains unresolved]"
                        ));
                }
                // Acknowledgement has no authority to certify or skip missing history.
                if previous_gap {
                    app.conversations.entry(acknowledged_id).or_default().gap = true;
                }
            }
            None
        }
        SessionOutcome::Create(result) => Some(result),
    }
}

fn apply_created(app: &mut App, result: Result<v2::NewSessionResponse, String>) {
    match result {
        Ok(created) => {
            app.creation_uncertain = false;
            let id = created.session_id.to_string();
            if let Some(reference) = created
                .meta
                .as_ref()
                .and_then(|meta| meta.get("hive"))
                .and_then(|hive| serde_json::from_value(hive["session"].clone()).ok())
            {
                app.references.insert(id.clone(), reference);
            }
            let state = app.conversations.entry(id.clone()).or_default();
            state.generation = 1;
            state.pilot_present = true;
            let seq = created
                .meta
                .as_ref()
                .and_then(|meta| meta.get("hive"))
                .and_then(|hive| hive["configSeq"].as_u64())
                .unwrap_or(0);
            state.config_snapshot(created.config_options, seq);
            if let Some(hive) = created.meta.as_ref().and_then(|meta| meta.get("hive")) {
                state.plans_snapshot(&hive["planSnapshot"]);
                state.transcript_snapshot(
                    &hive["transcript"],
                    hive["transcriptSeq"].as_u64().unwrap_or(0),
                );
            }
            app.owner = created.meta;
            let cwd = app
                .owner
                .as_ref()
                .and_then(|meta| meta.get("hive"))
                .and_then(|hive| hive["workspace"]["effectiveCwd"].as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    app.creation_target
                        .as_ref()
                        .map_or_else(|| app.cwd.clone(), |(_, cwd)| cwd.clone())
                });
            // Catalog refresh may already have discovered this durable pending creation.
            app.sessions.retain(|session| session.session_id != id);
            app.sessions.push(ui::SessionSummary {
                session_id: id.clone(),
                cwd,
                agent: app.creation_target.take().map_or_else(
                    || {
                        app.profiles
                            .get(app.profile_selected)
                            .cloned()
                            .unwrap_or_default()
                    },
                    |(agent, _)| agent,
                ),
                title: None,
                lifecycle: "active".into(),
                recovery: None,
                recovery_reason: None,
                activity: "idle".into(),
                last_activity_at: None,
                control_owner: None,
                generation: 1,
            });
            app.refresh_requested = true;
            app.select_session(id);
            app.status = "Session created".into();
        }
        Err(error) => {
            app.status = format!("Could not start session: {error}");
            app.page = Page::Profiles;
        }
    }
}

fn apply_update(app: &mut App, update: v2::UpdateSessionNotification) -> bool {
    let id = update.session_id.to_string();
    let current = app.active.as_ref() == Some(&id);
    let recovery = match &update.update {
        v2::SessionUpdate::Other(other) if other.session_update == "_hive_control" => other
            .fields
            .get("recoveryRequired")
            .and_then(serde_json::Value::as_bool),
        _ => None,
    };
    let state = app.conversations.entry(id.clone()).or_default();
    let applied = state.apply(update);
    if applied
        && current
        && let Some(required) = recovery
        && let Some(owner) = &mut app.owner
    {
        owner.entry("hive").or_insert_with(|| serde_json::json!({}))["recoveryRequired"] =
            required.into();
    }
    if app.stopping.as_ref() == Some(&id)
        && (!state.running || state.activity["session"] == "ended")
    {
        app.stopping = None;
        app.status = "Stopped".into();
    }
    if state.gap && current {
        app.status = "Activity gap detected; reconnect to reconcile history".into();
        return false;
    }
    if applied && current && !app.pilot() {
        app.status = "Observer — take control to interact".into();
    }
    true
}

async fn refresh(peer: &hive_protocol::remote::Peer, app: &mut App) -> Result<(), String> {
    let catalog = load_catalog(peer, app.launch_owner.clone()).await?;
    let inventory_error = catalog.providers.as_ref().err().cloned();
    apply_catalog(app, catalog);
    inventory_error.map_or(Ok(()), Err)
}

#[derive(Debug, thiserror::Error)]
enum InitialSelectionError {
    #[error(
        "Execution profile '{0}' is unavailable on the selected owner; refresh its inventory and choose a configured profile"
    )]
    Profile(String),
    #[error(
        "Agent '{0}' is unavailable in the selected owner/profile; refresh its inventory and choose a member agent"
    )]
    Agent(String),
}

fn apply_initial_selection(
    app: &mut App,
    profile: Option<&str>,
    agent: Option<&str>,
    attaching: bool,
) -> Result<(), InitialSelectionError> {
    if let Some(profile) = profile {
        if !app
            .execution_profiles
            .iter()
            .any(|entry| entry.id == profile)
        {
            return Err(InitialSelectionError::Profile(profile.into()));
        }
        select_execution_profile(app, Some(profile.into()));
        app.page = Page::Profiles;
    }
    if let Some(agent) = agent {
        app.profile_selected = app
            .profiles
            .iter()
            .position(|id| id == agent)
            .ok_or_else(|| InitialSelectionError::Agent(agent.into()))?;
        if !attaching {
            app.page = Page::Profiles;
        }
    }
    Ok(())
}

fn attachment_target(
    app: &App,
    session: Option<&str>,
    owner: Option<&hive_core::identity::NodeId>,
    reconnecting: bool,
) -> Option<(String, Option<hive_protocol::remote::SessionReference>)> {
    if reconnecting && let Some(id) = &app.active {
        return Some((id.clone(), app.references.get(id).cloned()));
    }
    let id = session.map(str::to_owned).or_else(|| app.active.clone())?;
    // An explicit owner scopes a native ID; a colliding gateway catalog key cannot override it.
    let reference = owner
        .map(|owner| hive_protocol::remote::SessionReference {
            owner: owner.clone(),
            id: id.clone(),
        })
        .or_else(|| app.references.get(&id).cloned());
    Some((id, reference))
}

async fn restore_attachment(
    peer: &hive_protocol::remote::Peer,
    app: &mut App,
    session: Option<&str>,
    owner: Option<&hive_core::identity::NodeId>,
    reconnecting: bool,
) {
    let Some((id, reference)) = attachment_target(app, session, owner, reconnecting) else {
        return;
    };
    let location = if let Some(reference) = reference {
        match peer.resolve(reference.clone()).await {
            Ok(location) => {
                // CLI native IDs are not route keys unless an authoritative mapping proves equality.
                if app.references.get(&id) == Some(&reference) {
                    app.relocate(&id, &location.session_id);
                }
                app.references
                    .insert(location.session_id.clone(), location.session);
                Some((location.session_id, location.cwd.0))
            }
            Err(error) => {
                app.owner = None;
                app.status = resume_failure(&error);
                None
            }
        }
    } else {
        let cwd = app
            .sessions
            .iter()
            .find(|session| session.session_id == id)
            .map_or_else(|| app.cwd.clone(), |session| session.cwd.clone());
        Some((id, cwd))
    };
    if let Some((id, cwd)) = location {
        let mode = if reconnecting { "observe" } else { "pilot" };
        if let Err(error) = resume(peer.connection(), app, id, cwd, mode).await {
            app.status = resume_failure(&error);
        }
    }
    // An owner attachment failure leaves the healthy gateway usable for navigation and other sessions.
}

struct Catalog {
    snapshot: SessionSnapshot,
    providers: Result<serde_json::Value, String>,
    owners: Vec<serde_json::Value>,
    references: BTreeMap<String, hive_protocol::remote::SessionReference>,
    availability: BTreeMap<String, String>,
    owner: Option<hive_core::identity::NodeId>,
    partial: bool,
}

async fn load_catalog(
    peer: &hive_protocol::remote::Peer,
    owner: Option<hive_core::identity::NodeId>,
) -> Result<Catalog, String> {
    let mut sessions = Vec::new();
    let mut references = BTreeMap::new();
    let mut availability = BTreeMap::new();
    let mut partial = false;
    let mut request = v2::ListSessionsRequest::new();
    let mut cursors = std::collections::HashSet::new();
    loop {
        let page = peer
            .connection()
            .send_request(request)
            .block_task()
            .await
            .map_err(|error| error.to_string())?;
        partial |= page
            .meta
            .as_ref()
            .and_then(|meta| meta.get("hive"))
            .is_some_and(|hive| hive["partial"] == true);
        for entry in page.sessions {
            let entry = serde_json::to_value(entry).map_err(|error| error.to_string())?;
            let mut summary = entry["_meta"]["hive"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            // Nested metadata can contain an owner's native ID; operations need the gateway alias.
            summary.insert("sessionId".into(), entry["sessionId"].clone());
            summary.insert("cwd".into(), entry["cwd"].clone());
            if entry["title"].is_string() {
                summary.insert("title".into(), entry["title"].clone());
            }
            let id = entry["sessionId"].as_str().unwrap_or_default().to_owned();
            if let Ok(reference) = serde_json::from_value(entry["_meta"]["hive"]["session"].clone())
            {
                references.insert(id.clone(), reference);
            }
            if let Some(value) = entry["_meta"]["hive"]["availability"].as_str() {
                availability.insert(id, value.to_owned());
            }
            sessions.push(
                serde_json::from_value(serde_json::Value::Object(summary))
                    .map_err(|error| error.to_string())?,
            );
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        if !cursors.insert(cursor.clone()) {
            return Err("Session inventory repeated a cursor; refresh discovery".into());
        }
        request = v2::ListSessionsRequest::new().cursor(cursor);
    }
    let owners = if peer.has_peers() {
        peer.list_peers().await.map_err(|error| error.to_string())?["peers"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let providers = load_inventory(peer, owner.as_ref()).await;
    let snapshot = SessionSnapshot { sessions };
    Ok(Catalog {
        snapshot,
        providers,
        owners,
        references,
        availability,
        owner,
        partial,
    })
}

async fn load_inventory(
    peer: &hive_protocol::remote::Peer,
    owner: Option<&hive_core::identity::NodeId>,
) -> Result<serde_json::Value, String> {
    let mut providers = serde_json::json!({"agents": [], "profiles": []});
    let mut params = hive_protocol::remote::ListParams::default();
    let mut cursors = std::collections::HashSet::new();
    loop {
        let page = match owner {
            Some(owner) => peer.list_at(owner, params).await,
            None => peer.list(params).await,
        }
        .map_err(|error| error.to_string())?;
        providers["defaultAgent"] = serde_json::to_value(page.default_agent).unwrap_or_default();
        providers["agents"].as_array_mut().expect("array").extend(
            page.agents
                .into_iter()
                .map(|agent| serde_json::to_value(agent).expect("agent serializes")),
        );
        let Some(cursor) = page.next_cursor else {
            break;
        };
        if !cursors.insert(cursor.clone()) {
            return Err("Agent inventory repeated a cursor; refresh discovery".into());
        }
        params = hive_protocol::remote::ListParams {
            cursor: Some(cursor),
            limit: None,
        };
    }
    if peer.has_profiles() {
        let mut params = hive_protocol::remote::ListParams::default();
        let mut cursors = std::collections::HashSet::new();
        loop {
            let page = match owner {
                Some(owner) => peer.list_profiles_at(owner, params).await,
                None => peer.list_profiles(params).await,
            }
            .map_err(|error| error.to_string())?;
            providers["defaultProfile"] =
                serde_json::to_value(page.default_profile).unwrap_or_default();
            providers["profiles"].as_array_mut().expect("array").extend(
                page.profiles
                    .into_iter()
                    .map(|profile| serde_json::to_value(profile).expect("profile serializes")),
            );
            let Some(cursor) = page.next_cursor else {
                break;
            };
            if !cursors.insert(cursor.clone()) {
                return Err("Profile inventory repeated a cursor; refresh discovery".into());
            }
            params = hive_protocol::remote::ListParams {
                cursor: Some(cursor),
                limit: None,
            };
        }
    }
    Ok(providers)
}

fn apply_catalog(app: &mut App, catalog: Catalog) {
    if catalog.partial {
        app.status = "Session list incomplete: some configured owners are unavailable; cached sessions remain visible".into();
    }
    for (route, reference) in &catalog.references {
        if let Some(old) = app
            .references
            .iter()
            .find_map(|(id, known)| (known == reference && id != route).then(|| id.clone()))
        {
            app.relocate(&old, route);
        }
    }
    app.references.extend(catalog.references);
    app.availability = catalog.availability.into_iter().collect();
    app.owners = catalog.owners;
    let sessions = catalog
        .snapshot
        .sessions
        .into_iter()
        .filter(|s| {
            app.all
                || (s.cwd == app.cwd
                    && app.references.get(&s.session_id).is_none_or(|reference| {
                        app.launch_owner.as_ref().map_or_else(
                            || {
                                app.peer
                                    .as_ref()
                                    .is_none_or(|peer| peer.node == reference.owner.as_str())
                            },
                            |owner| owner == &reference.owner,
                        )
                    }))
                || app.active.as_deref() == Some(s.session_id.as_str())
                || app.recent.contains(&s.session_id)
        })
        .collect();
    app.update_sessions(sessions);
    if app.launch_owner == catalog.owner {
        match catalog.providers {
            Ok(providers) => update_profiles(app, &providers),
            Err(error) => app.status = format!("Agent inventory unavailable: {error}"),
        }
    }
}

fn update_profiles(app: &mut App, providers: &serde_json::Value) {
    app.providers = providers.clone();
    app.execution_profiles =
        serde_json::from_value(providers["profiles"].clone()).unwrap_or_default();
    if app.execution_profile.as_ref().is_some_and(|id| {
        !app.execution_profiles
            .iter()
            .any(|profile| &profile.id == id)
    }) {
        app.execution_profile = None;
    }
    let execution = app.execution_profiles.iter().find(|profile| {
        Some(profile.id.as_str())
            == app
                .execution_profile
                .as_deref()
                .or_else(|| providers["defaultProfile"].as_str())
    });
    let selected_agent = app.profiles.get(app.profile_selected).cloned();
    // `selected` also indexes the session list on other pages; only remap it for profiles.
    let selected = (app.page == Page::Profiles)
        .then(|| app.profiles.get(app.selected).cloned())
        .flatten();
    app.profiles = providers["agents"]
        .as_array()
        .map(|agents| {
            agents
                .iter()
                .filter(|agent| {
                    execution.is_none_or(|profile| {
                        agent["id"]
                            .as_str()
                            .is_some_and(|id| profile.agents.iter().any(|member| member == id))
                    })
                })
                .filter_map(|agent| agent["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if execution.is_some() {
        app.profiles.insert(0, String::new());
    }
    app.profile_selected = selected_agent
        .as_ref()
        .and_then(|id| app.profiles.iter().position(|agent| agent == id))
        .unwrap_or(0);
    if app.page == Page::Profiles {
        app.selected = selected
            .as_ref()
            .and_then(|id| app.profiles.iter().position(|profile| profile == id))
            .unwrap_or(0);
    }
}

fn select_execution_profile(app: &mut App, profile: Option<String>) {
    app.execution_profile = profile;
    // A profile change is new intent, not an inventory refresh preserving an agent override.
    app.profiles.clear();
    app.profile_selected = 0;
    update_profiles(app, &app.providers.clone());
}

fn resume_failure(error: &agent_client_protocol::Error) -> String {
    if format!("{error:?}").contains("replay cursor unavailable") {
        "History expired: press Ctrl-R to attach without earlier events".into()
    } else if format!("{error:?}").contains("hive::transcript::unsupported") {
        "Agent cannot replay history: press Ctrl-R to attach without earlier events".into()
    } else if format!("{error:?}").contains("hive::transcript::history_missing") {
        "Agent retained no messages: press Ctrl-R to attach without earlier events".into()
    } else {
        format!("Could not attach: {error}")
    }
}

fn history_unavailable(error: &agent_client_protocol::Error) -> bool {
    let diagnostic = format!("{error:?}");
    [
        "replay cursor unavailable",
        "hive::transcript::unsupported",
        "hive::transcript::history_missing",
    ]
    .iter()
    .any(|code| diagnostic.contains(code))
}

fn show_interaction(app: &mut App, interaction: &Interaction) {
    app.editor.cancel_drag();
    if app.suspended_draft.is_none() {
        app.suspended_draft = Some(std::mem::take(&mut app.input));
        app.suspended_images = std::mem::take(&mut app.attachments);
        app.draft_revision = app.draft_revision.wrapping_add(1);
    }
    if let Some(view) = app.transcript() {
        view.clear_selection();
    }
    app.picker = None;
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

type PromptAck = (String, String, agent_client_protocol::Result<()>);

fn apply_prompt_ack(app: &mut App, (id, key, result): PromptAck) {
    // A late receipt belongs to its original intent, never to a newer draft in that session.
    if !app.pending.get(&id).is_some_and(|pending| pending.0 == key) {
        return;
    }
    app.submitting.remove(&id);
    match result {
        Ok(()) => {
            app.pending.remove(&id);
        }
        Err(error) => {
            let diagnostic = error.data.as_ref().and_then(|data| data.get("hive"));
            if diagnostic.is_some_and(|data| {
                data["code"] == "hive::prompt::outcome_uncertain"
                    && data["recoveryRequired"] == false
            }) {
                // A lost recovery response does not require acknowledging the same incident again.
                app.pending.remove(&id);
                app.conversations
                    .entry(id.clone())
                    .or_default()
                    .push_notice(format!(
                        "[Recovery already acknowledged for {key}; outcome remains unresolved]"
                    ));
            }
            let rejected = diagnostic
                .is_some_and(|data| data["code"] == "hive::prompt::not_dispatched")
                || error.code == Error::request_cancelled().code;
            if rejected && let Some((_, draft)) = app.pending.remove(&id) {
                app.rejected.insert(id.clone(), draft);
            }
            if app.active.as_deref() != Some(&id) {
                return;
            }
            if rejected {
                if app.input.is_empty()
                    && app.attachments.is_empty()
                    && app.dialog.is_none()
                    && app.suspended_draft.is_none()
                    && let Some(draft) = app.rejected.remove(&id)
                {
                    app.input = draft.text;
                    app.attachments = draft.images;
                    app.editor = editor::PromptEditor::default();
                }
                app.status =
                    "Prompt was not dispatched; edit the draft and submit new intent".into();
                if app.rejected.contains_key(&id) {
                    app.status.push_str(
                        "; newer draft kept — clear it and use Alt-R to restore rejected content",
                    );
                }
                if let Some(data) = diagnostic {
                    for field in ["message", "help"] {
                        if let Some(message) = data[field].as_str() {
                            app.status.push_str(&format!("; {message}"));
                        }
                    }
                }
            } else {
                app.status = diagnostic.map_or_else(|| "Prompt acknowledgement uncertain; observe the session, then use Ctrl-L to retrieve its receipt".into(), |data| format!("{} — {}", data["message"].as_str().unwrap_or("Prompt uncertain"), data["help"].as_str().unwrap_or("Inspect agent state")));
            }
            if let Some(required) = diagnostic.and_then(|data| data["recoveryRequired"].as_bool())
                && let Some(owner) = &mut app.owner
                && let Some(hive) = owner.get_mut("hive")
            {
                hive["recoveryRequired"] = required.into();
            }
        }
    }
}

fn send_prompt(cx: &V2ConnectionTo<Agent>, app: &App, acks: mpsc::UnboundedSender<PromptAck>) {
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
    let key = key.clone();
    tokio::spawn(async move {
        let result = cx
            .send_request(
                v2::PromptRequest::new(v2::SessionId::new(id.clone()), content.blocks()).meta(meta),
            )
            .block_task()
            .await;
        let _ = acks.send((id, key, result.map(|_| ())));
    });
}

async fn handle_key(
    cx: &V2ConnectionTo<Agent>,
    _address: &str,
    app: &mut App,
    key: event::KeyEvent,
    interaction: &mut Option<Interaction>,
    acks: mpsc::UnboundedSender<PromptAck>,
    _created: mpsc::UnboundedSender<Result<v2::NewSessionResponse, String>>,
) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }
    if interaction.is_none()
        && app.dialog.is_none()
        && let Some(mut input) = app.launch_cwd_input.take()
    {
        match key.code {
            KeyCode::Esc => return false,
            KeyCode::Enter => {
                let path = std::path::PathBuf::from(&input);
                if app.launch_owner.is_some()
                    && !hive_protocol::remote::owner_path_is_absolute(&path)
                {
                    app.status = "Enter an absolute directory on the selected owner".into();
                    app.launch_cwd_input = Some(input);
                } else {
                    match if app.launch_owner.is_some() {
                        Ok(path)
                    } else {
                        std::path::absolute(path)
                    } {
                        Ok(path) => {
                            app.cwd = path.clone();
                            app.explicit_cwd = true;
                            app.workspace_launch =
                                Some(hive_protocol::workspace::Launch::Existing {
                                    directory: path.display().to_string(),
                                });
                        }
                        Err(error) => {
                            app.status = error.to_string();
                            app.launch_cwd_input = Some(input);
                        }
                    }
                }
                return false;
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.push(ch),
            _ => {}
        }
        app.launch_cwd_input = Some(input);
        return false;
    }
    restore_draft(app);
    app.draft_revision = app.draft_revision.wrapping_add(1);
    if app.page == Page::Chat
        && app.dialog.is_none()
        && app.picker.is_none()
        && key.modifiers == KeyModifiers::ALT
    {
        if key.code == KeyCode::Backspace {
            app.attachments.pop();
            app.status = "Last image attachment removed".into();
            return false;
        }
        if key.code == KeyCode::Char('r')
            && app.input.is_empty()
            && app.attachments.is_empty()
            && let Some(draft) = app.active.as_ref().and_then(|id| app.rejected.remove(id))
        {
            app.input = draft.text;
            app.attachments = draft.images;
            app.status = "Rejected prompt restored; Enter submits new intent".into();
            return false;
        }
    }
    if interaction.is_some() && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
        app.dialog_scroll = if key.code == KeyCode::PageUp {
            app.dialog_scroll.saturating_sub(4)
        } else {
            app.dialog_scroll.saturating_add(4)
        };
        return false;
    }
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        if let (Some(id), Some(owner)) = (&app.active, &app.owner)
            && app.pilot()
        {
            let _ = cx.send_notification(
                v2::CancelSessionNotification::new(v2::SessionId::new(id.clone()))
                    .meta(owner.clone()),
            );
            if app.conversation().is_some_and(|state| state.running) {
                app.stopping = Some(id.clone());
                app.status = "Stopping…".into();
            }
            *interaction = None;
            app.dialog = None;
            app.input.clear();
            if app.suspended_draft.is_none() {
                app.attachments.clear();
            }
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
                        v2::CancelledPermissionOutcome::new().into(),
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
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        app.input.push(ch)
                    }
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
            restore_draft(app);
        }
        return false;
    }
    // Keep interaction answers and quitting responsive while preventing overlapping ownership RPCs.
    if app.session_operation.is_some() {
        if app.page == Page::Chat && app.editor.key(&mut app.input, key) {
            return false;
        }
        return key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::CONTROL);
    }
    let ctrl = key.modifiers == KeyModifiers::CONTROL;
    if app.picker.is_some() {
        let choices = app.config_choices();
        let picker = app.picker.as_mut().unwrap();
        picker.refresh(choices);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('p' | 'g'), true) => app.picker = None,
            (KeyCode::Char('q'), true) => return true,
            (KeyCode::Up, _) => picker.navigate(-1),
            (KeyCode::Down, _) => picker.navigate(1),
            (KeyCode::PageUp, _) => picker.navigate(-10),
            (KeyCode::PageDown, _) => picker.navigate(10),
            (KeyCode::Backspace, false) => {
                picker.query.pop();
            }
            (KeyCode::Char(ch), false)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                picker.query.push(ch)
            }
            (KeyCode::Enter, _) => apply_configuration(cx, app),
            _ => {}
        }
        let choices = app.config_choices();
        if let Some(picker) = app.picker.as_mut() {
            picker.refresh(choices);
        }
        return false;
    }
    if app.page == Page::Chat && app.editor.key(&mut app.input, key) {
        app.transcript_focused = false;
        return false;
    }
    match (key.code, ctrl) {
        (KeyCode::PageUp, _) | (KeyCode::PageDown, _) if app.page == Page::Chat => {
            if let Some(view) = app.transcript() {
                view.page(key.code == KeyCode::PageDown);
            }
        }
        (KeyCode::Home, _) | (KeyCode::End, _) if app.page == Page::Chat => {
            if let Some(view) = app.transcript() {
                view.boundary(key.code == KeyCode::End);
            }
        }
        (KeyCode::Up, true) | (KeyCode::Down, true) if app.page == Page::Chat => {
            if let Some(view) = app.transcript() {
                view.select_tool(key.code == KeyCode::Down);
            }
        }
        (KeyCode::Char('o'), true) if app.page == Page::Chat => {
            if let Some(view) = app.transcript() {
                view.toggle_selected();
            }
        }
        // Ctrl-M aliases Enter in terminals; Ctrl-P remains distinguishable for the model picker.
        (KeyCode::Char('p' | 'g'), true) => {
            app.editor.cancel_drag();
            if let Some(view) = app.transcript() {
                view.clear_selection();
            }
            if let Some(id) = app.active.clone() {
                let kind = if key.code == KeyCode::Char('g') {
                    picker::Kind::Settings
                } else {
                    picker::Kind::Model
                };
                let mut picker = picker::Picker::new(id, app.choices_for(kind));
                picker.kind = kind;
                app.picker = Some(picker);
            } else {
                app.status = "Open a session before changing configuration".into();
            }
        }
        (KeyCode::Esc, _) | (KeyCode::Char('q'), true) => return true,
        (KeyCode::Tab, _) => {
            app.toggle_sessions();
            app.refresh_requested = true;
        }
        (KeyCode::Char('b'), true) if app.page == Page::Chat => {
            if let Some(info) = app.previous_session() {
                start_resume(cx, app, info.session_id, info.cwd, "pilot", false);
            }
        }
        (KeyCode::Char('n'), true) => {
            app.page = Page::Profiles;
            app.profile_selected = 0;
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
                start_resume(cx, app, id, cwd, mode, false);
            }
        }
        (KeyCode::Char('y'), true) => {
            if let Some(id) = app.active.clone() {
                let cwd = app
                    .sessions
                    .iter()
                    .find(|session| session.session_id == id)
                    .map_or_else(|| app.cwd.clone(), |session| session.cwd.clone());
                start_resume(cx, app, id, cwd, "pilot", true);
            }
        }
        (KeyCode::Char('l'), true) => {
            if let Some(id) = &app.active
                && app.pending.contains_key(id)
                && app.pilot()
                && !app.submitting.contains(id)
            {
                app.submitting.insert(id.clone());
                send_prompt(cx, app, acks);
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
                start_resume(cx, app, id, cwd, mode, false);
            }
        }
        (KeyCode::Up, _) => {
            let selected = if app.page == Page::Profiles {
                &mut app.profile_selected
            } else {
                &mut app.selected
            };
            *selected = selected.saturating_sub(1);
        }
        (KeyCode::Down, _) => {
            let selected = if app.page == Page::Profiles {
                &mut app.profile_selected
            } else {
                &mut app.selected
            };
            *selected = selected.saturating_add(1).min(
                match app.page {
                    Page::Sessions => app.sessions.len(),
                    Page::Profiles => app.profiles.len(),
                    Page::Chat => 0,
                }
                .saturating_sub(1),
            )
        }
        (KeyCode::Char('p'), _) if app.page == Page::Profiles => {
            // Include implicit daemon selection so profiles never hide ungrouped agents forever.
            let next = app
                .execution_profile
                .as_ref()
                .and_then(|id| {
                    app.execution_profiles
                        .iter()
                        .position(|profile| &profile.id == id)
                })
                .map_or(0, |index| index + 1);
            let profile = app
                .execution_profiles
                .get(next)
                .map(|profile| profile.id.clone());
            select_execution_profile(app, profile);
        }
        (KeyCode::Char('o'), _) if app.page == Page::Profiles => {
            let next = app
                .launch_owner
                .as_ref()
                .and_then(|owner| {
                    app.owners
                        .iter()
                        .position(|peer| peer["owner"] == owner.as_str())
                })
                .map_or(0, |index| index + 1);
            app.launch_owner = app
                .owners
                .get(next)
                .and_then(|peer| peer["owner"].as_str())
                .and_then(|owner| hive_core::identity::NodeId::try_from(owner.to_owned()).ok());
            app.execution_profile = None;
            app.explicit_cwd = false;
            app.workspace_launch = None;
            app.cwd = if app.launch_owner.is_some() {
                "/".into()
            } else {
                std::env::current_dir().unwrap_or_else(|_| "/".into())
            };
            app.profiles.clear();
            app.execution_profiles.clear();
            app.providers = serde_json::Value::Null;
            app.refresh_requested = true;
            app.status = format!(
                "Launch owner: {}",
                app.launch_owner
                    .as_ref()
                    .map_or("Local", |owner| owner.as_str())
            );
        }
        (KeyCode::Char('d'), _) if app.page == Page::Profiles => {
            app.launch_cwd_input = Some(if app.explicit_cwd {
                app.cwd.display().to_string()
            } else {
                String::new()
            });
        }
        (KeyCode::Char('u'), _) if app.page == Page::Profiles && app.creation_uncertain => {
            // This explicit acknowledgement permits new intent without claiming the prior launch failed.
            app.creation_uncertain = false;
            app.status =
                "Uncertain launch acknowledged; a new launch may create another session".into();
        }
        (KeyCode::Char('a'), _) if app.page == Page::Sessions => {
            app.all = !app.all;
            app.refresh_requested = true;
        }
        (KeyCode::Char('o'), _) if app.page == Page::Sessions => {
            if let Some(info) = app.sessions.get(app.selected).cloned() {
                start_resume(cx, app, info.session_id, info.cwd, "observe", false);
            }
        }
        (KeyCode::Enter, _) => match app.page {
            Page::Sessions => {
                if let Some(info) = app.sessions.get(app.selected).cloned() {
                    start_resume(cx, app, info.session_id, info.cwd, "pilot", false);
                }
            }
            Page::Profiles => {
                if let Some(profile) = app.profiles.get(app.profile_selected).cloned() {
                    start_creation(cx, app, profile);
                }
            }
            Page::Chat => {
                if (!app.input.trim().is_empty() || !app.attachments.is_empty())
                    && app.pilot()
                    && !app
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner["hive"]["recoveryRequired"] == true)
                    && app.configuration.is_none()
                    && !app.conversation().is_some_and(|c| c.running)
                    && app
                        .active
                        .as_ref()
                        .is_some_and(|id| !app.pending.contains_key(id))
                {
                    if !app.attachments.is_empty()
                        && app
                            .owner
                            .as_ref()
                            .is_some_and(|owner| owner["hive"]["promptImages"] == false)
                    {
                        app.status = "This agent adapter does not accept images; choose an image-capable ACPv2 agent or remove attachments".into();
                        return false;
                    }
                    let key = uuid::Uuid::new_v4().to_string();
                    let id = app.active.clone().unwrap_or_default();
                    let content = draft::PromptDraft {
                        text: app.input.clone(),
                        images: app.attachments.clone(),
                    };
                    let mut meta = app.owner.clone().unwrap_or_default();
                    if let Some(hive) = meta.get_mut("hive") {
                        hive["key"] = key.clone().into();
                    }
                    let request =
                        v2::PromptRequest::new(v2::SessionId::new(id.clone()), content.blocks())
                            .meta(meta);
                    if let Err(error) = draft::check_request(&request) {
                        app.status = error.to_string();
                        return false;
                    }
                    app.input.clear();
                    app.attachments.clear();
                    app.draft_revision = app.draft_revision.wrapping_add(1);
                    // Only sequenced daemon messages enter history; the admission echo would duplicate this draft.
                    app.submitting.insert(id.clone());
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
        (KeyCode::Char(ch), false)
            if app.page == Page::Chat
                && !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            app.input.push(ch)
        }
        _ => {}
    }
    false
}

fn restore_draft(app: &mut App) {
    if app.dialog.is_none()
        && let Some(draft) = app.suspended_draft.take()
    {
        app.input = draft;
        app.attachments = std::mem::take(&mut app.suspended_images);
        app.draft_revision = app.draft_revision.wrapping_add(1);
    }
}

fn paste(app: &mut App, interaction: &Option<Interaction>, value: &str) {
    let value = match editor::import(value) {
        Ok(value) => value,
        Err(error) => {
            app.status = error.to_string();
            return;
        }
    };
    if interaction.is_none()
        && app.dialog.is_none()
        && let Some(input) = &mut app.launch_cwd_input
    {
        // Pasted newlines are path data to review, never Enter presses that save or launch.
        input.push_str(&value.replace('\n', " "));
        return;
    }
    if let Some(Interaction::Elicitation(request, _, draft)) = interaction {
        if FormDraft::fields(request)
            .get(draft.field)
            .is_some_and(|(_, schema)| FormDraft::choices(schema).is_empty())
        {
            app.input.push_str(&value.replace('\n', " "));
            app.draft_revision = app.draft_revision.wrapping_add(1);
        }
    } else if interaction.is_none() && app.dialog.is_none() {
        if let Some(picker) = app.picker.as_mut() {
            picker.query.push_str(&value.replace('\n', " "));
            app.draft_revision = app.draft_revision.wrapping_add(1);
            let choices = app.config_choices();
            if let Some(picker) = app.picker.as_mut() {
                picker.refresh(choices);
            }
        } else if app.page == Page::Chat {
            app.editor.insert(&mut app.input, &value);
            app.draft_revision = app.draft_revision.wrapping_add(1);
            app.transcript_focused = false;
        }
    }
}

fn clipboard_request(
    app: &mut App,
    interaction: &Option<Interaction>,
    command: char,
    clipboard: &clipboard::Clipboard,
    output: &mut impl std::io::Write,
) {
    if let Some(request) = make_clipboard_request(app, interaction, command)
        && let Some(completion) = clipboard.dispatch(request, &app.terminal_capabilities, output)
    {
        clipboard::complete(app, completion);
    }
}

fn make_clipboard_request(
    app: &mut App,
    interaction: &Option<Interaction>,
    command: char,
) -> Option<clipboard::Request> {
    let target = if let Some(Interaction::Elicitation(request, _, draft)) = interaction {
        if command != 'v'
            || !FormDraft::fields(request)
                .get(draft.field)
                .is_some_and(|(_, schema)| FormDraft::choices(schema).is_empty())
        {
            return None;
        }
        clipboard::Target::Question(app.dialog.clone().unwrap_or_default())
    } else if interaction.is_some() || app.dialog.is_some() {
        return None;
    } else if let Some(picker) = &app.picker {
        if command != 'v' {
            return None;
        }
        clipboard::Target::Picker(picker.query.clone())
    } else if app.page == Page::Chat {
        clipboard::Target::Chat
    } else {
        return None;
    };
    if matches!(target, clipboard::Target::Chat) {
        app.editor.prepare(&app.input, app.editor.area);
    }
    let selected = if app.transcript_focused {
        app.transcript().and_then(|view| view.selected_text())
    } else {
        app.editor.selected()
    };
    let selected_image = if app.transcript_focused {
        app.transcript().and_then(|view| view.selected_image())
    } else {
        None
    };
    let operation = match (command, selected) {
        ('v', _) if matches!(target, clipboard::Target::Chat) => clipboard::Operation::Paste,
        ('v', _) => clipboard::Operation::PasteText,
        ('c', _) if selected_image.is_some() => {
            clipboard::Operation::CopyImage(selected_image.expect("checked selected image"))
        }
        ('c', Some(text)) => clipboard::Operation::Copy(text),
        ('x', Some(text)) if !app.transcript_focused => clipboard::Operation::Cut(text),
        _ => {
            app.status = "Select text before copying or cutting".into();
            return None;
        }
    };
    Some(clipboard::Request {
        operation,
        session: app.active.clone(),
        original: app.input.clone(),
        editor: app.editor.clone(),
        target,
        revision: app.draft_revision,
    })
}

fn apply_configuration(cx: &V2ConnectionTo<Agent>, app: &mut App) {
    // Repeated Enter must preserve both the in-flight request and its pending feedback.
    if app.configuration.is_some() {
        if let Some(picker) = app.picker.as_mut() {
            picker.message = "Updating session configuration…".into();
        }
        return;
    }
    if !app.connected
        || !app.pilot()
        || app
            .owner
            .as_ref()
            .is_some_and(|owner| owner["hive"]["recoveryRequired"] == true)
        || app.conversation().is_some_and(|state| state.running)
        || app
            .active
            .as_ref()
            .is_some_and(|id| app.pending.contains_key(id))
    {
        app.status = "Take control of an idle session before changing its configuration".into();
        if let Some(picker) = app.picker.as_mut() {
            picker.message.clone_from(&app.status);
        }
        return;
    }
    let Some(picker) = app.picker.as_mut() else {
        return;
    };
    if app.active.as_ref() != Some(&picker.session) {
        return;
    }
    let Some((config_id, value, _)) = picker.selected().cloned() else {
        return;
    };
    let Some(owner) = app.owner.clone() else {
        return;
    };
    let id = picker.session.clone();
    let request = v2::SetSessionConfigOptionRequest::new(
        v2::SessionId::new(id.clone()),
        v2::SessionConfigId::new(config_id),
        v2::SessionConfigOptionValue::Id {
            value: v2::SessionConfigValueId::new(value),
        },
    )
    .meta(owner);
    let request = cx.send_request(request).block_task();
    let (sent, received) = oneshot::channel();
    app.configuration = Some(received);
    picker.pending = true;
    picker.message = "Updating session configuration…".into();
    app.status.clone_from(&picker.message);
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
        picker.pending = false;
        picker.message = error.to_string();
        app.status.clone_from(&picker.message);
    }
}

fn handle_mouse(app: &mut App, event: MouseEvent) {
    // Overlays own input: wheel/click events must never reach the history behind them.
    if let Some(picker) = app.picker.as_mut() {
        match event.kind {
            MouseEventKind::ScrollUp => picker.navigate(-3),
            MouseEventKind::ScrollDown => picker.navigate(3),
            _ => {}
        }
    } else if app.dialog.is_some() {
        match event.kind {
            MouseEventKind::ScrollUp => app.dialog_scroll = app.dialog_scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => app.dialog_scroll = app.dialog_scroll.saturating_add(3),
            _ => {}
        }
    } else if app.page == Page::Chat {
        app.editor.prepare(&app.input, app.editor.area);
        if app.editor.mouse(event) {
            app.transcript_focused = false;
        } else if let Some(view) = app.transcript() {
            let inside = view.area.contains((event.column, event.row).into());
            view.mouse(event);
            if inside {
                app.transcript_focused = true;
            }
        }
    }
}

fn configuration_result(
    app: &mut App,
    id: String,
    result: Result<v2::SetSessionConfigOptionResponse, String>,
) {
    // A dismissed/reopened picker has pending=false, so an old result cannot close it.
    let owns_result = app
        .picker
        .as_ref()
        .is_some_and(|p| p.pending && p.session == id);
    match result {
        Ok(response) => {
            let seq = response
                .meta
                .as_ref()
                .and_then(|meta| meta.get("hive"))
                .and_then(|hive| hive["configSeq"].as_u64())
                .unwrap_or(0);
            app.conversations
                .entry(id)
                .or_default()
                .config_snapshot(response.config_options, seq);
            app.status = "Session configuration updated".into();
            if owns_result {
                app.picker = None;
            }
        }
        Err(error) => {
            if owns_result && let Some(picker) = app.picker.as_mut() {
                picker.pending = false;
                picker.message.clone_from(&error);
            }
            app.status = error;
        }
    }
}

/// EOF must wake the foreground before the driver can publish its termination.
async fn gateway_disconnected(cx: &V2ConnectionTo<Agent>) {
    cx.incoming_closed().await;
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
                                        v2::CancelledPermissionOutcome::new().into(),
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

#[cfg(any(test, not(unix)))]
fn read_keys(keys: mpsc::UnboundedSender<Event>, mut next: impl FnMut() -> Option<Event>) {
    // Tokio waits for blocking workers on shutdown even when no more terminal input arrives.
    while !keys.is_closed() {
        if let Some(event) = next()
            && keys.send(event).is_err()
        {
            break;
        }
    }
}

enum ReaderControl {
    Pause(oneshot::Sender<()>),
    Resume,
}

fn terminal_reader(
    keys: mpsc::UnboundedSender<Event>,
    replies: mpsc::Sender<input::Reply>,
    controls: std::sync::mpsc::Receiver<ReaderControl>,
) {
    #[cfg(unix)]
    input::reader(keys, replies, controls);
    #[cfg(not(unix))]
    {
        let _ = replies;
        controlled_reader(keys, controls, || {
            event::poll(Duration::from_millis(20))
                .unwrap_or(false)
                .then(|| event::read().ok())
                .flatten()
        });
    }
}

#[cfg(any(test, not(unix)))]
fn controlled_reader(
    keys: mpsc::UnboundedSender<Event>,
    controls: std::sync::mpsc::Receiver<ReaderControl>,
    mut next: impl FnMut() -> Option<Event>,
) {
    let mut paused = false;
    read_keys(keys, || {
        // Controls are checked before every poll, even under a continuous stream of mouse events.
        let control = if paused {
            controls.recv_timeout(Duration::from_millis(20)).ok()
        } else {
            controls.try_recv().ok()
        };
        match control {
            Some(ReaderControl::Pause(ack)) => {
                paused = true;
                let _ = ack.send(());
            }
            Some(ReaderControl::Resume) => paused = false,
            None => {}
        }
        if paused {
            return None;
        }
        next()
    });
}

async fn editor_completion(
    task: &mut Option<external_editor::Task>,
) -> Result<external_editor::Edited, String> {
    match task {
        Some(task) => (&mut task.0)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string()),
        None => std::future::pending().await,
    }
}

pub async fn run(options: Options) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_with_clipboard(options, clipboard::Clipboard::new()).await
}

async fn run_with_clipboard(
    options: Options,
    mut clipboard: clipboard::Clipboard,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut app = App {
        launch_owner: options.owner.clone(),
        explicit_cwd: options.cwd.is_some(),
        terminal_capabilities: capabilities::TerminalCapabilities::detect(&options.terminal),
        cwd: options
            .cwd
            .clone()
            .map(|path| {
                if options.owner.is_some() {
                    Ok(path)
                } else {
                    std::path::absolute(path)
                }
            })
            .transpose()?
            .unwrap_or(if options.owner.is_some() {
                "/".into()
            } else {
                std::env::current_dir()?
            }),
        all: true,
        ..App::default()
    };
    app.workspace_launch = if let Some(directory) = &options.cwd {
        Some(hive_protocol::workspace::Launch::Existing {
            directory: if options.owner.is_some() {
                directory.clone()
            } else {
                std::path::absolute(directory)?
            }
            .display()
            .to_string(),
        })
    } else if let Some(id) = &options.workspace_id {
        Some(hive_protocol::workspace::Launch::Attach {
            id: id.clone(),
            owner: None,
        })
    } else if options.project.is_some()
        || options.workspace.is_some()
        || options.branch.is_some()
        || options.base_ref.is_some()
        || options.revision.is_some()
    {
        Some(hive_protocol::workspace::Launch::New {
            definition: None,
            revision: options.revision.clone(),
            project: options.project.clone(),
            policy: options.workspace.clone(),
            base_ref: options.base_ref.clone(),
            branch: options.branch.clone(),
        })
    } else {
        None
    };
    let credentials = hive_protocol::transport::Credentials::native()?;
    let mut guard = TerminalGuard::enter()?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let (keys_tx, mut keys) = mpsc::unbounded_channel();
    let (replies_tx, mut replies) = mpsc::channel(32);
    let (reader_control, controls) = std::sync::mpsc::channel();
    let reader =
        tokio::task::spawn_blocking(move || terminal_reader(keys_tx, replies_tx, controls));
    guard
        .program_status
        .start(app.terminal_capabilities.osc7501, terminal.backend_mut());
    let rich_paste = cfg!(unix)
        && app.terminal_capabilities.osc5522_read.source
            != capabilities::CapabilitySource::UnavailableTransport
        && !(app.terminal_capabilities.osc5522_read.source
            == capabilities::CapabilitySource::Override
            && !app.terminal_capabilities.osc5522_read.is_usable());
    if rich_paste {
        std::io::Write::write_all(terminal.backend_mut(), b"\x1b[?5522$p")?;
        std::io::Write::flush(terminal.backend_mut())?;
    }
    let mut editing = None;
    let mut editor_prefix: Option<std::time::Instant> = None;
    let mut quit = false;
    let mut connected_once = false;
    let mut selection_error = None;
    while !quit {
        let url = format!("{}://{}", options.transport, options.address);
        let (updates_tx, mut updates) = mpsc::unbounded_channel();
        let (interaction_tx, mut interactions) = mpsc::unbounded_channel();
        let (completion_tx, mut completions) = mpsc::unbounded_channel();
        let result = interaction_client(updates_tx, interaction_tx, completion_tx, credentials.acp(&url)?, async |cx| {
                 let peer = hive_protocol::remote::Peer::initialize(cx.clone(), v2::InitializeRequest::new(ProtocolVersion::V2,
                    v2::Implementation::new("hive-tui", env!("CARGO_PKG_VERSION")))
                    .capabilities(v2::ClientCapabilities::new().elicitation(v2::ElicitationCapabilities::new()
                        .form(v2::ElicitationFormCapabilities::new()).url(v2::ElicitationUrlCapabilities::new()))))
                 .await?;
                  let reconnecting = connected_once;
                  app.connected = true;
                  app.peer = Some(peer.clone());
                  if app.launch_owner.as_ref().is_some_and(|owner| owner.as_str() == peer.node) { app.launch_owner = None; }
                  if let Err(error) = refresh(&peer, &mut app).await {
                      app.status = error.clone();
                      // Failed discovery is retryable, but must not consume explicit startup selectors.
                      if !reconnecting && (options.profile.is_some() || options.agent.is_some()) { return Err(Error::internal_error().data(error)); }
                  }
                  if !reconnecting && let Err(error) = apply_initial_selection(&mut app, options.profile.as_deref(), options.agent.as_deref(), options.session.is_some()) {
                          let message = error.to_string();
                          selection_error = Some(error);
                          return Err(Error::invalid_params().data(message));
                  }
                  connected_once = true;
                  restore_attachment(&peer, &mut app, options.session.as_deref(), options.owner.as_ref(), reconnecting).await;
                   let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<PromptAck>();
                  let (created_tx, mut created_rx) = mpsc::unbounded_channel::<Result<v2::NewSessionResponse, String>>();
                 // Observe after reconnect; Ctrl-L explicitly retrieves a pending receipt.
                  let mut interaction: Option<Interaction> = None;
                  let mut queued = VecDeque::<Interaction>::new();
                  let mut url_flows = BTreeMap::<String, (bool, bool, tokio::time::Instant)>::new();
                  let mut interaction_tick = tokio::time::interval(Duration::from_millis(50));
                  let mut catalog_tick = tokio::time::interval(Duration::from_secs(2));
                  let mut frames = tokio::time::interval(Duration::from_millis(16));
                  frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                   let mut dirty = true;
                   let animation_clock = std::time::Instant::now();
                   let mut staged_updates = Vec::new();
                  let (catalog_tx, mut catalog_rx) = mpsc::unbounded_channel();
                  let mut refreshing = false;
                  loop {
                    if editing.is_none() {
                        let kind = interaction.as_ref().filter(|request| request.available(&app) && !request.expired() && !request.request_scoped()).map(|request| match request {
                            Interaction::Permission(..) => program_status::Kind::Permission,
                            Interaction::Elicitation(..) => program_status::Kind::Question,
                        });
                        guard.program_status.sync(program_status::project(&app, kind), terminal.backend_mut());
                    }
                    if app.refresh_requested && !refreshing {
                        app.refresh_requested = false;
                        refreshing = true;
                         let client = peer.clone();
                         let owner = app.launch_owner.clone();
                        let results = catalog_tx.clone();
                         cx.spawn(async move { let _ = results.send(load_catalog(&client, owner).await); Ok(()) })?;
                    }
                    if let Some(receiver) = app.configuration.as_mut() {
                        match receiver.try_recv() {
                             Ok((id, result)) => {
                                 app.configuration = None;
                                  configuration_result(&mut app, id, result);
                                  // A background result can arrive after the last dirty frame was drawn.
                                  dirty = true;
                            }
                             Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                                 app.configuration = None;
                                 let id = app.active.clone().unwrap_or_default();
                                  configuration_result(&mut app, id, Err("Configuration interrupted; reopen the session to inspect its current settings".into()));
                                  dirty = true;
                             }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                        }
                    }
                       tokio::select! {
                          result = operation_completion(&mut app.session_operation) => {
                              let operation = app.session_operation.take().expect("completed operation");
                              match result {
                                  Ok(result) => {
                                      if let Some(created) = apply_session_result(&mut app, result, operation.label) { apply_created(&mut app, created); }
                                  }
                                  Err(_) => { app.skip_replay = false; app.status = "Session operation interrupted; reconnect to inspect its outcome".into(); }
                              }
                              // Install the attachment boundary before accepting its snapshot notifications.
                              let mut reconciled = true;
                              for update in staged_updates.drain(..) { if !apply_update(&mut app, update) { reconciled = false; break; } }
                              if !reconciled { break; }
                              reconcile_interactions(&mut app, &mut interaction, &mut queued);
                          }
                          _ = tokio::signal::ctrl_c() => {
                              // In cooked mode SIGINT reaches the entire foreground process group.
                              // The editor receives it too; keep its parent alive to restore the TUI.
                              if editing.is_none() { quit = true; break; }
                          }
                          _ = frames.tick() => {
                              if dirty && editing.is_none() {
                                  restore_draft(&mut app);
                                   let capabilities = app.terminal_capabilities;
                                   guard.draw(&mut terminal, &capabilities, |frame, images| app.show_with_images(frame, images)).map_err(Error::into_internal_error)?;
                                  dirty = false;
                              }
                              continue;
                          }
                          result = editor_completion(&mut editing) => {
                              editing = None;
                              guard.resume().map_err(Error::into_internal_error)?;
                               guard.clear(&mut terminal).map_err(Error::into_internal_error)?;
                               while keys.try_recv().is_ok() {}
                               while replies.try_recv().is_ok() {}
                               let _ = reader_control.send(ReaderControl::Resume);
                               if rich_paste { std::io::Write::write_all(terminal.backend_mut(), b"\x1b[?5522$p").and_then(|()| std::io::Write::flush(terminal.backend_mut())).map_err(Error::into_internal_error)?; }
                              match result { Ok(edited) => external_editor::apply(&mut app, edited), Err(error) => app.status = error }
                          }
                          Some(completion) = clipboard.completions.recv(), if editing.is_none() => {
                              if let Some(completion) = clipboard.finish(completion, terminal.backend_mut()) { clipboard::complete(&mut app, completion); }
                          },
                          Some(reply) = replies.recv(), if editing.is_none() => {
                               match reply {
                                   input::Reply::ProgramStatus(features, received) => guard.program_status.receive(features, received, &mut app.terminal_capabilities.osc7501),
                                  input::Reply::PasteMode(status) if rich_paste && matches!(status, 1..=3) => {
                                      use std::io::Write;
                                      // Mark before writing so partial output failures still attempt restoration.
                                      guard.rich_mode_used = true;
                                      terminal.backend_mut().write_all(b"\x1b[?5522h").and_then(|()| terminal.backend_mut().flush()).map_err(Error::into_internal_error)?;
                                      clipboard.protocol.borrow_mut().native_paste = true;
                                      // Negotiation proves support, but does not grant arbitrary read consent.
                                      let cap = &mut app.terminal_capabilities.osc5522_write;
                                      cap.detected = capabilities::Support::Supported;
                                      if cap.source == capabilities::CapabilitySource::Automatic { cap.effective = capabilities::Support::Supported; }
                                      app.terminal_capabilities.osc5522_read.detected = capabilities::Support::Supported;
                                  }
                                  input::Reply::Clipboard(bytes) => {
                                      let outcome = clipboard.protocol.borrow_mut().receive(&bytes, || make_clipboard_request(&mut app, &interaction, 'v'), terminal.backend_mut());
                                      match outcome {
                                          Some(osc5522::Outcome::Complete(completion)) => clipboard::complete(&mut app, completion),
                                          Some(osc5522::Outcome::Image(request, mime, bytes)) => {
                                              if let Some(completion) = clipboard.prepare_image(request, mime, bytes) { clipboard::complete(&mut app, completion); }
                                          }
                                          None => {}
                                      }
                                  }
                                  input::Reply::Error(message) => { clipboard.protocol.borrow_mut().abort(); app.status = message; }
                                  _ => {}
                              }
                          },
                          Some(result) = catalog_rx.recv() => {
                              refreshing = false;
                              match result { Ok(catalog) => apply_catalog(&mut app, catalog), Err(error) => app.status = format!("Session list unavailable: {error}") }
                          }
                            _ = interaction_tick.tick() => {
                                // Replies may already be queued after a slow ACP initialization.
                                if replies.is_empty() { guard.program_status.tick(&mut app.terminal_capabilities.osc7501); }
                               if editing.is_none() && let Some(completion) = clipboard.protocol.borrow_mut().tick(terminal.backend_mut()) { clipboard::complete(&mut app, completion); }
                               let phase = (animation_clock.elapsed().as_millis() / 100) as usize;
                               let animated = editing.is_none() && app.animation_visible && phase != app.animation_phase;
                               app.animation_phase = phase;
                              let previous_status = app.status.clone();
                              let previous_dialog = app.dialog.is_some();
                              if editor_prefix.is_some_and(|started| started.elapsed() >= Duration::from_secs(2)) {
                                  editor_prefix = None;
                                  if app.status.starts_with("Ctrl-X:") { app.status.clear(); }
                              }
                              let scrolled = editing.is_none() && app.dialog.is_none() && app.picker.is_none() && app.page == Page::Chat && app.transcript().is_some_and(|view| view.autoscroll());
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
                               if editing.is_none() && interaction.is_none() && let Some(index) = queued.iter().position(|request| request.available(&app) && (app.session_operation.is_none() || request.request_scoped())) && let Some(next) = queued.remove(index) { editor_prefix = None; show_interaction(&mut app, &next); interaction = Some(next); }
                               if !animated && !scrolled && previous_status == app.status && previous_dialog == app.dialog.is_some() { continue; }
                         }
                         Some(id) = completions.recv() => {
                             if url_flows.len() >= 128 { url_flows.clear(); }
                             let flow = url_flows.entry(id.to_string()).or_insert_with(|| (false, false, tokio::time::Instant::now() + Duration::from_secs(120)));
                             flow.1 = true;
                             if flow.0 { app.status = "External elicitation completed".into(); }
                         }
                          Some(result) = created_rx.recv() => { apply_created(&mut app, result); reconcile_interactions(&mut app, &mut interaction, &mut queued); }
                         _ = gateway_disconnected(&cx) => break,
                          _ = catalog_tick.tick() => {
                              app.refresh_requested = true;
                         }
                          Some(update) = updates.recv() => {
                              if app.session_operation.is_some() { staged_updates.push(update); continue; }
                              if !apply_update(&mut app, update) { break; }
                              if !app.pilot() {
                                queued.retain(Interaction::request_scoped);
                                if !interaction.as_ref().is_some_and(Interaction::request_scoped) {
                                    interaction = None;
                                     if app.dialog.take().is_some() {
                                         app.input.clear();
                                         restore_draft(&mut app);
                                     }
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
                              if editing.is_none() && interaction.is_none() && next.available(&app) && (app.session_operation.is_none() || next.request_scoped()) { editor_prefix = None; show_interaction(&mut app, &next); interaction = Some(next); }
                             else if queued.len() < 128 { queued.push_back(next); }
                             // Dropping an overflow request safely cancels it at the daemon boundary.
                        }
                           Some(acknowledgement) = ack_rx.recv() => apply_prompt_ack(&mut app, acknowledgement),
                           Some(event) = keys.recv() => {
                              let key = match event {
                                 Event::Key(key) => key,
                                   Event::Mouse(mouse) => {
                                       if matches!(mouse.kind, MouseEventKind::Down(_)) { editor_prefix = None; }
                                       let before = (app.editor.caret, app.editor.selection());
                                       handle_mouse(&mut app, mouse);
                                       if before != (app.editor.caret, app.editor.selection()) { app.draft_revision = app.draft_revision.wrapping_add(1); }
                                       dirty = true; continue;
                                  }
                                  Event::Paste(value) => { editor_prefix = None; paste(&mut app, &interaction, &value); dirty = true; continue; }
                                  Event::FocusLost => {
                                      editor_prefix = None;
                                      app.editor.cancel_drag();
                                      if let Some(view) = app.transcript() { view.clear_selection(); }
                                      dirty = true;
                                      continue;
                                  }
                                   Event::Resize(_, _) => {
                                       // Pixel geometry can change even when cell dimensions do not.
                                       guard.images.invalidate(); dirty = true; continue;
                                   }
                                  _ => { dirty = true; continue; },
                              };
                              dirty = true;
                               if key.kind == KeyEventKind::Release { continue; }
                               if let Some(command) = clipboard::command(key) {
                                   editor_prefix = None;
                                   // The UI owns terminal output; workers must never interleave escapes with a redraw or editor.
                                   if editing.is_none() { clipboard_request(&mut app, &interaction, command, &clipboard, terminal.backend_mut()); }
                                   continue;
                               }
                              let prefix = editor_prefix.take().is_some_and(|started| started.elapsed() < Duration::from_secs(2));
                              if app.page == Page::Chat && app.dialog.is_none() && app.picker.is_none() {
                                  if prefix && key.code == KeyCode::Char('e') && key.modifiers == KeyModifiers::CONTROL {
                                      match external_editor::Prepared::new(app.active.clone(), app.input.clone()) {
                                          Ok(prepared) => {
                                              app.editor.cancel_drag();
                                              if let Some(view) = app.transcript() { view.clear_selection(); }
                                              let (sent, paused) = oneshot::channel();
                                               if reader_control.send(ReaderControl::Pause(sent)).is_ok() && paused.await.is_ok() {
                                                   app.draft_revision = app.draft_revision.wrapping_add(1);
                                                   clipboard.protocol.borrow_mut().cancel();
                                                   while keys.try_recv().is_ok() {}
                                                   while replies.try_recv().is_ok() {}
                                                  if let Err(error) = guard.suspend() {
                                                      let _ = guard.resume();
                                                      let _ = reader_control.send(ReaderControl::Resume);
                                                      app.status = format!("Could not hand terminal to editor: {error}; retry after checking the terminal");
                                                  } else { editing = Some(external_editor::Task(tokio::spawn(prepared.run()))); }
                                              }
                                          }
                                          Err(error) => app.status = error.to_string(),
                                      }
                                      continue;
                                  }
                                  if key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::CONTROL {
                                      editor_prefix = Some(std::time::Instant::now());
                                      app.status = "Ctrl-X: press Ctrl-E to edit the draft externally".into();
                                      continue;
                                  }
                              }
                             let before = app.active.clone();
                             let declined_url = if key.code == KeyCode::Esc { interaction.as_ref().and_then(Interaction::url_id) } else { None };
                              if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL { url_flows.clear(); }
                             if handle_key(&cx, &options.address, &mut app, key, &mut interaction, ack_tx.clone(), created_tx.clone()).await { quit = true; break; }
                             if let Some(id) = declined_url { url_flows.remove(&id); app.status = "External elicitation declined".into(); }
                             if before != app.active {
                                 interaction = None;
                                 app.dialog = None;
                             }
                         },
                    }
                    dirty = true;
                }
                Ok(())
            }).await;
        app.connected = false;
        app.peer = None;
        app.owner = None;
        // Reconnecting the transport alone cannot make cached activity authoritative again.
        // A successful attachment clears this fence; failed resumes must not revive old work.
        for conversation in app.conversations.values_mut() {
            conversation.gap = true;
        }
        if editing.is_none() {
            let _ = guard.program_status.clear(terminal.backend_mut());
        }
        app.session_operation = None;
        app.configuration = None;
        app.submitting.clear();
        app.stopping = None;
        editor_prefix = None;
        app.picker = None;
        // A disconnected form draft must never reappear as an ordinary chat prompt.
        if app.dialog.take().is_some() {
            app.input.clear();
        }
        if quit || selection_error.is_some() {
            break;
        }
        // Network failures can embed arbitrary server bodies; display only the protocol code.
        app.status = format!(
            "Disconnected (code {:?}); reconnecting to the same Hive session…",
            result.as_ref().err().map(|error| error.code)
        );
        if editing.is_none() {
            restore_draft(&mut app);
            let capabilities = app.terminal_capabilities;
            guard.draw(&mut terminal, &capabilities, |frame, images| {
                app.show_with_images(frame, images)
            })?;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    keys.close();
    reader.await?;
    if let Some(error) = selection_error {
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn creation_replaces_its_pending_catalog_entry_with_the_effective_workspace_scope() {
        let mut app = super::ui::App::default();
        app.sessions.push(serde_json::from_value(serde_json::json!({"sessionId":"hive-pending","cwd":"/launch","agent":"coder","lifecycle":"recovering","activity":"unknown","generation":1})).unwrap());
        let mut response =
            super::v2::NewSessionResponse::new(super::v2::SessionId::new("hive-pending"));
        let mut meta = super::v2::Meta::new();
        meta.insert(
            "hive".into(),
            serde_json::json!({"workspace":{"effectiveCwd":"/managed/task"}}),
        );
        response.meta = Some(meta);
        super::apply_created(&mut app, Ok(response));
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(
            app.sessions[0].cwd,
            std::path::PathBuf::from("/managed/task")
        );
        assert_eq!(app.sessions[0].lifecycle, "active");
    }
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires a controlling PTY; run this test inside a terminal"]
    async fn a_real_terminal_can_be_handed_to_a_child_and_restored_after_failure() {
        let mut guard = TerminalGuard::enter().unwrap();
        assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
                .unwrap();
        // The PTY parent records graphics bytes; no terminal response is needed.
        let mut capabilities = capabilities::TerminalCapabilities::default();
        capabilities.kitty_graphics.effective = capabilities::Support::Supported;
        let source = images::fixture_source();
        guard
            .draw(&mut terminal, &capabilities, |frame, images| {
                images::fixture_scene(frame, images, &source);
            })
            .unwrap();
        guard
            .draw(&mut terminal, &capabilities, |frame, images| {
                images::fixture_scene(frame, images, &source);
            })
            .unwrap();
        let (sender, mut keys) = mpsc::unbounded_channel();
        let (control, controls) = std::sync::mpsc::channel();
        let (replies, _received) = mpsc::channel(32);
        let reader =
            tokio::task::spawn_blocking(move || terminal_reader(sender, replies, controls));
        eprintln!("HIVE_PTY_IMAGE_RESIZE_READY");
        // The parent changes the actual PTY size before acknowledging with `r`.
        // Ignore a queued resize notification; draw must also notice geometry itself.
        // A second input parser can buffer bytes that the application's reader will never see.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(keys.recv().await.unwrap(), Event::Key(key) if key.code == KeyCode::Char('r')) {
                    break;
                }
            }
        }).await.expect("PTY parent did not acknowledge resize");
        eprintln!("HIVE_PTY_RESIZE_ACKNOWLEDGED");
        guard
            .draw(&mut terminal, &capabilities, |frame, images| {
                images::fixture_scene(frame, images, &source);
            })
            .unwrap();
        let (sent, paused) = oneshot::channel();
        control.send(ReaderControl::Pause(sent)).unwrap();
        tokio::time::timeout(Duration::from_secs(10), paused)
            .await
            .expect("terminal reader did not pause")
            .unwrap();
        eprintln!("HIVE_PTY_READER_PAUSED");
        guard.suspend().unwrap();
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
        let child = std::process::Command::new("stty")
            .arg("-a")
            .stdin(std::process::Stdio::inherit())
            .output()
            .unwrap();
        assert!(child.status.success());
        let settings = String::from_utf8(child.stdout).unwrap();
        let flags = settings
            .split(|ch: char| ch.is_whitespace() || ch == ';')
            .collect::<Vec<_>>();
        // The PTY parent checks the full snapshot; this child must receive cooked, echoing input.
        for flag in ["icanon", "echo", "isig"] {
            assert!(flags.contains(&flag), "{settings}");
        }
        // Even a failed editor must be followed by a full display/input restoration.
        assert!(
            !tokio::process::Command::new("sh")
                .args(["-c", "exit 7"])
                .status()
                .await
                .unwrap()
                .success()
        );
        eprintln!("HIVE_PTY_EDITOR_EXITED");
        guard.resume().unwrap();
        guard.clear(&mut terminal).unwrap();
        guard
            .draw(&mut terminal, &capabilities, |frame, images| {
                images::fixture_scene(frame, images, &source);
            })
            .unwrap();
        control.send(ReaderControl::Resume).unwrap();
        assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
        eprintln!("HIVE_PTY_PASTE_READY");
        let pasted = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = keys.recv().await.unwrap();
                // SIGWINCH can arrive after the resize acknowledgement, especially
                // when a Cargo frontend shares this controlling terminal.
                if !matches!(event, Event::Resize(_, _)) {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(pasted, Event::Paste(ref value) if value == "one\ntwo"),
            "unexpected event: {pasted:?}"
        );
        eprintln!("HIVE_PTY_PASTE_RECEIVED");
        keys.close();
        tokio::time::timeout(Duration::from_secs(10), reader)
            .await
            .expect("terminal reader did not exit")
            .unwrap();
        eprintln!("HIVE_PTY_READER_EXITED");
        guard.suspend().unwrap();
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }

    #[test]
    #[ignore = "visual image fixture; requires an interactive terminal"]
    fn inline_images_and_thumbnails_can_be_inspected_in_a_terminal() {
        let mut guard = TerminalGuard::enter().unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
                .unwrap();
        let capabilities =
            capabilities::TerminalCapabilities::detect(&capabilities::TerminalOptions::default());
        let source = images::fixture_source();
        let mut hidden = false;
        let mut overlay = false;
        let mut acp_scene = true;
        let mut conversation = state::Conversation::default();
        conversation.apply(v2::UpdateSessionNotification::new("fixture", serde_json::from_value(serde_json::json!({
            "sessionUpdate":"agent_message", "messageId":"images", "content":[
                {"type":"text","text":"ACP scene: before first image"}, images::fixture_block(),
                {"type":"text","text":"Between ACP images"}, images::fixture_block(),
                {"type":"text","text":"After ACP images"}
            ]
        })).unwrap()).meta([("hive".into(), serde_json::json!({"seq":1}))].into_iter().collect::<v2::Meta>()));
        let mut transcript = transcript::Transcript::default();
        loop {
            guard
                .draw(&mut terminal, &capabilities, |frame, images| {
                    if !hidden {
                        if acp_scene {
                            transcript.render_with_images(
                                &conversation,
                                frame,
                                frame.area(),
                                false,
                                false,
                                images,
                            );
                        } else {
                            images::fixture_scene(frame, images, &source);
                        }
                    }
                    if overlay {
                        images.suppress();
                        let area = frame.area();
                        frame.render_widget(ratatui::widgets::Clear, area);
                        frame.render_widget(
                            ratatui::widgets::Paragraph::new("Overlay: images must be hidden"),
                            area,
                        );
                    }
                })
                .unwrap();
            match event::read().unwrap() {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('h') => hidden = !hidden,
                    KeyCode::Char('o') => overlay = !overlay,
                    KeyCode::Char('c') => guard.clear(&mut terminal).unwrap(),
                    KeyCode::Char('a') => acp_scene = !acp_scene,
                    KeyCode::Up => transcript.scroll(-1),
                    KeyCode::Down => transcript.scroll(1),
                    KeyCode::Home => transcript.boundary(false),
                    KeyCode::End => transcript.boundary(true),
                    _ => {}
                },
                Event::Resize(_, _) => guard.images.invalidate(),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn reader_pause_acknowledges_exclusive_terminal_ownership_until_resume() {
        let (keys, mut receiver) = mpsc::unbounded_channel();
        let (control, controls) = std::sync::mpsc::channel();
        let (sent, paused) = oneshot::channel();
        control.send(ReaderControl::Pause(sent)).unwrap();
        let worker = std::thread::spawn(move || {
            controlled_reader(keys, controls, || Some(Event::FocusGained))
        });
        paused.await.unwrap();
        assert!(receiver.try_recv().is_err());
        control.send(ReaderControl::Resume).unwrap();
        assert!(matches!(receiver.recv().await, Some(Event::FocusGained)));
        receiver.close();
        worker.join().unwrap();
    }

    #[test]
    fn multiline_paste_is_inserted_as_data_without_admitting_a_prompt() {
        let mut app = App {
            page: Page::Chat,
            input: "draft ".into(),
            ..Default::default()
        };
        paste(&mut app, &None, "one\r\ntwo\u{1b}[31m\u{3}");
        assert_eq!(app.input, "draft one\ntwo[31m");
        assert!(app.pending.is_empty());
    }

    #[test]
    fn questions_preserve_the_chat_draft_and_never_paste_permission_approval() {
        let (sender, _receiver) = oneshot::channel();
        let interaction = Interaction::Permission(
            v2::RequestPermissionRequest::new(v2::SessionId::new("one"), "Permission", vec![]),
            sender,
        );
        let mut app = App {
            input: "unsent draft".into(),
            ..Default::default()
        };
        show_interaction(&mut app, &interaction);
        paste(&mut app, &Some(interaction), "1\n");
        assert!(app.input.is_empty());
        app.input = "private answer".into();
        app.dialog = None;
        restore_draft(&mut app);
        assert_eq!(app.input, "unsent draft");
    }

    #[test]
    fn closing_the_keyboard_receiver_stops_an_idle_reader_without_another_key() {
        let (keys, receiver) = mpsc::unbounded_channel();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            read_keys(keys, || {
                let _ = started.send(());
                std::thread::sleep(Duration::from_millis(10));
                // A failing implementation eventually unblocks too, so it cannot hang the test runner.
                (std::time::Instant::now() >= deadline).then_some(Event::FocusGained)
            });
            let _ = finished.send(());
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(receiver);
        let result = done.recv_timeout(Duration::from_millis(500));
        worker.join().unwrap();
        result.expect("an idle keyboard reader must observe receiver closure");
    }

    #[test]
    fn named_profile_refresh_filters_agents_and_preserves_selection_independently() {
        let inventory = serde_json::json!({"agents":[{"id":"a"},{"id":"b"}], "defaultProfile":"personal", "profiles":[{"id":"personal","name":"Personal","agents":["b"],"projects":[],"defaultAgent":null,"defaultProject":null,"isDefault":true,"workspaces":{}}]});
        let mut app = App {
            page: Page::Profiles,
            ..App::default()
        };
        update_profiles(&mut app, &inventory);
        assert!(app.execution_profile.is_none());
        assert_eq!(app.profiles, ["", "b"]);
        app.profile_selected = 1;
        update_profiles(&mut app, &inventory);
        assert_eq!(app.profiles[app.profile_selected], "b");
        update_profiles(&mut app, &serde_json::json!({"agents":[{"id":"a"}]}));
        assert!(app.execution_profile.is_none());
        assert_eq!(app.profiles, ["a"]);
    }

    #[test]
    fn provider_refresh_preserves_selection_by_id_and_leaves_the_active_session_and_draft_intact() {
        let mut app = App {
            page: Page::Profiles,
            profiles: vec!["a".into(), "b".into()],
            selected: 1,
            active: Some("existing".into()),
            input: "unsent draft".into(),
            ..App::default()
        };
        app.update_sessions(vec![]);
        update_profiles(
            &mut app,
            &serde_json::json!({"agents":[{"id":"b"},{"id":"c"}]}),
        );
        assert_eq!(app.selected, 0);
        assert_eq!(app.profiles[app.selected], "b");
        update_profiles(&mut app, &serde_json::json!({"agents":[{"id":"c"}]}));
        assert_eq!(app.profiles[app.selected], "c");
        update_profiles(&mut app, &serde_json::json!({"agents":[]}));
        assert_eq!(app.selected, 0);
        assert_eq!(app.active.as_deref(), Some("existing"));
        assert_eq!(app.input, "unsent draft");
    }

    #[tokio::test]
    async fn the_tui_foreground_returns_on_eof_without_waiting_for_driver_termination() {
        let (transport, peer) = agent_client_protocol::Channel::duplex();
        let (updates, _) = mpsc::unbounded_channel();
        let (interactions, _) = mpsc::unbounded_channel();
        let (completions, _) = mpsc::unbounded_channel();
        // Keep the outbound receiver open: only incoming EOF should release the foreground.
        drop(peer.tx);
        tokio::time::timeout(
            Duration::from_secs(2),
            interaction_client(updates, interactions, completions, transport, async |cx| {
                gateway_disconnected(&cx).await;
                Ok(())
            }),
        )
        .await
        .expect("waiting for termination inside the foreground would deadlock EOF")
        .unwrap();
        drop(peer.rx);
    }

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
                    assert!(matches!(first?.outcome, v2::RequestPermissionOutcome::Cancelled(_)));
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
                        v2::CancelledPermissionOutcome::new().into(),
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
