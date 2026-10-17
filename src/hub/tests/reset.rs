use super::*;

fn capable(state: &mut State, agent: &AgentId, config: &Scheduling) {
    state
        .apply(
            &request("capability", Command::ResetCapability),
            Some(agent),
            0,
            config,
        )
        .unwrap();
}

fn fixture() -> (State, AgentId, AgentId, Scheduling) {
    let (mut state, architect, worker, config) = setup();
    capable(&mut state, &architect, &config);
    capable(&mut state, &worker, &config);
    (state, architect, worker, config)
}

fn ticket(state: &mut State, agent: &AgentId, config: &Scheduling) -> ResetReceipt {
    let value = state
        .apply(
            &request(
                "reset-poll",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
            Some(agent),
            1,
            config,
        )
        .unwrap();
    assert!(value["inbox"].is_null());
    assert_eq!(value["tasks"], json!([]));
    let control = &value["reset_control"];
    ResetReceipt {
        plan_id: control["plan_id"].as_str().unwrap().into(),
        token: control["token"].as_str().unwrap().into(),
    }
}

fn claim(
    state: &mut State,
    agent: &AgentId,
    receipt: &ResetReceipt,
    config: &Scheduling,
) -> Result<Value> {
    state.apply(
        &request(
            "claim",
            Command::ResetClaim {
                plan_id: receipt.plan_id.clone(),
                token: receipt.token.clone(),
            },
        ),
        Some(agent),
        1,
        config,
    )
}

fn fresh_join(
    state: &mut State,
    old: &AgentId,
    new: &str,
    receipt: &ResetReceipt,
    config: &Scheduling,
) -> Result<Value> {
    let member = state.members.get(old).cloned().unwrap_or_else(|| {
        let progress = state.snapshot(vec![], None, 1).resets[&SpaceId::new("w1")]
            .participants
            .iter()
            .find(|p| &p.participant.agent_id == old)
            .unwrap()
            .clone();
        Member {
            id: old.clone(),
            name: progress.participant.name,
            role: progress.participant.role,
            pane_id: progress.participant.pane_id,
            space_id: SpaceId::new("w1"),
            activity: Activity::Offline,
            last_seen: 0,
            idle_since: 0,
        }
    });
    state.apply(
        &request(
            "fresh",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new(new),
                name: member.name,
                role: member.role,
                pane_id: member.pane_id,
                space_id: member.space_id,
                reset: Some(receipt.clone()),
            },
        ),
        None,
        2,
        config,
    )
}

fn begin(state: &mut State) {
    let preview = state.reset_preview(&SpaceId::new("w1")).unwrap();
    state.start_reset(&preview).unwrap();
}

