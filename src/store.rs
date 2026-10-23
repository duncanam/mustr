//! SQLite persistence owned by one blocking actor. State changes commit before replies.

use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

use color_eyre::eyre::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension};

use crate::{hub::State, paths::check_regular, protocol::SpaceId};

const VERSION: u32 = 3;

pub(crate) struct Store {
    connection: Connection,
    space_id: SpaceId,
}

impl Store {
    pub fn open(path: &Path, space_id: SpaceId) -> Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        check_regular(path)?;
        let connection = Connection::open(path).wrap_err("opening mustr state database")?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version == 0 {
            let tables: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            ensure!(
                tables == 0,
                "unversioned mustr database contains data; refusing to overwrite it"
            );
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE hub (id INTEGER PRIMARY KEY CHECK(id = 1), payload TEXT NOT NULL);
                 CREATE TABLE owner (id INTEGER PRIMARY KEY CHECK(id = 1), space_id TEXT NOT NULL);
                ",
            )?;
            connection.execute(
                "INSERT INTO owner (id, space_id) VALUES (1, ?1)",
                [space_id.as_str()],
            )?;
            connection.execute_batch(&format!("PRAGMA user_version={VERSION}; COMMIT;"))?;
        } else {
            ensure!(
                version == VERSION,
                if version > VERSION {
                    "database belongs to a newer mustr version"
                } else {
                    "incompatible legacy mustr database; per-space state starts fresh and does not import old global history"
                }
            );
        }

        let owner: String = connection
            .query_row("SELECT space_id FROM owner WHERE id = 1", [], |row| {
                row.get(0)
            })
            .wrap_err("reading mustr database owner")?;
        ensure!(
            owner == space_id.as_str(),
            "state database is bound to another herdr space"
        );
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        Ok(Self {
            connection,
            space_id,
        })
    }

    pub fn load(&self) -> Result<State> {
        let payload: Option<String> = self
            .connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .optional()?;
        let state = match payload {
            Some(payload) => serde_json::from_str(&payload)
                .wrap_err("invalid hub state; refusing to silently discard task history")?,
            None => State::new(self.space_id.clone()),
        };
        state.validate_space(&self.space_id)?;
        Ok(state)
    }

    #[cfg(test)]
    pub(crate) fn set_read_only_for_test(&self, read_only: bool) {
        self.connection
            .pragma_update(None, "query_only", read_only)
            .unwrap();
    }

    pub fn save(&mut self, state: &State) -> Result<()> {
        state.validate_space(&self.space_id)?;
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
    use crate::protocol::{Activity, AgentId, Member, SpaceId, Task, TaskId, TaskState};

    #[test]
    fn refuses_legacy_data_without_changing_its_version_or_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE hub (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); PRAGMA user_version=2;")
            .unwrap();
        let payload = r#"{"members":{},"tasks":{},"messages":[],"automation":{}}"#;
        connection
            .execute("INSERT INTO hub VALUES (1, ?1)", [payload])
            .unwrap();
        drop(connection);

        assert!(Store::open(&path, SpaceId::new("w1")).is_err());
        let connection = Connection::open(&path).unwrap();
        let stored: String = connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(stored, payload);
        assert_eq!(version, 2);
    }

    #[test]
    fn refuses_future_schema_without_changing_its_version_or_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE hub (id INTEGER PRIMARY KEY, payload TEXT NOT NULL); PRAGMA user_version={};",
                VERSION + 1
            ))
            .unwrap();
        let payload = r#"{"history":"must remain intact"}"#;
        connection
            .execute("INSERT INTO hub VALUES (1, ?1)", [payload])
            .unwrap();
        drop(connection);

        let error = Store::open(&path, SpaceId::new("w1"))
            .err()
            .expect("future database schema must be rejected");
        assert!(
            error
                .to_string()
                .contains("database belongs to a newer mustr version"),
            "unexpected refusal reason: {error}"
        );
        let connection = Connection::open(&path).unwrap();
        let stored: String = connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(stored, payload);
        assert_eq!(version, VERSION + 1);
    }

    #[test]
    fn binds_v3_state_to_one_space_and_rejects_tampered_reset_owner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let space = SpaceId::new("w1");
        let mut store = Store::open(&path, space.clone()).unwrap();
        let mut state = State::new(space.clone());
        state.members.insert(
            AgentId::new("a"),
            crate::protocol::Member {
                id: AgentId::new("a"),
                name: "a".into(),
                role: "custom".into(),
                pane_id: "p".into(),
                space_id: space.clone(),
                manager: false,
                cwd: None,
                activity: crate::protocol::Activity::Offline,
                last_seen: 0,
                idle_since: 0,
            },
        );
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
        drop(store);
        assert!(Store::open(&path, SpaceId::new("w2")).is_err());

        let store = Store::open(&path, space).unwrap();
        let mut value = serde_json::to_value(state).unwrap();
        value["reset"] = serde_json::json!({
            "plan_id":"tampered",
            "space_id":"w2",
            "epoch":0,
            "state":"prepared",
            "note":"",
            "offline_count":0,
            "agents":[]
        });
        let payload = serde_json::to_string(&value).unwrap();
        store
            .connection
            .execute(
                "UPDATE hub SET payload = ?1 WHERE id = 1",
                [payload.clone()],
            )
            .unwrap();
        assert!(store.load().is_err());
        let stored: String = store
            .connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, payload, "failed validation must not rewrite data");
    }

    #[test]
    fn v3_payload_without_member_cwd_and_with_string_pr_url_remains_readable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let space = SpaceId::new("w1");
        let mut state = State::new(space.clone());
        // A legacy database's stored false is an explicit user choice and is
        // not upgraded to the new fresh-state default.
        state.automation.checkin_enabled = false;
        let agent = AgentId::new("agent");
        state.members.insert(
            agent.clone(),
            crate::protocol::Member {
                id: agent.clone(),
                name: "worker".into(),
                role: "custom".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                manager: false,
                cwd: None,
                activity: crate::protocol::Activity::Offline,
                last_seen: 1,
                idle_since: 1,
            },
        );
        state.automation.pr = Some(crate::watchers::PrWatch {
            url: Some("https://github.com/example/repo/pull/1".into()),
            cwd: "/checkout".into(),
            driver: agent.clone(),
            branch: Some("feature".into()),
            head: Some("abc".into()),
            baseline: None,
            next_at: 2,
            error: None,
        });
        let mut store = Store::open(&path, space.clone()).unwrap();
        store.save(&state).unwrap();
        let mut legacy = serde_json::to_value(&state).unwrap();
        legacy["members"]["agent"]
            .as_object_mut()
            .unwrap()
            .remove("manager");
        legacy["automation"]
            .as_object_mut()
            .unwrap()
            .remove("manager_checkin_at");
        store
            .connection
            .execute(
                "UPDATE hub SET payload = ?1 WHERE id = 1",
                [serde_json::to_string(&legacy).unwrap()],
            )
            .unwrap();
        let loaded = store.load().unwrap();
        assert!(!loaded.members[&agent].manager);
        assert_eq!(loaded.automation.manager_checkin_at, None);
        assert!(!loaded.automation.checkin_enabled);
        assert_eq!(loaded.members[&agent].cwd, None);
        assert_eq!(
            loaded.automation.pr.unwrap().url.as_deref(),
            Some("https://github.com/example/repo/pull/1")
        );
    }

    #[test]
    fn v3_fifo_fields_default_and_recover_without_a_schema_migration() {
        assert_eq!(VERSION, 3);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let space = SpaceId::new("w1");
        let requester = AgentId::new("requester");
        let owner = AgentId::new("owner");
        let mut state = State::new(space.clone());
        for id in [&requester, &owner] {
            state.members.insert(
                id.clone(),
                Member {
                    id: id.clone(),
                    name: id.to_string(),
                    role: "custom".into(),
                    pane_id: format!("w1:{}", id),
                    space_id: space.clone(),
                    manager: false,
                    cwd: None,
                    activity: Activity::Idle,
                    last_seen: 1,
                    idle_since: 1,
                },
            );
        }
        for (id, seq) in [("legacy-zero", 0), ("legacy-high", 17)] {
            state.tasks.insert(
                TaskId::new(id),
                Task {
                    id: TaskId::new(id),
                    seq,
                    space_id: space.clone(),
                    requester: requester.clone(),
                    owner: owner.clone(),
                    title: id.into(),
                    description: "legacy task".into(),
                    state: TaskState::Accepted,
                    revision: 1,
                    updated_at: 1,
                    checkin_at: 0,
                    nudges: 0,
                    note: String::new(),
                },
            );
        }
        let mut store = Store::open(&path, space.clone()).unwrap();
        store.save(&state).unwrap();
        let mut legacy = serde_json::to_value(&state).unwrap();
        legacy.as_object_mut().unwrap().remove("next_seq");
        legacy["tasks"]["legacy-zero"]
            .as_object_mut()
            .unwrap()
            .remove("seq");
        legacy["tasks"]["legacy-high"]["seq"] = serde_json::json!(17);
        let legacy_payload = serde_json::to_string(&legacy).unwrap();
        store
            .connection
            .execute(
                "UPDATE hub SET payload = ?1 WHERE id = 1",
                [&legacy_payload],
            )
            .unwrap();

        let mut loaded = store.load().unwrap();
        assert_eq!(loaded.tasks[&TaskId::new("legacy-zero")].seq, 0);
        assert_eq!(serde_json::to_value(&loaded).unwrap()["next_seq"], 0);
        loaded.recover();
        assert_eq!(serde_json::to_value(&loaded).unwrap()["next_seq"], 18);
        for member in loaded.members.values_mut() {
            member.activity = Activity::Idle;
        }
        let assigned = loaded
            .apply(
                &crate::protocol::Request {
                    id: "after-recovery".into(),
                    command: crate::protocol::Command::Assign {
                        to: owner.clone(),
                        title: "After restart".into(),
                        text: "Continue from a recovered sequence".into(),
                    },
                },
                Some(&requester),
                2,
                &crate::config::Scheduling::default(),
            )
            .unwrap();
        let assigned_id: TaskId = serde_json::from_value(assigned["task_id"].clone()).unwrap();
        assert_eq!(loaded.tasks[&assigned_id].seq, 18);

        let stored: String = store
            .connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        let version: u32 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(
            stored, legacy_payload,
            "loading must not rewrite legacy state"
        );
        assert_eq!(version, 3, "FIFO fields do not bump the v3 schema");
    }

    #[test]
    fn persists_state_and_rejects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let space = SpaceId::new("w1");
        let mut store = Store::open(&dir.path().join("hub.sqlite"), space.clone()).unwrap();
        store.save(&State::new(space)).unwrap();
        assert!(store.load().unwrap().tasks.is_empty());
        store
            .connection
            .execute("UPDATE hub SET payload = 'broken'", [])
            .unwrap();
        assert!(store.load().is_err());
    }
}
