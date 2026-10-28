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

async fn serve_pane_requests(listener: UnixListener, count: usize) {
    serve_pane_requests_as(listener, count, "pi").await;
}

async fn serve_pane_requests_as(listener: UnixListener, count: usize, agent: &str) {
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
                "pane": {"pane_id": pane_id, "workspace_id": "w1", "agent": agent}
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

async fn serve_one_pane_request_as(listener: UnixListener, agent: &str) {
    serve_pane_requests_as(listener, 1, agent).await;
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
                    startup_grant: None,
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
async fn startup_grant_join_acknowledges_only_after_real_socket_admission() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(roles_dir.join("valid.md"), "Valid role.\n").unwrap();
    let space = SpaceId::new("w1");
    let service = Service::start(
        space.clone(),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let issued = service
        .issue_startup_grant(
            LaunchId::new("launch-1".into()),
            "valid".into(),
            "w1:p1".into(),
        )
        .await
        .unwrap();
    let token = serde_json::to_value(&issued.handoff).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let herdr = Herdr::new(herdr_path).unwrap();
    let herdr_task = tokio::spawn(serve_pane_requests(listener, 2));

    let startup = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        1,
        request(
            "startup-join",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("startup-agent"),
                name: "startup-agent".into(),
                role: "valid".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: None,
                startup_grant: Some(issued.handoff),
            },
        ),
    )
    .await;
    assert!(startup.ok, "{}", startup.error.unwrap_or_default());
    let startup_result = startup.result.unwrap();
    assert_eq!(startup_result["startup_grant_protocol"], 1);
    assert!(
        !serde_json::to_string(&startup_result)
            .unwrap()
            .contains(&token)
    );

    let manual = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        2,
        request(
            "manual-join",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("manual-agent"),
                name: "manual-agent".into(),
                role: "valid".into(),
                pane_id: "w1:p2".into(),
                space_id: space.clone(),
                cwd: None,
                reset: None,
                startup_grant: None,
            },
        ),
    )
    .await;
    assert!(manual.ok, "{}", manual.error.unwrap_or_default());
    assert!(
        manual
            .result
            .unwrap()
            .get("startup_grant_protocol")
            .is_none()
    );
    herdr_task.await.unwrap();
    assert_eq!(service.snapshots.borrow().members.len(), 2);
    service.stop().await;
}

#[tokio::test]
async fn startup_grant_is_spent_before_role_resolution_failure() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(
        roles_dir.join("late.md"),
        "+++\ncompose = [\"missing\"]\n+++\nBroken role.\n",
    )
    .unwrap();
    let space = SpaceId::new("w1");
    let service = Service::start(
        space.clone(),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let issued = service
        .issue_startup_grant(
            LaunchId::new("launch-1".into()),
            "late".into(),
            "w1:p9".into(),
        )
        .await
        .unwrap();
    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let herdr = Herdr::new(herdr_path).unwrap();
    let bad_role = |id: &str| {
        request(
            id,
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("late-agent"),
                name: "late-agent".into(),
                role: "late".into(),
                pane_id: "w1:p9".into(),
                space_id: space.clone(),
                cwd: None,
                reset: None,
                startup_grant: Some(issued.handoff.clone()),
            },
        )
    };
    let rejected_role = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        1,
        bad_role("invalid-role"),
    )
    .await;
    assert!(!rejected_role.ok);
    assert!(rejected_role.error.unwrap().contains("late -> missing"));

    fs::write(roles_dir.join("late.md"), "Late role repaired.\n").unwrap();
    let observe_pane = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            serve_one_pane_request(listener),
        )
        .await
        .is_ok()
    });
    let replay = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        2,
        bad_role("repaired-replay"),
    )
    .await;
    assert!(!replay.ok, "a role repair must not revive the spent grant");
    assert!(
        !observe_pane.await.unwrap(),
        "replay must fail before pane lookup"
    );
    assert!(service.snapshots.borrow().members.is_empty());
    service.stop().await;
}