#[test]
fn scoped_completion_clears_all_history_bindings_and_cache_not_other_spaces() {
    let (mut state, architect, worker, config) = fixture();
    let task = assign(&mut state, &architect, &worker, &config);
    let template = state.tasks[&task].clone();
    for (index, task_state) in [
        TaskState::Assigned,
        TaskState::Active,
        TaskState::Blocked,
        TaskState::Waiting,
        TaskState::Review,
        TaskState::Paused,
        TaskState::Accepted,
        TaskState::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        let mut task = template.clone();
        task.id = TaskId::new(format!("history-{index}"));
        task.state = task_state;
        state.tasks.insert(task.id.clone(), task);
    }
    for index in 0..300 {
        let mut message = state.messages[0].clone();
        message.id = MessageId::new(format!("history-{index}"));
        message.delivery = Delivery::Received;
        state.messages.push(message);
    }
    let mut offline = state.members[&worker].clone();
    offline.id = AgentId::new("offline-history");
    offline.activity = Activity::Offline;
    state.members.insert(offline.id.clone(), offline);
    let other = AgentId::new("other");
    state
        .apply(
            &request(
                "other-join",
                Command::Join {
                    version: VERSION,
                    agent_id: other.clone(),
                    name: "other".into(),
                    role: "custom".into(),
                    pane_id: "w2:p1".into(),
                    space_id: SpaceId::new("w2"),
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
                "other-send",
                Command::Send {
                    to: other.clone(),
                    text: "Keep this history".into(),
                    wake: false,
                },
            ),
            Some(&other),
            0,
            &config,
        )
        .unwrap();
    state
        .automation
        .get_mut(&SpaceId::new("w2"))
        .unwrap()
        .generation = 42;
    let other_member = state.members[&other].clone();
    let other_messages: Vec<_> = state
        .messages
        .iter()
        .filter(|m| m.space_id == SpaceId::new("w2"))
        .cloned()
        .collect();
    let other_cache: Vec<_> = state
        .cache
        .iter()
        .filter(|c| c.sender == other)
        .cloned()
        .collect();
    let other_automation = state.automation[&SpaceId::new("w2")].clone();
    attach_pr(&mut state, &worker);
    state.automation.get_mut(&SpaceId::new("w1")).unwrap().todo = Some(TodoWatch {
        path: "/fixture/TODO.md".into(),
        driver: worker.clone(),
        total: 3,
        remaining: 2,
        next: "Old TODO context".into(),
        signature: "old".into(),
        nudges: 1,
        next_at: 0,
        error: None,
    });
    let preview = state.reset_preview(&SpaceId::new("w1")).unwrap();
    assert_eq!(preview.message_count, 301);
    assert_eq!(preview.offline_count, 1);
    state.start_reset(&preview).unwrap();
    assert_eq!(state.messages.len(), 302, "retain data until all receipts");
    let a = ticket(&mut state, &architect, &config);
    let b = ticket(&mut state, &worker, &config);
    assert!(!state.snapshot(vec![], None, 1).ready.contains_key(&worker));
    let public = serde_json::to_string(&state.snapshot(vec![], None, 1)).unwrap();
    assert!(!public.contains(&a.token));
    assert!(!public.contains(&b.token));
    claim(&mut state, &architect, &a, &config).unwrap();
    fresh_join(&mut state, &architect, "architect-fresh", &a, &config).unwrap();
    assert!(state.reset_frozen(&SpaceId::new("w1")));
    assert!(state.tasks.contains_key(&task));
    claim(&mut state, &worker, &b, &config).unwrap();
    fresh_join(&mut state, &worker, "worker-fresh", &b, &config).unwrap();
    assert!(!state.reset_frozen(&SpaceId::new("w1")));
    assert!(state.tasks.is_empty());
    assert_eq!(state.messages, other_messages);
    assert_eq!(state.cache, other_cache);
    assert_eq!(state.members[&other], other_member);
    assert_eq!(state.automation[&SpaceId::new("w2")], other_automation);
    let automation = &state.automation[&SpaceId::new("w1")];
    assert!(!automation.todo_enabled && !automation.checkin_enabled && !automation.pr_enabled);
    assert!(automation.todo.is_none() && automation.pr.is_none());
    assert!(automation.generation > 0);
    assert_eq!(state.members.len(), 3);
    assert!(state.retired.contains_key(&architect));
    assert!(state.retired.contains_key(&worker));
    assert!(state.retired.contains_key(&AgentId::new("offline-history")));
    assert!(
        state
            .apply(
                &request(
                    "assign",
                    Command::Assign {
                        to: worker.clone(),
                        title: "Task".into(),
                        text: "Implement and verify scope".into()
                    }
                ),
                Some(&architect),
                2,
                &config
            )
            .is_err(),
        "old cached success must not bypass retirement"
    );
    fresh_join(&mut state, &architect, "architect-fresh", &a, &config).unwrap();
    assert!(fresh_join(&mut state, &architect, "another-fresh", &a, &config).is_err());
    assert!(fresh_join(&mut state, &architect, architect.as_str(), &a, &config).is_err());
}

#[test]
fn confirmed_receipt_cannot_overwrite_a_moved_or_renamed_current_identity() {
    let (mut completed, architect, worker, config) = fixture();
    begin(&mut completed);
    let a = ticket(&mut completed, &architect, &config);
    let b = ticket(&mut completed, &worker, &config);
    claim(&mut completed, &architect, &a, &config).unwrap();
    fresh_join(&mut completed, &architect, "architect-fresh", &a, &config).unwrap();
    claim(&mut completed, &worker, &b, &config).unwrap();
    fresh_join(&mut completed, &worker, "worker-fresh", &b, &config).unwrap();
    let fresh = AgentId::new("architect-fresh");
    let leave = request("leave", Command::Leave);
    for (name, role, pane, space) in [
        ("renamed", "custom", "w1:p1", "w1"),
        ("architect", "reviewer", "w1:p1", "w1"),
        ("architect", "custom", "w1:p3", "w1"),
        ("architect", "custom", "w2:p1", "w2"),
    ] {
        let mut state = completed.clone();
        state.apply(&leave, Some(&fresh), 3, &config).unwrap();
        state
            .apply(
                &request(
                    "move",
                    Command::Join {
                        version: VERSION,
                        agent_id: fresh.clone(),
                        name: name.into(),
                        role: role.into(),
                        pane_id: pane.into(),
                        space_id: SpaceId::new(space),
                        reset: None,
                    },
                ),
                None,
                4,
                &config,
            )
            .unwrap();
        let before = state.clone();
        assert!(
            fresh_join(&mut state, &architect, fresh.as_str(), &a, &config).is_err(),
            "old receipt must reject current identity {name}/{role}/{pane}/{space}"
        );
        assert_eq!(
            state, before,
            "rejected receipt must not move, rename or otherwise mutate the current member"
        );
    }
    let mut missing = completed.clone();
    missing.members.remove(&fresh);
    let before = missing.clone();
    assert!(fresh_join(&mut missing, &architect, fresh.as_str(), &a, &config).is_err());
    assert_eq!(missing, before);
    // A disconnected but unchanged identity is the intended lost-reply/restart recovery.
    completed.apply(&leave, Some(&fresh), 3, &config).unwrap();
    fresh_join(&mut completed, &architect, fresh.as_str(), &a, &config).unwrap();
}

#[test]
fn public_progress_redacts_private_tokens_even_if_adapter_copies_them_into_errors() {
    let (mut state, architect, worker, config) = fixture();
    begin(&mut state);
    let a = ticket(&mut state, &architect, &config);
    let b = ticket(&mut state, &worker, &config);
    state
        .apply(
            &request(
                "private-error",
                Command::ResetStatus {
                    plan_id: a.plan_id.clone(),
                    token: a.token.clone(),
                    status: ResetReport::Blocked,
                    text: format!("Adapter copied {} and {} into an error", a.token, b.token),
                },
            ),
            Some(&architect),
            2,
            &config,
        )
        .unwrap();
    let public = serde_json::to_string(&state.snapshot(vec![], None, 2)).unwrap();
    assert!(!public.contains(&a.token));
    assert!(!public.contains(&b.token));
    assert!(public.contains("[private reset token]"));
}

#[test]
fn clearing_hub_todo_binding_does_not_modify_the_attached_user_file() {
    let (mut state, architect, worker, config) = fixture();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("TODO.md");
    let text = "- [ ] User's plan must be preserved\n";
    std::fs::write(&path, text).unwrap();
    state
        .apply(
            &request(
                "attach",
                Command::ConfigureTodo {
                    path: path.to_string_lossy().into_owned(),
                },
            ),
            Some(&worker),
            0,
            &config,
        )
        .unwrap();
    state.disconnect(&architect);
    state.disconnect(&worker);
    begin(&mut state);
    assert!(state.automation[&SpaceId::new("w1")].todo.is_none());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn ordinary_wire_clients_remain_compatible_but_cannot_initiate_reset() {
    let old_join: Request = serde_json::from_value(json!({"id":"legacy","op":"join","version":1,"agent_id":"legacy","name":"legacy","role":"custom","pane_id":"w1:p1","space_id":"w1"})).unwrap();
    assert!(matches!(
        old_join.command,
        Command::Join { reset: None, .. }
    ));
    for op in ["reset_start", "reset_preview", "reset_abandon"] {
        assert!(
            serde_json::from_value::<Request>(json!({"id":"unsafe","op":op,"space_id":"w1"}))
                .is_err()
        );
    }
}

#[test]
fn frozen_source_identities_cannot_move_away_or_reintroduce_old_context() {
    let (mut state, architect, worker, config) = fixture();
    let mut historical = state.members[&worker].clone();
    historical.id = AgentId::new("historical");
    historical.activity = Activity::Offline;
    state.members.insert(historical.id.clone(), historical);
    begin(&mut state);
    state.disconnect(&architect);
    for agent in [&architect, &AgentId::new("historical")] {
        assert!(
            state
                .apply(
                    &request(
                        "move-away",
                        Command::Join {
                            version: VERSION,
                            agent_id: agent.clone(),
                            name: "moved".into(),
                            role: "custom".into(),
                            pane_id: "w2:p1".into(),
                            space_id: SpaceId::new("w2"),
                            reset: None
                        }
                    ),
                    None,
                    2,
                    &config
                )
                .is_err()
        );
    }
    assert!(state.reset_frozen(&SpaceId::new("w1")));
}

#[test]
fn preflight_blocks_nonidle_unsupported_and_offered_clients_and_inventory_changes() {
    let (mut state, architect, worker, config) = setup();
    assert!(state.reset_preview(&SpaceId::new("w1")).is_err());
    capable(&mut state, &architect, &config);
    capable(&mut state, &worker, &config);
    state.members.get_mut(&worker).unwrap().activity = Activity::Working;
    assert!(state.reset_preview(&SpaceId::new("w1")).is_err());
    state.members.get_mut(&worker).unwrap().activity = Activity::Paused;
    assert!(
        state.reset_preview(&SpaceId::new("w1")).is_err(),
        "known human-paused participants must block before others replace sessions"
    );
    state.members.get_mut(&worker).unwrap().activity = Activity::Idle;
    assign(&mut state, &architect, &worker, &config);
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
    assert!(state.reset_preview(&SpaceId::new("w1")).is_err());
    state.messages[0].delivery = Delivery::Pending;
    let preview = state.reset_preview(&SpaceId::new("w1")).unwrap();
    state.members.get_mut(&worker).unwrap().name = "different".into();
    assert!(state.start_reset(&preview).is_err());
    assert!(!state.reset_frozen(&SpaceId::new("w1")));
}

#[test]
fn frozen_space_blocks_cached_and_ordinary_work_but_only_target_sees_its_control() {
    let (mut state, architect, worker, config) = fixture();
    let task = assign(&mut state, &architect, &worker, &config);
    let message = state.messages[0].id.clone();
    begin(&mut state);
    let a = ticket(&mut state, &architect, &config);
    let b = ticket(&mut state, &worker, &config);
    assert_ne!(a.token, b.token);
    assert!(claim(&mut state, &worker, &a, &config).is_err());
    let commands = [
        Command::Assign {
            to: worker.clone(),
            title: "Task".into(),
            text: "Implement and verify scope".into(),
        },
        Command::Send {
            to: worker.clone(),
            text: "Old work".into(),
            wake: true,
        },
        Command::Ack {
            message_id: message.clone(),
        },
        Command::Defer {
            message_id: message,
        },
        Command::Report {
            task_id: task.clone(),
            revision: 1,
            kind: ReportKind::Done,
            text: "Old completion".into(),
        },
        Command::Guide {
            task_id: task.clone(),
            revision: 1,
            text: "Old guide".into(),
        },
        Command::Accept {
            task_id: task.clone(),
            revision: 1,
            text: "Old accept".into(),
        },
        Command::Cancel {
            task_id: task,
            revision: 1,
            text: "Old cancel".into(),
        },
        Command::Mode {
            mode: Mode::Todo,
            enabled: true,
        },
        Command::ConfigureTodo {
            path: "/missing/TODO.md".into(),
        },
        Command::ConfigurePr {
            url: "https://github.com/example/repo/pull/1".into(),
            cwd: "/missing".into(),
        },
    ];
    for command in commands {
        let before = state.clone();
        assert!(
            state
                .apply(&request("assign", command), Some(&architect), 2, &config)
                .is_err()
        );
        assert_eq!(state, before);
    }
    assert!(
        state
            .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, 2)
            .is_err()
    );
    assert!(
        state
            .apply(
                &request(
                    "unrelated",
                    Command::Join {
                        version: VERSION,
                        agent_id: AgentId::new("intruder"),
                        name: "intruder".into(),
                        role: "custom".into(),
                        pane_id: "w1:p3".into(),
                        space_id: SpaceId::new("w1"),
                        reset: None
                    }
                ),
                None,
                2,
                &config
            )
            .is_err()
    );
    let before = state.messages.len();
    state.tick(10, &config).unwrap();
    assert_eq!(state.messages.len(), before);
}

#[test]
fn claim_is_persisted_single_use_and_receipts_validate_all_approved_identity_fields() {
    let (mut state, architect, _, config) = fixture();
    begin(&mut state);
    let receipt = ticket(&mut state, &architect, &config);
    assert!(fresh_join(&mut state, &architect, "fresh", &receipt, &config).is_err());
    assert_eq!(
        claim(&mut state, &architect, &receipt, &config).unwrap(),
        json!({"claimed":true})
    );
    assert!(claim(&mut state, &architect, &receipt, &config).is_err());
    let mut stored: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    stored.recover();
    assert!(stored.reset_frozen(&SpaceId::new("w1")));
    assert_eq!(
        stored.snapshot(vec![], None, 2).resets[&SpaceId::new("w1")].state,
        ResetPhase::Incomplete
    );
    // Reconnect old identity only to inspect/reconcile, never to get a new claim.
    stored
        .apply(
            &request(
                "rejoin",
                Command::Join {
                    version: VERSION,
                    agent_id: architect.clone(),
                    name: "architect".into(),
                    role: "custom".into(),
                    pane_id: "w1:p1".into(),
                    space_id: SpaceId::new("w1"),
                    reset: None,
                },
            ),
            None,
            2,
            &config,
        )
        .unwrap();
    capable(&mut stored, &architect, &config);
    assert!(claim(&mut stored, &architect, &receipt, &config).is_err());
    for (name, role, pane, space) in [
        ("wrong", "custom", "w1:p1", "w1"),
        ("architect", "wrong", "w1:p1", "w1"),
        ("architect", "custom", "w1:p3", "w1"),
        ("architect", "custom", "w1:p1", "w2"),
    ] {
        let before = stored.clone();
        assert!(
            stored
                .apply(
                    &request(
                        "bad-receipt",
                        Command::Join {
                            version: VERSION,
                            agent_id: AgentId::new("fresh"),
                            name: name.into(),
                            role: role.into(),
                            pane_id: pane.into(),
                            space_id: SpaceId::new(space),
                            reset: Some(receipt.clone())
                        }
                    ),
                    None,
                    3,
                    &config
                )
                .is_err()
        );
        assert_eq!(stored, before);
    }
    fresh_join(&mut stored, &architect, "fresh", &receipt, &config).unwrap();
    assert_eq!(
        stored.members[&AgentId::new("fresh")].activity,
        Activity::Paused
    );
    assert!(
        stored.reset_frozen(&SpaceId::new("w1")),
        "other receipt still missing"
    );
}

#[test]
fn abandon_retains_data_disables_replay_and_invalidates_claims_receipts_across_restart() {
    let (mut state, architect, worker, config) = fixture();
    let task = assign(&mut state, &architect, &worker, &config);
    attach_pr(&mut state, &worker);
    begin(&mut state);
    let a = ticket(&mut state, &architect, &config);
    let b = ticket(&mut state, &worker, &config);
    claim(&mut state, &architect, &a, &config).unwrap();
    fresh_join(&mut state, &architect, "fresh", &a, &config).unwrap();
    claim(&mut state, &worker, &b, &config).unwrap();
    state.messages[0].delivery = Delivery::Offered;
    state
        .abandon_reset(&SpaceId::new("w1"), &a.plan_id)
        .unwrap();
    assert!(!state.reset_frozen(&SpaceId::new("w1")));
    assert_eq!(state.messages[0].delivery, Delivery::Uncertain);
    assert_eq!(state.tasks[&task].state, TaskState::Paused);
    assert!(
        state.automation[&SpaceId::new("w1")].pr.is_some(),
        "retain binding for reconciliation"
    );
    assert!(!state.automation[&SpaceId::new("w1")].pr_enabled);
    assert!(state.members.contains_key(&AgentId::new("fresh")));
    assert!(state.retired.contains_key(&worker));
    assert!(fresh_join(&mut state, &worker, "late-fresh", &b, &config).is_err());
    assert!(claim(&mut state, &worker, &b, &config).is_err());
    state.recover();
    assert_eq!(
        state.snapshot(vec![], None, 3).resets[&SpaceId::new("w1")].state,
        ResetPhase::Abandoned
    );
    assert!(!state.reset_frozen(&SpaceId::new("w1")));
    assert!(fresh_join(&mut state, &architect, "fresh", &a, &config).is_err());
    assert!(state.tasks.contains_key(&task));
    let preview = state.reset_preview(&SpaceId::new("w1")).unwrap();
    assert!(preview.participants.is_empty());
    state.start_reset(&preview).unwrap();
    assert!(
        state.tasks.is_empty(),
        "a new, separately confirmed hub-only plan may clear retained history"
    );
}

#[test]
fn zero_connected_participants_clear_only_hub_state_and_generations_never_aba() {
    let (mut state, architect, worker, config) = fixture();
    assign(&mut state, &architect, &worker, &config);
    attach_pr(&mut state, &worker);
    let old_generation = state.automation[&SpaceId::new("w1")].generation;
    state.disconnect(&architect);
    state.disconnect(&worker);
    let preview = state.reset_preview(&SpaceId::new("w1")).unwrap();
    assert_eq!(preview.offline_count, 2);
    assert!(preview.participants.is_empty());
    state.start_reset(&preview).unwrap();
    let summary = &state.snapshot(vec![], None, 1).resets[&SpaceId::new("w1")];
    assert_eq!(summary.state, ResetPhase::Completed);
    assert!(summary.note.contains("retired, not remotely cleared"));
    assert!(state.tasks.is_empty() && state.messages.is_empty());
    assert!(state.automation[&SpaceId::new("w1")].generation > old_generation);
    attach_pr(&mut state, &worker);
    state
        .set_mode(&SpaceId::new("w1"), Mode::Pr, true, 2)
        .unwrap();
    let before = state.clone();
    state
        .pr_result(
            &SpaceId::new("w1"),
            old_generation,
            Ok(pr_observation(true)),
            3,
        )
        .unwrap();
    state
        .todo_result(
            &SpaceId::new("w1"),
            old_generation,
            Err("old TODO job".into()),
            3,
            &config,
        )
        .unwrap();
    assert_eq!(state, before);
    state
        .automation
        .get_mut(&SpaceId::new("w1"))
        .unwrap()
        .generation = u64::MAX;
    assert!(
        state
            .set_mode(&SpaceId::new("w1"), Mode::Todo, true, 3)
            .is_err()
    );
}

#[test]
fn disconnect_and_reported_veto_are_incomplete_not_success_or_a_new_attempt() {
    let (mut state, architect, worker, config) = fixture();
    begin(&mut state);
    let receipt = ticket(&mut state, &architect, &config);
    claim(&mut state, &architect, &receipt, &config).unwrap();
    state
        .apply(
            &request(
                "veto",
                Command::ResetStatus {
                    plan_id: receipt.plan_id.clone(),
                    token: receipt.token.clone(),
                    status: ResetReport::Cancelled,
                    text: "User cancelled the Pi session switch".into(),
                },
            ),
            Some(&architect),
            2,
            &config,
        )
        .unwrap();
    state.disconnect(&worker);
    let snapshot = state.snapshot(vec![], None, 3);
    assert_eq!(
        snapshot.resets[&SpaceId::new("w1")].state,
        ResetPhase::Incomplete
    );
    assert!(
        snapshot.resets[&SpaceId::new("w1")]
            .note
            .contains("dashboard can abandon")
    );
    assert!(state.reset_frozen(&SpaceId::new("w1")));
    assert!(claim(&mut state, &architect, &receipt, &config).is_err());
}
