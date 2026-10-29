use super::*;
use serde_json::Value;

fn request(id: &str, command: Command) -> Request {
    Request {
        id: id.into(),
        command,
    }
}

fn join(agent: &str, name: &str, pane: &str, reset: Option<ResetReceipt>) -> Request {
    request(
        "join",
        Command::Join {
            version: VERSION,
            agent_id: AgentId::new(agent),
            name: name.into(),
            role: "custom".into(),
            pane_id: pane.into(),
            space_id: SpaceId::new("w1"),
            cwd: None,
            context_modes: vec![],
            reset,
        },
    )
}

fn supported_join(agent: &str, name: &str, pane: &str, reset: Option<ResetReceipt>) -> Request {
    let mut request = join(agent, name, pane, reset);
    if let Command::Join { context_modes, .. } = &mut request.command {
        *context_modes = vec![ContextMode::Reuse, ContextMode::Fresh];
    }
    request
}

fn fixture() -> (Actor, tempfile::TempDir, watch::Receiver<Snapshot>) {
    let directory = tempfile::tempdir().unwrap();
    let space_id = SpaceId::new("w1");
    let (snapshots, receiver) = watch::channel(Snapshot::empty(space_id.clone()));
    let mut actor = Actor::new(
        space_id,
        directory.path().join("hub.sqlite"),
        Scheduling::default(),
        snapshots,
    )
    .unwrap();
    actor.space = Some(Space {
        id: SpaceId::new("w1"),
        label: "One".into(),
    });
    for (connection, agent, pane, manager) in [
        (1, "architect", "w1:p1", true),
        (2, "worker", "w1:p2", false),
    ] {
        let agent_id = AgentId::new(agent);
        actor
            .state
            .apply_validated_join_with_harness(
                &supported_join(agent, agent, pane, None),
                None,
                now(),
                &actor.scheduling,
                manager,
                ClientHarness::Pi,
            )
            .unwrap();
        actor
            .state
            .apply(
                &request(
                    "idle",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                ),
                Some(&agent_id),
                now(),
                &actor.scheduling,
            )
            .unwrap();
        actor
            .state
            .apply(
                &request("capable", Command::ResetCapability),
                Some(&agent_id),
                now(),
                &actor.scheduling,
            )
            .unwrap();
        actor.connections.insert(
            connection,
            Binding {
                agent: agent_id.clone(),
                space: SpaceId::new("w1"),
                epoch: 0,
                revoked: false,
            },
        );
        actor.incarnations.insert(agent_id, connection);
    }
    // Service tests use an explicitly quiet baseline; watchdog tests opt in.
    actor
        .state
        .set_mode(&SpaceId::new("w1"), Mode::Checkin, false, now())
        .unwrap();
    actor.store.save(&actor.state).unwrap();
    (actor, directory, receiver)
}

fn spawn(
    mut actor: Actor,
    snapshots: watch::Receiver<Snapshot>,
) -> (Service, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(128);
    let handle = tokio::task::spawn_blocking(move || actor.run(rx));
    (
        Service {
            space_id: SpaceId::new("w1"),
            tx,
            snapshots,
        },
        handle,
    )
}

