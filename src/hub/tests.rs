use super::*;

mod delivery;
mod reset;

fn request(id: &str, command: Command) -> Request {
    Request {
        id: id.into(),
        command,
    }
}
fn setup() -> (State, AgentId, AgentId, Scheduling) {
    let mut state = State::new(SpaceId::new("w1"));
    let config = Scheduling {
        checkin_seconds: 100,
        continuation_seconds: 5,
        max_nudges: 2,
    };
    let architect = AgentId::new("architect-id");
    let worker = AgentId::new("worker-id");
    for (id, name, pane, manager) in [
        (&architect, "architect", "w1:p1", true),
        (&worker, "worker", "w1:p2", false),
    ] {
        state
            .apply_validated_join(
                &request(
                    name,
                    Command::Join {
                        version: 1,
                        agent_id: id.clone(),
                        name: name.into(),
                        role: "custom".into(),
                        pane_id: pane.into(),
                        space_id: SpaceId::new("w1"),
                        cwd: None,
                        reset: None,
                        startup_grant: None,
                    },
                ),
                None,
                0,
                &config,
                manager,
            )
            .unwrap();
        state
            .apply(
                &request(
                    "sync",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                ),
                Some(id),
                0,
                &config,
            )
            .unwrap();
    }
    // Most state tests assume automation is explicitly off. Set the persisted
    // user choice directly so PR-generation fixtures remain at generation zero.
    state.automation.checkin_enabled = false;
    state.automation.manager_checkin_at = None;
    (state, architect, worker, config)
}
fn assign(state: &mut State, architect: &AgentId, worker: &AgentId, config: &Scheduling) -> TaskId {
    let result = state
        .apply(
            &request(
                "assign",
                Command::Assign {
                    to: worker.clone(),
                    title: "Task".into(),
                    text: "Implement and verify scope".into(),
                },
            ),
            Some(architect),
            0,
            config,
        )
        .unwrap();
    serde_json::from_value(result["task_id"].clone()).unwrap()
}
#[test]
fn working_recipients_receive_only_live_comments_without_head_of_line_blocking() {
    let (mut state, architect, worker, config) = setup();
    assign(&mut state, &architect, &worker, &config);
    let space = SpaceId::new("w1");
    for kind in [
        MessageKind::Guide,
        MessageKind::Checkin,
        MessageKind::Continue,
        MessageKind::Todo,
        MessageKind::Pr,
    ] {
        state
            .queue(
                &space,
                None,
                &worker,
                None,
                kind,
                "Idle-only work".into(),
                true,
                1,
            )
            .unwrap();
    }
    for kind in [
        MessageKind::Info,
        MessageKind::Request,
        MessageKind::Status,
        MessageKind::Blocked,
        MessageKind::Done,
    ] {
        state
            .queue(
                &space,
                Some(&architect),
                &worker,
                None,
                kind,
                "Live feedback".into(),
                false,
                1,
            )
            .unwrap();
        for (activity, receive) in [(Activity::Paused, true), (Activity::Working, false)] {
            let result = state
                .apply(
                    &request("gated", Command::Sync { activity, receive }),
                    Some(&worker),
                    2,
                    &config,
                )
                .unwrap();
            assert!(result["inbox"].is_null());
        }
        let result = state
            .apply(
                &request(
                    "live",
                    Command::Sync {
                        activity: Activity::Working,
                        receive: true,
                    },
                ),
                Some(&worker),
                2,
                &config,
            )
            .unwrap();
        let message: Message = serde_json::from_value(result["inbox"].clone()).unwrap();
        assert_eq!(message.kind, kind);
        state
            .apply(
                &request(
                    "ack",
                    Command::Ack {
                        message_id: message.id,
                    },
                ),
                Some(&worker),
                2,
                &config,
            )
            .unwrap();
    }
    let result = state
        .apply(
            &request(
                "busy",
                Command::Sync {
                    activity: Activity::Working,
                    receive: true,
                },
            ),
            Some(&worker),
            2,
            &config,
        )
        .unwrap();
    assert!(result["inbox"].is_null());
    assert_eq!(state.messages[0].delivery, Delivery::Pending);
    assert_eq!(
        receive(&mut state, &worker, &config, 3).unwrap().kind,
        MessageKind::Assignment
    );
}

