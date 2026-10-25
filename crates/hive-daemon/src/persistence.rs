//! The catalog is durable; ACP event payloads and controller credentials are deliberately not.
//!
//! Fingerprints and minimal acknowledgements form a safety journal, not a transcript archive.
//! Unfinished reservations survive restart so uncertain prompts are never executed twice.

use std::{path::PathBuf, sync::Mutex};

use crate::prompt_journal::{Fingerprint, JournalError, Outcome as PromptOutcome};
use crate::usage::Ledger;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Recovering,
    Active,
    Unavailable,
    Orphaned,
    RecoveryRequired,
    Lost,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recovering => "recovering",
            Self::Active => "active",
            Self::Unavailable => "unavailable",
            Self::Orphaned => "orphaned",
            Self::RecoveryRequired => "recovery_required",
            Self::Lost => "lost",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub downstream_id: String,
    pub cwd: String,
    // Resume must reactivate the same workspace scope and MCP connections, not just its cwd.
    #[serde(default)]
    pub additional_directories: Vec<agent_client_protocol::schema::v2::AbsolutePath>,
    #[serde(default)]
    pub mcp_servers: Vec<agent_client_protocol::schema::v2::McpServer>,
    pub profile: String,
    pub version: String,
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    pub last_activity_at: Option<u64>,
    pub title: Option<String>,
    pub next_seq: u64,
    pub reserved_seq: u64,
    pub generation: u64,
    pub outcome: Outcome,
    pub reason: Option<String>,
    pub uncertain: bool,
    #[serde(default)]
    pub usage: Ledger,
    // The fields below were added after the first schema shipped. They are optional in the JSON
    // blob so records written by earlier builds still decode (no SQL migration needed).
    /// Adapter that owns the downstream session: `acp` (default) or `opencode-server`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// OpenCode API flavor pinned at attach time (`v1` or `v2`); never switched silently on recovery.
    #[serde(default)]
    pub api: Option<String>,
    /// `hive` when Hive created the downstream session, `adopted` when it was found natively.
    #[serde(default = "default_origin")]
    pub origin: String,
    /// Opaque connection identity prevents saved native IDs from moving to another server.
    #[serde(default)]
    pub identity: Option<String>,
}

fn default_kind() -> String {
    "acp".into()
}

