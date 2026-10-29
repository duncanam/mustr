use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::net::UnixListener;

use super::*;

fn fixture() -> (tempfile::TempDir, LaunchDescriptor) {
    let directory = tempfile::Builder::new()
        .prefix("mustr-gw-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = directory.path();
    let manifest = serde_json::from_value(json!({
        "version": 1, "launch_id": uuid::Uuid::new_v4().to_string(), "server_key": "fixture-server",
        "space_id": "fixture-space", "pane_id": "fixture-pane", "tab_id": "fixture-tab",
        "socket": root.join("hub.sock"), "profile": "team", "slot": "architect", "instance": "team/architect",
        "tab_name": "Architect", "role": {"name": "architect", "description": "fixture", "prompt": "fixture", "manager": false},
        "model": "fixture/model", "thinking": "off", "cwd": root,
        "config_dir": root.join("config"), "state_dir": root.join("state"), "binary": root.join("mustr"),
        "descriptor_path": root.join("descriptor.json"), "ready_path": root.join("ready.json"),
        "spec_digest": "fixture", "jailed": true
    })).unwrap();
    (directory, manifest)
}

fn join_request(manifest: &LaunchDescriptor) -> Value {
    json!({"id": "join", "op": "join", "version": 1, "agent_id": "native-session", "name": "architect-fixture",
        "role": manifest.role.name, "pane_id": manifest.pane_id, "space_id": manifest.space_id,
        "cwd": manifest.cwd, "context_modes": ["reuse", "fresh"]})
}

async fn send(stream: &mut TcpStream, value: &Value) {
    stream
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn closed(stream: &mut TcpStream) {
    let result = timeout(Duration::from_secs(2), stream.read(&mut [0]))
        .await
        .unwrap();
    assert!(
        matches!(result, Ok(0)) || result.is_err(),
        "connection should close, not respond with peer data"
    );
}

#[tokio::test]
async fn authentication_and_scope_are_checked_before_connecting_to_the_host_hub() {
    let (_directory, manifest) = fixture();
    let hub = UnixListener::bind(&manifest.socket).unwrap();
    let gateway = Gateway::start(manifest.clone()).await.unwrap();
    for auth in [
        json!({"token": "invalid"}),
        json!({"token": gateway.endpoint.token, "socket": "/tmp/not-allowed"}),
        json!({"op": "join"}),
    ] {
        let mut client = TcpStream::connect(("127.0.0.1", gateway.endpoint.port))
            .await
            .unwrap();
        send(&mut client, &auth).await;
        closed(&mut client).await;
    }
    for (field, value) in [
        ("pane_id", json!("other-pane")),
        ("space_id", json!("other-space")),
        ("role", json!("manager")),
        ("cwd", json!("/")),
        ("reset", json!({"plan_id":"other", "token":"other"})),
    ] {
        let mut client = TcpStream::connect(("127.0.0.1", gateway.endpoint.port))
            .await
            .unwrap();
        send(&mut client, &json!({"token": gateway.endpoint.token})).await;
        let mut request = join_request(&manifest);
        request[field] = value;
        send(&mut client, &request).await;
        closed(&mut client).await;
    }
    assert!(
        timeout(Duration::from_millis(30), hub.accept())
            .await
            .is_err()
    );
    assert!(!manifest.ready_path.exists());
    gateway.shutdown().await;
}

#[tokio::test]
async fn oversized_auth_and_join_frames_are_bounded_and_rejected() {
    let (_directory, manifest) = fixture();
    let hub = UnixListener::bind(&manifest.socket).unwrap();
    let gateway = Gateway::start(manifest).await.unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", gateway.endpoint.port))
        .await
        .unwrap();
    client.write_all(&vec![b'x'; 1025]).await.unwrap();
    closed(&mut client).await;
    let mut client = TcpStream::connect(("127.0.0.1", gateway.endpoint.port))
        .await
        .unwrap();
    send(&mut client, &json!({"token": gateway.endpoint.token})).await;
    let _ = client.write_all(&vec![b'x'; MAX_FRAME + 1]).await;
    closed(&mut client).await;
    assert!(
        timeout(Duration::from_millis(30), hub.accept())
            .await
            .is_err()
    );
    gateway.shutdown().await;
}

async fn connected(gateway: &Gateway, manifest: &LaunchDescriptor) -> BufReader<TcpStream> {
    let mut client = TcpStream::connect(("127.0.0.1", gateway.endpoint.port))
        .await
        .unwrap();
    send(&mut client, &json!({"token": gateway.endpoint.token})).await;
    send(&mut client, &join_request(manifest)).await;
    let mut client = BufReader::new(client);
    let reply: Value =
        serde_json::from_slice(&frame(&mut client, MAX_FRAME).await.unwrap()).unwrap();
    assert_eq!(reply["ok"], true);
    send(
        client.get_mut(),
        &json!({"id":"subscribe", "op":"subscribe"}),
    )
    .await;
    let reply: Value =
        serde_json::from_slice(&frame(&mut client, MAX_FRAME).await.unwrap()).unwrap();
    assert_eq!(reply["ok"], true);
    client
}

#[tokio::test]
async fn readiness_push_reconnect_and_shutdown_preserve_nonreplay_and_single_participation() {
    let (_directory, manifest) = fixture();
    let listener = UnixListener::bind(&manifest.socket).unwrap();
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = seen.clone();
    let hub = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            loop {
                let Ok(bytes) = frame(&mut stream, MAX_FRAME).await else {
                    break;
                };
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                observed.lock().unwrap().push(request.clone());
                if request["op"] == "send" {
                    break;
                } // Applied; reply lost. Never retry it.
                let response = json!({"id":request["id"], "ok":true, "result":{}});
                stream
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                if request["op"] == "subscribe" {
                    stream
                        .get_mut()
                        .write_all(b"{\"event\":\"inbox_ready\"}\n")
                        .await
                        .unwrap();
                }
            }
        }
    });
    let gateway = Gateway::start(manifest.clone()).await.unwrap();
    let mut client = connected(&gateway, &manifest).await;
    let push: Value =
        serde_json::from_slice(&frame(&mut client, MAX_FRAME).await.unwrap()).unwrap();
    assert_eq!(push["event"], "inbox_ready");
    let receipt = std::fs::read_to_string(&manifest.ready_path).unwrap();
    assert!(!receipt.contains(&gateway.endpoint.token));
    assert!(!receipt.contains("hub.sock"));
    assert_eq!(
        serde_json::from_str::<Value>(&receipt).unwrap()["agent_id"],
        "native-session"
    );

    let mut duplicate = TcpStream::connect(("127.0.0.1", gateway.endpoint.port))
        .await
        .unwrap();
    send(&mut duplicate, &json!({"token":gateway.endpoint.token})).await;
    send(&mut duplicate, &join_request(&manifest)).await;
    closed(&mut duplicate).await;
    send(
        client.get_mut(),
        &json!({"id":"mutation", "op":"send", "to":"peer", "text":"once", "wake":false}),
    )
    .await;
    closed(client.get_mut()).await;

    let mut client = connected(&gateway, &manifest).await;
    frame(&mut client, MAX_FRAME).await.unwrap(); // Inbox hint after reconnect.
    assert_eq!(
        std::fs::read_to_string(&manifest.ready_path).unwrap(),
        receipt
    );
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|request| request["op"] == "send")
            .count(),
        1
    );
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|request| request["op"] == "join")
            .count(),
        2
    );
    let port = gateway.endpoint.port;
    gateway.shutdown().await;
    closed(client.get_mut()).await;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    hub.abort();
    let _ = hub.await;
}

