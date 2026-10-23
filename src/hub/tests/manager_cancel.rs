use super::*;

fn join_peer(state: &mut State, id: &str, config: &Scheduling) -> AgentId {
    let agent = AgentId::new(id);
    state
        .apply_validated_join(
            &request(
                &format!("join-{id}"),
                Command::Join {
                    version: VERSION,
                    agent_id: agent.clone(),
                    name: id.into(),
                    role: "custom".into(),
                    pane_id: format!("w1:{id}"),
                    space_id: SpaceId::new("w1"),
                    cwd: None,
                    reset: None,
                },
            ),
            None,
            1,
            config,
            false,
        )
        .unwrap();
    state
        .apply(
            &request(
                &format!("idle-{id}"),
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&agent),
            1,
            config,
        )
        .unwrap();
    agent
}

fn assign_as(
    state: &mut State,
    requester: &AgentId,
    owner: &AgentId,
    request_id: &str,
    now: u64,
    config: &Scheduling,
) -> TaskId {
    let result = state
        .apply(
            &request(
                request_id,
                Command::Assign {
                    to: owner.clone(),
                    title: format!("Task {request_id}"),
                    text: "Implement and verify this bounded task".into(),
                },
            ),
            Some(requester),
            now,
            config,
        )
        .unwrap();
    serde_json::from_value(result["task_id"].clone()).unwrap()
}

fn sync_activity(
    state: &mut State,
    agent: &AgentId,
    activity: Activity,
    request_id: &str,
    now: u64,
    config: &Scheduling,
) {
    state
        .apply(
            &request(
                request_id,
                Command::Sync {
                    activity,
                    receive: false,
                },
            ),
            Some(agent),
            now,
            config,
        )
        .unwrap();
}

fn owner_guard(
    state: &mut State,
    manager: &AgentId,
    owner: &AgentId,
    scope: CancelScope,
    request_id: &str,
    now: u64,
    config: &Scheduling,
) -> String {
    let roster = state
        .apply(
            &request(
                request_id,
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(manager),
            now,
            config,
        )
        .unwrap();
    roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["id"] == owner.to_string())
        .unwrap()["task_queue"]["cancellation_guards"][scope.as_str()]
    .as_str()
    .unwrap()
    .to_owned()
}

// Keep each wire field explicit in this test-only fixture helper.
#[allow(clippy::too_many_arguments)]
fn cancel_owner(
    state: &mut State,
    manager: &AgentId,
    owner: &AgentId,
    scope: CancelScope,
    expected_guard: &str,
    request_id: &str,
    text: &str,
    now: u64,
    config: &Scheduling,
) -> Result<serde_json::Value> {
    state.apply(
        &request(
            request_id,
            Command::CancelOwnerTasks {
                owner_id: owner.clone(),
                scope,
                expected_guard: expected_guard.into(),
                text: text.into(),
            },
        ),
        Some(manager),
        now,
        config,
    )
}

fn assert_stale_without_write(
    state: &mut State,
    manager: &AgentId,
    owner: &AgentId,
    scope: CancelScope,
    guard: &str,
    request_id: &str,
    config: &Scheduling,
) {
    let tasks_before = state.tasks.clone();
    let messages_before = state.messages.clone();
    let cache_before = state.cache.clone();
    let result = cancel_owner(
        state,
        manager,
        owner,
        scope,
        guard,
        request_id,
        "The user redirected this work.",
        20,
        config,
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("selection changed")
    );
    assert_eq!(state.tasks, tasks_before);
    assert_eq!(state.messages, messages_before);
    assert_eq!(state.cache, cache_before);
}

