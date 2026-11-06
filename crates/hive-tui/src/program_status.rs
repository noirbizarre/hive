//! OSC 7501 transport; agent state remains independent of terminal presentation.

use std::{
    io::Write,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::{
    capabilities::{Capability, CapabilitySource, Support},
    state::TurnOutcome,
    ui::App,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }
    const ALL: [Self; 5] = [
        Self::Idle,
        Self::Working,
        Self::Done,
        Self::Blocked,
        Self::Error,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
    const ALL: [Self; 3] = [Self::Permission, Self::Question, Self::Auth];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Features {
    states: Vec<State>,
    kinds: Vec<Kind>,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            states: State::ALL.to_vec(),
            kinds: Kind::ALL.to_vec(),
        }
    }
}

impl Features {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        // Include the OSC prefix and longest terminator in the protocol's hard cap.
        if bytes.len() > 4096 - 9 {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let mut pairs = text.split(':');
        if pairs.next()?.trim() != "?" {
            return None;
        }
        let mut features = Self::default();
        for pair in pairs {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.,+/=-".contains(&byte))
            {
                continue;
            }
            match key.trim() {
                "states" => {
                    features.states = State::ALL
                        .into_iter()
                        .filter(|s| value.trim().split(',').any(|v| v == s.name()))
                        .collect()
                }
                "kinds" => {
                    features.kinds = Kind::ALL
                        .into_iter()
                        .filter(|k| value.trim().split(',').any(|v| v == k.name()))
                        .collect()
                }
                _ => {}
            }
        }
        Some(features)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub state: State,
    pub kind: Option<Kind>,
    pub title: String,
    pub message: &'static str,
}

pub fn project(app: &App, kind: Option<Kind>) -> Option<Report> {
    let id = app.active.as_deref()?;
    let conversation = app.conversation()?;
    if !app.connected || conversation.gap || conversation.activity["session"] == "ended" {
        return None;
    }
    let (state, message) = match conversation.foreground.as_str() {
        "running" => (State::Working, "Agent working"),
        "requires_action" => (State::Blocked, "Waiting for user input"),
        "idle" => match conversation.outcome {
            Some(TurnOutcome::Done) => (State::Done, "Turn completed"),
            Some(TurnOutcome::Error) => (State::Error, "Agent failed or declined the request"),
            _ => (State::Idle, "Waiting for instructions"),
        },
        _ => return None,
    };
    Some(Report {
        state: if kind.is_some() {
            State::Blocked
        } else {
            state
        },
        kind,
        title: id.to_owned(),
        message: if kind.is_some() {
            "Waiting for user input"
        } else {
            message
        },
    })
}

fn text(value: &str, limit: usize) -> String {
    // The protocol rejects the entire report for even one decoded control character.
    let mut safe = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if safe.len() + ch.len_utf8() > limit {
            break;
        }
        safe.push(ch);
    }
    STANDARD.encode(safe)
}

pub struct Reporter {
    id: String,
    features: Option<Features>,
    pending: Option<Instant>,
    last: Option<String>,
    owned: bool,
    failed: bool,
}

impl Default for Reporter {
    fn default() -> Self {
        // A separate id prevents editor/shell status cleanup from erasing unrelated records.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self {
            id: format!(
                "hive-{}-{:x}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            features: None,
            pending: None,
            last: None,
            owned: false,
            failed: false,
        }
    }
}

impl Reporter {
    pub fn start(&mut self, cap: Capability, writer: &mut impl Write) {
        if cap.is_usable() {
            self.features = Some(Features::default());
        } else if cfg!(unix)
            && cap.source == CapabilitySource::Automatic
            && cap.effective == Support::Unknown
        {
            self.pending = Some(Instant::now());
            if writer
                .write_all(b"\x1b]7501;?\x1b\\")
                .and_then(|()| writer.flush())
                .is_err()
            {
                self.failed = true;
                self.pending = None;
            }
        }
    }

    pub fn receive(&mut self, features: Features, received: Instant, cap: &mut Capability) {
        // Delayed/unsolicited replies must not override explicit policy or revive a timeout.
        let Some(start) = self.pending.take() else {
            return;
        };
        if received.saturating_duration_since(start) <= Duration::from_millis(500) && !self.failed {
            cap.detected = Support::Supported;
            cap.effective = Support::Supported;
            self.features = Some(features);
        } else {
            cap.detected = Support::Unsupported;
            cap.effective = Support::Unsupported;
        }
    }

    pub fn tick(&mut self, cap: &mut Capability) {
        if self
            .pending
            .is_some_and(|start| start.elapsed() > Duration::from_millis(500))
        {
            self.pending = None;
            cap.detected = Support::Unsupported;
            cap.effective = Support::Unsupported;
        }
    }

