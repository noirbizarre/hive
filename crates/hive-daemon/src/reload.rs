//! Configuration publication and admission share a lock, never a downstream await.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use miette::Diagnostic;
use sha2::{Digest, Sha256};

use crate::{
    Broker,
    config::{Config, ConfigError, ConfigSource, Profile, ProfileKind},
};

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum ReloadError {
    #[error("this broker has no configuration file; start Hive with a configuration source")]
    #[diagnostic(code(hive::reload::no_source))]
    NoSource,
    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] ConfigError),
    #[error(
        "agent '{0}' is associated with existing sessions; use a new profile ID for a different server or adapter"
    )]
    #[diagnostic(code(hive::reload::profile_identity))]
    Identity(String),
    #[error("Hive is shutting down; start the daemon before reloading")]
    #[diagnostic(code(hive::reload::stopping))]
    Stopping,
    #[error("cannot reload configuration state; inspect daemon logs and restart Hive")]
    #[diagnostic(code(hive::reload::internal))]
    Internal,
}

impl ReloadError {
    pub(crate) fn response(self) -> (StatusCode, Json<serde_json::Value>) {
        let status = match &self {
            Self::Config(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Identity(_) | Self::NoSource => StatusCode::CONFLICT,
            Self::Stopping => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({
                "code": self.code().map(|code| code.to_string()),
                "message": self.to_string(),
                "help": self.help().map(|help| help.to_string()),
            })),
        )
    }
}

/// Hash only connection identity, never display/pricing settings. Length-delimited JSON
/// avoids ambiguous concatenation; secrets must not become durable plaintext or diagnostics.
pub(crate) fn identity(profile: &Profile) -> Option<String> {
    if profile.kind != ProfileKind::OpencodeServer {
        return None;
    }
    let endpoint = profile
        .endpoint
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let endpoint = reqwest::Url::parse(endpoint)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| endpoint.into());
    let input = serde_json::to_vec(&(
        "hive-opencode-identity-v1",
        endpoint,
        &profile.headers,
        profile
            .password_env
            .as_ref()
            .map(|_| profile.username.as_deref().unwrap_or("opencode")),
        &profile.password_env,
    ))
    .expect("connection identity contains only serializable values");
    Some(format!("v1:{:x}", Sha256::digest(input)))
}

pub(crate) struct Binding {
    pub profile: String,
    pub kind: ProfileKind,
    pub identity: Option<String>,
}

pub(crate) struct Registry {
    pub current: Arc<Config>,
    pub source: Option<ConfigSource>,
    pub bindings: HashMap<String, Binding>,
    pub stopping: bool,
}

impl Registry {
    pub fn check(&self, id: &str, profile: &Profile) -> Result<(), ReloadError> {
        let identity = identity(profile);
        for binding in self
            .bindings
            .values()
            .filter(|binding| binding.profile == id)
        {
            if (binding.kind == ProfileKind::OpencodeServer
                || profile.kind == ProfileKind::OpencodeServer)
                && (binding.kind != profile.kind
                    || binding.identity.is_none()
                    || binding.identity != identity)
            {
                return Err(ReloadError::Identity(id.into()));
            }
        }
        Ok(())
    }
}

/// Pending admission is released even when the initiating client cancels startup.
pub(crate) struct Reservation {
    pub registry: Arc<Mutex<Registry>>,
    pub id: String,
    pub retained: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.retained
            && let Ok(mut registry) = self.registry.lock()
        {
            registry.bindings.remove(&self.id);
        }
    }
}

impl Broker {
    pub(crate) fn config(&self) -> Arc<Config> {
        self.registry
            .lock()
            .expect("configuration lock poisoned")
            .current
            .clone()
    }

    /// Attach the original source to an embedded broker before serving it.
    pub fn with_config_source(self, source: ConfigSource) -> Self {
        self.registry
            .lock()
            .expect("configuration lock poisoned")
            .source = Some(source);
        self
    }

    pub async fn reload_config(&self) -> Result<u64, ReloadError> {
        // Serialize read-through-publication so a slower earlier request cannot win last.
        let _reload = self.reloading.lock().await;
        let source = {
            let registry = self.registry.lock().map_err(|_| ReloadError::Internal)?;
            if registry.stopping {
                return Err(ReloadError::Stopping);
            }
            registry.source.clone().ok_or(ReloadError::NoSource)?
        };
        let candidate = tokio::task::spawn_blocking(move || source.reload())
            .await
            .map_err(|_| ReloadError::Internal)??;
        let mut registry = self.registry.lock().map_err(|_| ReloadError::Internal)?;
        if registry.stopping {
            return Err(ReloadError::Stopping);
        }
        for (id, profile) in &candidate.agents {
            registry.check(id, profile)?;
        }
        registry.current = Arc::new(candidate);
        self.catalog.send_modify(|revision| *revision += 1);
        Ok(*self.catalog.borrow())
    }
}