fn receive(state: &mut State, agent: &AgentId, config: &Scheduling, now: u64) -> Option<Message> {
    let result = state
        .apply(
            &request(
                "sync",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(agent),
            now,
            config,
        )
        .unwrap();
    if result["inbox"].is_null() {
        return None;
    }
    let message: Message = serde_json::from_value(result["inbox"].clone()).unwrap();
    state
        .apply(
            &request(
                "ack",
                Command::Ack {
                    message_id: message.id.clone(),
                },
            ),
            Some(agent),
            now,
            config,
        )
        .unwrap();
    Some(message)
}

#[test]
fn ownership_revision_review_and_idempotency() {
    let (mut state, architect, worker, config) = setup();
    let id = assign(&mut state, &architect, &worker, &config);
    assert_eq!(id, assign(&mut state, &architect, &worker, &config));
    assert_eq!(state.tasks.len(), 1);
    assert_eq!(
        receive(&mut state, &worker, &config, 0).unwrap().kind,
        MessageKind::Assignment
    );
    assert!(
        state
            .apply(
                &request(
                    "other",
                    Command::Assign {
                        to: worker.clone(),
                        title: "Overlap".into(),
                        text: "No".into()
                    }
                ),
                Some(&architect),
                1,
                &config
            )
            .is_err()
    );
    assert!(
        state
            .apply(
                &request(
                    "early",
                    Command::Accept {
                        task_id: id.clone(),
                        revision: 1,
                        text: "ok".into()
                    }
                ),
                Some(&architect),
                1,
                &config
            )
            .is_err()
    );
    state
        .apply(
            &request(
                "done",
                Command::Report {
                    task_id: id.clone(),
                    revision: 1,
                    kind: ReportKind::Done,
                    text: "Tests passed".into(),
                },
            ),
            Some(&worker),
            1,
            &config,
        )
        .unwrap();
    state
        .apply(
            &request(
                "guide",
                Command::Guide {
                    task_id: id.clone(),
                    revision: 1,
                    text: "Fix review finding".into(),
                },
            ),
            Some(&architect),
            2,
            &config,
        )
        .unwrap();
    assert!(
        state
            .apply(
                &request(
                    "stale",
                    Command::Report {
                        task_id: id.clone(),
                        revision: 1,
                        kind: ReportKind::Done,
                        text: "old".into()
                    }
                ),
                Some(&worker),
                2,
                &config
            )
            .is_err()
    );
    receive(&mut state, &worker, &config, 2);
    state
        .apply(
            &request(
                "done2",
                Command::Report {
                    task_id: id.clone(),
                    revision: 2,
                    kind: ReportKind::Done,
                    text: "Verified again".into(),
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
                "accept",
                Command::Accept {
                    task_id: id.clone(),
                    revision: 2,
                    text: "Reviewed".into(),
                },
            ),
            Some(&architect),
            4,
            &config,
        )
        .unwrap();
    assert_eq!(state.tasks[&id].state, TaskState::Accepted);
}

#[test]
fn foreign_space_and_non_owner_requests_are_rejected() {
    let (mut state, architect, worker, config) = setup();
    let id = assign(&mut state, &architect, &worker, &config);
    assert!(
        state
            .apply(
                &request(
                    "bad",
                    Command::Report {
                        task_id: id,
                        revision: 1,
                        kind: ReportKind::Done,
                        text: "false".into()
                    }
                ),
                Some(&architect),
                0,
                &config
            )
            .is_err()
    );
    state.members.get_mut(&worker).unwrap().space_id = SpaceId::new("w2");
    assert!(
        state
            .apply(
                &request(
                    "foreign-heartbeat",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: true,
                    }
                ),
                Some(&worker),
                0,
                &config,
            )
            .is_err(),
        "a direct state call must reject a foreign member space"
    );
    assert!(
        state
            .set_mode(&SpaceId::new("w2"), Mode::Todo, true, 0)
            .is_err()
    );
    assert!(
        state
            .apply(
                &request(
                    "send",
                    Command::Send {
                        to: worker.clone(),
                        text: "hello".into(),
                        wake: true
                    }
                ),
                Some(&architect),
                0,
                &config
            )
            .is_err()
    );
    state.members.get_mut(&worker).unwrap().space_id = SpaceId::new("w1");
}

#[test]
fn scheduling_requires_opt_in_and_bounds_nudges() {
    let (mut state, architect, worker, config) = setup();
    let id = assign(&mut state, &architect, &worker, &config);
    receive(&mut state, &worker, &config, 0);
    state.tick(5, &config).unwrap();
    assert_eq!(state.messages.len(), 1);
    state
        .set_mode(&SpaceId::new("w1"), Mode::Todo, true, 5)
        .unwrap();
    state.tick(10, &config).unwrap();
    state.tick(11, &config).unwrap();
    assert_eq!(state.messages.len(), 2); // coalesced while pending
    assert_eq!(
        receive(&mut state, &worker, &config, 11).unwrap().kind,
        MessageKind::Continue
    );
    state.tick(15, &config).unwrap();
    receive(&mut state, &worker, &config, 15);
    state.tick(20, &config).unwrap();
    assert_eq!(state.tasks[&id].state, TaskState::Paused);
}

#[test]
fn periodic_manager_checkins_are_global_and_pause_cancels_queued_prompts() {
    let (mut state, manager, worker, mut config) = setup();
    config.checkin_seconds = 5;
    assign(&mut state, &manager, &worker, &config);
    receive(&mut state, &worker, &config, 0);
    state
        .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 0)
        .unwrap();
    state.tick(0, &config).unwrap();
    let checkin = state.messages.last().unwrap();
    assert_eq!(checkin.kind, MessageKind::Checkin);
    assert_eq!(checkin.to, manager);
    assert_eq!(checkin.from, None);
    assert_eq!(checkin.task_id, None);
    assert_eq!(checkin.revision, None);
    assert!(checkin.wake);
    state
        .apply(
            &request(
                "pause",
                Command::Sync {
                    activity: Activity::Paused,
                    receive: false,
                },
            ),
            Some(&manager),
            5,
            &config,
        )
        .unwrap();
    assert_eq!(state.messages.last().unwrap().delivery, Delivery::Cancelled);
    state.tick(6, &config).unwrap();
    assert_eq!(state.messages.len(), 2);
}

