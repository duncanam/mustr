use super::*;

mod availability;

use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;

fn request(id: &str, command: Command) -> Request {
    Request {
        id: id.to_owned(),
        command,
    }
}

async fn socket_join(
    service: &Service,
    herdr: &Herdr,
    bound_space: &SpaceId,
    roles_dir: &std::path::Path,
    connection_id: u64,
    request: Request,
) -> Response {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let herdr = herdr.clone();
    let bound_space = bound_space.clone();
    let service = service.clone();
    let roles_dir = roles_dir.to_path_buf();
    let task = tokio::spawn(async move {
        connection(
            server_stream,
            connection_id,
            &herdr,
            &bound_space,
            &service,
            &roles_dir,
        )
        .await
    });
    let mut client = client_stream;
    client
        .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    drop(reader);
    task.await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn socket_exchange(client: &mut BufReader<UnixStream>, request: Request) -> Response {
    client
        .get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    client.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn serve_pane_requests(listener: UnixListener, count: usize) {
    for _ in 0..count {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let pane_id = request["params"]["caller_pane_id"].as_str().unwrap();
        let response = json!({
            "id": request["id"],
            "result": {
                "pane": {"pane_id": pane_id, "workspace_id": "w1", "agent": "pi"}
            }
        });
        write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
    }
}

async fn serve_one_pane_request(listener: UnixListener) {
    serve_pane_requests(listener, 1).await;
}

#[test]
fn legacy_lock_probe_is_non_creating_and_detects_an_existing_exclusive_lock() {
    let directory = tempfile::tempdir().unwrap();
    let herdr_path = directory.path().join("herdr.sock");
    fs::write(&herdr_path, b"fixture").unwrap();
    let herdr = Herdr::new(herdr_path).unwrap();
    let lock_path = herdr.legacy_hub_socket().unwrap().with_extension("lock");

    reject_running_legacy_hub(&herdr).unwrap();
    assert!(
        !lock_path.exists(),
        "absent legacy locks must not be created"
    );

    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    lock.try_lock().unwrap();
    let error = reject_running_legacy_hub(&herdr).unwrap_err();
    assert!(error.to_string().contains("legacy session-wide mustr hub"));
    drop(lock);
    fs::remove_file(lock_path).unwrap();
}

#[tokio::test]
async fn server_attests_only_the_selected_root_role_as_manager() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(
        roles_dir.join("lead.md"),
        "+++\nmanager = true\n+++\nLead role.\n",
    )
    .unwrap();
    fs::write(
        roles_dir.join("manager-fragment.md"),
        "+++\nmanager = true\n+++\nReusable text only.\n",
    )
    .unwrap();
    fs::write(
        roles_dir.join("composed.md"),
        "+++\ncompose = ['manager-fragment']\n+++\nOrdinary root.\n",
    )
    .unwrap();
    let service = Service::start(
        SpaceId::new("w1"),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let space = SpaceId::new("w1");
    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let herdr = Herdr::new(herdr_path.clone()).unwrap();
    let herdr_task = tokio::spawn(serve_pane_requests(listener, 2));

    for (connection_id, agent, name, pane, role) in [
        (1, "lead-agent", "lead", "w1:p1", "lead"),
        (2, "composed-agent", "composed", "w1:p2", "composed"),
    ] {
        let response = socket_join(
            &service,
            &herdr,
            &space,
            &roles_dir,
            connection_id,
            request(
                &format!("join-{agent}"),
                Command::Join {
                    version: VERSION,
                    agent_id: AgentId::new(agent),
                    name: name.into(),
                    role: role.into(),
                    pane_id: pane.into(),
                    space_id: space.clone(),
                    cwd: None,
                    reset: None,
                },
            ),
        )
        .await;
        assert!(response.ok, "{}", response.error.unwrap_or_default());
    }
    herdr_task.await.unwrap();

    let mut snapshots = service.snapshots.clone();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while snapshots.borrow().members.len() < 2 {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let snapshot = snapshots.borrow().clone();
    let members = snapshot.members;
    assert!(snapshot.automation.checkin_enabled);
    assert!(snapshot.automation.manager_checkin_at.is_some());
    assert!(
        members
            .iter()
            .find(|member| member.id.as_str() == "lead-agent")
            .unwrap()
            .manager
    );
    assert!(
        !members
            .iter()
            .find(|member| member.id.as_str() == "composed-agent")
            .unwrap()
            .manager
    );
    assert_eq!(members.iter().filter(|member| member.manager).count(), 1);
    service.stop().await;
}

#[tokio::test]
async fn obsolete_report_conflict_keeps_the_same_socket_usable_after_a_guide_race() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(roles_dir.join("worker.md"), "Worker role.\n").unwrap();
    let service = Service::start(
        SpaceId::new("w1"),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let space = SpaceId::new("w1");
    let requester = AgentId::new("requester");
    let owner = AgentId::new("owner");
    let requester_join = request(
        "join-requester",
        Command::Join {
            version: VERSION,
            agent_id: requester.clone(),
            name: "requester".into(),
            role: "architect".into(),
            pane_id: "w1:p9".into(),
            space_id: space.clone(),
            cwd: None,
            reset: None,
        },
    );
    assert!(service.join(100, requester_join, true).await.unwrap().ok);
    assert!(
        service
            .request(
                100,
                request(
                    "requester-idle",
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                ),
            )
            .await
            .unwrap()
            .ok
    );

    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let herdr = Herdr::new(herdr_path.clone()).unwrap();
    let herdr_task = tokio::spawn(serve_pane_requests(listener, 5));
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let server_herdr = herdr.clone();
    let server_space = space.clone();
    let server_service = service.clone();
    let server_roles = roles_dir.clone();
    let connection_task = tokio::spawn(async move {
        connection(
            server_stream,
            2,
            &server_herdr,
            &server_space,
            &server_service,
            &server_roles,
        )
        .await
    });
    let mut client = BufReader::new(client_stream);
    let joined = socket_exchange(
        &mut client,
        request(
            "join-owner",
            Command::Join {
                version: VERSION,
                agent_id: owner.clone(),
                name: "worker".into(),
                role: "worker".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: None,
            },
        ),
    )
    .await;
    assert!(joined.ok, "{:?}", joined.error);
    let idle = socket_exchange(
        &mut client,
        request(
            "owner-idle",
            Command::Sync {
                activity: Activity::Idle,
                receive: false,
            },
        ),
    )
    .await;
    assert!(idle.ok, "{:?}", idle.error);

    let assigned = service
        .request(
            100,
            request(
                "assign",
                Command::Assign {
                    to: owner.clone(),
                    title: "Reconcile a stale report".into(),
                    text: "A new Guide may race a staged terminal report.".into(),
                },
            ),
        )
        .await
        .unwrap();
    assert!(assigned.ok, "{:?}", assigned.error);
    let task_id: TaskId =
        serde_json::from_value(assigned.result.unwrap()["task_id"].clone()).unwrap();

    let before_guide = socket_exchange(
        &mut client,
        request(
            "sync-before-guide",
            Command::Sync {
                activity: Activity::Idle,
                receive: false,
            },
        ),
    )
    .await;
    assert!(before_guide.ok, "{:?}", before_guide.error);
    assert_eq!(before_guide.result.unwrap()["tasks"][0]["revision"], 1);

    let guided = service
        .request(
            100,
            request(
                "guide",
                Command::Guide {
                    task_id: task_id.clone(),
                    revision: 1,
                    text: "Advance the task before the old report arrives.".into(),
                },
            ),
        )
        .await
        .unwrap();
    assert!(guided.ok, "{:?}", guided.error);

    let stale_report = socket_exchange(
        &mut client,
        request(
            "stale-report",
            Command::Report {
                task_id: task_id.clone(),
                revision: 1,
                kind: ReportKind::Done,
                text: "Obsolete staged report".into(),
            },
        ),
    )
    .await;
    assert!(!stale_report.ok);
    assert_eq!(stale_report.error_code.as_deref(), Some("task_conflict"));

    let current = socket_exchange(
        &mut client,
        request(
            "sync-after-conflict",
            Command::Sync {
                activity: Activity::Idle,
                receive: false,
            },
        ),
    )
    .await;
    assert!(
        current.ok,
        "the same socket must remain usable: {:?}",
        current.error
    );
    let current_task = current.result.unwrap()["tasks"][0].clone();
    assert_eq!(current_task["id"], serde_json::to_value(&task_id).unwrap());
    assert_eq!(current_task["revision"], 2);
    assert_eq!(
        current_task["state"], "assigned",
        "the rejected report does not pause newer work"
    );

    drop(client);
    connection_task.await.unwrap().unwrap();
    herdr_task.await.unwrap();
    service.stop().await;
}

#[tokio::test]
async fn retired_connection_error_is_typed_on_the_wire_before_actor_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(roles_dir.join("worker.md"), "Worker role.\n").unwrap();
    let service = Service::start(
        SpaceId::new("w1"),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let space = SpaceId::new("w1");
    service
        .tx
        .send(Event::Space(Ok(Space {
            id: space.clone(),
            label: "Fixture".into(),
        })))
        .await
        .unwrap();

    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let herdr = Herdr::new(herdr_path.clone()).unwrap();
    let herdr_task = tokio::spawn(serve_pane_requests(listener, 4));
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    let server_herdr = herdr.clone();
    let server_space = space.clone();
    let server_service = service.clone();
    let server_roles = roles_dir.clone();
    let connection_task = tokio::spawn(async move {
        connection(
            server_stream,
            1,
            &server_herdr,
            &server_space,
            &server_service,
            &server_roles,
        )
        .await
    });
    let mut client = BufReader::new(client_stream);
    let joined = socket_exchange(
        &mut client,
        request(
            "join-worker",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("worker"),
                name: "worker".into(),
                role: "worker".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: None,
            },
        ),
    )
    .await;
    assert!(joined.ok, "{:?}", joined.error);
    assert!(
        socket_exchange(
            &mut client,
            request("reset-capability", Command::ResetCapability)
        )
        .await
        .ok
    );
    assert!(
        socket_exchange(
            &mut client,
            request(
                "idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
        )
        .await
        .ok
    );

    let preview = service.reset_preview().await.unwrap();
    service.reset_start(preview).await.unwrap();
    let mut snapshots = service.snapshots.clone();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while snapshots
            .borrow()
            .reset
            .as_ref()
            .is_none_or(|reset| reset.state != ResetPhase::Prepared)
        {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let plan_id = snapshots.borrow().reset.as_ref().unwrap().plan_id.clone();
    service.reset_abandon(plan_id).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while snapshots
            .borrow()
            .reset
            .as_ref()
            .is_none_or(|reset| reset.state != ResetPhase::Abandoned)
        {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let before = snapshots.borrow().clone();

    let retired = socket_exchange(
        &mut client,
        request(
            "retired-sync",
            Command::Sync {
                activity: Activity::Paused,
                receive: true,
            },
        ),
    )
    .await;
    assert!(!retired.ok);
    assert_eq!(retired.error_code.as_deref(), Some("connection_retired"));
    assert_eq!(
        retired.error.as_deref(),
        Some("connection is retired; rejoin explicitly from the correct session")
    );
    let after = snapshots.borrow().clone();
    assert_eq!(after.members, before.members);
    assert_eq!(after.tasks, before.tasks);
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.automation, before.automation);
    assert_eq!(after.reset, before.reset);
    assert_eq!(after.epoch, before.epoch);

    drop(client);
    connection_task.await.unwrap().unwrap();
    herdr_task.await.unwrap();
    service.stop().await;
}

#[tokio::test]
async fn invalid_reset_receipt_join_does_not_consume_valid_admission() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(roles_dir.join("valid.md"), "Valid role.\n").unwrap();
    fs::write(
        roles_dir.join("invalid.md"),
        "+++\ncompose = [\"missing\"]\n+++\nInvalid role.\n",
    )
    .unwrap();
    let service = Service::start(
        SpaceId::new("w1"),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let old_agent = AgentId::new("old-agent");
    let space = SpaceId::new("w1");
    service
        .request(
            1,
            request(
                "initial-join",
                Command::Join {
                    version: VERSION,
                    agent_id: old_agent.clone(),
                    name: "worker".into(),
                    role: "valid".into(),
                    pane_id: "w1:p1".into(),
                    space_id: space.clone(),
                    cwd: None,
                    reset: None,
                },
            ),
        )
        .await
        .unwrap();
    service
        .request(1, request("capability", Command::ResetCapability))
        .await
        .unwrap();
    service
        .request(
            1,
            request(
                "idle",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
        )
        .await
        .unwrap();
    service
        .tx
        .send(Event::Space(Ok(Space {
            id: space.clone(),
            label: "Fixture".into(),
        })))
        .await
        .unwrap();
    let preview = service.reset_preview().await.unwrap();
    service.reset_start(preview).await.unwrap();
    let reset_sync = service
        .request(
            1,
            request(
                "reset-sync",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
        )
        .await
        .unwrap();
    let reset_result = reset_sync.result.unwrap();
    let control = &reset_result["reset_control"];
    let plan_id = control["plan_id"].as_str().unwrap().to_owned();
    let token = control["token"].as_str().unwrap().to_owned();
    service
        .request(
            1,
            request(
                "claim",
                Command::ResetClaim {
                    plan_id: plan_id.clone(),
                    token: token.clone(),
                },
            ),
        )
        .await
        .unwrap();

    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let herdr_task = tokio::spawn(serve_one_pane_request(listener));
    let herdr = Herdr::new(herdr_path).unwrap();
    let receipt = ResetReceipt { plan_id, token };
    let invalid = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        2,
        request(
            "invalid-replacement",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("new-agent"),
                name: "worker".into(),
                role: "invalid".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: Some(receipt.clone()),
            },
        ),
    )
    .await;
    assert!(!invalid.ok);
    assert!(invalid.error.unwrap().contains("invalid -> missing"));

    // The reset token remains authoritative after role rejection; a valid role
    // can use the same receipt and complete precisely the approved replacement.
    let retry = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        3,
        request(
            "valid-replacement",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("new-agent"),
                name: "worker".into(),
                role: "valid".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: Some(receipt),
            },
        ),
    )
    .await;
    assert!(retry.ok, "{}", retry.error.unwrap_or_default());
    assert_eq!(retry.result.unwrap()["reset_receipt"], true);
    herdr_task.await.unwrap();
    service.stop().await;
}
