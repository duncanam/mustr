//! SQLite persistence owned by one blocking actor. State changes commit before replies.

use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

use color_eyre::eyre::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::{hub::State, paths::check_regular, protocol::SpaceId};

const VERSION: u32 = 4;

pub(crate) struct Store {
    connection: Connection,
    space_id: SpaceId,
}

fn migrate_v3(connection: &mut Connection, space_id: &SpaceId) -> Result<()> {
    // Server::start holds the per-space startup lock before Store::open reaches this migration.
    // The SQLite transaction also serializes the validation and version-only write.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(
        version == 3,
        "state schema changed while starting; retry the hub"
    );

    let owner: String = transaction
        .query_row("SELECT space_id FROM owner WHERE id = 1", [], |row| {
            row.get(0)
        })
        .wrap_err("reading v3 mustr database owner")?;
    ensure!(
        owner == space_id.as_str(),
        "state database is bound to another herdr space"
    );

    let payload: Option<String> = transaction
        .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
        .optional()?;
    if let Some(payload) = payload {
        let state: State = serde_json::from_str(&payload)
            .wrap_err("invalid v3 hub state; refusing to silently discard task history")?;
        state.validate_space(space_id)?;
    }

    transaction.pragma_update(None, "user_version", VERSION)?;
    transaction
        .commit()
        .wrap_err("committing mustr v3-to-v4 semantic migration")
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
        let mut connection = Connection::open(path).wrap_err("opening mustr state database")?;
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
        } else if version == 3 {
            migrate_v3(&mut connection, &space_id)?;
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
    use crate::{
        config::Scheduling,
        hub::State,
        paths::digest,
        protocol::{
            Activity, AgentId, ClientHarness, Command, ContextMode, Delivery, Member, Request,
            SpaceId, Task, TaskId, TaskState,
        },
    };
    use std::path::Path;

    fn table_schema(connection: &Connection) -> Vec<(String, Option<String>)> {
        let mut statement = connection
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    fn create_v3_fixture(
        path: &Path,
        owner_space: &SpaceId,
        payload: &str,
    ) -> Vec<(String, Option<String>)> {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE hub (id INTEGER PRIMARY KEY CHECK(id = 1), payload TEXT NOT NULL);
                 CREATE TABLE owner (id INTEGER PRIMARY KEY CHECK(id = 1), space_id TEXT NOT NULL);
                 PRAGMA user_version=3;",
            )
            .unwrap();
        connection
            .execute("INSERT INTO hub (id, payload) VALUES (1, ?1)", [payload])
            .unwrap();
        connection
            .execute(
                "INSERT INTO owner (id, space_id) VALUES (1, ?1)",
                [owner_space.as_str()],
            )
            .unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3, "fixture must be v3 before Store::open");
        table_schema(&connection)
    }

    fn assert_v3_fixture_unchanged(
        path: &Path,
        owner_space: &SpaceId,
        payload: &str,
        schema: &[(String, Option<String>)],
    ) {
        let connection = Connection::open(path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let stored_payload: String = connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored_payload, payload);
        let stored_owner: String = connection
            .query_row("SELECT space_id FROM owner WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored_owner, owner_space.as_str());
        assert_eq!(table_schema(&connection), schema);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM hub", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM owner", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    fn join_for_cache_test(state: &mut State, id: &AgentId, manager: bool, now: u64) {
        let space = state.space_id().clone();
        let config = Scheduling::default();
        state
            .apply_validated_join_with_harness(
                &Request {
                    id: format!("join-{id}-{now}"),
                    command: Command::Join {
                        version: crate::protocol::VERSION,
                        agent_id: id.clone(),
                        name: id.to_string(),
                        role: "custom".into(),
                        pane_id: format!("w1:{id}"),
                        space_id: space,
                        cwd: None,
                        context_modes: vec![ContextMode::Reuse, ContextMode::Fresh],
                        reset: None,
                    },
                },
                None,
                now,
                &config,
                manager,
                ClientHarness::Pi,
            )
            .unwrap();
        state
            .apply(
                &Request {
                    id: format!("idle-{id}-{now}"),
                    command: Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                },
                Some(id),
                now,
                &config,
            )
            .unwrap();
    }

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
    fn refuses_malformed_v3_state_without_rewriting_payload_or_schema_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let payload = "{malformed legacy state";
        let space = SpaceId::new("w1");
        let schema = create_v3_fixture(&path, &space, payload);
        assert!(Store::open(&path, space.clone()).is_err());
        assert_v3_fixture_unchanged(&path, &space, payload, &schema);
    }

    #[test]
    fn refuses_unknown_v3_top_level_and_nested_typed_fields_without_mutation() {
        let space = SpaceId::new("w1");
        for nested in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("unknown-v3.sqlite");
            let mut value = serde_json::to_value(State::new(space.clone())).unwrap();
            if nested {
                value["automation"]["future_automation_field"] = serde_json::json!(true);
            } else {
                value["future_state_field"] = serde_json::json!(true);
            }
            let payload = serde_json::to_string(&value).unwrap();
            let schema = create_v3_fixture(&path, &space, &payload);
            assert_eq!(
                Connection::open(&path)
                    .unwrap()
                    .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                3,
                "negative fixture must still be v3 before opening"
            );
            assert!(Store::open(&path, space.clone()).is_err());
            assert_v3_fixture_unchanged(&path, &space, &payload, &schema);
        }
    }

    #[test]
    fn refuses_wrong_v3_payload_and_reset_spaces_without_mutation() {
        let requested_space = SpaceId::new("w1");
        let wrong_payload = serde_json::to_string(&State::new(SpaceId::new("w2"))).unwrap();
        let mut right_space = serde_json::to_value(State::new(requested_space.clone())).unwrap();
        right_space["reset"] = serde_json::json!({
            "plan_id":"wrong-space-reset",
            "space_id":"w2",
            "epoch":1,
            "state":"prepared",
            "note":"",
            "offline_count":0,
            "agents":[]
        });
        let wrong_reset = serde_json::to_string(&right_space).unwrap();

        for payload in [&wrong_payload, &wrong_reset] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("wrong-space-v3.sqlite");
            let schema = create_v3_fixture(&path, &requested_space, payload);
            assert!(Store::open(&path, requested_space.clone()).is_err());
            assert_v3_fixture_unchanged(&path, &requested_space, payload, &schema);
        }
    }

    #[test]
    fn rejects_wrong_owner_during_v3_migration_and_tampered_reset_afterward() {
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
        store
            .connection
            .pragma_update(None, "user_version", 3)
            .unwrap();
        drop(store);
        assert!(Store::open(&path, SpaceId::new("w2")).is_err());
        let version: u32 = Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3, "wrong-owner migration must roll back");

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
        store
            .connection
            .pragma_update(None, "user_version", 3)
            .unwrap();
        drop(store);
        let store = Store::open(&path, space.clone()).unwrap();
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
    fn populated_v3_history_and_cache_migrate_exactly_without_retagging_legacy_assign() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("populated-v3.sqlite");
        let space = SpaceId::new("w1");
        let requester = AgentId::new("requester");
        let owner = AgentId::new("owner");
        let mut state = State::new(space.clone());
        join_for_cache_test(&mut state, &requester, true, 1);
        join_for_cache_test(&mut state, &owner, false, 1);
        let request_id = "legacy-contextless-assign";
        let title = "Legacy cached assignment";
        let text = "Original v3 command had no context property";
        let receipt = state
            .apply(
                &Request {
                    id: request_id.into(),
                    command: Command::Assign {
                        to: owner.clone(),
                        title: title.into(),
                        text: text.into(),
                        context: ContextMode::Reuse,
                    },
                },
                Some(&requester),
                2,
                &Scheduling::default(),
            )
            .unwrap();
        let task_id: TaskId = serde_json::from_value(receipt["task_id"].clone()).unwrap();

        let mut legacy = serde_json::to_value(&state).unwrap();
        legacy.as_object_mut().unwrap().remove("next_seq");
        legacy["members"][requester.as_str()]
            .as_object_mut()
            .unwrap()
            .remove("manager");
        legacy["automation"]
            .as_object_mut()
            .unwrap()
            .remove("manager_checkin_at");
        legacy["tasks"][task_id.as_str()]
            .as_object_mut()
            .unwrap()
            .remove("context");
        legacy["tasks"][task_id.as_str()]["seq"] = serde_json::json!(0);
        legacy["tasks"][task_id.as_str()]
            .as_object_mut()
            .unwrap()
            .remove("seq");
        legacy["messages"][0]
            .as_object_mut()
            .unwrap()
            .remove("context");
        legacy["cache"][0]["fingerprint"] = serde_json::json!(digest(
            r#"{"op":"assign","to":"owner","title":"Legacy cached assignment","text":"Original v3 command had no context property"}"#
        ));
        legacy["cache"][0]["result"]
            .as_object_mut()
            .unwrap()
            .remove("context");
        let expected_cache = legacy["cache"].clone();
        let legacy_payload = serde_json::to_string(&legacy).unwrap();
        let schema = create_v3_fixture(&path, &space, &legacy_payload);

        let store = Store::open(&path, space).unwrap();
        let version: u32 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, VERSION);
        assert_eq!(
            table_schema(&store.connection),
            schema,
            "successful migration leaves every table definition unchanged"
        );
        let stored_payload: String = store
            .connection
            .query_row("SELECT payload FROM hub WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            stored_payload, legacy_payload,
            "migration does not rewrite populated history or cache bytes"
        );
        let mut loaded = store.load().unwrap();
        assert!(
            !loaded.members[&requester].manager,
            "omitted known manager default remains false"
        );
        assert_eq!(loaded.automation.manager_checkin_at, None);
        assert_eq!(
            loaded.tasks[&task_id].seq, 0,
            "omitted known task sequence defaults as before"
        );
        assert_eq!(loaded.tasks[&task_id].context, None);
        assert_eq!(loaded.messages[0].context, None);
        let loaded_json = serde_json::to_value(&loaded).unwrap();
        assert_eq!(
            loaded_json["next_seq"], 0,
            "omitted known sequence default remains unchanged until recovery"
        );
        assert_eq!(
            loaded_json["cache"], expected_cache,
            "cache fingerprints and opaque results are retained exactly"
        );
        assert!(loaded_json["cache"][0]["result"].get("context").is_none());

        loaded.recover();
        assert_eq!(loaded.tasks[&task_id].state, TaskState::Paused);
        assert_eq!(loaded.messages[0].delivery, Delivery::Uncertain);
        join_for_cache_test(&mut loaded, &requester, true, 3);
        let preserved_cache = serde_json::to_value(&loaded).unwrap()["cache"].clone();
        for mode in [ContextMode::Reuse, ContextMode::Fresh] {
            let before = loaded.clone();
            let error = loaded
                .apply(
                    &Request {
                        id: request_id.into(),
                        command: Command::Assign {
                            to: owner.clone(),
                            title: title.into(),
                            text: text.into(),
                            context: mode,
                        },
                    },
                    Some(&requester),
                    4,
                    &Scheduling::default(),
                )
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("request ID reused with different contents")
            );
            assert_eq!(
                loaded, before,
                "an old cache entry cannot replay or retag the legacy request"
            );
            let after = serde_json::to_value(&loaded).unwrap();
            assert_eq!(after["cache"], preserved_cache);
            assert_eq!(
                loaded.tasks.len(),
                1,
                "an explicit choice does not duplicate the cached legacy task"
            );
            assert_eq!(
                loaded.messages.len(),
                1,
                "the uncertain original assignment is not replayed"
            );
            assert_eq!(loaded.messages[0].delivery, Delivery::Uncertain);
        }
    }

    #[test]
    fn v3_fifo_fields_default_and_recover_through_lossless_migration() {
        assert_eq!(VERSION, 4);
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
                    context: None,
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
        store
            .connection
            .pragma_update(None, "user_version", 3)
            .unwrap();
        drop(store);
        let store = Store::open(&path, space.clone()).unwrap();

        let mut loaded = store.load().unwrap();
        assert_eq!(loaded.tasks[&TaskId::new("legacy-zero")].seq, 0);
        assert_eq!(serde_json::to_value(&loaded).unwrap()["next_seq"], 0);
        loaded.recover();
        assert_eq!(serde_json::to_value(&loaded).unwrap()["next_seq"], 18);
        for (id, manager) in [(&requester, true), (&owner, false)] {
            loaded
                .apply_validated_join_with_harness(
                    &crate::protocol::Request {
                        id: format!("rejoin-{id}"),
                        command: crate::protocol::Command::Join {
                            version: crate::protocol::VERSION,
                            agent_id: id.clone(),
                            name: id.to_string(),
                            role: "custom".into(),
                            pane_id: format!("w1:{id}"),
                            space_id: space.clone(),
                            cwd: None,
                            context_modes: vec![ContextMode::Reuse, ContextMode::Fresh],
                            reset: None,
                        },
                    },
                    None,
                    2,
                    &crate::config::Scheduling::default(),
                    manager,
                    ClientHarness::Pi,
                )
                .unwrap();
            loaded
                .apply(
                    &Request {
                        id: format!("idle-{id}"),
                        command: Command::Sync {
                            activity: Activity::Idle,
                            receive: false,
                        },
                    },
                    Some(id),
                    2,
                    &crate::config::Scheduling::default(),
                )
                .unwrap();
        }
        let assigned = loaded
            .apply(
                &crate::protocol::Request {
                    id: "after-recovery".into(),
                    command: crate::protocol::Command::Assign {
                        to: owner.clone(),
                        title: "After restart".into(),
                        text: "Continue from a recovered sequence".into(),
                        context: crate::protocol::ContextMode::Reuse,
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
        assert_eq!(version, VERSION, "v3 state must migrate to schema v4");
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