#[test]
fn requester_replay_after_finish_and_changed_payload_keep_original_idempotency() {
    let (mut state, manager, owner, config) = setup();
    let task = assign_as(
        &mut state,
        &manager,
        &owner,
        "self-requested-task",
        2,
        &config,
    );
    let cancel = request(
        "requester-cancel",
        Command::Cancel {
            task_id: task.clone(),
            revision: 1,
            text: "The requester redirected the task.".into(),
        },
    );
    let first = state.apply(&cancel, Some(&manager), 3, &config).unwrap();
    assert_eq!(state.tasks[&task].state, TaskState::Cancelled);
    state.members.get_mut(&manager).unwrap().manager = false;
    assert_eq!(
        state.apply(&cancel, Some(&manager), 4, &config).unwrap(),
        first
    );

    let altered = request(
        "requester-cancel",
        Command::Cancel {
            task_id: task.clone(),
            revision: 1,
            text: "A different payload must not replace the first cancellation.".into(),
        },
    );
    assert!(state.apply(&altered, Some(&manager), 5, &config).is_err());
    assert_eq!(state.tasks[&task].state, TaskState::Cancelled);
    assert_eq!(
        state.tasks[&task].note,
        "Manager architect-id cancelled: The requester redirected the task."
    );
}

#[test]
fn requester_cancel_keeps_the_legacy_receipt_and_records_the_actor() {
    let (mut state, _manager, owner, config) = setup();
    let requester = join_peer(&mut state, "requester-legacy-receipt", &config);
    let task = assign_as(
        &mut state,
        &requester,
        &owner,
        "requester-own-task",
        2,
        &config,
    );
    let result = state
        .apply(
            &request(
                "requester-own-cancel",
                Command::Cancel {
                    task_id: task.clone(),
                    revision: 1,
                    text: "The requester no longer needs this work.".into(),
                },
            ),
            Some(&requester),
            3,
            &config,
        )
        .unwrap();
    assert_eq!(
        result,
        serde_json::json!({"state":"cancelled","revision":1})
    );
    assert_eq!(
        state.tasks[&task].note,
        format!("Requester {requester} cancelled: The requester no longer needs this work.")
    );
    let notice = state.messages.last().unwrap();
    assert_eq!(notice.from, Some(requester));
    assert_eq!(notice.to, owner);
    assert_eq!(notice.kind, MessageKind::Info);
    assert!(!notice.wake);
}

#[test]
fn current_manager_is_rechecked_before_single_cancel_replay_and_nonmanagers_are_denied() {
    let (mut state, manager, owner, config) = setup();
    let requester = join_peer(&mut state, "requester-a", &config);
    let task = assign_as(
        &mut state,
        &requester,
        &owner,
        "requested-by-peer",
        2,
        &config,
    );
    let cancel = request(
        "manager-cancel-cache",
        Command::Cancel {
            task_id: task.clone(),
            revision: 1,
            text: "The user directed redirection.".into(),
        },
    );
    let first = state.apply(&cancel, Some(&manager), 3, &config).unwrap();
    assert_eq!(first["actor_id"], manager.to_string());
    assert_eq!(first["cancelled_count"], 1);

    let unchanged = state.tasks[&task].clone();
    let rejected = state
        .apply(
            &request(
                "owner-cannot-cancel-requester-task",
                Command::Cancel {
                    task_id: task.clone(),
                    revision: 1,
                    text: "Owner is not the requester or manager.".into(),
                },
            ),
            Some(&owner),
            4,
            &config,
        )
        .unwrap_err();
    assert!(rejected.to_string().contains("current admitted manager"));
    assert_eq!(state.tasks[&task], unchanged);

    state.members.get_mut(&manager).unwrap().manager = false;
    let replay_error = state
        .apply(&cancel, Some(&manager), 5, &config)
        .unwrap_err();
    assert!(
        replay_error
            .to_string()
            .contains("current admitted manager")
    );
    assert_eq!(state.tasks[&task], unchanged);
}

