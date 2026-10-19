use super::*;

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

async fn serve_one_pane_request(listener: UnixListener) {
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