/// ACP's CORS configuration does not cover operational routes appended to its router.
pub(crate) fn allowed_origin(headers: &HeaderMap, origin: Option<&str>) -> bool {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value == "cross-site")
    {
        return false;
    }
    let mut origins = headers.get_all("origin").iter();
    let Some(value) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(origin) = origin else {
        return false;
    };
    if value == origin {
        return true;
    }
    origin
        .strip_prefix("http://")
        .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
        .is_some_and(|address| {
            address.ip().is_loopback() && value == format!("http://localhost:{}", address.port())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("hive-reload-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(path)
        }
        fn config(&self, text: &str) -> ConfigSource {
            let path = self.0.join("config.toml");
            std::fs::write(&path, text).unwrap();
            ConfigSource::new(Some(path)).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ACP: &str = "default_agent = 'a'\n[agents.a]\ncommand = 'old-agent'\n";
    fn opencode(endpoint: &str) -> String {
        format!(
            "default_agent = 'a'\n[agents.a]\nkind = 'opencode-server'\nendpoint = '{endpoint}'\n"
        )
    }

    #[tokio::test]
    async fn fragment_changes_are_rediscovered_and_failed_composition_preserves_the_catalog() {
        let fixture = Fixture::new();
        let fragments = fixture.0.join("conf.d");
        std::fs::create_dir(&fragments).unwrap();
        let a = fragments.join("a.toml");
        let b = fragments.join("b.toml");
        std::fs::write(&a, "[agents.a]\ncommand = 'old-agent'").unwrap();
        let source = fixture.config("include = ['conf.d/*.toml']");
        let broker = Broker::new(source.load().unwrap()).with_config_source(source);
        std::fs::write(&a, "[agents.a]\ncommand = 'edited-agent'").unwrap();
        std::fs::write(&b, "[agents.b]\ncommand = 'added-agent'").unwrap();
        assert_eq!(broker.reload_config().await.unwrap(), 1);
        assert_eq!(
            broker.config().agents["a"].command.as_deref(),
            Some("edited-agent")
        );
        assert!(broker.config().agents.contains_key("b"));
        let before = broker.config();
        for invalid in ["[", "[agents.a]\ncommand = 'duplicate'"] {
            std::fs::write(&b, invalid).unwrap();
            assert!(broker.reload_config().await.is_err());
            assert!(Arc::ptr_eq(&before, &broker.config()));
            assert_eq!(*broker.catalog.borrow(), 1);
        }
        fixture.config("include = ['conf.d/*.toml', 'missing.toml']");
        std::fs::remove_file(&b).unwrap();
        assert!(broker.reload_config().await.is_err());
        assert!(Arc::ptr_eq(&before, &broker.config()));
        assert_eq!(*broker.catalog.borrow(), 1);
        fixture.config("include = ['conf.d/*.toml']");
        assert_eq!(broker.reload_config().await.unwrap(), 2);
        assert!(!broker.config().agents.contains_key("b"));
    }

    #[tokio::test]
    async fn only_valid_complete_candidates_replace_the_current_registry_and_revision() {
        let fixture = Fixture::new();
        let source = fixture.config(ACP);
        let broker = Broker::new(source.load().unwrap()).with_config_source(source);
        let before = broker.config();
        for invalid in [
            "token = 'secret-must-not-leak'\n[",
            "default_agent = 'missing'",
        ] {
            fixture.config(invalid);
            let error = broker.reload_config().await.unwrap_err();
            assert!(!error.to_string().contains("secret-must-not-leak"));
            assert!(Arc::ptr_eq(&before, &broker.config()));
            assert_eq!(*broker.catalog.borrow(), 0);
        }
        std::fs::remove_file(fixture.0.join("config.toml")).unwrap();
        assert!(matches!(
            broker.reload_config().await,
            Err(ReloadError::Config(ConfigError::Unreadable(_)))
        ));
        assert!(Arc::ptr_eq(&before, &broker.config()));
        // Editors commonly publish with rename rather than overwriting the open file.
        let replacement = fixture.0.join("replacement.toml");
        std::fs::write(
            &replacement,
            "default_agent = 'b'\n[agents.b]\ncommand = 'new-agent'",
        )
        .unwrap();
        std::fs::rename(replacement, fixture.0.join("config.toml")).unwrap();
        assert_eq!(broker.reload_config().await.unwrap(), 1);
        assert_eq!(broker.config().default_agent.as_deref(), Some("b"));
        assert!(!broker.config().agents.contains_key("a"));
        assert_eq!(before.agents["a"].command.as_deref(), Some("old-agent"));
        broker.shutdown().await;
        assert!(matches!(
            broker.reload_config().await,
            Err(ReloadError::Stopping)
        ));
    }

    #[tokio::test]
    async fn pending_identity_bindings_survive_profile_removal_and_release_on_cancelled_startup() {
        let fixture = Fixture::new();
        let original = opencode("http://127.0.0.1:10001");
        let source = fixture.config(&original);
        let broker = Broker::new(source.load().unwrap()).with_config_source(source);
        let profile = broker.config().agents["a"].clone();
        broker.registry.lock().unwrap().bindings.insert(
            "pending".into(),
            Binding {
                profile: "a".into(),
                kind: profile.kind,
                identity: identity(&profile),
            },
        );
        let reservation = Reservation {
            registry: broker.registry.clone(),
            id: "pending".into(),
            retained: false,
        };
        fixture.config("");
        broker.reload_config().await.unwrap();
        fixture.config(&opencode("http://127.0.0.1:10002"));
        assert!(matches!(
            broker.reload_config().await,
            Err(ReloadError::Identity(_))
        ));
        fixture.config(ACP);
        assert!(matches!(
            broker.reload_config().await,
            Err(ReloadError::Identity(_))
        ));
        fixture.config(&original);
        broker.reload_config().await.unwrap();
        drop(reservation);
        fixture.config(&opencode("http://127.0.0.1:10002"));
        broker.reload_config().await.unwrap();
    }

    #[tokio::test]
    async fn offline_identity_is_durable_and_legacy_records_get_a_startup_baseline() {
        let fixture = Fixture::new();
        let source = fixture.config(&opencode("http://127.0.0.1:10001"));
        let config = source.load().unwrap();
        let path = fixture.0.join("sessions.sqlite3");
        let store = crate::Store::open(path.clone()).unwrap();
        // Decode the legacy JSON shape, which deliberately has no identity field.
        let record: crate::Record = serde_json::from_value(serde_json::json!({
            "id":"saved", "downstream_id":"native", "cwd":"/tmp", "profile":"a", "version":"v2",
            "created_at":1, "last_activity_at":null, "title":null, "next_seq":1, "reserved_seq":0,
            "generation":1, "outcome":"unavailable", "reason":null, "uncertain":true, "kind":"opencode-server"
        })).unwrap();
        store.save(&record).unwrap();
        drop(store);
        let broker = Broker::persistent(config, path.clone())
            .unwrap()
            .with_config_source(source.clone());
        let known = broker.offline.lock().unwrap()["saved"].identity.clone();
        assert!(known.is_some());
        assert_eq!(
            broker.store.as_ref().unwrap().records().unwrap()[0].identity,
            known
        );
        drop(broker);
        fixture.config("");
        let broker = Broker::persistent(source.load().unwrap(), path)
            .unwrap()
            .with_config_source(source);
        fixture.config(&opencode("http://127.0.0.1:10002"));
        assert!(matches!(
            broker.reload_config().await,
            Err(ReloadError::Identity(_))
        ));
        fixture.config(&opencode("http://127.0.0.1:10001"));
        broker.reload_config().await.unwrap();
    }

    #[tokio::test]
    async fn an_in_memory_broker_reports_that_it_has_no_reload_source() {
        assert!(matches!(
            Broker::new(Config::default()).reload_config().await,
            Err(ReloadError::NoSource)
        ));
    }

    #[test]
    fn mutation_origins_are_exact_and_duplicate_or_cross_site_headers_are_rejected() {
        let mut headers = HeaderMap::new();
        assert!(allowed_origin(&headers, None));
        for origin in ["http://127.0.0.1:8765", "http://localhost:8765"] {
            headers.insert("origin", origin.parse().unwrap());
            assert!(allowed_origin(&headers, Some("http://127.0.0.1:8765")));
            assert!(!allowed_origin(&headers, None));
        }
        for origin in [
            "null",
            "http://localhost:1",
            "http://localhost.evil:8765",
            "https://example.com",
        ] {
            headers.insert("origin", origin.parse().unwrap());
            assert!(!allowed_origin(&headers, Some("http://127.0.0.1:8765")));
        }
        headers.insert("origin", "http://127.0.0.1:8765".parse().unwrap());
        headers.append("origin", "http://127.0.0.1:8765".parse().unwrap());
        assert!(!allowed_origin(&headers, Some("http://127.0.0.1:8765")));
        headers.remove("origin");
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!allowed_origin(&headers, None));
    }
}
