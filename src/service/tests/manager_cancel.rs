use super::*;

fn member(id: &AgentId, name: &str, manager: bool, activity: Activity) -> Member {
    Member {
        id: id.clone(),
        name: name.into(),
        role: if manager { "architect" } else { "implementor" }.into(),
        pane_id: format!("w1:{name}"),
        space_id: SpaceId::new("w1"),
        manager,
        cwd: None,
        activity,
        last_seen: 1,
        idle_since: 1,
    }
}

async fn start_cancel_fixture(
    pending_prompts: [usize; 3],
) -> (
    tempfile::TempDir,
    Service,
    tokio::task::JoinHandle<()>,
    State,
    AgentId,
    AgentId,
    String,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hub.sqlite");
    let space = SpaceId::new("w1");
    let manager = AgentId::new("manager");
    let owner = AgentId::new("owner");
    let requester_a = AgentId::new("requester-a");
    let requester_b = AgentId::new("requester-b");
    let requester_c = AgentId::new("requester-c");
    let mut state = State::new(space.clone());
    for (agent, name, is_manager, activity) in [
        (&manager, "manager", true, Activity::Idle),
        (&owner, "owner", false, Activity::Working),
        (&requester_a, "requester-a", false, Activity::Idle),
        (&requester_b, "requester-b", false, Activity::Idle),
        (&requester_c, "requester-c", false, Activity::Idle),
    ] {
        state
            .members
            .insert(agent.clone(), member(agent, name, is_manager, activity));
    }

    let tasks = [
        Task {
            id: TaskId::new("task-a"),
            seq: 1,
            space_id: space.clone(),
            requester: requester_a.clone(),
            owner: owner.clone(),
            title: "Holder".into(),
            description: "Active holder".into(),
            state: TaskState::Assigned,
            revision: 1,
            updated_at: 1,
            checkin_at: 0,
            nudges: 0,
            note: String::new(),
        },
        Task {
            id: TaskId::new("task-b"),
            seq: 2,
            space_id: space.clone(),
            requester: requester_b.clone(),
            owner: owner.clone(),
            title: "Queued B".into(),
            description: "Queued work".into(),
            state: TaskState::Queued,
            revision: 1,
            updated_at: 1,
            checkin_at: 0,
            nudges: 0,
            note: String::new(),
        },
        Task {
            id: TaskId::new("task-c"),
            seq: 3,
            space_id: space.clone(),
            requester: requester_c.clone(),
            owner: owner.clone(),
            title: "Queued C".into(),
            description: "Queued work".into(),
            state: TaskState::Queued,
            revision: 1,
            updated_at: 1,
            checkin_at: 0,
            nudges: 0,
            note: String::new(),
        },
    ];
    for task in &tasks {
        state.tasks.insert(task.id.clone(), task.clone());
    }

    let mut prompt_number = 0;
    for (task, prompt_count) in tasks.iter().zip(pending_prompts) {
        for _ in 0..prompt_count {
            state.messages.push(Message {
                id: MessageId::new(format!("selected-prompt-{prompt_number}")),
                space_id: space.clone(),
                from: Some(task.requester.clone()),
                to: owner.clone(),
                task_id: Some(task.id.clone()),
                revision: Some(task.revision),
                kind: MessageKind::Assignment,
                text: "Selected pending wake prompt".into(),
                wake: true,
                delivery: Delivery::Pending,
                created_at: 1,
                pr_ack: None,
            });
            prompt_number += 1;
        }
    }
    while state.messages.len() < 1_000 {
        let index = state.messages.len();
        state.messages.push(Message {
            id: MessageId::new(format!("uncertain-{index}")),
            space_id: space.clone(),
            from: None,
            to: owner.clone(),
            task_id: None,
            revision: None,
            kind: MessageKind::Info,
            text: "Outstanding delivery cannot be pruned".into(),
            wake: false,
            delivery: Delivery::Uncertain,
            created_at: 1,
            pr_ack: None,
        });
    }

    let guard_response = state
        .apply(
            &Request {
                id: "manager-roster".into(),
                command: Command::Sync {
                    activity: Activity::Idle,
                    receive: false,
                },
            },
            Some(&manager),
            2,
            &Scheduling::default(),
        )
        .unwrap();
    let guard = guard_response["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["id"] == owner.to_string())
        .unwrap()["task_queue"]["cancellation_guards"]["all_unfinished"]
        .as_str()
        .unwrap()
        .to_owned();

    let (snapshots, snapshot_rx) = watch::channel(Snapshot::empty(space.clone()));
    let mut actor = Actor::new(
        space.clone(),
        path.clone(),
        Scheduling::default(),
        snapshots,
    )
    .unwrap();
    actor.state = state;
    actor.store.save(&actor.state).unwrap();
    actor.connections.insert(
        1,
        Binding {
            agent: manager.clone(),
            space: space.clone(),
            epoch: 0,
            revoked: false,
        },
    );
    actor.incarnations.insert(manager.clone(), 1);

    let (tx, rx) = mpsc::channel(8);
    let handle = tokio::task::spawn_blocking(move || actor.run(rx));
    let service = Service {
        space_id: space,
        tx,
        snapshots: snapshot_rx,
    };
    let baseline = Store::open(&path, SpaceId::new("w1"))
        .unwrap()
        .load()
        .unwrap();
    (directory, service, handle, baseline, manager, owner, guard)
}

fn cancel_request(guard: &str, text: &str) -> Request {
    Request {
        id: "manager-cancel-capacity".into(),
        command: Command::CancelOwnerTasks {
            owner_id: AgentId::new("owner"),
            scope: CancelScope::AllUnfinished,
            expected_guard: guard.into(),
            text: text.into(),
        },
    }
}

#[tokio::test]
async fn capacity_failure_rolls_back_tasks_notes_prompts_and_idempotency_cache() {
    let (directory, service, handle, baseline, _manager, _owner, guard) =
        start_cancel_fixture([1, 0, 0]).await;
    let response = service
        .request(1, cancel_request(&guard, "User directed redirection."))
        .await
        .unwrap();
    assert!(!response.ok);
    assert!(response.error.unwrap().contains("message capacity reached"));
    assert_eq!(
        Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
            .unwrap()
            .load()
            .unwrap(),
        baseline,
        "failed notification capacity must not persist task notes, prompt dispositions, or cache entries"
    );

    let altered_replay = service
        .request(1, cancel_request(&guard, "A different retry payload."))
        .await
        .unwrap();
    assert!(!altered_replay.ok);
    assert!(
        altered_replay
            .error
            .unwrap()
            .contains("message capacity reached")
    );
    assert_eq!(
        Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
            .unwrap()
            .load()
            .unwrap(),
        baseline
    );
    service.stop().await;
    handle.await.unwrap();
}

#[tokio::test]
async fn all_selected_pending_prompts_are_reclaimed_before_any_notice() {
    // One prompt belongs to the first (holder) task, while four notices are
    // required. The other three reclaimable prompts belong to later queued
    // tasks. Per-task cancel-and-notify would fail after its first notice.
    let (directory, service, handle, _baseline, manager, owner, guard) =
        start_cancel_fixture([1, 1, 2]).await;
    let reason = "界".repeat(1_600);
    let response = service
        .request(1, cancel_request(&guard, &reason))
        .await
        .unwrap();
    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    assert_eq!(result["cancelled_count"], 3);
    assert_eq!(result["actor_id"], manager.to_string());
    assert_eq!(result["notices_queued"], 4);
    assert_eq!(result["notice_status"], "queued_not_delivered");

    let stored = Store::open(&directory.path().join("hub.sqlite"), SpaceId::new("w1"))
        .unwrap()
        .load()
        .unwrap();
    assert!(
        stored
            .tasks
            .values()
            .all(|task| task.state == TaskState::Cancelled)
    );
    assert!(
        stored
            .tasks
            .values()
            .all(|task| { task.note == format!("Manager manager cancelled: {reason}") })
    );
    assert_eq!(stored.messages.len(), 1_000);
    let notices: Vec<_> = stored
        .messages
        .iter()
        .filter(|message| {
            message.from.as_ref() == Some(&manager) && message.kind == MessageKind::Info
        })
        .collect();
    assert_eq!(notices.len(), 4);
    assert!(
        notices
            .iter()
            .all(|message| { message.delivery == Delivery::Pending && !message.wake })
    );
    let owner_notice = notices.iter().find(|message| message.to == owner).unwrap();
    assert_eq!(owner_notice.task_id, Some(TaskId::new("task-a")));
    assert!(
        owner_notice
            .text
            .contains("does not stop tools or external jobs")
    );
    let requester_notices: std::collections::BTreeSet<_> = notices
        .iter()
        .filter(|message| message.to != owner)
        .map(|message| message.to.to_string())
        .collect();
    assert_eq!(requester_notices.len(), 3);
    service.stop().await;
    handle.await.unwrap();
}