#[test]
fn manager_checkin_waits_for_availability_coalesces_and_never_falls_back() {
    let (mut state, manager, _, mut config) = setup();
    config.checkin_seconds = 5;
    let space = SpaceId::new("w1");
    state.set_mode(&space, Mode::Checkin, true, 0).unwrap();
    for (request_id, activity, at) in [
        ("working", Activity::Working, 0),
        ("paused", Activity::Paused, 1),
    ] {
        state
            .apply(
                &request(
                    request_id,
                    Command::Sync {
                        activity,
                        receive: false,
                    },
                ),
                Some(&manager),
                at,
                &config,
            )
            .unwrap();
        state.tick(at, &config).unwrap();
        assert!(state.messages.is_empty(), "no prompt while {activity:?}");
    }
    state
        .apply(
            &request(
                "idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&manager),
            2,
            &config,
        )
        .unwrap();
    state.tick(2, &config).unwrap();
    assert_eq!(state.messages.len(), 1);
    let old_id = state.messages[0].id.clone();
    state
        .apply(
            &request(
                "still-idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&manager),
            7,
            &config,
        )
        .unwrap();
    state.tick(7, &config).unwrap();
    assert_eq!(state.messages.len(), 1, "pending checkpoint coalesces");

    state.disconnect(&manager);
    assert!(!state.members[&manager].manager);
    state.tick(8, &config).unwrap();
    assert_eq!(
        state.messages.len(),
        1,
        "no other idle participant receives it"
    );
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);

    let replacement = AgentId::new("explicit-manager-replacement");
    state
        .apply_validated_join(
            &request(
                "replacement-join",
                Command::Join {
                    version: VERSION,
                    agent_id: replacement.clone(),
                    name: "coordinator".into(),
                    role: "different-manager-role".into(),
                    pane_id: "w1:p3".into(),
                    space_id: space,
                    cwd: None,
                    reset: None,
                    startup_grant: None,
                },
            ),
            None,
            9,
            &config,
            true,
        )
        .unwrap();
    state
        .apply(
            &request(
                "replacement-idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&replacement),
            9,
            &config,
        )
        .unwrap();
    state.tick(9, &config).unwrap();
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.messages[0].id, old_id);
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);
    assert_eq!(state.messages[1].to, replacement);
    assert_ne!(state.messages[1].id, state.messages[0].id);
}

#[test]
fn manager_checkin_is_independent_of_task_state_and_does_not_nudge_tasks() {
    let (mut state, architect, worker, config) = setup();
    let id = assign(&mut state, &architect, &worker, &config);
    state.messages.clear();
    state
        .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 0)
        .unwrap();
    let task_before = state.tasks[&id].clone();
    for status in [
        TaskState::Assigned,
        TaskState::Active,
        TaskState::Blocked,
        TaskState::Waiting,
        TaskState::Review,
        TaskState::Paused,
        TaskState::Accepted,
        TaskState::Cancelled,
    ] {
        state.tasks.get_mut(&id).unwrap().state = status;
        state.tick(10, &config).unwrap();
        assert_eq!(
            state.messages.len(),
            1,
            "coalesced checkpoint for {status:?}"
        );
        assert_eq!(state.messages[0].to, architect);
        assert_eq!(state.messages[0].task_id, None);
        assert!(state.messages[0].text.contains("tasks_truncated"));
        let roster = state
            .apply(
                &request(
                    "manager-roster",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                ),
                Some(&architect),
                10,
                &config,
            )
            .unwrap();
        assert_eq!(
            roster["tasks"][0]["state"],
            serde_json::to_value(status).unwrap()
        );
        assert_eq!(state.tasks[&id].nudges, task_before.nudges);
        assert_eq!(state.tasks[&id].updated_at, task_before.updated_at);
    }
}

