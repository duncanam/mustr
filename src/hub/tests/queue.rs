use super::*;

fn add_agent(
    state: &mut State,
    id: &AgentId,
    name: &str,
    pane: &str,
    now: u64,
    config: &Scheduling,
) {
    state
        .apply_validated_join_with_harness(
            &request(
                &format!("join-{name}"),
                Command::Join {
                    version: VERSION,
                    agent_id: id.clone(),
                    name: name.into(),
                    role: "custom".into(),
                    pane_id: pane.into(),
                    space_id: SpaceId::new("w1"),
                    cwd: None,
                    context_modes: vec![ContextMode::Reuse, ContextMode::Fresh],
                    reset: None,
                },
            ),
            None,
            now,
            config,
            false,
            ClientHarness::Pi,
        )
        .unwrap();
    state
        .apply(
            &request(
                &format!("sync-{name}"),
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

fn set_activity(
    state: &mut State,
    id: &AgentId,
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
            Some(id),
            now,
            config,
        )
        .unwrap();
}

fn submit_assignment(
    state: &mut State,
    requester: &AgentId,
    owner: &AgentId,
    request_id: &str,
    title: &str,
    now: u64,
    config: &Scheduling,
) -> (Request, TaskId, Value) {
    let request = request(
        request_id,
        Command::Assign {
            to: owner.clone(),
            title: title.into(),
            text: format!("Implement {title}"),
            context: ContextMode::Reuse,
        },
    );
    let manager = state
        .members
        .values()
        .find(|member| member.manager)
        .map(|member| member.id.clone())
        .expect("queue test fixture has an admitted manager");
    let value = state.apply(&request, Some(&manager), now, config).unwrap();
    let id: TaskId = serde_json::from_value(value["task_id"].clone()).unwrap();
    if requester != &manager {
        // Retain old multi-requester task cases as pre-existing history while
        // keeping new assignment creation manager-only.
        state.tasks.get_mut(&id).unwrap().requester = requester.clone();
        if let Some(message) = state
            .messages
            .iter_mut()
            .rev()
            .find(|message| message.task_id.as_ref() == Some(&id))
        {
            message.from = Some(requester.clone());
        }
    }
    (request, id, value)
}

#[test]
fn report_kind_matrix_routes_to_requester_with_expected_wake_and_task_state() {
    let cases = [
        (
            ReportKind::Status,
            MessageKind::Status,
            true,
            TaskState::Assigned,
            2,
        ),
        (
            ReportKind::Progress,
            MessageKind::Status,
            true,
            TaskState::Active,
            0,
        ),
        (
            ReportKind::Blocked,
            MessageKind::Blocked,
            true,
            TaskState::Blocked,
            2,
        ),
        (
            ReportKind::Waiting,
            MessageKind::Blocked,
            false,
            TaskState::Waiting,
            2,
        ),
        (
            ReportKind::Done,
            MessageKind::Done,
            true,
            TaskState::Review,
            2,
        ),
    ];
    for (index, (kind, expected_kind, expected_wake, expected_state, expected_nudges)) in
        cases.into_iter().enumerate()
    {
        let (mut state, manager, worker, config) = setup();
        let task_id = assign(&mut state, &manager, &worker, &config);
        if kind == ReportKind::Progress {
            state.tasks.get_mut(&task_id).unwrap().state = TaskState::Active;
        }
        state.tasks.get_mut(&task_id).unwrap().nudges = 2;
        state
            .apply(
                &request(
                    &format!("report-matrix-{index}"),
                    Command::Report {
                        task_id: task_id.clone(),
                        revision: 1,
                        kind,
                        text: "evidence for this report".into(),
                    },
                ),
                Some(&worker),
                2,
                &config,
            )
            .unwrap();
        let message = state.messages.last().unwrap();
        assert_eq!(message.to, manager, "report kind {kind:?}");
        assert_eq!(message.kind, expected_kind, "report kind {kind:?}");
        assert_eq!(message.wake, expected_wake, "report kind {kind:?}");
        assert_eq!(
            state.tasks[&task_id].state, expected_state,
            "report kind {kind:?}"
        );
        assert_eq!(
            state.tasks[&task_id].nudges, expected_nudges,
            "report kind {kind:?}"
        );
    }
}

fn roster(
    state: &mut State,
    caller: &AgentId,
    request_id: &str,
    now: u64,
    config: &Scheduling,
) -> Value {
    state
        .apply(
            &request(
                request_id,
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(caller),
            now,
            config,
        )
        .unwrap()
}

#[test]
fn assignments_queue_without_envelopes_and_promote_fifo_across_requesters() {
    let (mut state, architect, worker, config) = setup();
    let requester_two = AgentId::new("requester-two");
    let worker_two = AgentId::new("worker-two");
    add_agent(
        &mut state,
        &requester_two,
        "requester-two",
        "w1:p3",
        0,
        &config,
    );
    add_agent(&mut state, &worker_two, "worker-two", "w1:p4", 0, &config);
    set_activity(
        &mut state,
        &worker,
        Activity::Working,
        "worker-busy",
        1,
        &config,
    );
    set_activity(
        &mut state,
        &worker_two,
        Activity::Working,
        "worker-two-busy",
        1,
        &config,
    );

    let (first_request, first, first_result) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "first-worker",
        "first",
        1,
        &config,
    );
    let (_, other_owner, other_owner_result) = submit_assignment(
        &mut state,
        &requester_two,
        &worker_two,
        "other-owner",
        "other owner",
        2,
        &config,
    );
    let (_, second, second_result) = submit_assignment(
        &mut state,
        &requester_two,
        &worker,
        "second-worker",
        "second",
        3,
        &config,
    );
    let (_, third, third_result) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "third-worker",
        "third",
        4,
        &config,
    );

    assert_eq!(first_result["state"], "queued");
    assert_eq!(first_result["queue_position"], 1);
    assert_eq!(second_result["queue_position"], 2);
    assert_eq!(third_result["queue_position"], 3);
    assert_eq!(other_owner_result["queue_position"], 1);
    assert_eq!(state.tasks[&first].state, TaskState::Queued);
    assert_eq!(state.tasks[&other_owner].state, TaskState::Queued);
    assert_eq!(state.tasks[&second].state, TaskState::Queued);
    assert_eq!(state.tasks[&third].state, TaskState::Queued);
    assert!(
        state.messages.is_empty(),
        "queued work has no runnable envelope"
    );
    assert!(state.tasks[&first].seq < state.tasks[&second].seq);
    assert!(state.tasks[&second].seq < state.tasks[&third].seq);

    let before = roster(&mut state, &architect, "roster-before-dispatch", 4, &config);
    let owner_summary = before["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["id"] == worker.as_str())
        .unwrap();
    assert!(owner_summary["task_queue"]["holder"].is_null());
    assert_eq!(owner_summary["task_queue"]["queued_count"], 3);
    assert_eq!(
        owner_summary["task_queue"]["next_queued"]["id"],
        first.as_str()
    );
    assert_eq!(owner_summary["task_queue"]["next_queued"]["title"], "first");
    assert_eq!(
        owner_summary["task_queue"]["next_queued"]["requester"],
        architect.as_str()
    );
    for (id, expected_position) in [(&first, 1), (&second, 2), (&third, 3)] {
        let row = before["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"] == id.as_str())
            .unwrap();
        assert_eq!(row["state"], "queued");
        assert_eq!(row["queue_position"], expected_position);
    }

    set_activity(
        &mut state,
        &worker,
        Activity::Idle,
        "worker-free",
        5,
        &config,
    );
    state.tick(5, &config).unwrap();
    assert_eq!(state.tasks[&first].state, TaskState::Assigned);
    assert_eq!(state.tasks[&first].checkin_at, 105);
    assert_eq!(state.tasks[&first].nudges, 0);
    assert_eq!(state.tasks[&second].state, TaskState::Queued);
    assert_eq!(state.tasks[&third].state, TaskState::Queued);
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].kind, MessageKind::Assignment);
    assert_eq!(state.messages[0].task_id, Some(first.clone()));
    assert_eq!(state.messages[0].from, Some(architect.clone()));

    // Replaying the assignment request returns its original queued receipt and
    // must not create a duplicate task or envelope after the later promotion.
    let replay = state
        .apply(&first_request, Some(&architect), 6, &config)
        .unwrap();
    assert_eq!(replay, first_result);
    assert_eq!(state.tasks.len(), 4);
    assert_eq!(state.messages.len(), 1);
}

