//! ZJHive's modal client: the daemon catalog is authoritative, and a local
//! native ACP client bridges Zellij's completion-based WebAccess requests.

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use zellij_tile::prelude::*;

#[derive(Default)]
struct ZjHive {
    url: String,
    bridge: String,
    permitted: bool,
    command_permitted: bool,
    client: Option<String>,
    sessions: Vec<Value>,
    selected: usize,
    active: Option<String>,
    view: Value,
    seq: u64,
    lines: Vec<String>,
    input: String,
    status: String,
    stale: bool,
    skip_history: bool,
    sending: bool,
    tick: u64,
    last_view_tick: u64,
    last_catalog_tick: u64,
    serial: u64,
    epoch: u64,
    inflight: HashMap<u64, (String, u64)>,
    failures: u32,
    next_retry: u64,
    launching: bool,
}

register_plugin!(ZjHive);

impl ZjHive {
    fn request(&mut self, kind: &str, url: String, verb: HttpVerb, body: Value) {
        self.serial += 1;
        let serial = self.serial;
        self.inflight.insert(serial, (kind.into(), self.epoch));
        let mut context = BTreeMap::new();
        context.insert("serial".into(), serial.to_string());
        let mut headers = BTreeMap::new();
        if matches!(verb, HttpVerb::Post) {
            headers.insert("Content-Type".into(), "application/json".into());
        }
        web_request(
            url,
            verb,
            headers,
            if matches!(verb, HttpVerb::Post) {
                body.to_string().into_bytes()
            } else {
                vec![]
            },
            context,
        );
    }

    fn catalog(&mut self) {
        if self.permitted && !self.inflight.values().any(|(k, _)| k == "catalog") {
            self.request(
                "catalog",
                format!("{}/hive/v1/sessions", self.url.trim_end_matches('/')),
                HttpVerb::Get,
                Value::Null,
            );
        }
    }

    fn action(&mut self, kind: &str, payload: Value) {
        if !self.permitted {
            return;
        }
        let mut payload = payload;
        payload["action"] = json!(kind);
        if let Some(client) = &self.client {
            payload["client"] = json!(client);
        }
        self.request(
            kind,
            format!("{}/bridge", self.bridge.trim_end_matches('/')),
            HttpVerb::Post,
            payload,
        );
    }

    fn activate(&mut self, kind: &str) {
        let Some(session) = self.sessions.get(self.selected) else {
            return;
        };
        let Some(id) = session["sessionId"].as_str().map(str::to_owned) else {
            return;
        };
        let Some(cwd) = session["cwd"].as_str().map(str::to_owned) else {
            return;
        };
        if kind == "attach" || kind == "live" {
            self.epoch += 1;
            self.active = Some(id.clone());
            self.lines.clear();
            self.seq = 0;
            self.view = Value::Null;
        }
        self.action(kind, json!({"sessionId":id,"cwd":cwd}));
    }

