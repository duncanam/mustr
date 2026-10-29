use super::*;
use std::path::Path;

use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn only_current_admitted_manager_can_create_context_assignments() {
    for context in [ContextMode::Reuse, ContextMode::Fresh] {
        check_assignment_manager(context);
    }
}

fn check_assignment_manager(context: ContextMode) {
    let (mut state, manager, worker, config) = setup();
    // A worker's display name/role cannot confer server-admitted authority.
    state.members.get_mut(&worker).unwrap().name = "architect".into();
    state.members.get_mut(&worker).unwrap().role = "architect".into();
    let unauthorized = request(
        "worker-assign-context",
        Command::Assign {
            to: manager.clone(),
            title: "Choose context".into(),
            text: "Please delegate this task".into(),
            context,
        },
    );
    let before = state.clone();
    let error = state
        .apply(&unauthorized, Some(&worker), 1, &config)
        .unwrap_err();
    assert!(error.to_string().contains("current admitted manager"));
    assert_eq!(state, before, "nonmanager denial must be mutation-free");

    let authorized = request(
        "manager-assign-fresh-context",
        Command::Assign {
            to: worker.clone(),
            title: "Fresh task".into(),
            text: "Use an explicit task-scoped context".into(),
            context,
        },
    );
    let result = state
        .apply(&authorized, Some(&manager), 2, &config)
        .unwrap();
    assert_eq!(result["context"], serde_json::to_value(context).unwrap());
    let task_id: TaskId = serde_json::from_value(result["task_id"].clone()).unwrap();
    assert_eq!(state.tasks[&task_id].requester, manager);
    assert_eq!(state.tasks[&task_id].context, Some(context));

    state.members.get_mut(&manager).unwrap().manager = false;
    let before_replay = state.clone();
    let error = state
        .apply(&authorized, Some(&manager), 3, &config)
        .unwrap_err();
    assert!(error.to_string().contains("current admitted manager"));
    assert_eq!(
        state, before_replay,
        "cached success cannot outlive manager admission"
    );
}

fn join_as(
    state: &mut State,
    id: &AgentId,
    harness: ClientHarness,
    context_modes: Vec<ContextMode>,
    now: u64,
    config: &Scheduling,
) {
    join_as_with_manager(state, id, harness, context_modes, now, config, false);
}

fn join_as_with_manager(
    state: &mut State,
    id: &AgentId,
    harness: ClientHarness,
    context_modes: Vec<ContextMode>,
    now: u64,
    config: &Scheduling,
    manager: bool,
) {
    state
        .apply_validated_join_with_harness(
            &request(
                &format!("join-{id}-{now}"),
                Command::Join {
                    version: VERSION,
                    agent_id: id.clone(),
                    name: id.to_string(),
                    role: "custom".into(),
                    pane_id: format!("w1:{id}"),
                    space_id: SpaceId::new("w1"),
                    cwd: None,
                    context_modes,
                    reset: None,
                },
            ),
            None,
            now,
            config,
            manager,
            harness,
        )
        .unwrap();
    state
        .apply(
            &request(
                &format!("idle-{id}-{now}"),
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(id),
            now,
            config,
        )
        .unwrap();
}

fn write_v3_state(path: &Path, state: &State) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE hub (id INTEGER PRIMARY KEY CHECK(id = 1), payload TEXT NOT NULL);
             CREATE TABLE owner (id INTEGER PRIMARY KEY CHECK(id = 1), space_id TEXT NOT NULL);
             PRAGMA user_version=3;",
        )
        .unwrap();
    let payload = serde_json::to_string(state).unwrap();
    connection
        .execute(
            "INSERT INTO owner (id, space_id) VALUES (1, ?1)",
            [state.space_id().as_str()],
        )
        .unwrap();
    connection
        .execute("INSERT INTO hub (id, payload) VALUES (1, ?1)", [payload])
        .unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3, "fixture must be v3 before Store::open");
}