#[test]
fn manager_slot_is_unique_live_and_reconnect_requires_verified_role_metadata() {
    let (mut state, manager, _, config) = setup();
    assert!(state.members[&manager].manager);

    let second = AgentId::new("second-manager");
    let before = state.clone();
    assert!(
        state
            .apply_validated_join(
                &request(
                    "second-manager-join",
                    Command::Join {
                        version: VERSION,
                        agent_id: second.clone(),
                        name: "second-manager".into(),
                        role: "custom".into(),
                        pane_id: "w1:p3".into(),
                        space_id: SpaceId::new("w1"),
                        cwd: None,
                        reset: None,
                        startup_grant: None,
                    },
                ),
                None,
                1,
                &config,
                true,
            )
            .is_err()
    );
    assert_eq!(state, before, "duplicate manager Join must be atomic");

    state.disconnect(&manager);
    assert!(!state.members[&manager].manager);
    assert!(
        state
            .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 2)
            .is_err()
    );
    state
        .apply(
            &request(
                "unverified-manager-rejoin",
                Command::Join {
                    version: VERSION,
                    agent_id: manager.clone(),
                    name: "architect".into(),
                    role: "architect".into(),
                    pane_id: "w1:p1".into(),
                    space_id: SpaceId::new("w1"),
                    cwd: None,
                    reset: None,
                    startup_grant: None,
                },
            ),
            None,
            2,
            &config,
        )
        .unwrap();
    assert!(
        !state.members[&manager].manager,
        "role text is not authority"
    );
    assert!(
        state
            .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 2)
            .is_err()
    );
    state.disconnect(&manager);

    state
        .apply_validated_join(
            &request(
                "manager-rejoin",
                Command::Join {
                    version: VERSION,
                    agent_id: manager.clone(),
                    name: "architect".into(),
                    role: "custom".into(),
                    pane_id: "w1:p1".into(),
                    space_id: SpaceId::new("w1"),
                    cwd: None,
                    reset: None,
                    startup_grant: None,
                },
            ),
            None,
            3,
            &config,
            true,
        )
        .unwrap();
    assert!(state.members[&manager].manager);
    state.recover();
    assert!(!state.members[&manager].manager);
    assert!(!state.automation.checkin_enabled);
    assert_eq!(state.automation.manager_checkin_at, None);
}

#[test]
fn manager_checkin_requires_admission_and_runs_with_no_tasks() {
    let config = Scheduling::default();
    let space = SpaceId::new("w1");
    let mut state = State::new(space.clone());
    assert!(state.automation.checkin_enabled);
    assert!(state.set_mode(&space, Mode::Checkin, true, 10).is_err());
    assert!(
        state.automation.checkin_enabled,
        "no manager is a waiting state, not an implicit disable"
    );
    assert_eq!(state.automation.manager_checkin_at, None);

    let manager = AgentId::new("arbitrary-role-agent");
    state
        .apply_validated_join(
            &request(
                "manager-join",
                Command::Join {
                    version: VERSION,
                    agent_id: manager.clone(),
                    name: "coordinator".into(),
                    role: "coordinator".into(),
                    pane_id: "w1:p1".into(),
                    space_id: space.clone(),
                    cwd: None,
                    reset: None,
                    startup_grant: None,
                },
            ),
            None,
            10,
            &config,
            true,
        )
        .unwrap();
    state
        .apply(
            &request(
                "idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&manager),
            10,
            &config,
        )
        .unwrap();
    state.set_mode(&space, Mode::Checkin, true, 10).unwrap();
    assert_eq!(state.automation.manager_checkin_at, Some(10));
    state.tick(10, &config).unwrap();
    assert_eq!(state.tasks.len(), 0);
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].to, manager);
    assert_eq!(state.messages[0].kind, MessageKind::Checkin);
    assert_eq!(state.messages[0].task_id, None);
    assert_eq!(state.messages[0].revision, None);
    assert!(state.messages[0].wake);
    assert_eq!(
        state.automation.manager_checkin_at,
        Some(10 + config.checkin_seconds)
    );

    state.set_mode(&space, Mode::Checkin, false, 11).unwrap();
    assert_eq!(state.messages[0].delivery, Delivery::Cancelled);
    assert_eq!(state.automation.manager_checkin_at, None);
    state.tick(10 + config.checkin_seconds, &config).unwrap();
    assert_eq!(
        state.messages.len(),
        1,
        "mode off fences the stale deadline"
    );
}

#[test]
fn done_report_wakes_requester_and_remains_reviewable_with_all_modes_off() {
    for accept in [false, true] {
        let (mut state, requester, owner, config) = setup();
        let task = assign(&mut state, &requester, &owner, &config);
        receive(&mut state, &owner, &config, 1);
        assert!(!state.automation.todo_enabled);
        assert!(!state.automation.checkin_enabled);
        assert!(!state.automation.pr_enabled);

        state
            .apply(
                &request(
                    "done",
                    Command::Report {
                        task_id: task.clone(),
                        revision: 1,
                        kind: ReportKind::Done,
                        text: "Verified output and tests.".into(),
                    },
                ),
                Some(&owner),
                2,
                &config,
            )
            .unwrap();
        let queued = state.messages.last().unwrap();
        assert_eq!(queued.kind, MessageKind::Done);
        assert!(queued.wake);
        assert_eq!(queued.to, requester);
        assert_eq!(queued.task_id.as_ref(), Some(&task));
        assert_eq!(queued.revision, Some(1));
        assert_eq!(state.tasks[&task].state, TaskState::Review);

        let delivered = receive(&mut state, &requester, &config, 3).unwrap();
        assert_eq!(delivered.kind, MessageKind::Done);
        assert_eq!(
            state
                .messages
                .iter()
                .find(|message| message.id == delivered.id)
                .unwrap()
                .delivery,
            Delivery::Received
        );
        assert_eq!(state.tasks[&task].state, TaskState::Review);
        let command = if accept {
            Command::Accept {
                task_id: task.clone(),
                revision: 1,
                text: "Independently reviewed and accepted.".into(),
            }
        } else {
            Command::Guide {
                task_id: task.clone(),
                revision: 1,
                text: "Reconcile one remaining issue, then report again.".into(),
            }
        };
        state
            .apply(
                &request("requester-action", command),
                Some(&requester),
                4,
                &config,
            )
            .unwrap();
        assert_eq!(
            state.tasks[&task].state,
            if accept {
                TaskState::Accepted
            } else {
                TaskState::Assigned
            }
        );
        if !accept {
            assert_eq!(state.tasks[&task].revision, 2);
        }
    }
}

