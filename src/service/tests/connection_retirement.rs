use super::*;

fn request(id: &str, command: Command) -> Request {
    Request {
        id: id.into(),
        command,
    }
}

fn join() -> Request {
    request(
        "join",
        Command::Join {
            version: VERSION,
            agent_id: AgentId::new("worker"),
            name: "worker".into(),
            role: "implementor".into(),
            pane_id: "w1:p1".into(),
            space_id: SpaceId::new("w1"),
            cwd: None,
            reset: None,
        },
    )
}

fn fixture() -> (Actor, tempfile::TempDir, watch::Receiver<Snapshot>) {
    let directory = tempfile::tempdir().unwrap();
    let space = SpaceId::new("w1");
    let (snapshots, receiver) = watch::channel(Snapshot::empty(space.clone()));
    let mut actor = Actor::new(
        space.clone(),
        directory.path().join("hub.sqlite"),
        Scheduling::default(),
        snapshots,
    )
    .unwrap();
    actor
        .state
        .apply_validated_join(&join(), None, now(), &actor.scheduling, false)
        .unwrap();
    actor.connections.insert(
        1,
        Binding {
            agent: AgentId::new("worker"),
            space: space.clone(),
            epoch: 0,
            revoked: false,
        },
    );
    actor.incarnations.insert(AgentId::new("worker"), 1);
    // Lease expiry leaves the socket binding present. A subsequent explicit Join
    // replaces its incarnation, so requests on the still-open old socket are retired.
    actor.state.disconnect(&AgentId::new("worker"));
    actor.store.save(&actor.state).unwrap();
    (actor, directory, receiver)
}

fn spawn(
    mut actor: Actor,
    snapshots: watch::Receiver<Snapshot>,
) -> (Service, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(16);
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

#[tokio::test]
async fn old_incarnation_gets_typed_preexecution_rejection_without_affecting_rejoin() {
    let (actor, directory, snapshots) = fixture();
    let (service, handle) = spawn(actor, snapshots);

    let joined = service.request(2, join()).await.unwrap();
    assert!(joined.ok, "{:?}", joined.error);
    assert!(
        service
            .request(
                2,
                request(
                    "current-sync",
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
    let path = directory.path().join("hub.sqlite");
    let before = Store::open(&path, SpaceId::new("w1"))
        .unwrap()
        .load()
        .unwrap();

    let retired = service
        .request(
            1,
            request(
                "old-sync",
                Command::Sync {
                    activity: Activity::Paused,
                    receive: true,
                },
            ),
        )
        .await
        .unwrap();
    assert!(!retired.ok);
    assert_eq!(retired.error_code.as_deref(), Some("connection_retired"));
    assert_eq!(
        retired.error.as_deref(),
        Some("connection is retired; rejoin explicitly from the correct session")
    );
    assert_eq!(
        Store::open(&path, SpaceId::new("w1"))
            .unwrap()
            .load()
            .unwrap(),
        before,
        "the rejected old request must not mutate or persist actor state"
    );

    // A late disconnect for the obsolete socket must not take the replacement offline.
    service.tx.send(Event::Disconnect(1)).await.unwrap();
    let current = service
        .request(
            2,
            request(
                "current-after-old-disconnect",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
        )
        .await
        .unwrap();
    assert!(current.ok, "{:?}", current.error);

    let unbound = service
        .request(
            99,
            request(
                "unbound-sync",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
        )
        .await
        .unwrap();
    assert!(!unbound.ok);
    assert_eq!(
        unbound.error_code, None,
        "an unbound connection is not retired"
    );

    service.stop().await;
    handle.await.unwrap();
}