#[test]
fn pi_join_advertises_only_its_declared_context_modes() {
    let mut state = State::new(SpaceId::new("w1"));
    let config = Scheduling::default();
    let pi_modes = request(
        "pi-all-modes",
        Command::Join {
            version: VERSION,
            agent_id: AgentId::new("pi"),
            name: "pi".into(),
            role: "custom".into(),
            pane_id: "w1:p1".into(),
            space_id: SpaceId::new("w1"),
            cwd: None,
            context_modes: vec![ContextMode::Reuse, ContextMode::Fresh],
            reset: None,
        },
    );
    let result = state
        .apply_validated_join_with_harness(&pi_modes, None, 1, &config, false, ClientHarness::Pi)
        .unwrap();
    assert_eq!(result["context_selection"], 1);
    assert_eq!(
        result["context_modes"],
        serde_json::json!(["reuse", "fresh"])
    );
    let before = state.clone();

    let unverified = request(
        "unverified-modes",
        Command::Join {
            version: VERSION,
            agent_id: AgentId::new("unverified"),
            name: "unverified".into(),
            role: "custom".into(),
            pane_id: "w1:p2".into(),
            space_id: SpaceId::new("w1"),
            cwd: None,
            context_modes: vec![ContextMode::Reuse],
            reset: None,
        },
    );
    assert!(
        state
            .apply(&unverified, None, 1, &config)
            .unwrap_err()
            .to_string()
            .contains("server-verified pane harness")
    );
    assert_eq!(state, before);
}

#[test]
fn assign_requires_advertised_context_even_for_reuse() {
    let (mut state, architect, worker, config) = setup();
    state.context_capabilities.remove(&worker);
    let before = state.clone();
    let error = state
        .apply(
            &request(
                "assign-without-context-capability",
                Command::Assign {
                    to: worker,
                    title: "Explicit reuse is still negotiated".into(),
                    text: "No default applies to a recipient without reuse capability.".into(),
                    context: ContextMode::Reuse,
                },
            ),
            Some(&architect),
            1,
            &config,
        )
        .unwrap_err();
    assert!(error.to_string().contains("does not currently advertise"));
    assert_eq!(state, before, "rejected assignment has no state effects");
}

#[test]
fn selected_context_flows_through_task_assignment_guide_and_envelope() {
    let (mut state, architect, worker, config) = setup();
    let assigned = state
        .apply(
            &request(
                "assign-fresh",
                Command::Assign {
                    to: worker.clone(),
                    title: "Fresh context".into(),
                    text: "Use an explicitly selected task context.".into(),
                    context: ContextMode::Fresh,
                },
            ),
            Some(&architect),
            1,
            &config,
        )
        .unwrap();
    let task: TaskId = serde_json::from_value(assigned["task_id"].clone()).unwrap();
    assert_eq!(state.tasks[&task].context, Some(ContextMode::Fresh));
    assert_eq!(state.messages[0].context, Some(ContextMode::Fresh));

    let assignment = state
        .apply(
            &request(
                "receive-assignment",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&worker),
            2,
            &config,
        )
        .unwrap();
    assert_eq!(assignment["inbox"]["context"], "fresh");
    let message: MessageId = serde_json::from_value(assignment["inbox"]["id"].clone()).unwrap();
    state
        .apply(
            &request(
                "ack-assignment",
                Command::Ack {
                    message_id: message,
                },
            ),
            Some(&worker),
            2,
            &config,
        )
        .unwrap();
    state
        .apply(
            &request(
                "guide-fresh",
                Command::Guide {
                    task_id: task.clone(),
                    revision: 1,
                    text: "Continue in the same selected task context.".into(),
                },
            ),
            Some(&architect),
            3,
            &config,
        )
        .unwrap();
    assert_eq!(state.tasks[&task].context, Some(ContextMode::Fresh));
    assert_eq!(state.tasks[&task].revision, 2);
    assert_eq!(
        state.messages.last().unwrap().context,
        Some(ContextMode::Fresh)
    );
    let guide = state
        .apply(
            &request(
                "receive-guide",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&worker),
            4,
            &config,
        )
        .unwrap();
    assert_eq!(guide["inbox"]["kind"], "guide");
    assert_eq!(guide["inbox"]["context"], "fresh");
}