#[test]
fn manager_single_cancel_requires_exact_same_space_task_and_allows_self_target() {
    let (mut state, manager, owner, config) = setup();
    let requester = join_peer(&mut state, "requester-b", &config);
    let task = assign_as(&mut state, &requester, &owner, "exact-task", 2, &config);
    let before = state.tasks[&task].clone();
    assert!(
        state
            .apply(
                &request(
                    "stale-single-cancel",
                    Command::Cancel {
                        task_id: task.clone(),
                        revision: before.revision + 1,
                        text: "Stale revision".into(),
                    },
                ),
                Some(&manager),
                3,
                &config,
            )
            .is_err()
    );
    assert_eq!(state.tasks[&task], before);

    state.tasks.get_mut(&task).unwrap().space_id = SpaceId::new("w2");
    assert!(
        state
            .apply(
                &request(
                    "foreign-single-cancel",
                    Command::Cancel {
                        task_id: task.clone(),
                        revision: before.revision,
                        text: "Foreign target".into(),
                    },
                ),
                Some(&manager),
                4,
                &config,
            )
            .is_err()
    );
    state.tasks.get_mut(&task).unwrap().space_id = SpaceId::new("w1");

    let manager_owned = assign_as(
        &mut state,
        &requester,
        &manager,
        "manager-is-owner",
        5,
        &config,
    );
    let result = state
        .apply(
            &request(
                "manager-self-target-cancel",
                Command::Cancel {
                    task_id: manager_owned.clone(),
                    revision: 1,
                    text: "User redirected my own assigned work.".into(),
                },
            ),
            Some(&manager),
            6,
            &config,
        )
        .unwrap();
    assert_eq!(result["state"], "cancelled");
    assert_eq!(state.tasks[&manager_owned].state, TaskState::Cancelled);
}

#[test]
fn explicit_scopes_preserve_holder_and_never_dispatch_queued_work() {
    let (mut state, manager, owner, config) = setup();
    let holder = assign_as(&mut state, &manager, &owner, "holder-task", 2, &config);
    sync_activity(
        &mut state,
        &owner,
        Activity::Working,
        "working-owner",
        3,
        &config,
    );
    let queued_a = assign_as(&mut state, &manager, &owner, "queued-a", 4, &config);
    let queued_b = assign_as(&mut state, &manager, &owner, "queued-b", 5, &config);

    let queued_guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::QueuedOnly,
        "roster-queued",
        6,
        &config,
    );
    let queued_result = cancel_owner(
        &mut state,
        &manager,
        &owner,
        CancelScope::QueuedOnly,
        &queued_guard,
        "cancel-queued-only",
        "The user redirected queued work.",
        7,
        &config,
    )
    .unwrap();
    assert_eq!(queued_result["cancelled_count"], 2);
    assert_eq!(state.tasks[&holder].state, TaskState::Assigned);
    for task in [&queued_a, &queued_b] {
        assert_eq!(state.tasks[task].state, TaskState::Cancelled);
        assert!(
            state.tasks[task]
                .note
                .starts_with("Manager architect-id cancelled:")
        );
        assert!(!state.messages.iter().any(|message| {
            message.task_id.as_ref() == Some(task) && message.kind == MessageKind::Assignment
        }));
    }
    assert_eq!(queued_result["notices_queued"], 0);
    assert_eq!(
        state.messages.len(),
        1,
        "only the original holder was dispatched"
    );

    let all_guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        "roster-all",
        8,
        &config,
    );
    let all_result = cancel_owner(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &all_guard,
        "cancel-all-unfinished",
        "The user redirected the remaining holder.",
        9,
        &config,
    )
    .unwrap();
    assert_eq!(
        all_result["cancelled_task_ids"].as_array().unwrap().len(),
        1
    );
    assert_eq!(all_result["cancelled_task_ids"][0], holder.to_string());
    assert_eq!(state.tasks[&holder].state, TaskState::Cancelled);
    assert_eq!(all_result["notices_queued"], 1);
    let owner_notice = state.messages.last().unwrap();
    assert_eq!(owner_notice.to, owner);
    assert_eq!(owner_notice.from, Some(manager));
    assert_eq!(owner_notice.task_id, Some(holder));
    assert_eq!(owner_notice.kind, MessageKind::Info);
    assert!(!owner_notice.wake);
}

