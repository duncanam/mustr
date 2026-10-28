//! Controlled fake-herdr requests exercise the real validation deadline. No
//! sleeps, cached pane identity, live discovery or participant-command replay.

use super::*;

struct PendingHerdr {
    reader: BufReader<UnixStream>,
    request: Value,
}

impl PendingHerdr {
    async fn accept(listener: &UnixListener) -> Self {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "pane.current");
        Self { reader, request }
    }

    async fn result(mut self, result: Value) {
        let response = json!({"id": self.request["id"], "result": result});
        self.reader
            .get_mut()
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn pane(self) {
        let pane_id = self.request["params"]["caller_pane_id"].clone();
        self.result(json!({"pane": {"pane_id": pane_id, "workspace_id": "w1", "agent": "pi"}}))
            .await;
    }
}

async fn exchange(peer: &mut BufReader<UnixStream>, request: Request) -> Response {
    peer.get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    timeout(Duration::from_secs(8), peer.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !line.is_empty(),
        "connection closed instead of returning a response"
    );
    serde_json::from_str(&line).unwrap()
}

struct Fixture {
    _directory: tempfile::TempDir,
    herdr_path: PathBuf,
    listener: UnixListener,
    peer: Option<BufReader<UnixStream>>,
    task: JoinHandle<Result<()>>,
    service: Service,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Also reap the actor on an assertion failure: a detached connection
        // would otherwise retain its channel and a blocking actor indefinitely.
        self.task.abort();
        let (reply, _) = tokio::sync::oneshot::channel();
        let _ = self.service.tx.try_send(Event::Stop(reply));
    }
}

impl Fixture {
    async fn unjoined() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let roles_dir = directory.path().join("roles");
        fs::create_dir(&roles_dir).unwrap();
        fs::write(roles_dir.join("valid.md"), "Fixture worker role.\n").unwrap();
        let service = Service::start(
            SpaceId::new("w1"),
            directory.path().join("hub.sqlite"),
            crate::config::Scheduling::default(),
        )
        .await
        .unwrap();
        let herdr_path = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&herdr_path).unwrap();
        let herdr = Herdr::new(herdr_path.clone()).unwrap();
        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let connection_service = service.clone();
        let task = tokio::spawn(async move {
            let result = connection(
                server_stream,
                1,
                &herdr,
                &SpaceId::new("w1"),
                &connection_service,
                &roles_dir,
            )
            .await;
            // Mirror accept_loop, including the disconnect effects that the
            // timeout regression must avoid on its authenticated connection.
            connection_service
                .tx
                .send(Event::Disconnect(1))
                .await
                .unwrap();
            result
        });
        Self {
            _directory: directory,
            herdr_path,
            listener,
            peer: Some(BufReader::new(client_stream)),
            task,
            service,
        }
    }

    async fn new() -> Self {
        let mut fixture = Self::unjoined().await;
        let joined = fixture
            .valid("join-worker", Self::join("worker", "w1:p1"))
            .await;
        assert!(joined.ok);
        assert!(
            fixture
                .valid(
                    "worker-ready",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: false
                    }
                )
                .await
                .ok
        );
        // Direct actor setup represents a second already-verified connection;
        // only the worker's real server connection is under validation test.
        let joined = fixture
            .service
            .join(
                99,
                request("join-manager", Self::join("manager", "w1:p2")),
                true,
            )
            .await
            .unwrap();
        assert!(joined.ok);
        fixture
    }

    fn join(agent: &str, pane: &str) -> Command {
        Command::Join {
            version: VERSION,
            agent_id: AgentId::new(agent),
            name: agent.into(),
            role: "valid".into(),
            pane_id: pane.into(),
            space_id: SpaceId::new("w1"),
            cwd: None,
            reset: None,
            startup_grant: None,
        }
    }

    async fn valid(&mut self, id: &str, command: Command) -> Response {
        let (response, ()) = tokio::join!(
            exchange(self.peer.as_mut().unwrap(), request(id, command)),
            async { PendingHerdr::accept(&self.listener).await.pane().await }
        );
        response
    }

    async fn unavailable(&mut self, id: &str, command: Command) -> Response {
        let (response, pending) = tokio::join!(
            exchange(self.peer.as_mut().unwrap(), request(id, command)),
            PendingHerdr::accept(&self.listener)
        );
        // Keeping this accepted stream alive withheld the reply until herdr's
        // own deadline elapsed. No arbitrary delay manufactures the timeout.
        drop(pending);
        response
    }

    async fn state_with(&self, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let mut snapshots = self.service.snapshots.clone();
        timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap()
    }

    async fn active_and_offered(&mut self) -> (TaskId, MessageId) {
        let assigned = self
            .service
            .request(
                99,
                request(
                    "assign",
                    Command::Assign {
                        to: AgentId::new("worker"),
                        title: "Retain active work".into(),
                        text: "Synthetic active work; do not pause it on a topology timeout."
                            .into(),
                    },
                ),
            )
            .await
            .unwrap();
        assert!(assigned.ok, "{}", assigned.error.unwrap_or_default());
        let task = TaskId::new(assigned.result.unwrap()["task_id"].as_str().unwrap());
        let assignment = self
            .valid(
                "offer-assignment",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            )
            .await;
        let assignment_id =
            MessageId::new(assignment.result.unwrap()["inbox"]["id"].as_str().unwrap());
        assert!(
            self.valid(
                "ack-assignment",
                Command::Ack {
                    message_id: assignment_id
                }
            )
            .await
            .ok
        );
        assert!(
            self.service
                .request(
                    99,
                    request(
                        "info",
                        Command::Send {
                            to: AgentId::new("worker"),
                            text: "Retain this outstanding offer.".into(),
                            wake: false,
                        }
                    )
                )
                .await
                .unwrap()
                .ok
        );
        let offered = self
            .valid(
                "offer-info",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true,
                },
            )
            .await;
        let message = MessageId::new(offered.result.unwrap()["inbox"]["id"].as_str().unwrap());
        self.state_with(|snapshot| {
            snapshot
                .tasks
                .iter()
                .any(|task| task.state == TaskState::Active)
                && snapshot
                    .messages
                    .iter()
                    .any(|item| item.id == message && item.delivery == Delivery::Offered)
        })
        .await;
        (task, message)
    }

    async fn finish(mut self) {
        drop(self.peer.take());
        timeout(Duration::from_secs(1), &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.service.stop().await;
    }
}