#[tokio::test]
async fn startup_grants_reject_claude_panes_and_retire_the_grant() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(roles_dir.join("valid.md"), "Valid role.\n").unwrap();
    let space = SpaceId::new("w1");
    let service = Service::start(
        space.clone(),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let issued = service
        .issue_startup_grant(
            LaunchId::new("claude-launch".into()),
            "valid".into(),
            "w1:p1".into(),
        )
        .await
        .unwrap();
    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let herdr = Herdr::new(herdr_path).unwrap();
    let herdr_task = tokio::spawn(serve_one_pane_request_as(listener, "claude"));

    let response = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        1,
        request(
            "claude-startup-join",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("claude-agent"),
                name: "claude-agent".into(),
                role: "valid".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: None,
                startup_grant: Some(issued.handoff.clone()),
            },
        ),
    )
    .await;
    assert!(!response.ok, "startup launches are Pi-only");
    assert!(response.result.is_none());
    assert!(
        service
            .consume_startup_grant(issued.handoff, "valid".into(), "w1:p1".into(), space,)
            .await
            .is_err(),
        "the failed Pi-only check must retire the grant"
    );
    herdr_task.await.unwrap();
    assert!(service.snapshots.borrow().members.is_empty());
    service.stop().await;
}

#[tokio::test]
async fn startup_grant_is_spent_before_mixed_reset_join_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let roles_dir = directory.path().join("roles");
    fs::create_dir(&roles_dir).unwrap();
    fs::write(roles_dir.join("valid.md"), "Valid role.\n").unwrap();
    let space = SpaceId::new("w1");
    let service = Service::start(
        space.clone(),
        directory.path().join("hub.sqlite"),
        crate::config::Scheduling::default(),
    )
    .await
    .unwrap();
    let issued = service
        .issue_startup_grant(
            LaunchId::new("mixed-launch".into()),
            "valid".into(),
            "w1:p1".into(),
        )
        .await
        .unwrap();
    let herdr_path = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&herdr_path).unwrap();
    let observe_pane = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            serve_one_pane_request(listener),
        )
        .await
        .is_ok()
    });
    let herdr = Herdr::new(herdr_path).unwrap();
    let response = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        1,
        request(
            "mixed-startup-reset",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("startup-agent"),
                name: "startup-agent".into(),
                role: "valid".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: Some(ResetReceipt {
                    plan_id: "not-active".into(),
                    token: "not-active".into(),
                }),
                startup_grant: Some(issued.handoff.clone()),
            },
        ),
    )
    .await;
    assert!(!response.ok);
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("cannot be combined"))
    );
    assert!(response.result.is_none());
    assert!(
        !observe_pane.await.unwrap(),
        "mixed Join rejects before pane lookup"
    );
    assert!(
        service
            .consume_startup_grant(issued.handoff, "valid".into(), "w1:p1".into(), space,)
            .await
            .is_err(),
        "the recognized grant is spent before the reset-pair rejection"
    );
    assert!(service.snapshots.borrow().members.is_empty());
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
                    startup_grant: None,
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
    let startup = service
        .issue_startup_grant(
            LaunchId::new("mixed-reset-launch".into()),
            "valid".into(),
            "w1:p1".into(),
        )
        .await
        .unwrap();
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
    let mixed = socket_join(
        &service,
        &herdr,
        &space,
        &roles_dir,
        2,
        request(
            "mixed-startup-reset",
            Command::Join {
                version: VERSION,
                agent_id: AgentId::new("new-agent"),
                name: "worker".into(),
                role: "valid".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: None,
                reset: Some(receipt.clone()),
                startup_grant: Some(startup.handoff.clone()),
            },
        ),
    )
    .await;
    assert!(!mixed.ok);
    assert!(mixed.result.is_none());
    assert!(
        service
            .consume_startup_grant(
                startup.handoff,
                "valid".into(),
                "w1:p1".into(),
                space.clone(),
            )
            .await
            .is_err(),
        "reset start invalidates the unconsumed startup grant"
    );

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
                startup_grant: None,
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
                startup_grant: None,
            },
        ),
    )
    .await;
    assert!(retry.ok, "{}", retry.error.unwrap_or_default());
    assert_eq!(retry.result.unwrap()["reset_receipt"], true);
    herdr_task.await.unwrap();
    service.stop().await;
}
