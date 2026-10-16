use super::*;

fn offer(state: &mut State, agent: &AgentId, config: &Scheduling, now: u64) -> Value {
    state
        .apply(
            &request(
                "poll",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(agent),
            now,
            config,
        )
        .unwrap()
}

#[test]
fn only_one_offer_until_receipt_even_when_client_requests_more() {
    let (mut state, architect, worker, config) = setup();
    let info = state
        .queue(
            &SpaceId::new("w1"),
            Some(&architect),
            &worker,
            None,
            MessageKind::Info,
            "info".into(),
            false,
            0,
        )
        .unwrap();
    let task = assign(&mut state, &architect, &worker, &config);
    let first = offer(&mut state, &worker, &config, 1);
    assert_eq!(first["inbox"]["id"], info.as_str());
    assert_eq!(first["insertion_policy"], "live");
    // Long insertion keeps its one offer and heartbeats; no task is paused.
    for now in 2..40 {
        assert!(offer(&mut state, &worker, &config, now)["inbox"].is_null());
        state.tick(now, &config).unwrap();
        assert_eq!(state.tasks[&task].state, TaskState::Assigned);
    }
    state
        .apply(
            &request(
                "ack",
                Command::Ack {
                    message_id: info.clone(),
                },
            ),
            Some(&worker),
            40,
            &config,
        )
        .unwrap();
    state
        .apply(
            &request("duplicate-ack", Command::Ack { message_id: info }),
            Some(&worker),
            40,
            &config,
        )
        .unwrap();
    let next = offer(&mut state, &worker, &config, 40);
    assert_eq!(next["inbox"]["kind"], "assignment");
    assert_eq!(next["insertion_policy"], "idle_only");
}

#[test]
fn input_race_defers_uninserted_assignment_without_pausing_or_duplicating_it() {
    let (mut state, architect, worker, config) = setup();
    let task = assign(&mut state, &architect, &worker, &config);
    let first = offer(&mut state, &worker, &config, 1);
    let message_id: MessageId = serde_json::from_value(first["inbox"]["id"].clone()).unwrap();
    let defer = request(
        "defer",
        Command::Defer {
            message_id: message_id.clone(),
        },
    );
    assert!(state.apply(&defer, Some(&architect), 2, &config).is_err());
    state.apply(&defer, Some(&worker), 2, &config).unwrap();
    state.apply(&defer, Some(&worker), 2, &config).unwrap(); // request deduplication
    assert_eq!(state.tasks[&task].state, TaskState::Assigned);
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.messages[0].delivery, Delivery::Pending);
    assert!(
        state
            .apply(
                &request(
                    "early-ack",
                    Command::Ack {
                        message_id: message_id.clone()
                    }
                ),
                Some(&worker),
                2,
                &config
            )
            .is_err()
    );
    assert_eq!(
        receive(&mut state, &worker, &config, 3).unwrap().id,
        message_id
    );
    assert_eq!(state.tasks[&task].state, TaskState::Active);
    assert!(
        state
            .apply(
                &request("late-defer", Command::Defer { message_id }),
                Some(&worker),
                4,
                &config
            )
            .is_err()
    );
}

#[test]
fn deferring_a_superseded_offer_does_not_revive_cancelled_work() {
    for cancel in [true, false] {
        let (mut state, architect, worker, config) = setup();
        let task = assign(&mut state, &architect, &worker, &config);
        offer(&mut state, &worker, &config, 1);
        let message_id = state.messages[0].id.clone();
        let command = if cancel {
            Command::Cancel {
                task_id: task.clone(),
                revision: 1,
                text: "cancel".into(),
            }
        } else {
            Command::Guide {
                task_id: task.clone(),
                revision: 1,
                text: "revised".into(),
            }
        };
        state
            .apply(&request("revise", command), Some(&architect), 2, &config)
            .unwrap();
        state
            .apply(
                &request("defer", Command::Defer { message_id }),
                Some(&worker),
                3,
                &config,
            )
            .unwrap();
        assert_eq!(state.messages[0].delivery, Delivery::Cancelled);
        let next = receive(&mut state, &worker, &config, 4).unwrap();
        assert_eq!(
            next.kind,
            if cancel {
                MessageKind::Info
            } else {
                MessageKind::Guide
            }
        );
    }
}

