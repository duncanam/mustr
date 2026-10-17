//! SQLite persistence owned by one blocking actor. State changes commit before replies.

use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

use color_eyre::eyre::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension};

use crate::{hub::State, paths::check_regular};

pub(crate) struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        check_regular(path)?;
        let connection = Connection::open(path).wrap_err("opening mustr state database")?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // v2 adds durable reset fences. Older binaries must refuse this database,
        // not deserialize State while silently ignoring retired identities.
        ensure!(version <= 2, "database belongs to a newer mustr version");
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS hub (id INTEGER PRIMARY KEY CHECK(id = 1), payload TEXT NOT NULL); PRAGMA user_version=2;")?;
        Ok(Self { connection })
    }

    pub fn load(&self) -> Result<State> {
        let payload: Option<String> = self
            .connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .optional()?;
        match payload {
            Some(payload) => serde_json::from_str(&payload)
                .wrap_err("invalid hub state; refusing to silently discard task history"),
            None => Ok(State::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_read_only_for_test(&self, read_only: bool) {
        self.connection
            .pragma_update(None, "query_only", read_only)
            .unwrap();
    }

    pub fn save(&mut self, state: &State) -> Result<()> {
        let payload = serde_json::to_string(state)?;
        ensure!(
            payload.len() <= 16 * 1024 * 1024,
            "state exceeds 16 MiB; archive the stopped hub's history"
        );
        let transaction = self.connection.transaction()?;
        transaction.execute("INSERT INTO hub (id, payload) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET payload = excluded.payload", [payload])?;
        transaction.commit().wrap_err("committing hub state")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrates_legacy_state_defaults_and_refuses_future_versions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE hub (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
        connection
            .execute(
                "INSERT INTO hub VALUES (1, ?1)",
                [r#"{"members":{},"tasks":{},"messages":[],"automation":{}}"#],
            )
            .unwrap();
        drop(connection);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.load().unwrap(), State::default());
        let version: u32 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(
            version, 2,
            "old binaries must reject databases with reset fences"
        );
        store
            .connection
            .pragma_update(None, "user_version", 3)
            .unwrap();
        drop(store);
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn persists_state_and_rejects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("hub.sqlite")).unwrap();
        store.save(&State::default()).unwrap();
        assert!(store.load().unwrap().tasks.is_empty());
        store
            .connection
            .execute("UPDATE hub SET payload = 'broken'", [])
            .unwrap();
        assert!(store.load().is_err());
    }
}