#[test]
fn assignment_context_participates_in_idempotency_fingerprints() {
    let (mut state, architect, worker, config) = setup();
    let request_id = "same-assignment-id";
    let first = request(
        request_id,
        Command::Assign {
            to: worker.clone(),
            title: "Explicit reuse".into(),
            text: "The context choice is part of the mutation payload.".into(),
            context: ContextMode::Reuse,
        },
    );
    let created = state.apply(&first, Some(&architect), 1, &config).unwrap();
    let task: TaskId = serde_json::from_value(created["task_id"].clone()).unwrap();
    let conflicting = request(
        request_id,
        Command::Assign {
            to: worker,
            title: "Explicit reuse".into(),
            text: "The context choice is part of the mutation payload.".into(),
            context: ContextMode::Fresh,
        },
    );
    let error = state
        .apply(&conflicting, Some(&architect), 2, &config)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("request ID reused with different contents")
    );
    assert_eq!(state.tasks.len(), 1);
    assert_eq!(state.tasks[&task].context, Some(ContextMode::Reuse));
}

#[test]
fn missing_assign_context_is_rejected_by_the_protocol() {
    let request = serde_json::json!({
        "id": "missing-context",
        "op": "assign",
        "to": "worker",
        "title": "No default",
        "text": "Context is mandatory"
    });
    assert!(serde_json::from_value::<Request>(request).is_err());
}