#[tokio::test]
async fn transient_validation_timeouts_reject_unverified_commands_without_disconnect_or_replay() {
    let mut fixture = Fixture::new().await;
    let (task, message) = fixture.active_and_offered().await;
    let before = fixture.service.snapshots.borrow().clone();
    for (id, command) in [
        (
            "timed-sync",
            Command::Sync {
                activity: Activity::Working,
                receive: true,
            },
        ),
        (
            "timed-ack",
            Command::Ack {
                message_id: message.clone(),
            },
        ),
        (
            "timed-report",
            Command::Report {
                task_id: task.clone(),
                revision: 1,
                kind: ReportKind::Done,
                text: "Must not be applied or replayed.".into(),
            },
        ),
    ] {
        let rejected = fixture.unavailable(id, command).await;
        assert!(!rejected.ok);
        assert_eq!(
            serde_json::to_value(&rejected).unwrap()["error_code"],
            "identity_unavailable"
        );
        assert!(rejected.result.is_none());
        assert!(
            !fixture.task.is_finished(),
            "temporary availability is not a disconnect"
        );
        let after = fixture.service.snapshots.borrow().clone();
        assert_eq!(
            after.tasks, before.tasks,
            "no unverified report, task pause or uncertain work"
        );
        assert_eq!(
            after.messages, before.messages,
            "no unverified offer, ACK or uncertain delivery"
        );
        assert_eq!(
            after.members, before.members,
            "unverified activity is not applied"
        );
    }
    assert!(
        fixture
            .valid(
                "recovered-ack",
                Command::Ack {
                    message_id: message.clone()
                }
            )
            .await
            .ok
    );
    let recovered = fixture
        .valid(
            "recovered-sync",
            Command::Sync {
                activity: Activity::Idle,
                receive: false,
            },
        )
        .await;
    assert!(
        recovered.ok,
        "same authenticated connection recovers without Join"
    );
    let after = fixture
        .state_with(|snapshot| {
            snapshot
                .messages
                .iter()
                .any(|item| item.id == message && item.delivery == Delivery::Received)
        })
        .await;
    assert_eq!(
        after.tasks, before.tasks,
        "a rejected mutation was never replayed after recovery"
    );
    assert!(
        after
            .members
            .iter()
            .any(|member| member.manager && member.id.as_str() == "manager")
    );
    assert!(
        after.automation.checkin_enabled,
        "manager default-on state is unchanged"
    );
    fixture.finish().await;
}