#[test]
fn bulk_cancel_aggregates_per_requester_preserves_full_unicode_and_allows_paused_owner() {
    let (mut state, manager, owner, config) = setup();
    let requester_a = join_peer(&mut state, "requester-a", &config);
    let requester_b = join_peer(&mut state, "requester-b", &config);
    let holder = assign_as(
        &mut state,
        &requester_a,
        &owner,
        "requester-a-holder",
        2,
        &config,
    );
    sync_activity(
        &mut state,
        &owner,
        Activity::Working,
        "owner-working",
        3,
        &config,
    );
    let queued_a = assign_as(
        &mut state,
        &requester_a,
        &owner,
        "requester-a-queued",
        4,
        &config,
    );
    let queued_b = assign_as(
        &mut state,
        &requester_b,
        &owner,
        "requester-b-queued",
        5,
        &config,
    );
    sync_activity(
        &mut state,
        &owner,
        Activity::Paused,
        "owner-paused",
        6,
        &config,
    );

    let guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        "roster-paused-owner",
        7,
        &config,
    );
    let reason = "界".repeat(1_600);
    let result = cancel_owner(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "manager-bulk-notices",
        &reason,
        8,
        &config,
    )
    .unwrap();
    assert_eq!(result["cancelled_count"], 3);
    assert_eq!(result["actor_id"], manager.to_string());
    assert_eq!(result["notices_queued"], 3);
    assert_eq!(result["notice_status"], "queued_not_delivered");
    for task_id in [&holder, &queued_a, &queued_b] {
        assert_eq!(state.tasks[task_id].state, TaskState::Cancelled);
        assert_eq!(
            state.tasks[task_id].note,
            format!("Manager {manager} cancelled: {reason}")
        );
    }
    assert_eq!(state.members[&owner].activity, Activity::Paused);

    let owner_notices: Vec<_> = state
        .messages
        .iter()
        .filter(|message| {
            message.from.as_ref() == Some(&manager) && message.kind == MessageKind::Info
        })
        .collect();
    assert_eq!(owner_notices.len(), 3);
    let holder_notice = owner_notices
        .iter()
        .find(|message| message.to == owner)
        .unwrap();
    assert_eq!(holder_notice.task_id, Some(holder));
    assert!(!holder_notice.wake);
    assert!(holder_notice.text.contains(&reason));
    let requester_a_notice = owner_notices
        .iter()
        .find(|message| message.to == requester_a)
        .unwrap();
    assert!(requester_a_notice.text.contains("2 task(s)"));
    assert!(requester_a_notice.text.contains(&reason));
    let requester_b_notice = owner_notices
        .iter()
        .find(|message| message.to == requester_b)
        .unwrap();
    assert!(requester_b_notice.text.contains("1 task(s)"));
    assert!(requester_b_notice.text.contains(&reason));
    assert!(owner_notices.iter().all(|message| {
        message.delivery == Delivery::Pending
            && !message.wake
            && message.from == Some(manager.clone())
    }));
}