    fn consume_view(&mut self, view: Value) {
        let Some(id) = view["sessionId"].as_str() else {
            return;
        };
        if self.active.as_deref() != Some(id) {
            return;
        }
        if view["seq"].as_u64().is_some_and(|n| n < self.seq) {
            self.status = "Daemon restarted or replay reset; reopen this session".into();
            self.stale = true;
            return;
        }
        if let Some(updates) = view["updates"].as_array() {
            for entry in updates {
                let Some(seq) = entry["seq"].as_u64() else {
                    continue;
                };
                if seq <= self.seq {
                    continue;
                }
                if seq != self.seq + 1 {
                    self.status = "History gap: press Ctrl-R for live-only attach".into();
                    self.stale = true;
                    self.skip_history = true;
                    return;
                }
                self.seq = seq;
                let update = &entry["update"];
                if let Some(text) = update["content"]["text"].as_str() {
                    let who = if update["sessionUpdate"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("user")
                    {
                        "You"
                    } else {
                        "Agent"
                    };
                    self.lines.push(format!("{who}: {text}"));
                } else if update["sessionUpdate"] == "state_update" {
                    self.lines.push(format!(
                        "Activity: {}",
                        update["state"].as_str().unwrap_or("unknown")
                    ));
                } else if update["sessionUpdate"] == "_hive_control" {
                    self.lines.push("Control changed".into());
                }
            }
        }
        self.view = view;
        self.stale = false;
        self.last_view_tick = self.tick;
        self.failures = 0;
        self.status = if self.view["pilot"] == true {
            "Pilot — Enter sends; Ctrl-C cancels; h relinquishes".into()
        } else {
            "Observer — t takes control; prompts are read-only".into()
        };
    }

    fn failed(&mut self, kind: &str, message: String) {
        self.status = message;
        if kind == "catalog" || kind == "poll" || kind == "register" {
            self.stale = true;
            self.failures = self.failures.saturating_add(1);
            self.next_retry = self.tick + (1 << self.failures.min(3));
        }
    }

    fn result(&mut self, code: u16, body: &[u8], context: BTreeMap<String, String>) -> bool {
        let Some(serial) = context.get("serial").and_then(|n| n.parse::<u64>().ok()) else {
            return false;
        };
        let Some((kind, epoch)) = self.inflight.remove(&serial) else {
            return false;
        };
        if epoch != self.epoch && kind != "catalog" && kind != "register" {
            return false;
        }
        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => {
                self.failed(&kind, format!("{kind}: invalid HTTP response ({code})"));
                return true;
            }
        };
        if code != 200 {
            if kind == "prompt" || kind == "retry" {
                self.sending = false;
            }
            let message = value["error"].as_str().unwrap_or("Request failed");
            self.failed(&kind, format!("{kind}: {message} (HTTP {code})"));
            if message.contains("History expired") {
                self.skip_history = true;
            }
            if code == 404 && kind != "catalog" {
                self.client = None;
            }
            return true;
        }
        match kind.as_str() {
            "catalog" => {
                if let Some(sessions) = value["sessions"].as_array() {
                    let selected_id = self
                        .sessions
                        .get(self.selected)
                        .and_then(|s| s["sessionId"].as_str())
                        .map(str::to_owned);
                    self.sessions = sessions.clone();
                    self.last_catalog_tick = self.tick;
                    if self.active.is_none() {
                        self.stale = false;
                    }
                    self.selected = selected_id
                        .and_then(|id| self.sessions.iter().position(|s| s["sessionId"] == id))
                        .unwrap_or_else(|| {
                            self.selected.min(self.sessions.len().saturating_sub(1))
                        });
                    self.failures = 0;
                    if self
                        .active
                        .as_ref()
                        .is_some_and(|id| !self.sessions.iter().any(|s| s["sessionId"] == *id))
                    {
                        self.active = None;
                        self.stale = true;
                        self.status = "Session no longer exists in the daemon".into();
                    }
                } else {
                    self.failed(&kind, "Invalid session catalog".into());
                }
            }
            "health" => self.action("register", json!({})),
            "register" => {
                self.client = value["client"].as_str().map(str::to_owned);
                self.status = "Select a Hive session with ↑/↓ and Enter".into();
                if let Some(index) = self
                    .sessions
                    .iter()
                    .position(|s| s["sessionId"] == self.active.as_deref().unwrap_or(""))
                {
                    self.selected = index;
                    self.activate("attach");
                }
            }
            "poll" | "attach" | "takeover" | "relinquish" | "live" => {
                if kind == "live" {
                    self.seq = value["seq"].as_u64().unwrap_or(0);
                    self.lines.push("[Earlier history unavailable]".into());
                }
                self.consume_view(value);
            }
            "prompt" => {
                self.sending = false;
                self.input.clear();
                self.status = "Prompt accepted".into();
            }
            "retry" => {
                self.sending = false;
                self.input.clear();
                self.status = "Prompt retry acknowledged".into();
            }
            "detach" => {
                self.active = None;
                self.view = Value::Null;
                self.epoch += 1;
            }
            _ => {
                self.status = format!("{kind} succeeded");
            }
        }
        true
    }