    fn encode(&self, mut report: Report) -> Option<String> {
        let features = self.features.as_ref()?;
        if !features.states.contains(&report.state) {
            // Fall back to a truthful coarse state, never success or activity invented from idle.
            report.state = match report.state {
                State::Blocked if features.states.contains(&State::Working) => State::Working,
                State::Done | State::Error if features.states.contains(&State::Idle) => State::Idle,
                _ => return None,
            };
        }
        let mut encoded = format!(
            "\x1b]7501;state={}:id={}:app=hive:title={}:msg={}",
            report.state.name(),
            self.id,
            text(&report.title, 192),
            text(report.message, 2048)
        );
        if report.state == State::Blocked
            && let Some(kind) = report.kind.filter(|k| features.kinds.contains(k))
        {
            encoded.push_str(":kind=");
            encoded.push_str(kind.name());
        }
        encoded.push_str("\x1b\\");
        Some(encoded)
    }

    pub fn sync(&mut self, report: Option<Report>, writer: &mut impl Write) {
        if self.failed {
            return;
        }
        let Some(encoded) = report.and_then(|r| self.encode(r)) else {
            let _ = self.clear(writer);
            return;
        };
        if self.last.as_ref() == Some(&encoded) {
            return;
        }
        self.owned = true;
        if writer
            .write_all(encoded.as_bytes())
            .and_then(|()| writer.flush())
            .is_ok()
        {
            self.last = Some(encoded);
        } else {
            self.failed = true;
            let _ = self.clear(writer);
        }
    }