#[test]
fn manager_role_does_not_override_requester_only_task_authority() {
    let (mut state, manager, worker, config) = setup();
    let requester = AgentId::new("other-requester");
    state
        .apply_validated_join(
            &request(
                "other-requester-join",
                Command::Join {
                    version: VERSION,
                    agent_id: requester.clone(),
                    name: "other-requester".into(),
                    role: "custom".into(),
                    pane_id: "w1:p3".into(),
                    space_id: SpaceId::new("w1"),
                    cwd: None,
                    reset: None,
                    startup_grant: None,
                },
            ),
            None,
            1,
            &config,
            false,
        )
        .unwrap();
    state
        .apply(
            &request(
                "requester-idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&requester),
            1,
            &config,
        )
        .unwrap();
    let task = assign(&mut state, &requester, &worker, &config);
    state.tasks.get_mut(&task).unwrap().state = TaskState::Review;
    let before = state.tasks[&task].clone();

    for action in [
        Command::Guide {
            task_id: task.clone(),
            revision: before.revision,
            text: "Manager cannot guide another requester task.".into(),
        },
        Command::Accept {
            task_id: task.clone(),
            revision: before.revision,
            text: "Manager cannot accept another requester task.".into(),
        },
        Command::Cancel {
            task_id: task.clone(),
            revision: before.revision,
            text: "Manager cannot cancel another requester task.".into(),
        },
    ] {
        assert!(
            state
                .apply(
                    &request("manager-action", action),
                    Some(&manager),
                    2,
                    &config
                )
                .is_err()
        );
        assert_eq!(state.tasks[&task], before);
    }
    assert!(state.members[&manager].manager);
}

#[test]
fn recovery_preserves_checkin_choice_but_rejoins_never_replay_old_checkpoints() {
    let config = Scheduling::default();
    let space = SpaceId::new("w1");
    let manager = AgentId::new("manager-session");
    let mut state = State::new(space.clone());
    assert!(state.automation.checkin_enabled);
    state.tick(0, &config).unwrap();
    assert!(
        state.messages.is_empty(),
        "default-on waits without a manager"
    );
    state
        .apply_validated_join(
            &request(
                "manager-join",
                Command::Join {
                    version: VERSION,
                    agent_id: manager.clone(),
                    name: "coordinator".into(),
                    role: "custom".into(),
                    pane_id: "w1:p1".into(),
                    space_id: space.clone(),
                    cwd: None,
                    reset: None,
                    startup_grant: None,
                },
            ),
            None,
            1,
            &config,
            true,
        )
        .unwrap();
    state
        .apply(
            &request(
                "idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&manager),
            1,
            &config,
        )
        .unwrap();
    state.tick(1, &config).unwrap();
    let old_id = state.messages[0].id.clone();
    assert_eq!(state.messages[0].kind, MessageKind::Checkin);
    state.recover();
    assert!(state.automation.checkin_enabled);
    assert_eq!(state.automation.manager_checkin_at, None);
    assert!(state.members.values().all(|member| !member.manager));
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);

    state
        .apply_validated_join(
            &request(
                "manager-rejoin",
                Command::Join {
                    version: VERSION,
                    agent_id: manager.clone(),
                    name: "coordinator".into(),
                    role: "custom".into(),
                    pane_id: "w1:p1".into(),
                    space_id: space.clone(),
                    cwd: None,
                    reset: None,
                    startup_grant: None,
                },
            ),
            None,
            2,
            &config,
            true,
        )
        .unwrap();
    state
        .apply(
            &request(
                "rejoin-idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
            Some(&manager),
            2,
            &config,
        )
        .unwrap();
    state.tick(2, &config).unwrap();
    assert_eq!(state.messages.len(), 2);
    assert_eq!(state.messages[0].id, old_id);
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);
    assert_ne!(state.messages[0].id, state.messages[1].id);
    assert_eq!(state.messages[1].delivery, Delivery::Pending);

    let mut explicitly_off = State::new(space.clone());
    explicitly_off
        .set_mode(&space, Mode::Checkin, false, 3)
        .unwrap();
    explicitly_off.recover();
    assert!(!explicitly_off.automation.checkin_enabled);
}

#[test]
fn crash_recovery_never_replays_ambiguous_work() {
    let (mut state, architect, worker, config) = setup();
    let id = assign(&mut state, &architect, &worker, &config);
    state
        .apply(
            &request(
                "offer",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&worker),
            1,
            &config,
        )
        .unwrap();
    state.recover();
    assert_eq!(state.tasks[&id].state, TaskState::Paused);
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);
    assert!(
        state
            .members
            .values()
            .all(|m| m.activity == Activity::Offline)
    );
}