async fn poll(service: &Service, connection: u64) -> Value {
    let response = service
        .request(
            connection,
            request(
                "poll",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
        )
        .await
        .unwrap();
    assert!(response.ok, "{:?}", response.error);
    response.result.unwrap()
}

fn receipt(value: &Value) -> ResetReceipt {
    let control = &value["reset_control"];
    ResetReceipt {
        plan_id: control["plan_id"].as_str().unwrap().into(),
        token: control["token"].as_str().unwrap().into(),
    }
}

async fn claim(service: &Service, connection: u64, receipt: &ResetReceipt) -> Response {
    service
        .request(
            connection,
            request(
                "claim",
                Command::ResetClaim {
                    plan_id: receipt.plan_id.clone(),
                    token: receipt.token.clone(),
                },
            ),
        )
        .await
        .unwrap()
}

#[test]
fn preview_requires_existing_space_single_nonce_and_unchanged_counts_content_and_incarnation() {
    let (mut actor, _directory, _snapshots) = fixture();
    assert!(actor.preview(SpaceId::new("missing")).is_err());
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor.space = None;
    assert!(actor.start_reset(preview).is_err());
    actor.space = Some(Space {
        id: SpaceId::new("w1"),
        label: "One".into(),
    });
    let expired = actor.preview(SpaceId::new("w1")).unwrap();
    let current = actor.preview(SpaceId::new("w1")).unwrap();
    assert!(actor.start_reset(expired).is_err());
    actor.incarnations.insert(AgentId::new("worker"), 99);
    assert!(actor.start_reset(current).is_err());
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor
        .state
        .apply(
            &request(
                "message",
                Command::Send {
                    to: AgentId::new("worker"),
                    text: "New history after preview".into(),
                    wake: false,
                },
            ),
            Some(&AgentId::new("architect")),
            now(),
            &actor.scheduling,
        )
        .unwrap();
    assert!(actor.start_reset(preview).is_err());
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor.state.messages[0].text = "Changed content, same counts".into();
    assert!(actor.start_reset(preview).is_err());
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor
        .state
        .set_mode(&SpaceId::new("w1"), Mode::Checkin, true, now())
        .unwrap();
    assert!(actor.start_reset(preview).is_err());
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor.start_reset(preview.clone()).unwrap();
    assert!(actor.start_reset(preview).is_err());
}

#[tokio::test]
async fn actor_rejects_foreign_control_events_and_topology_never_rebinds_identity() {
    let (actor, _directory, snapshots) = fixture();
    let (service, handle) = spawn(actor, snapshots);

    let (reply, response) = oneshot::channel();
    service
        .tx
        .send(Event::Mode {
            space: SpaceId::new("w2"),
            mode: Mode::Checkin,
            enabled: true,
            epoch: 0,
            reply,
        })
        .await
        .unwrap();
    assert!(response.await.unwrap().is_err());

    let (reply, response) = oneshot::channel();
    service
        .tx
        .send(Event::ResetPreview {
            space: SpaceId::new("w2"),
            reply,
        })
        .await
        .unwrap();
    assert!(response.await.unwrap().is_err());

    let (reply, response) = oneshot::channel();
    service
        .tx
        .send(Event::ResetAbandon {
            space: SpaceId::new("w2"),
            plan_id: "foreign".into(),
            reply,
        })
        .await
        .unwrap();
    assert!(response.await.unwrap().is_err());

    let mut observed = service.snapshots.clone();
    let _ = observed.borrow_and_update();
    service
        .tx
        .send(Event::Space(Ok(Space {
            id: SpaceId::new("w2"),
            label: "Wrong space".into(),
        })))
        .await
        .unwrap();
    let observed_snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let snapshot = observed.borrow_and_update().clone();
            if snapshot.space_id == SpaceId::new("w1")
                && snapshot.space.is_none()
                && snapshot
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("another space"))
            {
                break snapshot;
            }
            observed.changed().await.unwrap();
        }
    })
    .await;
    service.stop().await;
    handle.await.unwrap();
    let observed_snapshot =
        observed_snapshot.expect("foreign-space topology result was not published");
    assert_eq!(observed_snapshot.space_id, SpaceId::new("w1"));
    assert!(observed_snapshot.space.is_none());
    assert!(
        observed_snapshot
            .error
            .as_deref()
            .unwrap()
            .contains("another space")
    );
}

#[test]
fn failed_prepare_does_not_advance_fence_publish_plan_or_consume_preview() {
    let (mut actor, directory, snapshots) = fixture();
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    let before = actor.state.clone();
    let stored_before = Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
        .unwrap()
        .load()
        .unwrap();
    actor.store.set_read_only_for_test(true);
    assert!(actor.start_reset(preview.clone()).is_err());
    assert_eq!(actor.state, before);
    assert!(snapshots.borrow().reset.is_none());
    assert_eq!(
        Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
            .unwrap()
            .load()
            .unwrap(),
        stored_before
    );
    assert!(actor.sender(1).is_ok());
    actor.store.set_read_only_for_test(false);
    actor.start_reset(preview).unwrap();
    assert_eq!(actor.state.reset_epoch(&SpaceId::new("w1")), 1);
}