#[test]
fn queued_work_rejects_owner_actions_and_requester_cancel_is_bookkeeping_only() {
    let (mut state, architect, worker, config) = setup();
    set_activity(&mut state, &worker, Activity::Working, "busy", 1, &config);
    let (_, first, _) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "queued-first",
        "first",
        1,
        &config,
    );
    let (_, second, _) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "queued-second",
        "second",
        1,
        &config,
    );

    for (index, kind) in [
        ReportKind::Status,
        ReportKind::Progress,
        ReportKind::Blocked,
        ReportKind::Waiting,
        ReportKind::Done,
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            state
                .apply(
                    &request(
                        &format!("queued-report-{index}"),
                        Command::Report {
                            task_id: first.clone(),
                            revision: 1,
                            kind,
                            text: "not dispatched".into(),
                        },
                    ),
                    Some(&worker),
                    2,
                    &config,
                )
                .is_err()
        );
    }
    for (request_id, command) in [
        (
            "queued-guide",
            Command::Guide {
                task_id: first.clone(),
                revision: 1,
                text: "not dispatched".into(),
            },
        ),
        (
            "queued-accept",
            Command::Accept {
                task_id: first.clone(),
                revision: 1,
                text: "not dispatched".into(),
            },
        ),
    ] {
        assert!(
            state
                .apply(&request(request_id, command), Some(&architect), 2, &config)
                .is_err()
        );
    }
    assert_eq!(state.tasks[&first].state, TaskState::Queued);
    assert!(state.messages.is_empty());

    state
        .apply(
            &request(
                "cancel-queued",
                Command::Cancel {
                    task_id: first.clone(),
                    revision: 1,
                    text: "No longer needed".into(),
                },
            ),
            Some(&architect),
            2,
            &config,
        )
        .unwrap();
    assert_eq!(state.tasks[&first].state, TaskState::Cancelled);
    assert!(
        state.messages.is_empty(),
        "queued cancellation must not message the owner"
    );
    assert_eq!(state.queue_position(&second), Some(1));

    set_activity(&mut state, &worker, Activity::Idle, "free", 3, &config);
    state.tick(3, &config).unwrap();
    assert_eq!(state.tasks[&second].state, TaskState::Assigned);
    assert_eq!(state.messages.len(), 1);
    let offered = state
        .apply(
            &request(
                "receive-second",
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
    let assignment: Message = serde_json::from_value(offered["inbox"].clone()).unwrap();
    state
        .apply(
            &request(
                "ack-second",
                Command::Ack {
                    message_id: assignment.id,
                },
            ),
            Some(&worker),
            4,
            &config,
        )
        .unwrap();
    assert_eq!(state.tasks[&second].state, TaskState::Active);

    let (_, third, _) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "queued-third",
        "third",
        5,
        &config,
    );
    state
        .apply(
            &request(
                "done-second",
                Command::Report {
                    task_id: second.clone(),
                    revision: 1,
                    kind: ReportKind::Done,
                    text: "Finished".into(),
                },
            ),
            Some(&worker),
            5,
            &config,
        )
        .unwrap();
    state.tick(5, &config).unwrap();
    assert_eq!(state.tasks[&second].state, TaskState::Review);
    assert_eq!(state.tasks[&third].state, TaskState::Queued);
    state
        .apply(
            &request(
                "accept-second",
                Command::Accept {
                    task_id: second.clone(),
                    revision: 1,
                    text: "Reviewed".into(),
                },
            ),
            Some(&architect),
            6,
            &config,
        )
        .unwrap();
    assert_eq!(state.tasks[&second].state, TaskState::Accepted);
    assert_eq!(state.tasks[&third].state, TaskState::Queued);
    state.tick(6, &config).unwrap();
    assert_eq!(state.tasks[&third].state, TaskState::Assigned);
}