fn attach_pr(state: &mut State, worker: &AgentId) {
    let a = &mut state.automation;
    a.pr_enabled = true;
    a.pr = Some(PrWatch {
        url: Some("https://github.com/example/repo/pull/1".into()),
        cwd: "/fixture".into(),
        driver: worker.clone(),
        branch: None,
        head: None,
        baseline: None,
        next_at: 0,
        error: None,
    });
}
fn pr_observation(failure: bool) -> PrSnapshot {
    PrSnapshot {
        url: "https://github.com/example/repo/pull/1".into(),
        cwd: "/fixture/repo".into(),
        branch: "feature".into(),
        head: "abc".into(),
        local_head: "abc".into(),
        open: true,
        feedback: BTreeMap::from([(
            "comment:1".into(),
            ("old-hash".into(), "Existing comment".into()),
        )]),
        failures: if failure {
            BTreeMap::from([("check-hash".into(), "Failed test".into())])
        } else {
            BTreeMap::new()
        },
    }
}

#[test]
fn first_pr_mode_on_binds_authenticated_sender_and_retries_same_failed_discovery() {
    let (mut state, _, worker, config) = setup();
    let checkout = tempfile::tempdir().unwrap();
    state.members.get_mut(&worker).unwrap().cwd = Some(checkout.path().to_path_buf());
    let result = state
        .apply(
            &request(
                "discover-pr",
                Command::Mode {
                    mode: Mode::Pr,
                    enabled: true,
                },
            ),
            Some(&worker),
            10,
            &config,
        )
        .unwrap();
    assert_eq!(result, json!({"enabled":true}));
    assert!(state.automation.pr_enabled);
    let watch = state.automation.pr.as_ref().unwrap();
    assert_eq!(watch.driver, worker);
    assert_eq!(watch.cwd, checkout.path());
    assert_eq!(watch.url, None);
    assert_eq!(watch.error, None);
    assert!(watch.baseline.is_none());
    let original_generation = state.automation.generation;

    state
        .pr_result(
            &SpaceId::new("w1"),
            original_generation,
            Err("gh pr view failed; check authentication".into()),
            11,
        )
        .unwrap();
    assert!(!state.automation.pr_enabled);
    assert_eq!(state.automation.pr.as_ref().unwrap().url, None);
    assert!(state.automation.pr.as_ref().unwrap().error.is_some());
    assert!(state.automation.pr.as_ref().unwrap().baseline.is_none());

    state
        .set_mode(&SpaceId::new("w1"), Mode::Pr, true, 12)
        .unwrap();
    let retry = state.automation.pr.as_ref().unwrap();
    assert!(state.automation.pr_enabled);
    assert_eq!(retry.driver, worker);
    assert_eq!(retry.cwd, checkout.path());
    assert_eq!(retry.url, None);
    assert_eq!(retry.error, None);
    assert!(state.automation.generation > original_generation);
}

#[test]
fn automatic_pr_discovery_requires_a_live_unpaused_member_with_checkout() {
    let (mut state, _, worker, config) = setup();
    let before = state.clone();
    assert!(
        state
            .apply(
                &request(
                    "missing-cwd",
                    Command::Mode {
                        mode: Mode::Pr,
                        enabled: true
                    }
                ),
                Some(&worker),
                1,
                &config,
            )
            .is_err()
    );
    assert_eq!(state, before);

    let checkout = tempfile::tempdir().unwrap();
    state.members.get_mut(&worker).unwrap().cwd = Some(checkout.path().to_path_buf());
    state.members.get_mut(&worker).unwrap().activity = Activity::Paused;
    let before = state.clone();
    assert!(
        state
            .apply(
                &request(
                    "paused",
                    Command::Mode {
                        mode: Mode::Pr,
                        enabled: true
                    }
                ),
                Some(&worker),
                1,
                &config,
            )
            .is_err()
    );
    assert_eq!(state, before);
}

#[test]
fn explicit_pr_attachment_remains_off_until_the_user_enables_it() {
    let (mut state, _, worker, config) = setup();
    let checkout = tempfile::tempdir().unwrap();
    state
        .apply(
            &request(
                "attach-explicit-pr",
                Command::ConfigurePr {
                    url: "https://github.com/example/repo/pull/1".into(),
                    cwd: checkout.path().display().to_string(),
                },
            ),
            Some(&worker),
            10,
            &config,
        )
        .unwrap();
    let watch = state.automation.pr.as_ref().unwrap();
    assert!(!state.automation.pr_enabled);
    assert_eq!(
        watch.url.as_deref(),
        Some("https://github.com/example/repo/pull/1")
    );
    assert_eq!(watch.driver, worker);
    assert_eq!(watch.cwd, checkout.path());
    assert!(watch.baseline.is_none());

    state
        .apply(
            &request(
                "enable-explicit-pr",
                Command::Mode {
                    mode: Mode::Pr,
                    enabled: true,
                },
            ),
            Some(&worker),
            11,
            &config,
        )
        .unwrap();
    assert!(state.automation.pr_enabled);
    assert_eq!(
        state.automation.pr.as_ref().unwrap().url.as_deref(),
        Some("https://github.com/example/repo/pull/1")
    );
}

