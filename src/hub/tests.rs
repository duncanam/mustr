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
    for (id, name, pane) in [
        (&architect, "architect", "w1:p1"),
        (&worker, "worker", "w1:p2"),
    ] {
        state
            .apply(
                &request(
                    name,
                    Command::Join {
                        version: 1,
                        agent_id: id.clone(),
                        name: name.into(),
                        role: "custom".into(),
                        pane_id: pane.into(),
                        space_id: SpaceId::new("w1"),
                        reset: None,
                    },
                ),
                None,
                0,
                &config,
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
fn periodic_checkins_are_independent_and_pause_cancels_queued_prompts() {
    let (mut state, architect, worker, mut config) = setup();
    config.checkin_seconds = 5;
    assign(&mut state, &architect, &worker, &config);
    receive(&mut state, &worker, &config, 0);
    state
        .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 0)
        .unwrap();
    state.tick(5, &config).unwrap();
    assert_eq!(state.messages.last().unwrap().kind, MessageKind::Checkin);
    state
        .apply(
            &request(
                "pause",
                Command::Sync {
                    activity: Activity::Paused,
                    receive: false,
                },
            ),
            Some(&worker),
            5,
            &config,
        )
        .unwrap();
    assert_eq!(state.messages.last().unwrap().delivery, Delivery::Cancelled);
    state.tick(6, &config).unwrap();
    assert_eq!(state.messages.len(), 2);
}

#[test]
fn non_active_tasks_never_receive_continuation_or_checkins() {
    let (mut state, architect, worker, config) = setup();
    let id = assign(&mut state, &architect, &worker, &config);
    state.messages.clear();
    state
        .set_mode(&SpaceId::new("w1"), Mode::Todo, true, 0)
        .unwrap();
    state
        .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 0)
        .unwrap();
    for status in [
        TaskState::Assigned,
        TaskState::Blocked,
        TaskState::Waiting,
        TaskState::Review,
        TaskState::Paused,
        TaskState::Accepted,
        TaskState::Cancelled,
    ] {
        state.tasks.get_mut(&id).unwrap().state = status;
        state.tasks.get_mut(&id).unwrap().checkin_at = 0;
        state.tick(10, &config).unwrap();
        assert!(
            state.messages.is_empty(),
            "unexpected reminder for {status:?}"
        );
    }
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
        url: "https://github.com/example/repo/pull/1".into(),
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