#[test]
fn every_nonqueued_unfinished_state_holds_the_owner_slot() {
    assert!(!TaskState::Queued.holds_slot());
    assert!(!TaskState::Accepted.holds_slot());
    assert!(!TaskState::Cancelled.holds_slot());
    for holder_state in [
        TaskState::Assigned,
        TaskState::Active,
        TaskState::Blocked,
        TaskState::Waiting,
        TaskState::Review,
        TaskState::Paused,
    ] {
        assert!(holder_state.holds_slot(), "{holder_state:?}");
        let (mut state, architect, worker, config) = setup();
        let holder = assign(&mut state, &architect, &worker, &config);
        let (_, queued, _) = submit_assignment(
            &mut state, &architect, &worker, "backlog", "backlog", 1, &config,
        );
        state.tasks.get_mut(&holder).unwrap().state = holder_state;
        state.tick(1, &config).unwrap();
        assert_eq!(state.tasks[&holder].state, holder_state);
        assert_eq!(state.tasks[&queued].state, TaskState::Queued);
    }
}

#[test]
fn todo_driver_reset_freeze_and_retirement_gate_promotion() {
    let (mut todo_state, architect, worker, config) = setup();
    set_activity(
        &mut todo_state,
        &worker,
        Activity::Working,
        "todo-busy",
        1,
        &config,
    );
    let (_, todo_task, _) = submit_assignment(
        &mut todo_state,
        &architect,
        &worker,
        "todo-queued",
        "todo queued",
        1,
        &config,
    );
    todo_state.automation.todo_enabled = true;
    todo_state.automation.todo = Some(TodoWatch {
        path: "/fixture/TODO.md".into(),
        driver: worker.clone(),
        total: 1,
        remaining: 1,
        next: "next item".into(),
        signature: "fixture".into(),
        nudges: 0,
        next_at: 0,
        error: None,
    });
    set_activity(
        &mut todo_state,
        &worker,
        Activity::Idle,
        "todo-free",
        2,
        &config,
    );
    todo_state.tick(2, &config).unwrap();
    assert_eq!(todo_state.tasks[&todo_task].state, TaskState::Queued);
    assert!(todo_state.messages.is_empty());
    todo_state.automation.todo_enabled = false;
    todo_state.tick(3, &config).unwrap();
    assert_eq!(todo_state.tasks[&todo_task].state, TaskState::Assigned);

    let (mut retired_state, architect, worker, config) = setup();
    set_activity(
        &mut retired_state,
        &worker,
        Activity::Working,
        "retired-busy",
        1,
        &config,
    );
    let (_, retired_task, _) = submit_assignment(
        &mut retired_state,
        &architect,
        &worker,
        "retired-queued",
        "retired queued",
        1,
        &config,
    );
    set_activity(
        &mut retired_state,
        &worker,
        Activity::Idle,
        "retired-free",
        2,
        &config,
    );
    retired_state.retired.insert(worker.clone());
    retired_state.tick(2, &config).unwrap();
    assert_eq!(retired_state.tasks[&retired_task].state, TaskState::Queued);

    let (mut reset_state, architect, worker, config) = setup();
    for agent in [&architect, &worker] {
        reset_state
            .apply(
                &request("reset-capability", Command::ResetCapability),
                Some(agent),
                0,
                &config,
            )
            .unwrap();
    }
    set_activity(
        &mut reset_state,
        &worker,
        Activity::Working,
        "reset-busy",
        1,
        &config,
    );
    let (_, reset_task, _) = submit_assignment(
        &mut reset_state,
        &architect,
        &worker,
        "reset-queued",
        "reset queued",
        1,
        &config,
    );
    set_activity(
        &mut reset_state,
        &worker,
        Activity::Idle,
        "reset-free",
        2,
        &config,
    );
    let preview = reset_state.reset_preview(&SpaceId::new("w1")).unwrap();
    reset_state.start_reset(&preview).unwrap();
    reset_state.tick(3, &config).unwrap();
    assert!(reset_state.reset_frozen(&SpaceId::new("w1")));
    assert_eq!(reset_state.tasks[&reset_task].state, TaskState::Queued);
    assert!(reset_state.messages.is_empty());
}