#[test]
fn disconnect_requires_explicit_guidance_even_after_reconnect_with_automation_on() {
    let (mut state, architect, worker, config) = setup();
    let task = assign(&mut state, &architect, &worker, &config);
    offer(&mut state, &worker, &config, 1);
    let message_id = state.messages[0].id.clone();
    state
        .set_mode(&SpaceId::new("w1"), Mode::Todo, true, 1)
        .unwrap();
    state
        .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 1)
        .unwrap();
    state.disconnect(&worker);
    state
        .apply(
            &request(
                "rejoin",
                Command::Join {
                    version: VERSION,
                    agent_id: worker.clone(),
                    name: "worker".into(),
                    role: "custom".into(),
                    pane_id: "w1:p2".into(),
                    space_id: SpaceId::new("w1"),
                },
            ),
            None,
            2,
            &config,
        )
        .unwrap();
    for now in 2..150 {
        assert!(offer(&mut state, &worker, &config, now)["inbox"].is_null());
        state
            .apply(
                &request(
                    "presence",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                ),
                Some(&architect),
                now,
                &config,
            )
            .unwrap();
        state.tick(now, &config).unwrap();
        assert_eq!(state.tasks[&task].state, TaskState::Paused);
    }
    assert!(
        state
            .apply(
                &request(
                    "late-ack",
                    Command::Ack {
                        message_id: message_id.clone()
                    }
                ),
                Some(&worker),
                150,
                &config
            )
            .is_err()
    );
    assert!(
        state
            .apply(
                &request("unsafe-replay", Command::Defer { message_id }),
                Some(&worker),
                150,
                &config
            )
            .is_err()
    );
    state
        .apply(
            &request(
                "guide",
                Command::Guide {
                    task_id: task.clone(),
                    revision: 1,
                    text: "Reconciled: no external work. Resume review.".into(),
                },
            ),
            Some(&architect),
            150,
            &config,
        )
        .unwrap();
    assert_eq!(
        receive(&mut state, &worker, &config, 150).unwrap().kind,
        MessageKind::Guide
    );
    assert_eq!(state.tasks[&task].state, TaskState::Active);
    assert_eq!(state.tasks[&task].revision, 2);
}

#[test]
fn disabling_automation_while_offered_then_deferring_does_not_revive_it() {
    for (kind, mode) in [
        (MessageKind::Todo, Mode::Todo),
        (MessageKind::Continue, Mode::Todo),
        (MessageKind::Checkin, Mode::Checkin),
        (MessageKind::Pr, Mode::Pr),
    ] {
        let (mut state, _, worker, config) = setup();
        let space = SpaceId::new("w1");
        if kind == MessageKind::Pr {
            attach_pr(&mut state, &worker);
        }
        state.set_mode(&space, mode, true, 0).unwrap();
        let message_id = state
            .queue(
                &space,
                None,
                &worker,
                None,
                kind,
                "automatic work".into(),
                true,
                0,
            )
            .unwrap();
        assert!(offer(&mut state, &worker, &config, 1)["inbox"].is_object());
        state.set_mode(&space, mode, false, 2).unwrap();
        assert_eq!(state.messages[0].delivery, Delivery::Offered);
        state
            .apply(
                &request("defer", Command::Defer { message_id }),
                Some(&worker),
                3,
                &config,
            )
            .unwrap();
        assert_eq!(state.messages[0].delivery, Delivery::Cancelled);
        assert!(offer(&mut state, &worker, &config, 4)["inbox"].is_null());
    }
}

#[test]
fn push_readiness_uses_full_queue_and_never_offers_or_leaks_content() {
    let (mut state, architect, worker, config) = setup();
    let space = SpaceId::new("w1");
    let oldest = state
        .queue(
            &space,
            Some(&architect),
            &worker,
            None,
            MessageKind::Info,
            "oldest".into(),
            false,
            0,
        )
        .unwrap();
    for _ in 0..205 {
        state
            .queue(
                &space,
                Some(&worker),
                &architect,
                None,
                MessageKind::Info,
                "other recipient".into(),
                false,
                0,
            )
            .unwrap();
    }
    let snapshot = state.snapshot(vec![], None, 1);
    assert_eq!(snapshot.messages.len(), 200);
    assert_eq!(snapshot.ready[&worker], oldest);
    assert!(
        serde_json::to_value(&snapshot)
            .unwrap()
            .get("ready")
            .is_none()
    );
    assert_eq!(
        state.messages[0].delivery,
        Delivery::Pending,
        "hints are not offers"
    );
    offer(&mut state, &worker, &config, 1);
    assert!(!state.snapshot(vec![], None, 1).ready.contains_key(&worker));
    state.disconnect(&worker);
    assert!(!state.snapshot(vec![], None, 1).ready.contains_key(&worker));
}

#[test]
fn acceptance_cancels_only_notification_not_completed_task() {
    let (mut state, architect, worker, config) = setup();
    let task = assign(&mut state, &architect, &worker, &config);
    receive(&mut state, &worker, &config, 1);
    state
        .apply(
            &request(
                "done",
                Command::Report {
                    task_id: task.clone(),
                    revision: 1,
                    kind: ReportKind::Done,
                    text: "done".into(),
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
                "accept",
                Command::Accept {
                    task_id: task.clone(),
                    revision: 1,
                    text: "independent review".into(),
                },
            ),
            Some(&architect),
            3,
            &config,
        )
        .unwrap();
    assert_eq!(state.tasks[&task].state, TaskState::Accepted);
    assert_eq!(
        state
            .messages
            .iter()
            .find(|m| m.kind == MessageKind::Done)
            .unwrap()
            .delivery,
        Delivery::Cancelled
    );
}