    pub fn clear(&mut self, writer: &mut impl Write) -> std::io::Result<()> {
        self.last = None;
        if self.owned {
            write!(writer, "\x1b]7501;state=clear:id={}\x1b\\", self.id)?;
            writer.flush()?;
            self.owned = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Reporter {
        Reporter {
            id: "hive-test".into(),
            features: Some(Features::default()),
            ..Reporter::default()
        }
    }

    fn report(state: State) -> Report {
        Report {
            state,
            kind: None,
            title: "session".into(),
            message: "Agent working",
        }
    }

    #[test]
    fn reports_replace_all_fields_and_cleanup_only_the_owned_record() {
        let mut reporter = enabled();
        let mut bytes = Vec::new();
        let mut blocked = report(State::Blocked);
        blocked.kind = Some(Kind::Permission);
        reporter.sync(Some(blocked.clone()), &mut bytes);
        let first = bytes.len();
        reporter.sync(Some(blocked), &mut bytes);
        assert_eq!(first, bytes.len());
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            "\x1b]7501;state=blocked:id=hive-test:app=hive:title=c2Vzc2lvbg==:msg=QWdlbnQgd29ya2luZw==:kind=permission\x1b\\"
        );
        reporter.sync(Some(report(State::Idle)), &mut bytes);
        assert!(!String::from_utf8_lossy(&bytes[first..]).contains("kind="));
        reporter.clear(&mut bytes).unwrap();
        assert!(bytes.ends_with(b"\x1b]7501;state=clear:id=hive-test\x1b\\"));
        let end = bytes.len();
        reporter.clear(&mut bytes).unwrap();
        assert_eq!(end, bytes.len());
    }

    #[test]
    fn detection_distinguishes_missing_empty_and_repeated_feature_lists() {
        assert_eq!(Features::parse(b"?"), Some(Features::default()));
        let features = Features::parse(
            b"? : states=working : kinds= : future=yes : states=idle,blocked,future",
        )
        .unwrap();
        assert_eq!(features.states, vec![State::Idle, State::Blocked]);
        assert!(features.kinds.is_empty());
        assert!(Features::parse(b"state=working").is_none());
        assert_eq!(
            Features::parse(b"?:states=idle;malformed"),
            Some(Features::default())
        );
        assert!(Features::parse(&vec![b'?'; 4096]).is_none());
    }

    #[test]
    fn feature_subsets_degrade_without_claiming_unrelated_states() {
        let mut reporter = enabled();
        reporter.features = Features::parse(b"?:states=idle,working:kinds=");
        assert!(
            reporter
                .encode(report(State::Blocked))
                .unwrap()
                .contains("state=working:")
        );
        for state in [State::Done, State::Error] {
            assert!(
                reporter
                    .encode(report(state))
                    .unwrap()
                    .contains("state=idle:")
            );
        }
        reporter.features = Features::parse(b"?:states=blocked:kinds=");
        let mut blocked = report(State::Blocked);
        blocked.kind = Some(Kind::Auth);
        assert!(!reporter.encode(blocked).unwrap().contains("kind="));
        assert!(reporter.encode(report(State::Working)).is_none());
    }

    #[test]
    fn encoded_context_is_single_line_and_bounded_at_utf8_boundaries() {
        let value = text(&format!("\x1b\n\t\u{7f}\u{85}{}", "界".repeat(2000)), 192);
        let decoded = String::from_utf8(STANDARD.decode(value).unwrap()).unwrap();
        assert!(decoded.len() <= 192);
        assert!(!decoded.chars().any(char::is_control));
        let mut large = report(State::Working);
        large.title = "界".repeat(2000);
        assert!(enabled().encode(large).unwrap().len() <= 4096);
    }

    #[test]
    fn disabled_and_unavailable_transports_emit_nothing_even_during_cleanup() {
        for source in [
            CapabilitySource::Override,
            CapabilitySource::UnavailableTransport,
        ] {
            let mut reporter = Reporter::default();
            let mut bytes = Vec::new();
            reporter.start(
                Capability {
                    source,
                    effective: Support::Unsupported,
                    detected: Support::Unknown,
                },
                &mut bytes,
            );
            reporter.receive(
                Features::default(),
                Instant::now(),
                &mut Capability::default(),
            );
            reporter.sync(Some(report(State::Working)), &mut bytes);
            reporter.clear(&mut bytes).unwrap();
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn negotiation_times_out_and_late_replies_cannot_revive_it() {
        let mut reporter = Reporter::default();
        let mut cap = Capability::default();
        let mut bytes = Vec::new();
        reporter.start(cap, &mut bytes);
        if cfg!(unix) {
            assert_eq!(bytes, b"\x1b]7501;?\x1b\\");
        }
        reporter.pending = Some(Instant::now() - Duration::from_secs(1));
        reporter.tick(&mut cap);
        reporter.receive(Features::default(), Instant::now(), &mut cap);
        assert_eq!(cap.effective, Support::Unsupported);
        assert!(reporter.features.is_none());
    }

    #[test]
    fn promptly_received_replies_survive_slow_acp_startup() {
        let mut reporter = Reporter::default();
        let start = Instant::now() - Duration::from_secs(1);
        reporter.pending = Some(start);
        let mut cap = Capability::default();
        reporter.receive(
            Features::default(),
            start + Duration::from_millis(10),
            &mut cap,
        );
        assert!(cap.is_usable());
    }

    #[test]
    fn failed_flushes_preserve_cleanup_ownership_and_disable_new_reports() {
        struct BrokenFlush(Vec<u8>);
        impl Write for BrokenFlush {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("fixture flush"))
            }
        }
        let mut reporter = enabled();
        let mut writer = BrokenFlush(Vec::new());
        reporter.sync(Some(report(State::Working)), &mut writer);
        assert!(reporter.owned && reporter.failed);
        let length = writer.0.len();
        reporter.sync(Some(report(State::Done)), &mut writer);
        assert_eq!(length, writer.0.len());
        let mut cleanup = Vec::new();
        reporter.clear(&mut cleanup).unwrap();
        assert_eq!(cleanup, b"\x1b]7501;state=clear:id=hive-test\x1b\\");
    }

    #[test]
    fn selected_session_transitions_replace_status_and_stale_context_is_cleared() {
        let mut app = App {
            connected: true,
            ..App::default()
        };
        app.select_session("first".into());
        app.conversations.entry("first".into()).or_default();
        assert!(project(&app, None).is_none());
        for (foreground, outcome, kind, expected) in [
            ("running", None, None, State::Working),
            (
                "requires_action",
                None,
                Some(Kind::Permission),
                State::Blocked,
            ),
            ("running", None, None, State::Working),
            ("idle", Some(TurnOutcome::Done), None, State::Done),
            ("idle", Some(TurnOutcome::Error), None, State::Error),
            ("idle", None, None, State::Idle),
        ] {
            let conversation = app.conversations.get_mut("first").unwrap();
            conversation.foreground = foreground.into();
            conversation.outcome = outcome;
            assert_eq!(project(&app, kind).unwrap().state, expected);
        }
        app.conversations.get_mut("first").unwrap().outcome = Some(TurnOutcome::Done);
        app.select_session("second".into());
        app.conversations
            .entry("second".into())
            .or_default()
            .foreground = "running".into();
        assert_eq!(project(&app, None).unwrap().title, "second");
        app.select_session("first".into());
        assert_eq!(project(&app, None).unwrap().state, State::Idle);
        app.conversations.get_mut("first").unwrap().gap = true;
        assert!(project(&app, None).is_none());
        app.conversations.get_mut("first").unwrap().gap = false;
        app.conversations.get_mut("first").unwrap().activity =
            serde_json::json!({"session":"ended"});
        assert!(project(&app, None).is_none());
        app.conversations.get_mut("first").unwrap().activity = serde_json::Value::Null;
        app.connected = false;
        assert!(project(&app, None).is_none());
    }
}