#[test]
fn valid_empty_scope_is_a_noop_and_foreign_or_nonmanager_bulk_targets_are_denied() {
    let (mut state, manager, owner, config) = setup();
    let guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        "empty-roster",
        1,
        &config,
    );
    let result = cancel_owner(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "empty-clear",
        "No work to redirect.",
        2,
        &config,
    )
    .unwrap();
    assert_eq!(result["cancelled_count"], 0);
    assert_eq!(result["cancelled_task_ids"], serde_json::json!([]));
    assert_eq!(result["notices_queued"], 0);
    assert_eq!(result["notice_status"], "none");

    let queued_guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::QueuedOnly,
        "nonmanager-guard-roster",
        3,
        &config,
    );
    let nonmanager_error = state
        .apply(
            &request(
                "nonmanager-bulk",
                Command::CancelOwnerTasks {
                    owner_id: owner.clone(),
                    scope: CancelScope::QueuedOnly,
                    expected_guard: queued_guard,
                    text: "Cannot cancel another owner's tasks.".into(),
                },
            ),
            Some(&owner),
            4,
            &config,
        )
        .unwrap_err();
    assert!(
        nonmanager_error
            .to_string()
            .contains("current admitted manager")
    );

    state.members.get_mut(&owner).unwrap().space_id = SpaceId::new("w2");
    assert!(
        cancel_owner(
            &mut state,
            &manager,
            &owner,
            CancelScope::AllUnfinished,
            &guard,
            "foreign-owner-bulk",
            "Foreign owner",
            5,
            &config,
        )
        .unwrap_err()
        .to_string()
        .contains("another space")
    );
}

#[test]
fn bulk_cancellation_remains_blocked_by_the_active_reset_fence() {
    let (mut state, manager, owner, config) = setup();
    state.reset_capable.insert(manager.clone());
    state.reset_capable.insert(owner.clone());
    let guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        "pre-reset-guard",
        1,
        &config,
    );
    let preview = state.reset_preview(&SpaceId::new("w1")).unwrap();
    state.start_reset(&preview).unwrap();
    let before = state.clone();
    let error = cancel_owner(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "cancel-during-reset",
        "Reset takes precedence over cancellation.",
        2,
        &config,
    )
    .unwrap_err();
    assert!(error.to_string().contains("frozen for fresh start"));
    assert_eq!(state, before);
}

#[test]
fn queued_offline_owner_can_be_cleared_and_notifications_remain_nonwaking() {
    let (mut state, manager, owner, config) = setup();
    let requester = join_peer(&mut state, "requester-offline", &config);
    let task = assign_as(&mut state, &requester, &owner, "offline-holder", 2, &config);
    state
        .apply(
            &request("owner-leaves", Command::Leave),
            Some(&owner),
            3,
            &config,
        )
        .unwrap();
    assert_eq!(state.members[&owner].activity, Activity::Offline);
    assert_eq!(state.tasks[&task].state, TaskState::Paused);

    let guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        "offline-owner-roster",
        4,
        &config,
    );
    let result = cancel_owner(
        &mut state,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "offline-owner-clear",
        "The user redirected work from the offline owner.",
        5,
        &config,
    )
    .unwrap();
    assert_eq!(result["cancelled_count"], 1);
    assert_eq!(result["notices_queued"], 2);
    let owner_notice = state
        .messages
        .iter()
        .find(|message| message.to == owner && message.from.as_ref() == Some(&manager))
        .unwrap();
    assert_eq!(owner_notice.kind, MessageKind::Info);
    assert!(!owner_notice.wake);
    assert_eq!(owner_notice.delivery, Delivery::Pending);
    assert!(state.messages.iter().any(|message| {
        message.to == requester && message.from.as_ref() == Some(&manager) && !message.wake
    }));
}