#[tokio::test]
async fn host_controls_and_second_join_never_cross_the_gateway() {
    let (_directory, manifest) = fixture();
    for request in [
        json!({"id":"x", "op":"reset_capability"}),
        json!({"id":"x", "op":"reset_claim", "plan_id":"p", "token":"t"}),
        json!({"id":"x", "op":"reset_status", "plan_id":"p", "token":"t", "status":"failed", "text":"x"}),
        json!({"id":"x", "op":"configure_todo", "path":"/private/todo"}),
        json!({"id":"x", "op":"configure_pr", "url":"https://example.invalid", "cwd":"/private"}),
        json!({"id":"x", "op":"mode", "mode":"pr", "enabled":true}),
        join_request(&manifest),
    ] {
        let bytes = format!("{request}\n");
        let mut reader = BufReader::new(bytes.as_bytes());
        let mut writer = Vec::new();
        assert!(forward_requests(&mut reader, &mut writer).await.is_err());
        assert!(writer.is_empty());
    }
}

#[tokio::test]
async fn failed_hub_admission_cannot_create_a_readiness_receipt() {
    let (_directory, manifest) = fixture();
    let listener = UnixListener::bind(&manifest.socket).unwrap();
    let hub = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        frame(&mut stream, MAX_FRAME).await.unwrap();
        stream
            .get_mut()
            .write_all(b"{\"id\":\"join\",\"ok\":false,\"error\":\"identity unavailable\"}\n")
            .await
            .unwrap();
    });
    let gateway = Gateway::start(manifest.clone()).await.unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", gateway.endpoint.port))
        .await
        .unwrap();
    send(&mut client, &json!({"token":gateway.endpoint.token})).await;
    send(&mut client, &join_request(&manifest)).await;
    let mut client = BufReader::new(client);
    let reply: Value =
        serde_json::from_slice(&frame(&mut client, MAX_FRAME).await.unwrap()).unwrap();
    assert_eq!(reply["ok"], false);
    assert!(!manifest.ready_path.exists());
    closed(client.get_mut()).await;
    gateway.shutdown().await;
    hub.await.unwrap();
}