#[test]
fn promotion_capacity_failure_leaves_queue_intact_and_does_not_abort_tick() {
    let (mut state, architect, worker, config) = setup();
    for index in 0..MAX_MESSAGES {
        state
            .queue(
                &SpaceId::new("w1"),
                None,
                &architect,
                None,
                MessageKind::Info,
                format!("capacity-{index}"),
                false,
                0,
            )
            .unwrap();
    }
    set_activity(
        &mut state,
        &worker,
        Activity::Working,
        "capacity-busy",
        1,
        &config,
    );
    let (_, task, _) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "capacity-queued",
        "capacity queued",
        1,
        &config,
    );
    set_activity(
        &mut state,
        &worker,
        Activity::Idle,
        "capacity-free",
        19,
        &config,
    );
    assert!(state.tick(20, &config).is_ok());
    assert_eq!(state.members[&architect].activity, Activity::Offline);
    assert_eq!(state.tasks[&task].state, TaskState::Queued);
    assert_eq!(state.messages.len(), MAX_MESSAGES);

    state.messages[0].delivery = Delivery::Cancelled;
    state.tick(21, &config).unwrap();
    assert_eq!(state.tasks[&task].state, TaskState::Assigned);
    assert_eq!(state.messages.len(), MAX_MESSAGES);
    assert!(state.messages.iter().any(|message| {
        message.kind == MessageKind::Assignment && message.task_id.as_ref() == Some(&task)
    }));
}

