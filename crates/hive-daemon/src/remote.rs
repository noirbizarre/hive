//! Connection-local negotiation and inventory snapshots never acquire session authority.

use super::*;
use hive_protocol::remote::{
    AgentProfile, AgentsPage, LaunchRequirements, ListParams, ProfileCapabilities, RemoteError,
};

const CURSOR_LIFETIME: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Phase {
    #[default]
    BeforeCreate,
    Published,
    NativeKnown,
    Registered,
}

#[derive(Default)]
pub(super) struct Startup {
    pub phase: Phase,
    pub native_id: Option<String>,
}

pub(super) fn publishing(control: &Mutex<Control>) -> Result<(), Error> {
    let state = control.lock().map_err(|_| Error::internal_error())?;
    state
        .startup
        .lock()
        .map_err(|_| Error::internal_error())?
        .phase = Phase::Published;
    Ok(())
}

pub(super) fn created(control: &Mutex<Control>, id: String) -> Result<(), Error> {
    let state = control.lock().map_err(|_| Error::internal_error())?;
    let mut startup = state.startup.lock().map_err(|_| Error::internal_error())?;
    startup.phase = Phase::NativeKnown;
    startup.native_id = Some(id);
    Ok(())
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Mode {
    #[default]
    Legacy,
    Unsupported,
    Negotiated,
}

#[derive(Default)]
pub(super) struct Connection {
    pub initialized: bool,
    pub mode: Mode,
    snapshot: Option<Snapshot>,
    profile_snapshot: Option<Snapshot>,
}

struct Snapshot {
    config: Arc<Config>,
    cursor: String,
    position: usize,
    expires: tokio::time::Instant,
}

impl Connection {
    pub fn list_profiles(
        &mut self,
        broker: &Broker,
        raw: serde_json::Value,
    ) -> Result<hive_protocol::remote::ProfilesPage, Error> {
        if !self.initialized || self.mode != Mode::Negotiated {
            return Err(RemoteError::Unsupported.wire());
        }
        if ["cursor", "limit"]
            .iter()
            .any(|key| raw.get(key).is_some_and(serde_json::Value::is_null))
        {
            return Err(RemoteError::InvalidParams.wire());
        }
        let params: ListParams =
            serde_json::from_value(raw).map_err(|_| RemoteError::InvalidParams.wire())?;
        let limit = params.validate()?;
        if self
            .profile_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.expires <= tokio::time::Instant::now())
        {
            self.profile_snapshot = None;
        }
        if let Some(cursor) = &params.cursor {
            if self
                .profile_snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.cursor != *cursor)
            {
                return Err(RemoteError::InvalidParams.wire());
            }
        } else {
            self.profile_snapshot = Some(Snapshot {
                config: broker
                    .registry
                    .lock()
                    .map_err(|_| Error::internal_error())?
                    .current
                    .clone(),
                cursor: String::new(),
                position: 0,
                expires: tokio::time::Instant::now() + CURSOR_LIFETIME,
            });
        }
        let snapshot = self
            .profile_snapshot
            .as_mut()
            .expect("fresh or validated snapshot");
        let default_profile = snapshot.config.default_profile.clone();
        let profiles = snapshot
            .config
            .public_execution_profiles()
            .into_iter()
            .skip(snapshot.position)
            .take(limit)
            .collect::<Vec<_>>();
        snapshot.position += profiles.len();
        let next_cursor = if snapshot.position < snapshot.config.profiles.len() {
            snapshot.cursor = uuid::Uuid::new_v4().to_string();
            Some(snapshot.cursor.clone())
        } else {
            None
        };
        if next_cursor.is_none() {
            self.profile_snapshot = None;
        }
        Ok(hive_protocol::remote::ProfilesPage {
            profiles,
            default_profile,
            next_cursor,
        })
    }

    pub fn list(&mut self, broker: &Broker, raw: serde_json::Value) -> Result<AgentsPage, Error> {
        if !self.initialized || self.mode != Mode::Negotiated {
            return Err(RemoteError::Unsupported.wire());
        }
        if ["cursor", "limit"]
            .iter()
            .any(|key| raw.get(key).is_some_and(serde_json::Value::is_null))
        {
            return Err(RemoteError::InvalidParams.wire());
        }
        let params: ListParams =
            serde_json::from_value(raw).map_err(|_| RemoteError::InvalidParams.wire())?;
        let limit = params.validate()?;
        if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.expires <= tokio::time::Instant::now())
        {
            self.snapshot = None;
        }
        if let Some(cursor) = &params.cursor {
            if self.snapshot.as_ref().is_none_or(|snapshot| {
                snapshot.cursor != *cursor || snapshot.expires <= tokio::time::Instant::now()
            }) {
                return Err(RemoteError::InvalidParams.wire());
            }
        } else {
            let config = broker
                .registry
                .lock()
                .map_err(|_| Error::internal_error())?
                .current
                .clone();
            self.snapshot = Some(Snapshot {
                config,
                cursor: String::new(),
                position: 0,
                expires: tokio::time::Instant::now() + CURSOR_LIFETIME,
            });
        }
        let snapshot = self.snapshot.as_mut().expect("fresh or validated snapshot");
        let default_agent = snapshot
            .config
            .default_agent
            .as_ref()
            .filter(|id| snapshot.config.agents.contains_key(*id))
            .cloned();
        let agents = snapshot
            .config
            .agents
            .iter()
            .skip(snapshot.position)
            .take(limit)
            .map(|(id, profile)| project(id, profile, default_agent.as_deref()))
            .collect::<Vec<_>>();
        snapshot.position += agents.len();
        let next_cursor = if snapshot.position < snapshot.config.agents.len() {
            // Rotate tokens so the caller cannot manufacture offsets or replay an old page.
            snapshot.cursor = uuid::Uuid::new_v4().to_string();
            Some(snapshot.cursor.clone())
        } else {
            None
        };
        if next_cursor.is_none() {
            self.snapshot = None;
        }
        Ok(AgentsPage {
            agents,
            default_agent,
            next_cursor,
        })
    }
}