#[test]
fn cancellation_guards_cover_hidden_tasks_additions_revisions_states_and_promotion() {
    let (mut base, manager, owner, config) = setup();
    sync_activity(
        &mut base,
        &owner,
        Activity::Working,
        "owner-working-for-guard",
        2,
        &config,
    );
    for index in 0..70 {
        assign_as(
            &mut base,
            &manager,
            &owner,
            &format!("guard-task-{index}"),
            3 + index,
            &config,
        );
    }
    let guard = owner_guard(
        &mut base,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        "large-guard-roster",
        100,
        &config,
    );
    let repeated = owner_guard(
        &mut base,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        "large-guard-roster-again",
        101,
        &config,
    );
    assert_eq!(guard, repeated, "guard serialization is deterministic");
    assert_eq!(guard.len(), 24);

    let roster = base
        .apply(
            &request(
                "large-roster-visible-tasks",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&manager),
            102,
            &config,
        )
        .unwrap();
    assert_eq!(roster["tasks_truncated"], true);
    let visible: BTreeSet<_> = roster["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_str().unwrap().to_owned())
        .collect();
    let hidden = base
        .tasks
        .keys()
        .find(|task_id| !visible.contains(&task_id.to_string()))
        .unwrap()
        .clone();
    let queue = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["id"] == owner.to_string())
        .unwrap();
    assert_eq!(queue["task_queue"]["queued_count"], 70);
    assert!(queue["task_queue"]["cancellation_guards"]["all_unfinished"].is_string());

    let mut revision_changed = base.clone();
    revision_changed.tasks.get_mut(&hidden).unwrap().revision += 1;
    assert_stale_without_write(
        &mut revision_changed,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "hidden-revision-stale",
        &config,
    );

    let mut task_added = base.clone();
    assign_as(
        &mut task_added,
        &manager,
        &owner,
        "guard-new-task",
        103,
        &config,
    );
    assert_stale_without_write(
        &mut task_added,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "new-task-stale",
        &config,
    );

    let mut state_changed = base.clone();
    state_changed.tasks.get_mut(&hidden).unwrap().state = TaskState::Waiting;
    assert_stale_without_write(
        &mut state_changed,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "hidden-state-stale",
        &config,
    );

    let mut promoted = base;
    sync_activity(
        &mut promoted,
        &owner,
        Activity::Idle,
        "owner-idle-for-promotion",
        104,
        &config,
    );
    promoted.tick(105, &config).unwrap();
    assert!(
        promoted
            .tasks
            .values()
            .any(|task| task.owner == owner && task.state == TaskState::Assigned)
    );
    assert_stale_without_write(
        &mut promoted,
        &manager,
        &owner,
        CancelScope::AllUnfinished,
        &guard,
        "promotion-stale",
        &config,
    );
}

#[test]
fn composed_manager_notes_and_notices_are_bounded_before_any_mutation() {
    let (mut state, manager, owner, config) = setup();
    let requester = join_peer(&mut state, "requester-long-note", &config);
    sync_activity(
        &mut state,
        &owner,
        Activity::Working,
        "owner-working-long-note",
        2,
        &config,
    );
    let queued = assign_as(
        &mut state,
        &requester,
        &owner,
        "queued-long-note",
        3,
        &config,
    );
    let guard = owner_guard(
        &mut state,
        &manager,
        &owner,
        CancelScope::QueuedOnly,
        "long-note-roster",
        4,
        &config,
    );
    let tasks_before = state.tasks.clone();
    let messages_before = state.messages.clone();
    let cache_before = state.cache.clone();
    // The reason fits the base text limit, but the full aggregate requester
    // notice does not; it must not be silently clipped or partially committed.
    let reason = "x".repeat(5_940);
    let error = cancel_owner(
        &mut state,
        &manager,
        &owner,
        CancelScope::QueuedOnly,
        &guard,
        "overlong-composed-notice",
        &reason,
        5,
        &config,
    )
    .unwrap_err();
    assert!(error.to_string().contains("6000 bytes"));
    assert_eq!(state.tasks, tasks_before);
    assert_eq!(state.messages, messages_before);
    assert_eq!(state.cache, cache_before);
    assert_eq!(state.tasks[&queued].state, TaskState::Queued);

    let too_long = format!("{}x", "x".repeat(6_000));
    assert!(
        cancel_owner(
            &mut state,
            &manager,
            &owner,
            CancelScope::QueuedOnly,
            &guard,
            "overlong-note",
            &too_long,
            6,
            &config,
        )
        .is_err()
    );
    assert_eq!(state.tasks[&queued].state, TaskState::Queued);
}