#[test]
fn discovered_watch_pins_url_root_only_after_complete_success_and_never_retargets() {
    let (mut state, _, worker, _config) = setup();
    let checkout = tempfile::tempdir().unwrap();
    let nested = checkout.path().join("package");
    std::fs::create_dir(&nested).unwrap();
    state.members.get_mut(&worker).unwrap().cwd = Some(nested.clone());
    state
        .enable_pr_discovery(&SpaceId::new("w1"), &worker, 10)
        .unwrap();
    let generation = state.automation.generation;
    assert_eq!(state.automation.pr.as_ref().unwrap().url, None);
    assert!(state.automation.pr.as_ref().unwrap().baseline.is_none());

    let mut discovered = pr_observation(false);
    discovered.cwd = checkout.path().to_path_buf();
    state
        .pr_result(&SpaceId::new("w1"), generation, Ok(discovered), 11)
        .unwrap();
    let pinned = state.automation.pr.as_ref().unwrap();
    assert_eq!(
        pinned.url.as_deref(),
        Some("https://github.com/example/repo/pull/1")
    );
    assert_eq!(pinned.cwd, checkout.path());
    assert!(pinned.baseline.is_some());

    let before_baseline = pinned.baseline.clone();
    let mut changed = pr_observation(false);
    changed.url = "https://github.com/another/repo/pull/2".into();
    changed.cwd = checkout.path().to_path_buf();
    state
        .pr_result(&SpaceId::new("w1"), generation, Ok(changed), 12)
        .unwrap();
    let pinned = state.automation.pr.as_ref().unwrap();
    assert!(!state.automation.pr_enabled);
    assert_eq!(
        pinned.url.as_deref(),
        Some("https://github.com/example/repo/pull/1")
    );
    assert_eq!(pinned.baseline, before_baseline);
    assert!(
        pinned
            .error
            .as_deref()
            .is_some_and(|error| error.contains("retarget"))
    );

    // A first-setup toggle racing an already resolved attachment only toggles it;
    // it never replaces the explicit URL with branch discovery.
    let (mut attached, _, attached_driver, config) = setup();
    attach_pr(&mut attached, &attached_driver);
    attached
        .apply(
            &request(
                "enable-explicit",
                Command::Mode {
                    mode: Mode::Pr,
                    enabled: true,
                },
            ),
            Some(&attached_driver),
            20,
            &config,
        )
        .unwrap();
    assert_eq!(
        attached.automation.pr.as_ref().unwrap().url.as_deref(),
        Some("https://github.com/example/repo/pull/1")
    );
    let before = attached.clone();
    assert!(
        attached
            .enable_pr_discovery(&SpaceId::new("w1"), &attached_driver, 21)
            .is_err()
    );
    assert_eq!(
        attached, before,
        "a picker race cannot replace an attached PR"
    );
}

#[test]
fn unresolved_pr_results_are_fenced_by_off_reset_disconnect_and_reconfigure() {
    let checkout = tempfile::tempdir().unwrap();

    let (mut off, _, off_driver, _) = setup();
    off.members.get_mut(&off_driver).unwrap().cwd = Some(checkout.path().to_path_buf());
    off.enable_pr_discovery(&SpaceId::new("w1"), &off_driver, 1)
        .unwrap();
    let off_generation = off.automation.generation;
    off.set_mode(&SpaceId::new("w1"), Mode::Pr, false, 2)
        .unwrap();
    let before = off.clone();
    let mut observation = pr_observation(false);
    observation.cwd = checkout.path().to_path_buf();
    off.pr_result(
        &SpaceId::new("w1"),
        off_generation,
        Ok(observation.clone()),
        3,
    )
    .unwrap();
    assert_eq!(off, before);

    let (mut reset, reset_requester, reset_driver, reset_config) = setup();
    reset.members.get_mut(&reset_driver).unwrap().cwd = Some(checkout.path().to_path_buf());
    reset
        .enable_pr_discovery(&SpaceId::new("w1"), &reset_driver, 1)
        .unwrap();
    let reset_generation = reset.automation.generation;
    for agent in [&reset_requester, &reset_driver] {
        reset
            .apply(
                &request("capability", Command::ResetCapability),
                Some(agent),
                1,
                &reset_config,
            )
            .unwrap();
    }
    let preview = reset.reset_preview(&SpaceId::new("w1")).unwrap();
    reset.start_reset(&preview).unwrap();
    let before = reset.clone();
    reset
        .pr_result(
            &SpaceId::new("w1"),
            reset_generation,
            Ok(observation.clone()),
            3,
        )
        .unwrap();
    assert_eq!(reset, before);

    let (mut disconnected, _, disconnected_driver, _) = setup();
    disconnected
        .members
        .get_mut(&disconnected_driver)
        .unwrap()
        .cwd = Some(checkout.path().to_path_buf());
    disconnected
        .enable_pr_discovery(&SpaceId::new("w1"), &disconnected_driver, 1)
        .unwrap();
    let disconnected_generation = disconnected.automation.generation;
    disconnected.disconnect(&disconnected_driver);
    let before = disconnected.clone();
    disconnected
        .pr_result(
            &SpaceId::new("w1"),
            disconnected_generation,
            Ok(observation.clone()),
            3,
        )
        .unwrap();
    assert_eq!(disconnected, before);

    let (mut replaced, _, replaced_driver, config) = setup();
    replaced.members.get_mut(&replaced_driver).unwrap().cwd = Some(checkout.path().to_path_buf());
    replaced
        .enable_pr_discovery(&SpaceId::new("w1"), &replaced_driver, 1)
        .unwrap();
    let pending_generation = replaced.automation.generation;
    replaced
        .apply(
            &request(
                "explicit-reconfigure",
                Command::ConfigurePr {
                    url: "https://github.com/example/repo/pull/2".into(),
                    cwd: checkout.path().display().to_string(),
                },
            ),
            Some(&replaced_driver),
            2,
            &config,
        )
        .unwrap();
    let before = replaced.clone();
    replaced
        .pr_result(&SpaceId::new("w1"), pending_generation, Ok(observation), 3)
        .unwrap();
    assert_eq!(replaced, before);
}