fn project(id: &str, profile: &Profile, default: Option<&str>) -> AgentProfile {
    let mut versions = Vec::new();
    if profile.kind == ProfileKind::Acp {
        for version in &profile.versions {
            if !versions.contains(version) {
                versions.push(*version);
            }
        }
    }
    AgentProfile {
        id: id.into(),
        name: profile
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(id)
            .into(),
        kind: profile.kind.as_str().into(),
        transport: profile.transport().into(),
        versions,
        is_default: default == Some(id),
        capabilities: ProfileCapabilities::default(),
        launch: LaunchRequirements {
            cwd: "required".into(),
            workspace: if profile.workspace.is_some() {
                "configured"
            } else {
                "unsupported"
            }
            .into(),
        },
    }
}

pub(super) fn launch(
    raw: serde_json::Value,
    connection: &Connection,
) -> Result<(v2::NewSessionRequest, bool), Error> {
    let hive = hive_protocol::remote::hive_metadata(&raw)?;
    if connection.mode == Mode::Unsupported && hive.is_some() {
        return Err(RemoteError::Unsupported.wire());
    }
    let negotiated = connection.mode == Mode::Negotiated;
    if negotiated {
        if raw
            .get("_meta")
            .and_then(|meta| meta.get("hive"))
            .is_some_and(serde_json::Value::is_null)
        {
            return Err(RemoteError::InvalidParams.wire());
        }
        if let Some(hive) = hive {
            let hive = hive
                .as_object()
                .ok_or_else(|| RemoteError::InvalidParams.wire())?;
            if hive
                .keys()
                .any(|key| !["agent", "profile", "workspace", "adopt"].contains(&key.as_str()))
                || hive
                    .get("agent")
                    .is_some_and(|id| id.as_str().is_none_or(str::is_empty))
                || hive
                    .get("profile")
                    .is_some_and(|id| id.as_str().is_none_or(str::is_empty))
                || (!hive.contains_key("agent")
                    && !hive.contains_key("workspace")
                    && !hive.contains_key("profile"))
            {
                return Err(RemoteError::InvalidParams.wire());
            }
            if let Some(adopt) = hive.get("adopt") {
                let adopt = adopt
                    .as_object()
                    .ok_or_else(|| RemoteError::InvalidParams.wire())?;
                if adopt.len() != 1
                    || adopt
                        .get("nativeId")
                        .and_then(serde_json::Value::as_str)
                        .is_none_or(str::is_empty)
                {
                    return Err(RemoteError::InvalidParams.wire());
                }
            }
            if let Some(workspace) = hive.get("workspace") {
                // Deserialize the complete operational selector before any owner-side effects.
                serde_json::from_value::<hive_protocol::workspace::Launch>(workspace.clone())
                    .map_err(|_| RemoteError::InvalidParams.wire())?
                    .validate()
                    .map_err(RemoteError::wire)?;
            }
        }
    }
    let request: v2::NewSessionRequest =
        serde_json::from_value(raw).map_err(|_| RemoteError::InvalidParams.wire())?;
    if negotiated {
        if !request.additional_directories.is_empty() || !request.mcp_servers.is_empty() {
            return Err(RemoteError::Unsupported.wire());
        }
        if !request.cwd.0.is_absolute() {
            return Err(RemoteError::InvalidWorkspace.wire());
        }
    }
    Ok((request, negotiated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cursor_expiry_replacement_and_connection_isolation_never_restart_a_listing() {
        let config: Config = serde_json::from_value(
            json!({"agents":{"a":{"command":"unused"},"b":{"command":"unused"}}}),
        )
        .unwrap();
        let broker = Broker::new(config);
        let mut connection = Connection {
            initialized: true,
            mode: Mode::Negotiated,
            ..Default::default()
        };
        let old = connection
            .list(&broker, json!({"limit":1}))
            .unwrap()
            .next_cursor
            .unwrap();
        let mut other = Connection {
            initialized: true,
            mode: Mode::Negotiated,
            ..Default::default()
        };
        assert!(other.list(&broker, json!({"cursor":old})).is_err());
        let current = connection
            .list(&broker, json!({"limit":1}))
            .unwrap()
            .next_cursor
            .unwrap();
        assert!(connection.list(&broker, json!({"cursor":old})).is_err());
        connection.snapshot.as_mut().unwrap().expires = tokio::time::Instant::now();
        assert!(connection.list(&broker, json!({"cursor":current})).is_err());
        assert_eq!(connection.list(&broker, json!({})).unwrap().agents.len(), 2);
    }

    #[test]
    fn profile_pages_hold_one_snapshot_and_reject_foreign_expired_and_agent_cursors() {
        let config: Config = toml::from_str("[agents.a]\ncommand = 'unused'\n[agents.b]\ncommand = 'unused'\n[profiles.first]\nagents = ['a']\n[profiles.second]\nagents = ['b']").unwrap();
        let broker = Broker::new(config);
        let mut connection = Connection {
            initialized: true,
            mode: Mode::Negotiated,
            ..Default::default()
        };
        let cursor = connection
            .list_profiles(&broker, json!({"limit":1}))
            .unwrap()
            .next_cursor
            .unwrap();
        let mut other = Connection {
            initialized: true,
            mode: Mode::Negotiated,
            ..Default::default()
        };
        assert!(
            other
                .list_profiles(&broker, json!({"cursor":cursor}))
                .is_err()
        );
        let agent_cursor = connection
            .list(&broker, json!({"limit":1}))
            .unwrap()
            .next_cursor
            .unwrap();
        assert!(
            connection
                .list_profiles(&broker, json!({"cursor":agent_cursor}))
                .is_err()
        );
        broker.registry.lock().unwrap().current = Arc::new(Config::default());
        let next = connection
            .list_profiles(&broker, json!({"cursor":cursor}))
            .unwrap();
        assert_eq!(next.profiles[0].id, "second");
        assert!(
            connection
                .list_profiles(&broker, json!({}))
                .unwrap()
                .profiles
                .is_empty()
        );
        let config: Config = toml::from_str("[profiles.a]\n[profiles.b]").unwrap();
        broker.registry.lock().unwrap().current = Arc::new(config);
        let cursor = connection
            .list_profiles(&broker, json!({"limit":1}))
            .unwrap()
            .next_cursor
            .unwrap();
        connection.profile_snapshot.as_mut().unwrap().expires = tokio::time::Instant::now();
        assert!(
            connection
                .list_profiles(&broker, json!({"cursor":cursor}))
                .is_err()
        );
    }

    #[test]
    fn malformed_profile_selectors_cannot_fall_back_to_default_admission() {
        // A Unix root alone is not absolute on Windows; keep cwd validation out of this test.
        let cwd = std::env::current_dir().unwrap();
        let connection = Connection {
            initialized: true,
            mode: Mode::Negotiated,
            ..Default::default()
        };
        for profile in [json!(null), json!(""), json!(false), json!({})] {
            assert!(
                launch(
                    json!({"cwd":cwd, "mcpServers":[], "_meta":{"hive":{"profile":profile}}}),
                    &connection
                )
                .is_err()
            );
        }
        assert!(
            launch(
                json!({"cwd":cwd, "mcpServers":[], "_meta":{"hive":{"profile":"personal"}}}),
                &connection
            )
            .is_ok()
        );
    }

    #[test]
    fn non_acp_inventory_does_not_claim_acp_versions_or_expose_launch_configuration() {
        let config: Config = serde_json::from_value(json!({"agents":{"native":{"kind":"opencode-server", "endpoint":"http://PRIVATE_SENTINEL", "headers":{"key":"PRIVATE_SENTINEL"}, "workspace":"private-policy", "versions":["v2"]}}})).unwrap();
        let profile = project("native", &config.agents["native"], None);
        assert!(profile.versions.is_empty());
        assert_eq!(profile.launch.workspace, "configured");
        let value = serde_json::to_string(&profile).unwrap();
        assert!(!value.contains("PRIVATE_SENTINEL"));
        assert!(!value.contains("private-policy"));
    }
}