    fn on_key(&mut self, key: KeyWithModifier) -> bool {
        let ctrl = key.key_modifiers.contains(&KeyModifier::Ctrl);
        if ctrl && key.bare_key == BareKey::Char('c') && self.view["pilot"] == true && !self.stale {
            self.action("cancel", json!({}));
            return true;
        }
        if let Some(request) = self.view["interaction"]["request"].as_object()
            && self.view["pilot"] == true
            && !self.stale
        {
            match key.bare_key {
                BareKey::Esc => {
                    self.action("answer", json!({"answer": if self.view["interaction"]["type"] == "permission" { json!({}) } else { json!({"action":"decline"}) }}));
                    return true;
                }
                BareKey::Enter => {
                    let answer = if self.view["interaction"]["type"] == "permission" {
                        let Some(option) = request
                            .get("options")
                            .and_then(Value::as_array)
                            .and_then(|a| {
                                self.input
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|n| n.checked_sub(1))
                                    .and_then(|index| a.get(index))
                            })
                        else {
                            self.status = "Enter a listed permission number".into();
                            return true;
                        };
                        json!({"optionId":option["optionId"]})
                    } else if request.get("mode").and_then(Value::as_str) == Some("url") {
                        json!({"action":"accept"})
                    } else {
                        let mut content = serde_json::Map::new();
                        for pair in self.input.split(',').filter(|p| !p.trim().is_empty()) {
                            let Some((k, v)) = pair.split_once('=') else {
                                self.status = "Use key=value pairs".into();
                                return true;
                            };
                            content.insert(k.trim().into(), json!(v.trim()));
                        }
                        json!({"action":"accept", "content":content})
                    };
                    self.action("answer", json!({"answer":answer}));
                    self.input.clear();
                    return true;
                }
                BareKey::Backspace => {
                    self.input.pop();
                    return true;
                }
                BareKey::Char(ch) if !ctrl => {
                    self.input.push(ch);
                    return true;
                }
                _ => return false,
            }
        }
        match key.bare_key {
            BareKey::Up if self.active.is_none() => self.selected = self.selected.saturating_sub(1),
            BareKey::Down if self.active.is_none() => {
                self.selected = (self.selected + 1).min(self.sessions.len().saturating_sub(1))
            }
            BareKey::Enter if self.active.is_none() && self.client.is_some() => {
                self.activate("attach")
            }
            BareKey::Enter
                if self.view["pilot"] == true
                    && !self.stale
                    && !self.sending
                    && !self.input.trim().is_empty() =>
            {
                let text = self.input.clone();
                self.sending = true;
                self.action("prompt", json!({"text":text,"key":format!("zellij-{}-{}", self.client.as_deref().unwrap_or(""), self.serial + 1)}));
                self.status = "Awaiting prompt acknowledgement…".into();
            }
            BareKey::Backspace if self.active.is_some() => {
                self.input.pop();
            }
            BareKey::Char('t') if ctrl && self.active.is_some() && !self.stale => {
                self.activate("takeover")
            }
            BareKey::Char('h') if ctrl && self.active.is_some() && self.view["pilot"] == true => {
                self.activate("relinquish")
            }
            BareKey::Char('r') if ctrl && self.skip_history && self.active.is_some() => {
                self.skip_history = false;
                self.activate("live");
            }
            BareKey::Char('r') if ctrl && self.active.is_some() => self.action("retry", json!({})),
            BareKey::Esc if self.active.is_some() => {
                self.action("detach", json!({}));
                // A fresh ACP connection keeps a late detach from affecting the next selection.
                self.client = None;
                self.active = None;
                self.view = Value::Null;
                self.epoch += 1;
                self.input.clear();
                self.sending = false;
                self.status = "Detached; the Hive session remains running".into();
            }
            BareKey::Char(ch)
                if self.active.is_some() && self.view["pilot"] == true && !self.stale && !ctrl =>
            {
                self.input.push(ch)
            }
            _ => return false,
        }
        true
    }
}