fn default_origin() -> String {
    "hive".into()
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "session database {path} contains an invalid request journal entry ({source}); restore a backup or inspect the journal without deleting pending reservations"
    )]
    Journal {
        path: PathBuf,
        #[source]
        source: JournalError,
    },
    #[error(
        "session database {path}: {source}; check disk space and permissions, then restart Hive"
    )]
    Sql {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "session database {path} has schema version {version}; upgrade Hive rather than overwriting it"
    )]
    Future { path: PathBuf, version: u32 },
    #[error(
        "session database {path} contains an invalid session record: {source}; restore a backup"
    )]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "cannot prepare private Hive state directory {path}: {source}; check ownership and permissions"
    )]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct Store {
    connection: Mutex<Connection>,
    path: PathBuf,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Store {
    pub fn open(path: PathBuf) -> Result<Self, StoreError> {
        let parent = path.parent().expect("database has a parent");
        if !parent.exists() {
            let base = parent.parent().expect("Hive directory has a parent");
            std::fs::create_dir_all(base).map_err(|source| StoreError::Directory {
                path: base.into(),
                source,
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(parent)
                    .or_else(|error| {
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(|source| StoreError::Directory {
                        path: parent.into(),
                        source,
                    })?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir(parent).map_err(|source| StoreError::Directory {
                path: parent.into(),
                source,
            })?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(parent)
                .map_err(|source| StoreError::Directory {
                    path: parent.into(),
                    source,
                })?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(StoreError::Directory {
                    path: parent.into(),
                    source: std::io::Error::other("use a directory with mode 0700"),
                });
            }
        }
        let existing = std::fs::symlink_metadata(&path).ok();
        if existing.as_ref().is_some_and(|meta| !meta.is_file()) {
            return Err(StoreError::Directory {
                path: path.clone(),
                source: std::io::Error::other(
                    "session database must be a regular file; remove the link after checking its target",
                ),
            });
        }
        #[cfg(unix)]
        if let Some(meta) = &existing {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if meta.permissions().mode() & 0o077 != 0 || meta.nlink() != 1 {
                return Err(StoreError::Directory {
                    path: path.clone(),
                    source: std::io::Error::other(
                        "use a private database file with mode 0600 and one link",
                    ),
                });
            }
        }
        let mut db = Connection::open(&path).map_err(|source| StoreError::Sql {
            path: path.clone(),
            source,
        })?;
        #[cfg(unix)]
        if existing.is_none() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
                |source| StoreError::Directory {
                    path: path.clone(),
                    source,
                },
            )?;
        }
        let wrap = |source| StoreError::Sql {
            path: path.clone(),
            source,
        };
        db.busy_timeout(std::time::Duration::from_secs(3))
            .map_err(wrap)?;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(wrap)?;
        if version > 2 {
            return Err(StoreError::Future { path, version });
        }
        if version == 0 {
            let migration = db.transaction().map_err(wrap)?;
            migration.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE prompts (session_id TEXT NOT NULL, key TEXT NOT NULL, content TEXT NOT NULL, result TEXT, PRIMARY KEY(session_id, key), FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE); PRAGMA user_version = 1;").map_err(wrap)?;
            migration.commit().map_err(wrap)?;
        }
        if version < 2 {
            let migration = db.transaction().map_err(wrap)?;
            migration
                .execute_batch(
                    "ALTER TABLE prompts ADD COLUMN content_fingerprint TEXT NOT NULL DEFAULT ''; ",
                )
                .map_err(wrap)?;
            // Load identities only: migration memory must not grow with every saved prompt body.
            let keys = {
                let mut statement = migration
                    .prepare("SELECT session_id,key FROM prompts")
                    .map_err(wrap)?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(wrap)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(wrap)?
            };
            let journal = |source| StoreError::Journal {
                path: path.clone(),
                source,
            };
            for (session, key) in keys {
                let (content, result): (String, Option<String>) = migration
                    .query_row(
                        "SELECT content,result FROM prompts WHERE session_id=?1 AND key=?2",
                        params![session, key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(wrap)?;
                let fingerprint = Fingerprint::legacy(&content).map_err(journal)?;
                let result = result
                    .as_deref()
                    .map(|value| PromptOutcome::legacy(value).and_then(|outcome| outcome.encode()))
                    .transpose()
                    .map_err(journal)?;
                // Scrub bodies and arbitrary result payloads without releasing any reservation.
                migration.execute("UPDATE prompts SET content='',content_fingerprint=?3,result=?4 WHERE session_id=?1 AND key=?2",
                    params![session, key, fingerprint.as_str(), result]).map_err(wrap)?;
            }
            migration
                .pragma_update(None, "user_version", 2)
                .map_err(wrap)?;
            migration.commit().map_err(wrap)?;
        }
        // A future schema or failed migration must not change the existing journal mode.
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(wrap)?;
        db.pragma_update(None, "foreign_keys", "ON").map_err(wrap)?;
        Ok(Self {
            connection: Mutex::new(db),
            path,
        })
    }

    fn sql(&self, source: rusqlite::Error) -> StoreError {
        StoreError::Sql {
            path: self.path.clone(),
            source,
        }
    }

    fn journal(&self, source: JournalError) -> StoreError {
        StoreError::Journal {
            path: self.path.clone(),
            source,
        }
    }

    pub fn records(&self) -> Result<Vec<Record>, StoreError> {
        let db = self.connection.lock().expect("store lock poisoned");
        let mut statement = db
            .prepare("SELECT record FROM sessions ORDER BY id")
            .map_err(|e| self.sql(e))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| self.sql(e))?;
        rows.map(|row| {
            let json = row.map_err(|e| self.sql(e))?;
            serde_json::from_str(&json).map_err(|source| StoreError::Decode {
                path: self.path.clone(),
                source,
            })
        })
        .collect()
    }

    pub fn prune(&self, before: u64) -> Result<(), StoreError> {
        for record in self.records()? {
            if matches!(record.outcome, Outcome::Orphaned | Outcome::Lost)
                && !record.uncertain
                && record.last_activity_at.unwrap_or(record.created_at) < before
            {
                self.remove(&record.id)?;
            }
        }
        Ok(())
    }

    pub fn save(&self, record: &Record) -> Result<(), StoreError> {
        let mut updated = record.clone();
        updated.updated_at = now_millis();
        let json = serde_json::to_string(&updated).map_err(|source| StoreError::Decode {
            path: self.path.clone(),
            source,
        })?;
        self.connection.lock().expect("store lock poisoned")
            .execute("INSERT INTO sessions (id, record) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![record.id, json])
            .map_err(|e| self.sql(e))?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .expect("store lock poisoned")
            .execute("DELETE FROM sessions WHERE id=?1", [id])
            .map_err(|e| self.sql(e))?;
        Ok(())
    }

    pub fn reserve_prompt(
        &self,
        record: &Record,
        key: &str,
        fingerprint: &Fingerprint,
    ) -> Result<(), StoreError> {
        let mut db = self.connection.lock().expect("store lock poisoned");
        let tx = db.transaction().map_err(|e| self.sql(e))?;
        let mut updated = record.clone();
        updated.updated_at = now_millis();
        let json = serde_json::to_string(&updated).map_err(|source| StoreError::Decode {
            path: self.path.clone(),
            source,
        })?;
        tx.execute(
            "UPDATE sessions SET record=?2 WHERE id=?1",
            params![record.id, json],
        )
        .map_err(|e| self.sql(e))?;
        tx.execute(
            "INSERT INTO prompts (session_id,key,content,content_fingerprint) VALUES (?1,?2,'',?3)",
            params![record.id, key, fingerprint.as_str()],
        )
        .map_err(|e| self.sql(e))?;
        tx.commit().map_err(|e| self.sql(e))?;
        Ok(())
    }

    pub fn prompt(
        &self,
        id: &str,
        key: &str,
    ) -> Result<Option<(Fingerprint, Option<PromptOutcome>)>, StoreError> {
        let db = self.connection.lock().expect("store lock poisoned");
        let row: Option<(String, Option<String>)> = db
            .query_row(
                "SELECT content_fingerprint,result FROM prompts WHERE session_id=?1 AND key=?2",
                params![id, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| self.sql(e))?;
        row.map(|(fingerprint, result)| {
            Ok((
                Fingerprint::parse(fingerprint).map_err(|e| self.journal(e))?,
                result
                    .as_deref()
                    .map(PromptOutcome::decode)
                    .transpose()
                    .map_err(|e| self.journal(e))?,
            ))
        })
        .transpose()
    }

    pub fn finish_prompt(
        &self,
        id: &str,
        key: &str,
        result: &PromptOutcome,
    ) -> Result<(), StoreError> {
        let result = result.encode().map_err(|e| self.journal(e))?;
        self.connection
            .lock()
            .expect("store lock poisoned")
            .execute(
                "UPDATE prompts SET result=?3 WHERE session_id=?1 AND key=?2",
                params![id, key, result],
            )
            .map_err(|e| self.sql(e))?;
        Ok(())
    }
}

/// Parent of the `hive/` state directory. Shared with `hive doctor` so both agree on it.
pub(crate) fn state_base() -> Result<PathBuf, StoreError> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| StoreError::Directory {
            path: PathBuf::from("hive"),
            source: std::io::Error::other("set XDG_STATE_HOME or HOME"),
        })
}

pub fn default_path() -> Result<PathBuf, StoreError> {
    Ok(state_base()?.join("hive/sessions.sqlite3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary() -> PathBuf {
        std::env::temp_dir()
            .join(format!("hive-storage-{}", uuid::Uuid::new_v4()))
            .join("hive/sessions.sqlite3")
    }

    fn record(id: &str) -> Record {
        Record {
            id: id.into(),
            downstream_id: "agent-session".into(),
            cwd: "/tmp".into(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            profile: "mock".into(),
            version: "v2".into(),
            created_at: 1,
            updated_at: 1,
            last_activity_at: None,
            title: None,
            next_seq: 1,
            reserved_seq: 0,
            generation: 1,
            outcome: Outcome::Active,
            reason: None,
            uncertain: false,
            usage: Ledger::default(),
            kind: "acp".into(),
            api: None,
            origin: "hive".into(),
            identity: None,
        }
    }

    fn legacy_database() -> (PathBuf, Connection) {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        store.save(&record("one")).unwrap();
        drop(store);
        let db = Connection::open(&path).unwrap();
        // Recreate the exact schema-1 journal; registry JSON is deliberately untouched.
        db.execute_batch(
            "ALTER TABLE prompts DROP COLUMN content_fingerprint; PRAGMA user_version=1;",
        )
        .unwrap();
        (path, db)
    }

    #[test]
    fn migration_scrubs_bodies_and_metadata_without_releasing_reservations_or_losing_acknowledgements()
     {
        let (path, db) = legacy_database();
        let registry: String = db
            .query_row("SELECT record FROM sessions", [], |row| row.get(0))
            .unwrap();
        let body = r#"[{"type":"text","text":"private prompt sentinel"}]"#;
        for (key, result) in [
            ("pending", None),
            (
                "ack",
                Some(r#"{"Ok":{"messageId":"original","_meta":{"private":"metadata sentinel"}}}"#),
            ),
            ("failed", Some(r#"{"Err":"private error sentinel"}"#)),
        ] {
            db.execute(
                "INSERT INTO prompts (session_id,key,content,result) VALUES ('one',?1,?2,?3)",
                params![key, body, result],
            )
            .unwrap();
        }
        drop(db);
        for _ in 0..2 {
            let store = Store::open(path.clone()).unwrap();
            let fingerprint = Fingerprint::legacy(body).unwrap();
            assert_eq!(
                store.prompt("one", "pending").unwrap(),
                Some((fingerprint.clone(), None))
            );
            assert_eq!(
                store.prompt("one", "ack").unwrap(),
                Some((
                    fingerprint.clone(),
                    Some(PromptOutcome::Acknowledged {
                        message_id: "original".into()
                    })
                ))
            );
            assert_eq!(
                store.prompt("one", "failed").unwrap(),
                Some((fingerprint, Some(PromptOutcome::Failed {})))
            );
            let db = store.connection.lock().unwrap();
            assert_eq!(
                db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                2
            );
            assert_eq!(
                db.query_row("SELECT record FROM sessions", [], |row| row
                    .get::<_, String>(0))
                    .unwrap(),
                registry
            );
            let mut rows = db
                .prepare("SELECT content,content_fingerprint,result FROM prompts")
                .unwrap();
            for row in rows
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .unwrap()
            {
                let (content, fingerprint, result) = row.unwrap();
                assert!(content.is_empty());
                assert!(Fingerprint::parse(fingerprint.clone()).is_ok());
                let logical_row = format!("{content}{fingerprint}{result:?}");
                assert!(!logical_row.contains("sentinel"));
            }
        }
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_legacy_rows_roll_back_the_whole_upgrade_without_echoing_private_content() {
        for (content, result) in [
            ("private malformed content sentinel", None),
            ("[]", Some("private malformed outcome sentinel")),
        ] {
            let (path, db) = legacy_database();
            db.execute("INSERT INTO prompts VALUES ('one','valid','[]',NULL)", [])
                .unwrap();
            db.execute(
                "INSERT INTO prompts VALUES ('one','invalid',?1,?2)",
                params![content, result],
            )
            .unwrap();
            drop(db);
            let error = Store::open(path.clone()).err().unwrap();
            assert!(matches!(error, StoreError::Journal { .. }));
            assert!(!error.to_string().contains("sentinel"));
            let db = Connection::open(&path).unwrap();
            assert_eq!(
                db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                db.query_row(
                    "SELECT content FROM prompts WHERE key='invalid'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                content
            );
            assert!(
                db.prepare("SELECT content_fingerprint FROM prompts")
                    .is_err()
            );
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM prompts", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                2
            );
            drop(db);
            std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn new_journal_rows_store_only_fingerprints_and_typed_outcomes_and_corrupt_rows_fail_closed() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        store.save(&record("one")).unwrap();
        let fingerprint = Fingerprint::content(&["private prompt sentinel".into()]).unwrap();
        store
            .reserve_prompt(&record("one"), "key", &fingerprint)
            .unwrap();
        store
            .finish_prompt("one", "key", &PromptOutcome::Failed {})
            .unwrap();
        let db = store.connection.lock().unwrap();
        let (content, result): (String, String) = db
            .query_row("SELECT content,result FROM prompts", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert!(content.is_empty());
        assert_eq!(result, r#"{"status":"failed"}"#);
        db.execute("UPDATE prompts SET content_fingerprint=''", [])
            .unwrap();
        drop(db);
        assert!(matches!(
            store.prompt("one", "key"),
            Err(StoreError::Journal { .. })
        ));
        let db = store.connection.lock().unwrap();
        db.execute(
            "UPDATE prompts SET content_fingerprint=?1,result='invalid'",
            [fingerprint.as_str()],
        )
        .unwrap();
        drop(db);
        assert!(matches!(
            store.prompt("one", "key"),
            Err(StoreError::Journal { .. })
        ));
        drop(store);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn usage_records_survive_restart_without_changing_legacy_record_shape() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        let mut session = record("one");
        session.usage.record(crate::usage::Turn {
            id: "prompt-key".into(),
            agent: "mock".into(),
            timestamp: 12,
            tokens: Some(crate::usage::Tokens {
                total_tokens: Some(30),
                input_tokens: Some(20),
                output_tokens: Some(10),
                ..Default::default()
            }),
            estimate: None,
        });
        store.save(&session).unwrap();
        drop(store);
        let reopened = Store::open(path).unwrap();
        let saved = reopened.records().unwrap().pop().unwrap();
        assert_eq!(saved.usage.summary().tokens.total_tokens, Some(30));
        assert!(!saved.usage.clone().record(saved.usage.turns[0].clone()));
    }

    #[test]
    fn recovery_keeps_workspace_roots_and_mcp_connections_alongside_the_original_identity() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        let mut session = record("one");
        session
            .additional_directories
            .push(PathBuf::from("/workspace/other").into());
        session.mcp_servers = vec![agent_client_protocol::schema::v2::McpServer::Stdio(
            agent_client_protocol::schema::v2::McpServerStdio::new(
                "fixture",
                PathBuf::from("/fixture-mcp"),
            ),
        )];
        store.save(&session).unwrap();
        drop(store);
        let store = Store::open(path.clone()).unwrap();
        let recovered = store.records().unwrap().pop().unwrap();
        assert_eq!(recovered.id, session.id);
        assert_eq!(recovered.downstream_id, session.downstream_id);
        assert_eq!(recovered.cwd, session.cwd);
        assert_eq!(
            recovered.additional_directories,
            session.additional_directories
        );
        assert_eq!(recovered.mcp_servers, session.mcp_servers);
        drop(store);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn a_record_written_before_native_adoption_decodes_as_an_acp_hive_session() {
        // The exact shape earlier builds stored: no kind, api or origin.
        let legacy = r#"{"id":"hive-1","downstream_id":"d","cwd":"/tmp","profile":"p",
            "version":"v2","created_at":1,"last_activity_at":null,"title":null,"next_seq":1,
            "reserved_seq":0,"generation":1,"outcome":"active","reason":null,"uncertain":false}"#;
        let record: Record = serde_json::from_str(legacy).unwrap();
        assert_eq!(record.kind, "acp");
        assert_eq!(record.origin, "hive");
        assert_eq!(record.api, None);
        assert!(record.additional_directories.is_empty());
        assert!(record.mcp_servers.is_empty());
    }

    #[test]
    fn reopening_the_database_keeps_independent_sessions_and_interrupted_prompt_keys() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        store.save(&record("one")).unwrap();
        store.save(&record("two")).unwrap();
        let mut first = record("one");
        first.uncertain = true;
        let fingerprint = Fingerprint::content(&[]).unwrap();
        store.reserve_prompt(&first, "key", &fingerprint).unwrap();
        drop(store);
        let store = Store::open(path.clone()).unwrap();
        assert_eq!(store.records().unwrap().len(), 2);
        assert_eq!(
            store.prompt("one", "key").unwrap(),
            Some((fingerprint, None))
        );
        assert!(store.prompt("two", "key").unwrap().is_none());
        store.remove("one").unwrap();
        assert!(store.prompt("one", "key").unwrap().is_none());
        assert_eq!(store.records().unwrap().len(), 1);
        drop(store);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn a_future_database_version_is_rejected_without_migrating_or_overwriting_it() {
        let path = temporary();
        Store::open(path.clone()).unwrap();
        let db = Connection::open(&path).unwrap();
        db.pragma_update(None, "journal_mode", "DELETE").unwrap();
        db.pragma_update(None, "user_version", 42).unwrap();
        drop(db);
        assert!(matches!(
            Store::open(path.clone()),
            Err(StoreError::Future { version: 42, .. })
        ));
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            42
        );
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_prompt_reservations_do_not_change_another_session() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        store.save(&record("one")).unwrap();
        store.save(&record("two")).unwrap();
        let fingerprint = Fingerprint::content(&[]).unwrap();
        store
            .reserve_prompt(&record("one"), "key", &fingerprint)
            .unwrap();
        let mut changed = record("one");
        changed.uncertain = true;
        assert!(
            store
                .reserve_prompt(
                    &changed,
                    "key",
                    &Fingerprint::content(&["changed".into()]).unwrap()
                )
                .is_err()
        );
        assert_eq!(store.prompt("one", "key").unwrap().unwrap().0, fingerprint);
        assert!(
            !store
                .records()
                .unwrap()
                .iter()
                .find(|record| record.id == "one")
                .unwrap()
                .uncertain
        );
        assert!(store.prompt("two", "key").unwrap().is_none());
        drop(store);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn an_empty_unversioned_database_migrates_without_discarding_other_records() {
        let path = temporary();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                path.parent().unwrap(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE unrelated (value TEXT); INSERT INTO unrelated VALUES ('keep');",
        )
        .unwrap();
        drop(db);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let store = Store::open(path.clone()).unwrap();
        store.save(&record("after-migration")).unwrap();
        drop(store);
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM unrelated", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            2
        );
        drop(db);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn a_large_catalog_is_loaded_without_corrupting_independent_ids() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        for number in 0..120 {
            store.save(&record(&format!("session-{number}"))).unwrap();
        }
        drop(store);
        let store = Store::open(path.clone()).unwrap();
        assert_eq!(store.records().unwrap().len(), 120);
        drop(store);
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
