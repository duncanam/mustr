use super::*;

// Keep fixture inputs explicit so the duplicate-identity cases are easy to audit.
#[allow(clippy::too_many_arguments)]
fn join_participant(
    state: &mut State,
    request_id: &str,
    id: &str,
    name: &str,
    role: &str,
    pane: &str,
    manager: bool,
    scheduling: &Scheduling,
) -> Result<Value> {
    let agent_id = AgentId::new(id);
    state.apply_validated_join_with_harness(
        &request(
            request_id,
            Command::Join {
                version: VERSION,
                agent_id,
                name: name.into(),
                role: role.into(),
                pane_id: pane.into(),
                space_id: SpaceId::new("w1"),
                cwd: None,
                context_modes: vec![ContextMode::Reuse, ContextMode::Fresh],
                reset: None,
            },
        ),
        None,
        0,
        scheduling,
        manager,
        ClientHarness::Pi,
    )
}

fn set_activity(
    state: &mut State,
    request_id: &str,
    id: &AgentId,
    activity: Activity,
    now: u64,
    scheduling: &Scheduling,
) {
    state
        .apply(
            &request(
                request_id,
                Command::Sync {
                    activity,
                    receive: false,
                },
            ),
            Some(id),
            now,
            scheduling,
        )
        .unwrap();
}

#[test]
fn same_non_manager_role_can_join_while_peers_are_paused_idle_and_working() {
    let roles = ["researcher", "reviewer", "implementor", "custom"];
    let scheduling = Scheduling::default();
    let mut state = State::new(SpaceId::new("w1"));
    let mut now = 0;
    let mut next_pane = 1;

    for role in roles {
        let mut ids = Vec::new();
        for index in 0..4 {
            let id = AgentId::new(format!("{role}-{index}"));
            let name = format!("{role}-participant-{index}");
            let pane = format!("w1:p{next_pane}");
            next_pane += 1;
            join_participant(
                &mut state,
                &format!("join-{role}-{index}"),
                id.as_str(),
                &name,
                role,
                &pane,
                false,
                &scheduling,
            )
            .unwrap();
            ids.push(id);

            // The next join happens before changing a newly joined peer, so
            // each role admits peers alongside Paused, Idle, and Working.
            match index {
                1 => set_activity(
                    &mut state,
                    &format!("idle-{role}"),
                    &ids[0],
                    Activity::Idle,
                    now,
                    &scheduling,
                ),
                2 => set_activity(
                    &mut state,
                    &format!("working-{role}"),
                    &ids[1],
                    Activity::Working,
                    now,
                    &scheduling,
                ),
                _ => {}
            }
            now += 1;
        }
    }

    let roster = state.snapshot(None, None, now);
    assert_eq!(roster.members.len(), roles.len() * 4);
    for role in roles {
        let peers: Vec<_> = roster
            .members
            .iter()
            .filter(|member| member.role == role)
            .collect();
        assert_eq!(peers.len(), 4, "all {role} peers remain in the roster");
        assert!(peers.iter().all(|member| !member.manager));
        assert!(
            peers
                .iter()
                .any(|member| member.activity == Activity::Paused)
        );
        assert!(peers.iter().any(|member| member.activity == Activity::Idle));
        assert!(
            peers
                .iter()
                .any(|member| member.activity == Activity::Working)
        );
    }
}

#[test]
fn trusted_manager_metadata_is_singleton_and_join_identity_guards_remain() {
    let scheduling = Scheduling::default();
    let mut state = State::new(SpaceId::new("w1"));

    // This bool represents manager metadata resolved by the server from the
    // selected root role, not a client-supplied Join field or role-name rule.
    join_participant(
        &mut state,
        "join-first-manager",
        "manager-one-id",
        "research-coordinator",
        "research-coordinator",
        "w1:p1",
        true,
        &scheduling,
    )
    .unwrap();

    let before_second_manager = state.clone();
    let second_manager = join_participant(
        &mut state,
        "join-second-manager",
        "manager-two-id",
        "different-manager-name",
        "composed-manager-root",
        "w1:p4",
        true,
        &scheduling,
    )
    .unwrap_err();
    assert!(
        second_manager
            .to_string()
            .contains("another manager is already admitted")
    );
    assert_eq!(
        state, before_second_manager,
        "rejected manager Join is inert"
    );

    // A custom role whose name mentions manager is not privileged when the
    // server-resolved root metadata says false; distinct participants may use it.
    join_participant(
        &mut state,
        "join-custom-one",
        "custom-one-id",
        "custom-manager-one",
        "manager-custom",
        "w1:p2",
        false,
        &scheduling,
    )
    .unwrap();
    join_participant(
        &mut state,
        "join-custom-two",
        "custom-two-id",
        "custom-manager-two",
        "manager-custom",
        "w1:p3",
        false,
        &scheduling,
    )
    .unwrap();

    for (request_id, id, name, role, pane, expected) in [
        (
            "duplicate-agent",
            "custom-one-id",
            "new-custom-name",
            "manager-custom",
            "w1:p5",
            "this participant is already connected",
        ),
        (
            "duplicate-pane",
            "new-pane-id",
            "new-pane-name",
            "another-custom",
            "w1:p2",
            "another participant already occupies this pane",
        ),
        (
            "duplicate-name",
            "new-name-id",
            "custom-manager-one",
            "another-custom",
            "w1:p5",
            "name already in use in this space",
        ),
    ] {
        let before = state.clone();
        let error = join_participant(
            &mut state,
            request_id,
            id,
            name,
            role,
            pane,
            false,
            &scheduling,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "{request_id}: {error}"
        );
        assert_eq!(state, before, "rejected {request_id} Join is inert");
    }

    let roster = state.snapshot(None, None, 0);
    assert_eq!(roster.members.len(), 3);
    assert_eq!(
        roster
            .members
            .iter()
            .filter(|member| member.manager)
            .count(),
        1
    );
    assert_eq!(
        roster
            .members
            .iter()
            .filter(|member| member.role == "manager-custom")
            .count(),
        2
    );
}