impl ZellijPlugin for ZjHive {
    fn load(&mut self, config: BTreeMap<String, String>) {
        self.url = config
            .get("url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:8765".into());
        self.bridge = config
            .get("bridge")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:8766".into());
        self.status = "Waiting for WebAccess and RunCommands permissions".into();
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::WebRequestResult,
            EventType::RunCommandResult,
            EventType::Timer,
            EventType::Key,
        ]);
        request_permission(&[PermissionType::WebAccess, PermissionType::RunCommands]);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permitted = true;
                self.command_permitted = true;
                self.catalog();
                // A running helper answers health; a missing one is launched below.
                self.request(
                    "health",
                    format!("{}/health", self.bridge),
                    HttpVerb::Get,
                    Value::Null,
                );
                set_timeout(2.0);
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.status = "WebAccess/RunCommands denied; grant permissions to attach".into();
                true
            }
            Event::WebRequestResult(code, _, body, context) => {
                if context
                    .get("serial")
                    .and_then(|s| s.parse::<u64>().ok())
                    .and_then(|s| self.inflight.get(&s))
                    .is_some_and(|(kind, _)| kind == "health")
                    && code != 200
                {
                    if let Some(serial) = context.get("serial").and_then(|s| s.parse::<u64>().ok())
                    {
                        self.inflight.remove(&serial);
                    }
                    if self.command_permitted && !self.launching {
                        let listen = self.bridge.trim_start_matches("http://").to_owned();
                        run_command(
                            &[
                                "hive",
                                "zellij-bridge",
                                "--listen",
                                &listen,
                                "--url",
                                &self.url,
                            ],
                            BTreeMap::new(),
                        );
                        self.launching = true;
                        self.status = "Starting local Hive ACP bridge…".into();
                    }
                    return true;
                }
                self.result(code, &body, context)
            }
            Event::RunCommandResult(_, _, _, _) => {
                self.launching = false;
                self.status = "Bridge process exited; retrying".into();
                true
            }
            Event::Timer(_) => {
                self.tick += 1;
                // A request whose host callback never arrives must not leave pilot controls enabled.
                if self.active.is_some() && self.tick.saturating_sub(self.last_view_tick) > 3
                    || self.active.is_none() && self.tick.saturating_sub(self.last_catalog_tick) > 4
                {
                    self.stale = true;
                }
                if self.permitted && self.tick >= self.next_retry {
                    if self.tick.is_multiple_of(2) {
                        self.catalog();
                    }
                    if self.client.is_none()
                        && !self
                            .inflight
                            .values()
                            .any(|(k, _)| k == "health" || k == "register")
                    {
                        self.request(
                            "health",
                            format!("{}/health", self.bridge),
                            HttpVerb::Get,
                            Value::Null,
                        );
                    }
                    if self.active.is_some()
                        && self.client.is_some()
                        && !self.inflight.values().any(|(k, _)| k == "poll")
                    {
                        self.action("poll", json!({"seq":self.seq+1}));
                    }
                }
                set_timeout(2.0);
                true
            }
            Event::Key(key) => self.on_key(key),
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        let mut lines = vec!["╭ ZJHive · Hive sessions ╮".to_owned()];
        if self.active.is_none() {
            if self.sessions.is_empty() {
                lines.push("No Hive sessions; start one in TUI/WebUI".into());
            }
            for (i, session) in self.sessions.iter().enumerate() {
                let owner = if session["controlOwner"].is_null() {
                    "free"
                } else {
                    "owned"
                };
                lines.push(format!(
                    "{} {} · {} · {} · {}",
                    if i == self.selected { "▶" } else { " " },
                    session["sessionId"].as_str().unwrap_or("?"),
                    session["title"]
                        .as_str()
                        .unwrap_or(session["agent"].as_str().unwrap_or("?")),
                    session["activity"].as_str().unwrap_or("unknown"),
                    owner
                ));
            }
            lines.push("↑/↓ select · Enter observe".into());
        } else {
            let owner = self
                .sessions
                .iter()
                .find(|s| s["sessionId"] == self.active.as_deref().unwrap_or(""))
                .is_some_and(|s| !s["controlOwner"].is_null());
            let control = if self.view["pilot"] == true {
                "pilot"
            } else if owner {
                "another client owns control"
            } else {
                "no pilot"
            };
            lines.push(format!(
                "{} · {} · {}",
                self.active.as_deref().unwrap_or("?"),
                control,
                if self.view["running"] == true {
                    "running"
                } else {
                    "idle/unknown"
                }
            ));
            let visible = rows.saturating_sub(7);
            lines.extend(self.lines.iter().rev().take(visible).rev().cloned());
            if !self.view["interaction"].is_null() {
                let req = &self.view["interaction"]["request"];
                if self.view["interaction"]["type"] == "permission" {
                    lines.push(format!(
                        "Permission: {}",
                        req["title"].as_str().unwrap_or("Agent request")
                    ));
                    if let Some(options) = req["options"].as_array() {
                        for (i, option) in options.iter().enumerate() {
                            lines.push(format!(
                                "{}: {}",
                                i + 1,
                                option["name"].as_str().unwrap_or("?")
                            ));
                        }
                    }
                    lines.push("Enter option number · Esc decline".into());
                } else {
                    lines.push(format!(
                        "Input: {}",
                        req["message"].as_str().unwrap_or("Agent request")
                    ));
                    if req["mode"] == "url" {
                        lines.push(format!(
                            "Open {} then press Enter",
                            req["url"].as_str().unwrap_or("?")
                        ));
                    } else {
                        let fields = req["requestedSchema"]["properties"]
                            .as_object()
                            .map(|p| p.keys().cloned().collect::<Vec<_>>().join(", "))
                            .unwrap_or_default();
                        lines.push(format!("Fields: {fields} · enter key=value pairs"));
                    }
                    lines.push("Enter accept · Esc decline".into());
                }
            }
            lines.push(format!("› {}", self.input));
            lines.push("Ctrl-T takeover · Ctrl-H release · Ctrl-R retry · Esc detach".into());
        }
        lines.push(format!(
            "{}{}",
            if self.stale { "STALE · " } else { "" },
            self.status
        ));
        for line in lines.into_iter().take(rows) {
            // Control characters in daemon-provided titles or chunks must not escape the pane.
            let printable: String = line
                .chars()
                .filter(|ch| !ch.is_control())
                .take(cols)
                .collect();
            println!("{printable}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn key(bare_key: BareKey, ctrl: bool) -> KeyWithModifier {
        KeyWithModifier {
            bare_key,
            key_modifiers: if ctrl {
                BTreeSet::from([KeyModifier::Ctrl])
            } else {
                BTreeSet::new()
            },
        }
    }

    fn deliver(plugin: &mut ZjHive, kind: &str, code: u16, value: Value) {
        plugin.serial += 1;
        let serial = plugin.serial;
        plugin.inflight.insert(serial, (kind.into(), plugin.epoch));
        assert!(plugin.result(
            code,
            value.to_string().as_bytes(),
            BTreeMap::from([("serial".into(), serial.to_string())])
        ));
    }

    #[test]
    fn a_late_selection_result_cannot_replace_the_active_session() {
        let mut plugin = ZjHive {
            permitted: true,
            sessions: vec![
                json!({"sessionId":"hive-a", "cwd":"/tmp"}),
                json!({"sessionId":"hive-b", "cwd":"/tmp"}),
            ],
            ..ZjHive::default()
        };
        plugin.activate("attach");
        plugin.selected = 1;
        plugin.activate("attach");
        assert_eq!(plugin.active.as_deref(), Some("hive-b"));
        plugin.consume_view(json!({"sessionId":"hive-a", "updates":[]}));
        assert_eq!(plugin.active.as_deref(), Some("hive-b"));
    }

    #[test]
    fn missing_updates_are_not_presented_as_current_activity() {
        let mut plugin = ZjHive {
            active: Some("hive-a".into()),
            seq: 3,
            ..ZjHive::default()
        };
        plugin.consume_view(json!({"sessionId":"hive-a","seq":5,"pilot":true,"updates":[{"seq":5,"update":{"sessionUpdate":"state_update","state":"idle"}}]}));
        assert!(plugin.stale);
        assert!(plugin.skip_history);
        assert!(plugin.lines.is_empty());
        assert_ne!(plugin.view["pilot"], true);
    }

    #[test]
    fn a_pilot_loses_controls_when_the_next_snapshot_reports_an_observer() {
        let mut plugin = ZjHive {
            active: Some("hive-a".into()),
            ..ZjHive::default()
        };
        plugin.consume_view(json!({"sessionId":"hive-a","seq":0,"pilot":true,"updates":[]}));
        assert_eq!(plugin.view["pilot"], true);
        plugin.consume_view(json!({"sessionId":"hive-a","seq":0,"pilot":false,"updates":[]}));
        assert_eq!(plugin.view["pilot"], false);
    }

    #[test]
    fn a_late_poll_result_from_an_old_selection_is_ignored() {
        let mut plugin = ZjHive {
            active: Some("hive-new".into()),
            epoch: 2,
            ..ZjHive::default()
        };
        plugin.inflight.insert(7, ("poll".into(), 1));
        assert!(!plugin.result(
            200,
            br#"{"sessionId":"hive-old","pilot":true}"#,
            BTreeMap::from([("serial".into(), "7".into())])
        ));
        assert_eq!(plugin.active.as_deref(), Some("hive-new"));
        assert_eq!(plugin.view, Value::Null);
    }

    #[test]
    fn a_catalog_selection_observes_before_takeover_and_only_the_pilot_can_prompt() {
        let mut plugin = ZjHive {
            permitted: true,
            client: Some("client".into()),
            ..ZjHive::default()
        };
        deliver(
            &mut plugin,
            "catalog",
            200,
            json!({"sessions":[
                {"sessionId":"hive-one", "cwd":"/work", "activity":"idle", "controlOwner":"old"},
                {"sessionId":"hive-two", "cwd":"/other", "activity":"unknown", "controlOwner":null}
            ]}),
        );
        assert!(plugin.on_key(key(BareKey::Down, false)));
        assert_eq!(plugin.selected, 1);
        assert!(plugin.on_key(key(BareKey::Up, false)));
        assert!(plugin.on_key(key(BareKey::Enter, false)));
        assert_eq!(plugin.active.as_deref(), Some("hive-one"));
        assert!(plugin.lines.is_empty());
        deliver(
            &mut plugin,
            "attach",
            200,
            json!({"sessionId":"hive-one","pilot":false,"running":false,"seq":3,"updates":[
                {"seq":1,"update":{"sessionUpdate":"user_message_chunk","content":{"text":"hello"}}},
                {"seq":2,"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"world"}}},
                {"seq":3,"update":{"sessionUpdate":"state_update","state":"idle"}}
            ]}),
        );
        assert_eq!(plugin.seq, 3);
        assert_eq!(plugin.lines.len(), 3);
        assert!(!plugin.on_key(key(BareKey::Char('x'), false)));
        assert!(plugin.on_key(key(BareKey::Char('t'), true)));
        deliver(
            &mut plugin,
            "takeover",
            200,
            json!({"sessionId":"hive-one","pilot":true,"running":false,"seq":4,"updates":[
                {"seq":4,"update":{"sessionUpdate":"_hive_control"}}
            ]}),
        );
        assert!(plugin.on_key(key(BareKey::Char('t'), false)));
        assert!(plugin.on_key(key(BareKey::Char('h'), false)));
        assert!(plugin.on_key(key(BareKey::Char('r'), false)));
        assert!(plugin.on_key(key(BareKey::Char('e'), false)));
        assert!(plugin.on_key(key(BareKey::Char('e'), false)));
        assert_eq!(plugin.input, "three");
        assert!(plugin.on_key(key(BareKey::Enter, false)));
        assert!(plugin.sending);
        assert!(!plugin.on_key(key(BareKey::Enter, false)));
        deliver(&mut plugin, "prompt", 200, json!({"accepted":true}));
        assert!(!plugin.sending);
        assert!(plugin.input.is_empty());
        assert!(plugin.on_key(key(BareKey::Char('c'), true)));
        assert!(plugin.on_key(key(BareKey::Char('h'), true)));
        deliver(
            &mut plugin,
            "relinquish",
            200,
            json!({"sessionId":"hive-one","pilot":false,"running":false,"seq":5,"updates":[
                {"seq":5,"update":{"sessionUpdate":"_hive_control"}}
            ]}),
        );
        assert!(!plugin.on_key(key(BareKey::Char('c'), true)));
        assert!(plugin.on_key(key(BareKey::Esc, false)));
        assert!(plugin.active.is_none());
        assert!(plugin.client.is_none());
        plugin.render(3, 12);
    }

    #[test]
    fn missing_history_and_failed_requests_require_an_explicit_live_only_retry() {
        let mut plugin = ZjHive {
            permitted: true,
            client: Some("client".into()),
            active: Some("hive-one".into()),
            sessions: vec![json!({"sessionId":"hive-one","cwd":"/work","activity":"unknown"})],
            ..ZjHive::default()
        };
        deliver(
            &mut plugin,
            "attach",
            409,
            json!({"error":"History expired; attach without history explicitly"}),
        );
        assert!(plugin.skip_history);
        assert!(plugin.on_key(key(BareKey::Char('r'), true)));
        assert_eq!(plugin.active.as_deref(), Some("hive-one"));
        deliver(
            &mut plugin,
            "live",
            200,
            json!({"sessionId":"hive-one","seq":260,"updates":[],"pilot":false,"running":true}),
        );
        assert_eq!(plugin.seq, 260);
        assert!(!plugin.stale);
        assert!(
            plugin
                .lines
                .iter()
                .any(|line| line.contains("Earlier history unavailable"))
        );
        deliver(
            &mut plugin,
            "poll",
            503,
            json!({"error":"ACP client reconnecting"}),
        );
        assert!(plugin.stale);
        assert!(!plugin.on_key(key(BareKey::Char('t'), true)));
        deliver(
            &mut plugin,
            "poll",
            200,
            json!({"sessionId":"hive-one","seq":261,"updates":[
            {"seq":261,"update":{"sessionUpdate":"state_update","state":"idle"}}
        ],"pilot":false}),
        );
        assert!(!plugin.stale);
        assert_eq!(plugin.seq, 261);
        plugin.render(6, 26);
    }

    #[test]
    fn pilot_input_actions_are_validated_before_the_bridge_receives_them() {
        let mut plugin = ZjHive {
            permitted: true,
            client: Some("client".into()),
            active: Some("hive-one".into()),
            ..ZjHive::default()
        };
        plugin.consume_view(json!({"sessionId":"hive-one","seq":0,"updates":[],"pilot":true,
            "interaction":{"type":"permission","request":{"title":"Run tool","options":[{"optionId":"allow","name":"Allow once"}]}}}));
        plugin.render(8, 40);
        assert!(plugin.on_key(key(BareKey::Enter, false)));
        assert!(plugin.status.contains("listed permission"));
        assert!(plugin.on_key(key(BareKey::Char('1'), false)));
        assert!(plugin.on_key(key(BareKey::Enter, false)));
        assert!(plugin.inflight.values().any(|(kind, _)| kind == "answer"));
        deliver(&mut plugin, "answer", 200, json!({"answered":true}));
        plugin.consume_view(json!({"sessionId":"hive-one","seq":0,"updates":[],"pilot":true,
            "interaction":{"type":"elicitation","request":{"message":"Approve?","mode":"form","requestedSchema":{"properties":{"approved":{"type":"boolean"}}}}}}));
        plugin.render(8, 40);
        assert!(plugin.on_key(key(BareKey::Char('x'), false)));
        assert!(plugin.on_key(key(BareKey::Enter, false)));
        assert!(plugin.status.contains("key=value"));
        assert!(plugin.on_key(key(BareKey::Backspace, false)));
        for ch in "approved=true".chars() {
            assert!(plugin.on_key(key(BareKey::Char(ch), false)));
        }
        assert!(plugin.on_key(key(BareKey::Enter, false)));
        plugin.consume_view(json!({"sessionId":"hive-one","seq":0,"updates":[],"pilot":true,
            "interaction":{"type":"elicitation","request":{"message":"Authenticate","mode":"url","url":"https://example.com"}}}));
        assert!(plugin.on_key(key(BareKey::Enter, false)));
        assert!(plugin.on_key(key(BareKey::Esc, false)));
        plugin.render(8, 40);
    }

    #[test]
    fn permission_and_bridge_failures_leave_the_plugin_read_only_until_a_fresh_poll() {
        let mut plugin = ZjHive::default();
        plugin.load(BTreeMap::from([
            ("url".into(), "http://127.0.0.1:9901".into()),
            ("bridge".into(), "http://127.0.0.1:9902".into()),
        ]));
        assert_eq!(plugin.url, "http://127.0.0.1:9901");
        assert!(plugin.update(Event::PermissionRequestResult(PermissionStatus::Denied)));
        assert!(!plugin.permitted);
        assert!(plugin.status.contains("denied"));
        assert!(plugin.update(Event::PermissionRequestResult(PermissionStatus::Granted)));
        assert!(plugin.permitted);
        assert!(plugin.inflight.values().any(|(kind, _)| kind == "health"));
        let health = plugin
            .inflight
            .iter()
            .find(|(_, (kind, _))| kind == "health")
            .map(|(serial, _)| *serial)
            .unwrap();
        assert!(plugin.update(Event::WebRequestResult(
            503,
            BTreeMap::new(),
            b"{}".to_vec(),
            BTreeMap::from([("serial".into(), health.to_string())])
        )));
        assert!(plugin.launching);
        assert!(plugin.status.contains("Starting"));
        assert!(plugin.update(Event::RunCommandResult(
            Some(1),
            Vec::new(),
            Vec::new(),
            BTreeMap::new()
        )));
        assert!(!plugin.launching);
        deliver(
            &mut plugin,
            "catalog",
            200,
            json!({"sessions":[{"sessionId":"hive-one","cwd":"/work","activity":"running","controlOwner":null}]}),
        );
        plugin.render(6, 30);
        plugin.client = Some("client".into());
        plugin.active = Some("hive-one".into());
        plugin.consume_view(json!({"sessionId":"hive-one","seq":0,"updates":[],"pilot":true}));
        plugin.next_retry = 100;
        for _ in 0..4 {
            assert!(plugin.update(Event::Timer(0.0)));
        }
        assert!(plugin.stale);
        assert!(!plugin.on_key(key(BareKey::Char('t'), true)));
        plugin.consume_view(json!({"sessionId":"hive-one","seq":0,"updates":[],"pilot":true}));
        assert!(!plugin.stale);
        assert!(plugin.update(Event::Key(key(BareKey::Char('c'), true))));
        plugin.render(3, 12);
    }

    #[test]
    fn malformed_catalogs_and_expired_bridge_handles_do_not_keep_stale_control() {
        let mut plugin = ZjHive {
            permitted: true,
            client: Some("old".into()),
            active: Some("hive-one".into()),
            ..ZjHive::default()
        };
        deliver(&mut plugin, "catalog", 200, json!({"unexpected":true}));
        assert!(plugin.stale);
        deliver(
            &mut plugin,
            "poll",
            404,
            json!({"error":"Bridge connection expired; reconnect"}),
        );
        assert!(plugin.client.is_none());
        assert!(plugin.status.contains("expired"));
        deliver(&mut plugin, "health", 200, json!({"ready":true}));
        assert!(plugin.inflight.values().any(|(kind, _)| kind == "register"));
        plugin.sessions = vec![json!({"sessionId":"hive-one","cwd":"/work","activity":"unknown"})];
        deliver(&mut plugin, "register", 200, json!({"client":"new"}));
        assert_eq!(plugin.client.as_deref(), Some("new"));
        assert!(plugin.inflight.values().any(|(kind, _)| kind == "attach"));
        deliver(&mut plugin, "catalog", 200, json!({"sessions":[]}));
        assert!(plugin.active.is_none());
        assert!(plugin.status.contains("no longer exists"));
    }
}