#[test]
fn queue_summaries_and_holder_visibility_survive_task_roster_truncation() {
    let (mut state, architect, worker, config) = setup();
    let holder = assign(&mut state, &architect, &worker, &config);
    let mut queued = Vec::new();
    for index in 0..70 {
        let (_, id, result) = submit_assignment(
            &mut state,
            &architect,
            &worker,
            &format!("backlog-{index}"),
            &format!("queued-{index}"),
            1,
            &config,
        );
        assert_eq!(result["state"], "queued");
        queued.push(id);
    }
    let result = roster(&mut state, &worker, "worker-roster", 2, &config);
    assert_eq!(result["tasks"].as_array().unwrap().len(), 64);
    assert_eq!(result["tasks_truncated"], true);
    assert_eq!(result["tasks"][0]["id"], holder.as_str());
    assert_eq!(result["tasks"][0]["state"], "assigned");
    assert_eq!(result["tasks"][1]["queue_position"], 1);
    assert_eq!(result["tasks"][63]["queue_position"], 63);
    let worker_summary = result["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["id"] == worker.as_str())
        .unwrap();
    assert_eq!(
        worker_summary["task_queue"]["holder"]["id"],
        holder.as_str()
    );
    assert_eq!(worker_summary["task_queue"]["holder"]["state"], "assigned");
    assert_eq!(worker_summary["task_queue"]["queued_count"], 70);
    assert_eq!(
        worker_summary["task_queue"]["next_queued"]["id"],
        queued[0].as_str()
    );
}

#[test]
fn queued_tasks_survive_disconnect_and_recovery_then_promote_in_original_order() {
    let (mut state, architect, worker, config) = setup();
    set_activity(
        &mut state,
        &worker,
        Activity::Working,
        "restart-busy",
        1,
        &config,
    );
    let (_, first, _) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "restart-first",
        "first",
        1,
        &config,
    );
    let (_, second, _) = submit_assignment(
        &mut state,
        &architect,
        &worker,
        "restart-second",
        "second",
        1,
        &config,
    );
    let first_seq = state.tasks[&first].seq;
    let second_seq = state.tasks[&second].seq;
    state.disconnect(&worker);
    assert_eq!(state.tasks[&first].state, TaskState::Queued);
    assert_eq!(state.tasks[&second].state, TaskState::Queued);
    state.tick(2, &config).unwrap();
    assert!(
        state.messages.is_empty(),
        "offline owners are never promoted"
    );

    let stored = serde_json::to_vec(&state).unwrap();
    let mut recovered: State = serde_json::from_slice(&stored).unwrap();
    recovered.recover();
    assert_eq!(recovered.tasks[&first].state, TaskState::Queued);
    assert_eq!(recovered.tasks[&second].state, TaskState::Queued);
    assert_eq!(recovered.tasks[&first].seq, first_seq);
    assert_eq!(recovered.tasks[&second].seq, second_seq);
    add_agent(&mut recovered, &worker, "worker", "w1:p2", 3, &config);
    recovered.tick(4, &config).unwrap();
    assert_eq!(recovered.tasks[&first].state, TaskState::Assigned);
    assert_eq!(recovered.tasks[&second].state, TaskState::Queued);
    assert_eq!(recovered.messages.len(), 1);
    assert_eq!(recovered.messages[0].task_id, Some(first));
}

#[test]
fn sequence_exhaustion_fails_closed_without_creating_a_task() {
    let (mut state, architect, worker, config) = setup();
    state.next_seq = u64::MAX;
    let before_tasks = state.tasks.clone();
    assert!(
        state
            .apply(
                &request(
                    "exhausted",
                    Command::Assign {
                        to: worker,
                        title: "Never allocated".into(),
                        text: "Sequence space is exhausted".into(),
                        context: ContextMode::Reuse,
                    },
                ),
                Some(&architect),
                1,
                &config,
            )
            .is_err()
    );
    assert_eq!(state.tasks, before_tasks);
    assert_eq!(state.next_seq, u64::MAX);
    assert!(state.messages.is_empty());

    let (mut recovered, architect, worker, config) = setup();
    let task = assign(&mut recovered, &architect, &worker, &config);
    recovered.tasks.get_mut(&task).unwrap().seq = u64::MAX;
    recovered.next_seq = 1;
    recovered.recover();
    assert_eq!(recovered.next_seq, u64::MAX);
    recovered.members.get_mut(&architect).unwrap().activity = Activity::Idle;
    recovered.members.get_mut(&worker).unwrap().activity = Activity::Idle;
    assert!(
        recovered
            .apply(
                &request(
                    "exhausted-after-recovery",
                    Command::Assign {
                        to: worker,
                        title: "Never allocated".into(),
                        text: "Recovered maximum sequence".into(),
                        context: ContextMode::Reuse,
                    },
                ),
                Some(&architect),
                1,
                &config,
            )
            .is_err()
    );
}
