use super::*;

fn request(id: &str, command: Command) -> Request {
    Request {
        id: id.into(),
        command,
    }
}

#[tokio::test]
async fn stale_report_conflict_is_typed_and_does_not_retire_the_connection() {
    let directory = tempfile::tempdir().unwrap();
    let space = SpaceId::new("w1");
    let service = Service::start(
        space.clone(),
        directory.path().join("hub.sqlite"),
        Scheduling::default(),
    )
    .await
    .unwrap();
    let requester = AgentId::new("requester");
    let owner = AgentId::new("owner");

    let join_request = |agent_id: &AgentId, name: &str, pane_id: &str| {
        request(
            &format!("join-{name}"),
            Command::Join {
                version: VERSION,
                agent_id: agent_id.clone(),
                name: name.into(),
                role: "custom".into(),
                pane_id: pane_id.into(),
                space_id: space.clone(),
                cwd: None,
                context_modes: vec![ContextMode::Reuse, ContextMode::Fresh],
                reset: None,
            },
        )
    };
    assert!(
        service
            .join(
                1,
                join_request(&requester, "requester", "w1:p1"),
                true,
                ClientHarness::Pi,
            )
            .await
            .unwrap()
            .ok
    );
    assert!(
        service
            .join(
                2,
                join_request(&owner, "owner", "w1:p2"),
                false,
                ClientHarness::Pi,
            )
            .await
            .unwrap()
            .ok
    );
    for (connection, id) in [(1, "requester-idle"), (2, "owner-idle")] {
        let response = service
            .request(
                connection,
                request(
                    id,
                    Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                ),
            )
            .await
            .unwrap();
        assert!(response.ok, "{:?}", response.error);
    }

    let assigned = service
        .request(
            1,
            request(
                "assign",
                Command::Assign {
                    to: owner,
                    title: "Check stale report handling".into(),
                    text: "The staged report must not be replayed after guidance.".into(),
                    context: ContextMode::Reuse,
                },
            ),
        )
        .await
        .unwrap();
    assert!(assigned.ok, "{:?}", assigned.error);
    let task_id: TaskId =
        serde_json::from_value(assigned.result.unwrap()["task_id"].clone()).unwrap();

    let guided = service
        .request(
            1,
            request(
                "guide",
                Command::Guide {
                    task_id: task_id.clone(),
                    revision: 1,
                    text: "A newer authoritative task revision".into(),
                },
            ),
        )
        .await
        .unwrap();
    assert!(guided.ok, "{:?}", guided.error);
    assert_eq!(guided.result.unwrap()["revision"], 2);

    let stale_report = service
        .request(
            2,
            request(
                "obsolete-report",
                Command::Report {
                    task_id: task_id.clone(),
                    revision: 1,
                    kind: ReportKind::Done,
                    text: "The old staged report must not execute.".into(),
                },
            ),
        )
        .await
        .unwrap();
    assert!(!stale_report.ok);
    assert_eq!(stale_report.error_code.as_deref(), Some("task_conflict"));
    assert!(
        stale_report
            .error
            .as_deref()
            .unwrap()
            .contains("refresh the roster")
    );

    let unauthorized_report = service
        .request(
            1,
            request(
                "unauthorized-report",
                Command::Report {
                    task_id: task_id.clone(),
                    revision: 1,
                    kind: ReportKind::Done,
                    text: "Staleness does not grant ownership.".into(),
                },
            ),
        )
        .await
        .unwrap();
    assert!(!unauthorized_report.ok);
    assert!(unauthorized_report.error_code.is_none());
    assert!(
        unauthorized_report
            .error
            .as_deref()
            .unwrap()
            .contains("only the owner")
    );

    for (id, command) in [
        (
            "stale-guide",
            Command::Guide {
                task_id: task_id.clone(),
                revision: 1,
                text: "Direct stale guidance remains rejected.".into(),
            },
        ),
        (
            "stale-cancel",
            Command::Cancel {
                task_id: task_id.clone(),
                revision: 1,
                text: "Direct stale cancellation remains rejected.".into(),
            },
        ),
    ] {
        let response = service.request(1, request(id, command)).await.unwrap();
        assert!(!response.ok);
        assert!(
            response.error_code.is_none(),
            "only report conflicts receive the benign code"
        );
        assert!(
            response
                .error
                .as_deref()
                .unwrap()
                .contains("refresh the roster")
        );
    }

    let latest = service
        .request(
            2,
            request(
                "sync-after-conflict",
                Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            ),
        )
        .await
        .unwrap();
    assert!(
        latest.ok,
        "the same actor connection remains usable: {:?}",
        latest.error
    );
    let task = latest.result.unwrap()["tasks"][0].clone();
    assert_eq!(task["id"], serde_json::to_value(task_id).unwrap());
    assert_eq!(task["revision"], 2);
    assert_eq!(
        task["state"], "assigned",
        "a rejected obsolete report does not pause the newer task"
    );

    service.stop().await;
}