#[tokio::test]
async fn actual_identity_changes_and_invalid_topology_still_close_the_connection() {
    for result in [
        json!({"pane": {"pane_id": "w1:p1", "workspace_id": "w2", "agent": "pi"}}),
        json!({"pane": {"pane_id": "w1:p9", "workspace_id": "w1", "agent": "pi"}}),
        json!({"pane": {"pane_id": "w1:p1", "workspace_id": "w1", "agent": "claude"}}),
        json!({"pane": {"pane_id": "w1:p1", "workspace_id": "w1", "agent": null}}),
        json!({"pane": null}),
        json!({"unrecognized": "not a pane"}),
    ] {
        let mut fixture = Fixture::new().await;
        let (task, message) = fixture.active_and_offered().await;
        let (response, ()) = tokio::join!(
            exchange(
                fixture.peer.as_mut().unwrap(),
                request(
                    "identity-changed",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: true
                    }
                )
            ),
            async {
                PendingHerdr::accept(&fixture.listener)
                    .await
                    .result(result.clone())
                    .await
            }
        );
        assert!(!response.ok, "{result}");
        assert!(
            serde_json::to_value(response)
                .unwrap()
                .get("error_code")
                .is_none(),
            "identity disproval is not retryable"
        );
        let after = fixture
            .state_with(|snapshot| {
                snapshot.members.iter().any(|member| {
                    member.id.as_str() == "worker" && member.activity == Activity::Offline
                })
            })
            .await;
        assert_eq!(
            after
                .tasks
                .iter()
                .find(|item| item.id == task)
                .unwrap()
                .state,
            TaskState::Paused
        );
        assert_eq!(
            after
                .messages
                .iter()
                .find(|item| item.id == message)
                .unwrap()
                .delivery,
            Delivery::Uncertain
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn an_empty_transport_reply_can_recover_but_malformed_json_cannot() {
    let mut fixture = Fixture::new().await;
    let (response, ()) = tokio::join!(
        exchange(
            fixture.peer.as_mut().unwrap(),
            request(
                "empty-reply",
                Command::Sync {
                    activity: Activity::Working,
                    receive: true
                }
            )
        ),
        async {
            drop(PendingHerdr::accept(&fixture.listener).await);
        }
    );
    assert_eq!(response.error_code.as_deref(), Some("identity_unavailable"));
    assert!(response.result.is_none());
    assert!(!fixture.task.is_finished());
    assert!(
        fixture
            .valid(
                "after-empty-reply",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false
                }
            )
            .await
            .ok
    );

    let (response, ()) = tokio::join!(
        exchange(
            fixture.peer.as_mut().unwrap(),
            request(
                "malformed",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: true
                }
            )
        ),
        async {
            let mut pending = PendingHerdr::accept(&fixture.listener).await;
            pending
                .reader
                .get_mut()
                .write_all(b"{malformed JSON}\n")
                .await
                .unwrap();
        }
    );
    assert!(!response.ok);
    assert!(response.error_code.is_none());
    fixture
        .state_with(|snapshot| {
            snapshot.members.iter().any(|member| {
                member.id.as_str() == "worker" && member.activity == Activity::Offline
            })
        })
        .await;
    fixture.finish().await;
}

#[tokio::test]
async fn unavailable_initial_join_is_not_mistaken_for_an_authenticated_rejection() {
    let mut fixture = Fixture::unjoined().await;
    // The typed keep-alive contract never covers an unadmitted identity.
    let response = fixture
        .unavailable("unverified-join", Fixture::join("unverified", "w1:p9"))
        .await;
    assert!(!response.ok);
    assert!(response.error_code.is_none());
    assert!(fixture.service.snapshots.borrow().members.is_empty());
    fixture.finish().await;
}

#[tokio::test]
async fn server_replacement_during_an_inflight_valid_response_is_fatal() {
    let mut fixture = Fixture::new().await;
    let path = fixture.herdr_path.clone();
    let (response, replacement) = tokio::join!(
        exchange(
            fixture.peer.as_mut().unwrap(),
            request(
                "replacement",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false
                }
            )
        ),
        async {
            let pending = PendingHerdr::accept(&fixture.listener).await;
            fs::remove_file(&path).unwrap();
            let replacement = UnixListener::bind(&path).unwrap();
            pending.pane().await;
            replacement
        }
    );
    assert!(
        !response.ok,
        "even a valid old response cannot attest a replacement server"
    );
    assert!(
        response
            .error
            .as_deref()
            .unwrap()
            .contains("server changed")
    );
    assert!(
        serde_json::to_value(&response)
            .unwrap()
            .get("error_code")
            .is_none()
    );
    drop(replacement);
    fixture.finish().await;
}