#[tokio::test]
async fn failed_claim_is_not_published_as_success_or_consumed_in_memory() {
    let (mut actor, directory, snapshots) = fixture();
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor.start_reset(preview).unwrap();
    actor.store.set_read_only_for_test(true);
    let (service, handle) = spawn(actor, snapshots);
    let control = poll(&service, 1).await;
    let ticket = receipt(&control);
    let response = claim(&service, 1, &ticket).await;
    assert!(!response.ok);
    let after = poll(&service, 1).await;
    assert_eq!(after["reset_control"]["state"], "pending");
    let stored = Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        stored
            .snapshot(None, None, now())
            .reset
            .unwrap()
            .participants[0]
            .state,
        ResetAgentState::Pending
    );
    service.stop().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn failed_abandon_preserves_freeze_history_and_live_control_binding() {
    let (mut actor, _directory, snapshots) = fixture();
    actor
        .state
        .apply(
            &request(
                "assign",
                Command::Assign {
                    to: AgentId::new("worker"),
                    title: "Retain task".into(),
                    text: "Retain details".into(),
                    context: ContextMode::Reuse,
                },
            ),
            Some(&AgentId::new("architect")),
            now(),
            &actor.scheduling,
        )
        .unwrap();
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor.start_reset(preview).unwrap();
    actor.store.set_read_only_for_test(true);
    let (service, handle) = spawn(actor, snapshots);
    let ticket = receipt(&poll(&service, 1).await);
    assert!(service.reset_abandon(ticket.plan_id).await.is_err());
    let after = poll(&service, 1).await;
    assert_eq!(after["reset_summary"]["state"], "prepared");
    assert_eq!(after["reset_control"]["state"], "pending");
    assert_eq!(service.snapshots.borrow().tasks.len(), 1);
    service.stop().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn old_sockets_cached_requests_and_disconnect_cannot_affect_fresh_replacements() {
    let (mut actor, directory, snapshots) = fixture();
    actor
        .state
        .apply(
            &request(
                "send",
                Command::Send {
                    to: AgentId::new("worker"),
                    text: "Old send".into(),
                    wake: false,
                },
            ),
            Some(&AgentId::new("architect")),
            now(),
            &actor.scheduling,
        )
        .unwrap();
    let (service, handle) = spawn(actor, snapshots);
    let preview = service.reset_preview().await.unwrap();
    service.reset_start(preview).await.unwrap();
    let a = receipt(&poll(&service, 1).await);
    let b = receipt(&poll(&service, 2).await);
    assert!(claim(&service, 1, &a).await.ok);
    assert!(!claim(&service, 1, &a).await.ok);
    let stored = Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        stored
            .snapshot(None, None, now())
            .reset
            .unwrap()
            .participants[0]
            .state,
        ResetAgentState::Claimed
    );
    assert!(
        service
            .request(
                3,
                join("architect-fresh", "architect", "w1:p1", Some(a.clone()))
            )
            .await
            .unwrap()
            .ok
    );
    assert!(
        !service
            .request(
                1,
                request(
                    "send",
                    Command::Send {
                        to: AgentId::new("worker"),
                        text: "Old send".into(),
                        wake: false
                    }
                )
            )
            .await
            .unwrap()
            .ok
    );
    assert!(
        !service
            .request(5, join("architect", "architect", "w1:p1", None))
            .await
            .unwrap()
            .ok
    );
    assert!(claim(&service, 2, &b).await.ok);
    assert!(
        service
            .request(4, join("worker-fresh", "worker", "w1:p2", Some(b.clone())))
            .await
            .unwrap()
            .ok
    );
    service.tx.send(Event::Disconnect(1)).await.unwrap();
    service.tx.send(Event::Disconnect(2)).await.unwrap();
    let fresh = poll(&service, 3).await;
    assert_eq!(fresh["members"].as_array().unwrap().len(), 2);
    assert_eq!(fresh["tasks"], json!([]));
    assert!(fresh["reset_control"].is_null());
    assert!(service.snapshots.borrow().messages.is_empty());
    assert!(
        service
            .request(3, join("architect-fresh", "architect", "w1:p1", Some(a)))
            .await
            .unwrap()
            .ok,
        "same receipt/identity is idempotent, not another session"
    );
    let (reply, response) = oneshot::channel();
    service
        .tx
        .send(Event::Mode {
            space: SpaceId::new("w1"),
            mode: Mode::Todo,
            enabled: true,
            epoch: 0,
            reply,
        })
        .await
        .unwrap();
    assert!(
        response.await.unwrap().is_err(),
        "pre-reset mode action must not re-enable after completion"
    );
    assert!(!service.snapshots.borrow().automation.todo_enabled);
    assert!(
        service.reset_abandon(b.plan_id).await.is_err(),
        "completion wins; abandonment must not undo it"
    );
    service.stop().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn abandon_revokes_old_connections_but_keeps_verified_fresh_connections_and_history() {
    let (mut actor, _directory, snapshots) = fixture();
    actor
        .state
        .apply(
            &request(
                "assign",
                Command::Assign {
                    to: AgentId::new("worker"),
                    title: "Retain".into(),
                    text: "Reconcile me".into(),
                    context: ContextMode::Reuse,
                },
            ),
            Some(&AgentId::new("architect")),
            now(),
            &actor.scheduling,
        )
        .unwrap();
    let (service, handle) = spawn(actor, snapshots);
    let preview = service.reset_preview().await.unwrap();
    service.reset_start(preview).await.unwrap();
    let a = receipt(&poll(&service, 1).await);
    let b = receipt(&poll(&service, 2).await);
    assert!(claim(&service, 1, &a).await.ok);
    assert!(
        service
            .request(3, join("architect-fresh", "architect", "w1:p1", Some(a)))
            .await
            .unwrap()
            .ok
    );
    service.reset_abandon(b.plan_id.clone()).await.unwrap();
    let old_idle = service
        .request(
            2,
            request(
                "old-idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            ),
        )
        .await
        .unwrap();
    assert!(!old_idle.ok);
    assert_eq!(old_idle.error_code.as_deref(), Some("connection_retired"));
    let old_claim = claim(&service, 2, &b).await;
    assert!(!old_claim.ok);
    assert_eq!(old_claim.error_code.as_deref(), Some("connection_retired"));
    let fresh = poll(&service, 3).await;
    assert_eq!(fresh["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(fresh["tasks"][0]["state"], "paused");
    assert_eq!(
        service.snapshots.borrow().reset.as_ref().unwrap().state,
        ResetPhase::Abandoned
    );
    // Unclaimed old identity can explicitly rejoin; a claimed one is retired.
    assert!(
        service
            .request(4, join("worker", "worker", "w1:p2", None))
            .await
            .unwrap()
            .ok
    );
    let claimed_identity = service
        .request(5, join("architect", "architect", "w1:p1", None))
        .await
        .unwrap();
    assert!(!claimed_identity.ok);
    assert_eq!(
        claimed_identity.error_code, None,
        "fresh-Join admission rejection is not a connection-retired request"
    );
    service.stop().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn failed_final_receipt_keeps_history_claim_and_fence_durable() {
    let (mut actor, directory, snapshots) = fixture();
    actor
        .state
        .apply(
            &request(
                "assign",
                Command::Assign {
                    to: AgentId::new("worker"),
                    title: "Must not disappear".into(),
                    text: "Retain until durable final receipt".into(),
                    context: ContextMode::Reuse,
                },
            ),
            Some(&AgentId::new("architect")),
            now(),
            &actor.scheduling,
        )
        .unwrap();
    let preview = actor.preview(SpaceId::new("w1")).unwrap();
    actor.start_reset(preview).unwrap();
    let a = receipt(
        &actor
            .state
            .apply(
                &request(
                    "poll-a",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: true,
                    },
                ),
                Some(&AgentId::new("architect")),
                now(),
                &actor.scheduling,
            )
            .unwrap(),
    );
    let b = receipt(
        &actor
            .state
            .apply(
                &request(
                    "poll-b",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: true,
                    },
                ),
                Some(&AgentId::new("worker")),
                now(),
                &actor.scheduling,
            )
            .unwrap(),
    );
    for (agent, ticket) in [("architect", &a), ("worker", &b)] {
        actor
            .state
            .apply(
                &request(
                    "claim",
                    Command::ResetClaim {
                        plan_id: ticket.plan_id.clone(),
                        token: ticket.token.clone(),
                    },
                ),
                Some(&AgentId::new(agent)),
                now(),
                &actor.scheduling,
            )
            .unwrap();
    }
    actor
        .state
        .apply(
            &join("architect-fresh", "architect", "w1:p1", Some(a)),
            None,
            now(),
            &actor.scheduling,
        )
        .unwrap();
    actor.store.save(&actor.state).unwrap();
    actor.store.set_read_only_for_test(true);
    let (service, handle) = spawn(actor, snapshots);
    let response = service
        .request(4, join("worker-fresh", "worker", "w1:p2", Some(b.clone())))
        .await
        .unwrap();
    assert!(!response.ok);
    let control = poll(&service, 2).await;
    assert_eq!(control["reset_control"]["state"], "claimed");
    assert!(!claim(&service, 2, &b).await.ok);
    let stored = Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(stored.tasks.len(), 1);
    assert!(!stored.members.contains_key(&AgentId::new("worker-fresh")));
    assert!(stored.reset_frozen(&SpaceId::new("w1")));
    service.stop().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn restart_retains_partial_plan_no_claim_retry_and_human_abandon_reopens_safely() {
    let (actor, directory, snapshots) = fixture();
    let (service, handle) = spawn(actor, snapshots);
    let preview = service.reset_preview().await.unwrap();
    service.reset_start(preview).await.unwrap();
    let a = receipt(&poll(&service, 1).await);
    let b = receipt(&poll(&service, 2).await);
    assert!(claim(&service, 1, &a).await.ok);
    service.stop().await;
    handle.await.unwrap();
    let space_id = SpaceId::new("w1");
    let (snapshots, receiver) = watch::channel(Snapshot::empty(space_id.clone()));
    let mut actor = Actor::new(
        space_id,
        directory.path().join("hub.sqlite"),
        Scheduling::default(),
        snapshots,
    )
    .unwrap();
    actor.space = Some(Space {
        id: SpaceId::new("w1"),
        label: "One".into(),
    });
    let (service, handle) = spawn(actor, receiver);
    assert!(
        service
            .request(10, join("architect", "architect", "w1:p1", None))
            .await
            .unwrap()
            .ok
    );
    assert!(
        service
            .request(10, request("cap", Command::ResetCapability))
            .await
            .unwrap()
            .ok
    );
    let control = poll(&service, 10).await;
    assert_eq!(control["reset_summary"]["state"], "incomplete");
    assert_eq!(control["reset_control"]["state"], "uncertain");
    assert!(!claim(&service, 10, &a).await.ok);
    assert!(
        service
            .request(11, join("worker", "worker", "w1:p2", None))
            .await
            .unwrap()
            .ok
    );
    assert!(
        service
            .request(11, request("cap", Command::ResetCapability))
            .await
            .unwrap()
            .ok
    );
    assert_eq!(
        poll(&service, 11).await["reset_control"]["state"],
        "blocked"
    );
    assert!(
        !claim(&service, 11, &b).await.ok,
        "restart requires human recovery even for unclaimed blocked participants"
    );
    service.reset_abandon(a.plan_id.clone()).await.unwrap();
    assert!(
        !service
            .request(12, join("late-fresh", "architect", "w1:p1", Some(a)))
            .await
            .unwrap()
            .ok
    );
    assert!(
        !service
            .request(12, join("architect", "architect", "w1:p1", None))
            .await
            .unwrap()
            .ok
    );
    assert!(
        service
            .request(13, join("manual-fresh", "architect", "w1:p1", None))
            .await
            .unwrap()
            .ok
    );
    service.stop().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn late_disconnect_from_a_previous_normal_incarnation_does_not_offline_rejoin() {
    let (mut actor, _directory, snapshots) = fixture();
    actor.state.disconnect(&AgentId::new("worker"));
    let (service, handle) = spawn(actor, snapshots);
    assert!(
        service
            .request(8, join("worker", "worker", "w1:p2", None))
            .await
            .unwrap()
            .ok
    );
    service.tx.send(Event::Disconnect(2)).await.unwrap();
    let value = poll(&service, 8).await;
    assert_eq!(
        value["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "worker")
            .unwrap()["activity"],
        "idle"
    );
    service.stop().await;
    handle.await.unwrap();
}