#[test]
fn pr_ack_is_internal_bounded_and_only_advances_on_receipt() {
    let (mut state, _, worker, config) = setup();
    attach_pr(&mut state, &worker);
    let space = SpaceId::new("w1");
    let mut observation = pr_observation(true);
    for i in 0..10_000 {
        observation.feedback.insert(
            format!("inline:{i}"),
            (format!("hash-{i}"), "Existing".into()),
        );
    }
    state.pr_result(&space, 0, Ok(observation), 0).unwrap();
    assert!(
        state
            .automation
            .pr
            .as_ref()
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .failures
            .is_empty()
    );
    let response = state
        .apply(
            &request(
                "sync",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(&worker),
            1,
            &config,
        )
        .unwrap();
    assert!(serde_json::to_vec(&response).unwrap().len() < 16 * 1024);
    assert!(response["inbox"]["pr_ack"].is_null());
    let id: MessageId = serde_json::from_value(response["inbox"]["id"].clone()).unwrap();
    state
        .apply(
            &request("ack", Command::Ack { message_id: id }),
            Some(&worker),
            1,
            &config,
        )
        .unwrap();
    assert_eq!(
        state
            .automation
            .pr
            .as_ref()
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .failures,
        vec!["check-hash"]
    );
    assert!(state.messages[0].pr_ack.is_none());
}

#[test]
fn pr_errors_and_closure_cancel_pending_work_without_losing_baselines() {
    let (mut state, _, worker, _) = setup();
    attach_pr(&mut state, &worker);
    let space = SpaceId::new("w1");
    state
        .pr_result(&space, 0, Ok(pr_observation(true)), 0)
        .unwrap();
    state
        .pr_result(&space, 0, Err("local branch changed".into()), 1)
        .unwrap();
    assert!(!state.automation.pr_enabled);
    assert_eq!(state.messages[0].delivery, Delivery::Cancelled);
    state.set_mode(&space, Mode::Pr, true, 2).unwrap();
    let generation = state.automation.generation;
    state
        .pr_result(&space, generation - 1, Ok(pr_observation(true)), 2)
        .unwrap();
    assert_eq!(state.messages.len(), 1); // stale in-flight poll ignored
    state
        .pr_result(&space, generation, Ok(pr_observation(true)), 3)
        .unwrap();
    assert_eq!(state.messages.len(), 2); // cancelled failure was not acknowledged
    let mut closed = pr_observation(true);
    closed.open = false;
    state.pr_result(&space, generation, Ok(closed), 4).unwrap();
    assert!(!state.automation.pr_enabled);
    assert!(
        state
            .messages
            .iter()
            .all(|m| m.delivery == Delivery::Cancelled)
    );
    assert_eq!(
        state
            .automation
            .pr
            .as_ref()
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .feedback["comment:1"],
        "old-hash"
    );
}

#[test]
fn disconnect_disables_watchers_owned_by_that_member() {
    let (mut state, _, worker, _) = setup();
    state.automation.checkin_enabled = true;
    attach_pr(&mut state, &worker);
    let generation = state.automation.generation;
    state.disconnect(&worker);
    assert!(state.automation.checkin_enabled);
    assert!(!state.automation.pr_enabled);
    assert!(state.automation.generation > generation);
    assert!(
        state
            .set_mode(&SpaceId::new("w2"), Mode::Checkin, true, 0)
            .is_err()
    );
}

#[test]
fn disable_cancels_undelivered_todo_and_pr_prompts() {
    let (mut state, _, worker, _) = setup();
    let space = SpaceId::new("w1");
    state
        .queue(
            &space,
            None,
            &worker,
            None,
            MessageKind::Todo,
            "todo".into(),
            true,
            0,
        )
        .unwrap();
    state
        .queue(
            &space,
            None,
            &worker,
            None,
            MessageKind::Pr,
            "pr".into(),
            true,
            0,
        )
        .unwrap();
    state.set_mode(&space, Mode::Todo, false, 1).unwrap();
    state.set_mode(&space, Mode::Pr, false, 1).unwrap();
    assert!(
        state
            .messages
            .iter()
            .all(|m| m.delivery == Delivery::Cancelled)
    );
}