#[test]
fn unsupported_fifo_head_stays_queued_after_capability_loss_and_blocks_followers() {
    let (mut state, architect, worker, config) = setup();
    let first = assign(&mut state, &architect, &worker, &config);
    state
        .apply(
            &request(
                "queue-fresh-head",
                Command::Assign {
                    to: worker.clone(),
                    title: "Fresh head".into(),
                    text: "A queued head selected fresh on Pi.".into(),
                    context: ContextMode::Fresh,
                },
            ),
            Some(&architect),
            1,
            &config,
        )
        .unwrap();
    let fresh = state
        .tasks
        .values()
        .find(|task| task.title == "Fresh head")
        .unwrap()
        .id
        .clone();
    state
        .apply(
            &request(
                "queue-reuse-follower",
                Command::Assign {
                    to: worker.clone(),
                    title: "Reuse follower".into(),
                    text: "This must not pass the unsupported FIFO head.".into(),
                    context: ContextMode::Reuse,
                },
            ),
            Some(&architect),
            2,
            &config,
        )
        .unwrap();
    let follower = state
        .tasks
        .values()
        .find(|task| task.title == "Reuse follower")
        .unwrap()
        .id
        .clone();
    assert_eq!(state.tasks[&first].state, TaskState::Assigned);
    assert_eq!(state.tasks[&fresh].state, TaskState::Queued);
    assert_eq!(state.tasks[&follower].state, TaskState::Queued);

    state
        .apply(
            &request(
                "finish-holder",
                Command::Report {
                    task_id: first.clone(),
                    revision: 1,
                    kind: ReportKind::Done,
                    text: "The existing holder is complete.".into(),
                },
            ),
            Some(&worker),
            3,
            &config,
        )
        .unwrap();
    state
        .apply(
            &request(
                "accept-holder",
                Command::Accept {
                    task_id: first,
                    revision: 1,
                    text: "Completion reviewed.".into(),
                },
            ),
            Some(&architect),
            4,
            &config,
        )
        .unwrap();

    state.disconnect(&worker);
    join_as(
        &mut state,
        &worker,
        ClientHarness::Pi,
        vec![ContextMode::Reuse],
        5,
        &config,
    );
    state.promote_queued_tasks(6, &config).unwrap();
    assert_eq!(state.tasks[&fresh].state, TaskState::Queued);
    assert_eq!(state.tasks[&follower].state, TaskState::Queued);

    let roster = state
        .apply(
            &request(
                "context-roster",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&architect),
            6,
            &config,
        )
        .unwrap();
    let fresh_row = roster["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == fresh.as_str())
        .unwrap();
    let follower_row = roster["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == follower.as_str())
        .unwrap();
    assert_eq!(fresh_row["queue_position"], 1);
    assert_eq!(follower_row["queue_position"], 2);
    assert!(
        fresh_row["context_blocked"]
            .as_str()
            .unwrap()
            .contains("fresh")
    );
    assert!(follower_row.get("context_blocked").is_none());
    let worker_row = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["id"] == worker.as_str())
        .unwrap();
    assert_eq!(worker_row["context_modes"], serde_json::json!(["reuse"]));
    let next = &worker_row["task_queue"]["next_queued"];
    assert_eq!(next["id"], fresh.as_str());
    assert!(next["context_blocked"].as_str().unwrap().contains("fresh"));
}

#[test]
fn guide_rechecks_context_support_after_a_pi_client_rejoins_without_fresh() {
    let (mut state, architect, worker, config) = setup();
    let task = state
        .apply(
            &request(
                "assign-fresh",
                Command::Assign {
                    to: worker.clone(),
                    title: "Fresh task".into(),
                    text: "This older Pi client does not advertise Fresh.".into(),
                    context: ContextMode::Fresh,
                },
            ),
            Some(&architect),
            1,
            &config,
        )
        .unwrap();
    let task: TaskId = serde_json::from_value(task["task_id"].clone()).unwrap();
    state.disconnect(&worker);
    join_as(
        &mut state,
        &worker,
        ClientHarness::Pi,
        vec![ContextMode::Reuse],
        2,
        &config,
    );
    let before = state.clone();
    let error = state
        .apply(
            &request(
                "guide-after-capability-loss",
                Command::Guide {
                    task_id: task,
                    revision: 1,
                    text: "Do not resume without the selected context capability.".into(),
                },
            ),
            Some(&architect),
            3,
            &config,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("owner no longer advertises the task's selected context mode")
    );
    assert_eq!(state, before, "rejected Guide has no state effects");
}

#[test]
fn offer_time_check_still_blocks_an_unsupported_selected_mode() {
    let (mut state, architect, worker, config) = setup();
    let assigned = state
        .apply(
            &request(
                "assign-fresh",
                Command::Assign {
                    to: worker.clone(),
                    title: "Fresh assignment".into(),
                    text: "Offer-time capability is checked again.".into(),
                    context: ContextMode::Fresh,
                },
            ),
            Some(&architect),
            1,
            &config,
        )
        .unwrap();
    let assigned: TaskId = serde_json::from_value(assigned["task_id"].clone()).unwrap();
    state
        .context_capabilities
        .get_mut(&worker)
        .unwrap()
        .remove(&ContextMode::Fresh);
    let offer = state
        .apply(
            &request(
                "offer-after-capability-loss",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&worker),
            2,
            &config,
        )
        .unwrap();
    assert!(offer["inbox"].is_null());
    assert_eq!(state.tasks[&assigned].state, TaskState::Assigned);
    assert_eq!(state.messages[0].delivery, Delivery::Pending);
}

#[test]
fn selected_mode_task_does_not_pass_offer_when_its_message_mode_is_missing() {
    let (mut state, architect, worker, config) = setup();
    let assigned = state
        .apply(
            &request(
                "assign-fresh-for-message-mismatch",
                Command::Assign {
                    to: worker.clone(),
                    title: "Fresh assignment".into(),
                    text: "Task and message context must agree.".into(),
                    context: ContextMode::Fresh,
                },
            ),
            Some(&architect),
            1,
            &config,
        )
        .unwrap();
    let task: TaskId = serde_json::from_value(assigned["task_id"].clone()).unwrap();
    state.messages[0].context = None;
    let rejected = state
        .apply(
            &request(
                "offer-mismatched-context",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&worker),
            2,
            &config,
        )
        .unwrap();
    assert!(rejected["inbox"].is_null());
    assert_eq!(state.messages[0].delivery, Delivery::Pending);
    assert_eq!(state.tasks[&task].context, Some(ContextMode::Fresh));
}

#[test]
fn migrated_legacy_queue_head_and_modern_follower_progress_without_inference() {
    let (mut old_state, requester, owner, config) = setup();
    let space = old_state.space_id().clone();
    old_state.members.get_mut(&requester).unwrap().activity = Activity::Offline;
    old_state.members.get_mut(&owner).unwrap().activity = Activity::Offline;
    let legacy_id = TaskId::new("legacy-queued-head");
    old_state.tasks.insert(
        legacy_id.clone(),
        Task {
            id: legacy_id.clone(),
            seq: 1,
            space_id: space.clone(),
            requester: requester.clone(),
            owner: owner.clone(),
            title: "Legacy queued head".into(),
            description: "Continue this existing task without manufacturing a mode.".into(),
            state: TaskState::Queued,
            revision: 1,
            updated_at: 1,
            checkin_at: 0,
            nudges: 0,
            note: String::new(),
            context: None,
        },
    );
    old_state.next_seq = 2;
    let directory = tempdir().unwrap();
    let database = directory.path().join("legacy-v3.sqlite");
    write_v3_state(&database, &old_state);
    let store = crate::store::Store::open(&database, space.clone()).unwrap();
    let mut state = store.load().unwrap();
    state.recover();
    join_as_with_manager(
        &mut state,
        &requester,
        ClientHarness::Pi,
        vec![ContextMode::Reuse, ContextMode::Fresh],
        2,
        &config,
        true,
    );
    join_as(
        &mut state,
        &owner,
        ClientHarness::Pi,
        vec![ContextMode::Reuse, ContextMode::Fresh],
        3,
        &config,
    );

    let follower_receipt = state
        .apply(
            &request(
                "new-reuse-follower",
                Command::Assign {
                    to: owner.clone(),
                    title: "Modern reuse follower".into(),
                    text: "A newly created assignment still has an explicit mode.".into(),
                    context: ContextMode::Reuse,
                },
            ),
            Some(&requester),
            4,
            &config,
        )
        .unwrap();
    let follower_id: TaskId = serde_json::from_value(follower_receipt["task_id"].clone()).unwrap();
    assert_eq!(follower_receipt["context"], "reuse");
    assert_eq!(follower_receipt["state"], "queued");
    assert_eq!(state.tasks[&legacy_id].context, None);
    assert_eq!(state.tasks[&legacy_id].state, TaskState::Assigned);
    assert_eq!(state.tasks[&follower_id].context, Some(ContextMode::Reuse));
    assert_eq!(state.tasks[&follower_id].state, TaskState::Queued);

    let roster = state
        .apply(
            &request(
                "migrated-legacy-roster",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&requester),
            5,
            &config,
        )
        .unwrap();
    let legacy_row = roster["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == legacy_id.as_str())
        .unwrap();
    assert!(
        legacy_row.get("context").is_none(),
        "legacy mode remains unset in the task roster"
    );
    assert!(
        legacy_row.get("context_blocked").is_none(),
        "legacy unset is not presented as a new capability blocker"
    );
    let owner_row = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == owner.as_str())
        .unwrap();
    assert_eq!(owner_row["task_queue"]["holder"]["id"], legacy_id.as_str());
    assert!(owner_row["task_queue"]["holder"].get("context").is_none());

    let legacy_message = state
        .messages
        .iter()
        .position(|message| message.task_id.as_ref() == Some(&legacy_id))
        .unwrap();
    assert_eq!(state.messages[legacy_message].context, None);
    state.messages[legacy_message].context = Some(ContextMode::Reuse);
    let mismatched_legacy = state
        .apply(
            &request(
                "reject-mismatched-legacy-message",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            6,
            &config,
        )
        .unwrap();
    assert!(mismatched_legacy["inbox"].is_null());
    assert_eq!(state.messages[legacy_message].delivery, Delivery::Pending);
    state.messages[legacy_message].context = None;
    let legacy_offer = state
        .apply(
            &request(
                "offer-migrated-legacy-message",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            7,
            &config,
        )
        .unwrap();
    assert_eq!(legacy_offer["inbox"]["kind"], "assignment");
    assert!(
        legacy_offer["inbox"].get("context").is_none(),
        "legacy envelope does not acquire Reuse/Fresh"
    );
    assert_eq!(
        legacy_offer["inbox"]["id"],
        state.messages[legacy_message].id.as_str()
    );
    state
        .apply(
            &request(
                "ack-migrated-legacy-assignment",
                Command::Ack {
                    message_id: state.messages[legacy_message].id.clone(),
                },
            ),
            Some(&owner),
            8,
            &config,
        )
        .unwrap();
    state
        .apply(
            &request(
                "report-migrated-legacy-done",
                Command::Report {
                    task_id: legacy_id.clone(),
                    revision: 1,
                    kind: ReportKind::Done,
                    text: "Existing legacy work is complete.".into(),
                },
            ),
            Some(&owner),
            9,
            &config,
        )
        .unwrap();
    state
        .apply(
            &request(
                "accept-migrated-legacy-done",
                Command::Accept {
                    task_id: legacy_id.clone(),
                    revision: 1,
                    text: "Completion reviewed.".into(),
                },
            ),
            Some(&requester),
            10,
            &config,
        )
        .unwrap();
    state.promote_queued_tasks(11, &config).unwrap();
    assert_eq!(state.tasks[&follower_id].state, TaskState::Assigned);

    let info = state
        .apply(
            &request(
                "receive-legacy-acceptance-info",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            12,
            &config,
        )
        .unwrap();
    assert_eq!(info["inbox"]["kind"], "info");
    state
        .apply(
            &request(
                "ack-legacy-acceptance-info",
                Command::Ack {
                    message_id: serde_json::from_value(info["inbox"]["id"].clone()).unwrap(),
                },
            ),
            Some(&owner),
            12,
            &config,
        )
        .unwrap();
    let follower_message = state
        .messages
        .iter()
        .position(|message| message.task_id.as_ref() == Some(&follower_id))
        .unwrap();
    state.messages[follower_message].context = None;
    let mismatched_follower = state
        .apply(
            &request(
                "reject-missing-reuse-message-mode",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            13,
            &config,
        )
        .unwrap();
    assert!(
        mismatched_follower["inbox"].is_null(),
        "a Some-mode task cannot use the legacy None exception"
    );
    assert_eq!(state.messages[follower_message].delivery, Delivery::Pending);
    state.messages[follower_message].context = Some(ContextMode::Reuse);
    let follower_offer = state
        .apply(
            &request(
                "offer-contextual-reuse-follower",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            14,
            &config,
        )
        .unwrap();
    assert_eq!(follower_offer["inbox"]["kind"], "assignment");
    assert_eq!(follower_offer["inbox"]["context"], "reuse");
    assert_eq!(state.tasks[&follower_id].context, Some(ContextMode::Reuse));
}

#[test]
fn legacy_queued_task_promotes_and_offers_without_context_capabilities() {
    let (mut state, requester, owner, config) = setup();
    let task_id = TaskId::new("legacy-no-capability");
    state.tasks.insert(
        task_id.clone(),
        Task {
            id: task_id.clone(),
            seq: 1,
            space_id: SpaceId::new("w1"),
            requester,
            owner: owner.clone(),
            title: "Legacy without mode capability".into(),
            description: "Continue with the existing unset policy.".into(),
            state: TaskState::Queued,
            revision: 1,
            updated_at: 1,
            checkin_at: 0,
            nudges: 0,
            note: String::new(),
            context: None,
        },
    );
    state.next_seq = 2;
    state.disconnect(&owner);
    join_as(&mut state, &owner, ClientHarness::Pi, vec![], 2, &config);
    assert!(state.context_capabilities[&owner].is_empty());
    state.promote_queued_tasks(3, &config).unwrap();
    assert_eq!(state.tasks[&task_id].state, TaskState::Assigned);
    assert_eq!(state.tasks[&task_id].context, None);
    let offer = state
        .apply(
            &request(
                "offer-legacy-without-context-capability",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            4,
            &config,
        )
        .unwrap();
    assert_eq!(offer["inbox"]["kind"], "assignment");
    assert!(offer["inbox"].get("context").is_none());
}

#[test]
fn paused_legacy_task_keeps_report_guide_and_uncertain_delivery_safeguards() {
    let (mut state, requester, owner, config) = setup();
    let task_id = TaskId::new("legacy-paused-task");
    state.tasks.insert(
        task_id.clone(),
        Task {
            id: task_id.clone(),
            seq: 1,
            space_id: SpaceId::new("w1"),
            requester: requester.clone(),
            owner: owner.clone(),
            title: "Paused legacy task".into(),
            description: "Reconcile before continuing.".into(),
            state: TaskState::Assigned,
            revision: 1,
            updated_at: 1,
            checkin_at: 0,
            nudges: 0,
            note: String::new(),
            context: None,
        },
    );
    state.next_seq = 2;
    state.messages.push(Message {
        id: MessageId::new("legacy-assignment-uncertain"),
        space_id: SpaceId::new("w1"),
        from: Some(requester.clone()),
        to: owner.clone(),
        task_id: Some(task_id.clone()),
        revision: Some(1),
        kind: MessageKind::Assignment,
        text: "Paused legacy task".into(),
        wake: true,
        delivery: Delivery::Pending,
        created_at: 1,
        pr_ack: None,
        context: None,
    });
    state.disconnect(&owner);
    assert_eq!(state.tasks[&task_id].state, TaskState::Paused);
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);
    join_as(&mut state, &owner, ClientHarness::Pi, vec![], 2, &config);

    let status = state
        .apply(
            &request(
                "legacy-paused-current-status",
                Command::Report {
                    task_id: task_id.clone(),
                    revision: 1,
                    kind: ReportKind::Status,
                    text: "Still reconciling the existing task.".into(),
                },
            ),
            Some(&owner),
            3,
            &config,
        )
        .unwrap();
    assert_eq!(status["reported"], "status");
    assert_eq!(state.tasks[&task_id].state, TaskState::Paused);
    assert_eq!(state.messages.last().unwrap().context, None);

    let guide_receipt = state
        .apply(
            &request(
                "guide-paused-legacy-task",
                Command::Guide {
                    task_id: task_id.clone(),
                    revision: 1,
                    text: "Authorized continuation after reconciliation.".into(),
                },
            ),
            Some(&requester),
            4,
            &config,
        )
        .unwrap();
    assert_eq!(guide_receipt["state"], "assigned");
    assert_eq!(guide_receipt["revision"], 2);
    assert!(
        guide_receipt.get("context").is_none(),
        "Guide receipt does not manufacture a context choice"
    );
    assert_eq!(state.tasks[&task_id].context, None);
    let guide_message = state
        .messages
        .iter()
        .position(|message| {
            message.kind == MessageKind::Guide && message.task_id.as_ref() == Some(&task_id)
        })
        .unwrap();
    assert_eq!(state.messages[guide_message].context, None);

    let stale_before = state.clone();
    let stale = state
        .apply(
            &request(
                "legacy-stale-report-after-guide",
                Command::Report {
                    task_id: task_id.clone(),
                    revision: 1,
                    kind: ReportKind::Status,
                    text: "Stale report must remain rejected.".into(),
                },
            ),
            Some(&owner),
            5,
            &config,
        )
        .unwrap_err();
    assert!(stale.to_string().contains("stale task revision"));
    assert_eq!(state, stale_before);

    state.messages[guide_message].context = Some(ContextMode::Reuse);
    let mismatched_guide = state
        .apply(
            &request(
                "reject-mismatched-none-guide",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            6,
            &config,
        )
        .unwrap();
    assert!(
        mismatched_guide["inbox"].is_null(),
        "a legacy None task cannot accept a fabricated Some-mode Guide"
    );
    assert_eq!(state.messages[guide_message].delivery, Delivery::Pending);
    state.messages[guide_message].context = None;

    let roster = state
        .apply(
            &request(
                "paused-legacy-unset-roster",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&requester),
            7,
            &config,
        )
        .unwrap();
    let row = roster["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == task_id.as_str())
        .unwrap();
    assert!(row.get("context").is_none());
    assert!(row.get("context_blocked").is_none());

    let guide_offer = state
        .apply(
            &request(
                "offer-authorized-legacy-guide",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&owner),
            8,
            &config,
        )
        .unwrap();
    assert_eq!(guide_offer["inbox"]["kind"], "guide");
    assert!(guide_offer["inbox"].get("context").is_none());
    assert_eq!(
        state.messages[0].delivery,
        Delivery::Uncertain,
        "the original ambiguous assignment is never resurrected"
    );
    assert_eq!(
        state
            .messages
            .iter()
            .filter(|message| message.kind == MessageKind::Assignment)
            .count(),
        1
    );
    state
        .apply(
            &request(
                "ack-authorized-legacy-guide",
                Command::Ack {
                    message_id: serde_json::from_value(guide_offer["inbox"]["id"].clone()).unwrap(),
                },
            ),
            Some(&owner),
            8,
            &config,
        )
        .unwrap();
    let current_status = state
        .apply(
            &request(
                "legacy-current-report-after-guide",
                Command::Report {
                    task_id: task_id.clone(),
                    revision: 2,
                    kind: ReportKind::Status,
                    text: "Current revision report remains authorized.".into(),
                },
            ),
            Some(&owner),
            9,
            &config,
        )
        .unwrap();
    assert_eq!(current_status["reported"], "status");
    assert_eq!(state.tasks[&task_id].state, TaskState::Active);
    assert_eq!(state.tasks[&task_id].revision, 2);
    assert_eq!(state.tasks[&task_id].context, None);
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);
}

#[test]
fn readiness_uses_the_same_context_eligibility_as_the_sync_offer() {
    let (mut state, architect, worker, config) = setup();
    state
        .apply(
            &request(
                "assign-unsupported-fresh-first",
                Command::Assign {
                    to: worker.clone(),
                    title: "Unsupported first message".into(),
                    text: "A later live Info remains deliverable.".into(),
                    context: ContextMode::Fresh,
                },
            ),
            Some(&architect),
            1,
            &config,
        )
        .unwrap();
    state
        .context_capabilities
        .get_mut(&worker)
        .unwrap()
        .remove(&ContextMode::Fresh);
    let info_id = state
        .queue(
            &SpaceId::new("w1"),
            Some(&architect),
            &worker,
            None,
            MessageKind::Info,
            "This later message is eligible under the normal working policy.".into(),
            true,
            2,
        )
        .unwrap();
    let snapshot = state.snapshot(None, None, 2);
    assert_eq!(snapshot.ready.get(&worker), Some(&info_id));
    let offered = state
        .apply(
            &request(
                "sync-after-ready-hint",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&worker),
            2,
            &config,
        )
        .unwrap();
    assert_eq!(offered["inbox"]["id"], info_id.as_str());
    assert_eq!(state.messages[0].delivery, Delivery::Pending);
    assert_eq!(state.messages[1].delivery, Delivery::Offered);
}
