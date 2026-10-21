//! The catalog is durable; ACP event payloads and controller credentials are deliberately not.

use std::{path::PathBuf, sync::Mutex};

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
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
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
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(wrap)?;
        let version: u32 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(wrap)?;
        if version > 1 {
            return Err(StoreError::Future { path, version });
        }
        if version == 0 {
            let migration = db.transaction().map_err(wrap)?;
            migration.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE prompts (session_id TEXT NOT NULL, key TEXT NOT NULL, content TEXT NOT NULL, result TEXT, PRIMARY KEY(session_id, key), FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE); PRAGMA user_version = 1;").map_err(wrap)?;
            migration.commit().map_err(wrap)?;
        }
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
        content: &str,
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
            "INSERT INTO prompts (session_id,key,content) VALUES (?1,?2,?3)",
            params![record.id, key, content],
        )
        .map_err(|e| self.sql(e))?;
        tx.commit().map_err(|e| self.sql(e))?;
        Ok(())
    }

    pub fn prompt(
        &self,
        id: &str,
        key: &str,
    ) -> Result<Option<(String, Option<String>)>, StoreError> {
        let db = self.connection.lock().expect("store lock poisoned");
        db.query_row(
            "SELECT content,result FROM prompts WHERE session_id=?1 AND key=?2",
            params![id, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| self.sql(e))
    }

    pub fn finish_prompt(&self, id: &str, key: &str, result: &str) -> Result<(), StoreError> {
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

pub fn default_path() -> Result<PathBuf, StoreError> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| StoreError::Directory {
            path: PathBuf::from("hive"),
            source: std::io::Error::other("set XDG_STATE_HOME or HOME"),
        })?;
    Ok(base.join("hive/sessions.sqlite3"))
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
        }
    }

    #[test]
    fn reopening_the_database_keeps_independent_sessions_and_interrupted_prompt_keys() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        store.save(&record("one")).unwrap();
        store.save(&record("two")).unwrap();
        let mut first = record("one");
        first.uncertain = true;
        store.reserve_prompt(&first, "key", "[]").unwrap();
        drop(store);
        let store = Store::open(path.clone()).unwrap();
        assert_eq!(store.records().unwrap().len(), 2);
        assert_eq!(
            store.prompt("one", "key").unwrap(),
            Some(("[]".into(), None))
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
        db.pragma_update(None, "user_version", 42).unwrap();
        drop(db);
        assert!(matches!(
            Store::open(path.clone()),
            Err(StoreError::Future { version: 42, .. })
        ));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_prompt_reservations_do_not_change_another_session() {
        let path = temporary();
        let store = Store::open(path.clone()).unwrap();
        store.save(&record("one")).unwrap();
        store.save(&record("two")).unwrap();
        store.reserve_prompt(&record("one"), "key", "[]").unwrap();
        assert!(store.reserve_prompt(&record("one"), "key", "[1]").is_err());
        assert_eq!(store.prompt("one", "key").unwrap().unwrap().0, "[]");
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
            1
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
