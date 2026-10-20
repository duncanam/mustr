use super::super::pr_ui::{Action, Dialog, eligible};
use super::*;

#[test]
fn discovery_candidates_are_connected_unpaused_and_have_absolute_checkout_paths() {
    let mut snapshot = populated();
    snapshot.members[1].activity = Activity::Paused;

    let mut offline = snapshot.members[0].clone();
    offline.id = AgentId::new("offline");
    offline.name = "Offline".into();
    offline.activity = Activity::Offline;
    snapshot.members.push(offline);

    let mut missing = snapshot.members[0].clone();
    missing.id = AgentId::new("missing-cwd");
    missing.name = "Missing cwd".into();
    missing.cwd = None;
    snapshot.members.push(missing);

    let mut relative = snapshot.members[0].clone();
    relative.id = AgentId::new("relative-cwd");
    relative.name = "Relative cwd".into();
    relative.cwd = Some(std::path::PathBuf::from("checkout"));
    snapshot.members.push(relative);

    let mut foreign = snapshot.members[0].clone();
    foreign.id = AgentId::new("foreign");
    foreign.name = "Foreign".into();
    foreign.space_id = SpaceId::new("w2");
    snapshot.members.push(foreign);

    let candidates = eligible(&snapshot, &SpaceId::new("w1"));
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].agent_id, AgentId::new("ada"));
    assert_eq!(candidates[0].epoch, snapshot.epoch);
    assert_eq!(
        candidates[0].cwd,
        std::path::PathBuf::from("/workspace/orchard")
    );
}

#[test]
fn checkout_picker_is_keyboard_modal_and_returns_the_explicit_selection() {
    let snapshot = populated();
    let candidates = eligible(&snapshot, &snapshot.space_id);
    assert_eq!(candidates.len(), 2);
    let mut view = selected_view(&snapshot);
    view.pr_dialog = Some(Dialog::choose(candidates));

    let text = screen(&render_view(&snapshot, &mut view, 80, 20, false));
    for expected in [
        "Select PR checkout",
        "Ada · architect · w1:p1",
        "/workspace/orchard",
        "Lin · implementor · w1:p2",
        "Enter discover",
        "Esc cancel",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    let tab = view.work_tab;
    assert!(matches!(
        view.pr_key(KeyCode::Char('f')),
        Some(Action::None)
    ));
    assert_eq!(view.work_tab, tab, "modal keys cannot switch the work tab");
    assert!(matches!(view.pr_key(KeyCode::Down), Some(Action::None)));
    match view.pr_key(KeyCode::Enter) {
        Some(Action::Select(candidate)) => {
            assert_eq!(candidate.agent_id, AgentId::new("lin"));
            assert_eq!(candidate.epoch, snapshot.epoch);
            assert_eq!(
                candidate.cwd,
                std::path::PathBuf::from("/workspace/orchard")
            );
        }
        _ => panic!("Enter selects the highlighted connected checkout"),
    }
}

#[test]
fn shrinking_below_minimum_hides_pr_selection_until_rendered_again() {
    let snapshot = populated();
    let candidates = eligible(&snapshot, &snapshot.space_id);
    let mut view = selected_view(&snapshot);
    view.pr_dialog = Some(Dialog::choose(candidates));

    assert!(matches!(view.pr_key(KeyCode::Enter), Some(Action::None)));
    let wide = screen(&render_view(&snapshot, &mut view, 80, 20, false));
    assert!(wide.contains("Select PR checkout"));
    assert!(matches!(view.pr_key(KeyCode::Down), Some(Action::None)));

    let mut escape_view = view.clone();
    render_view(&snapshot, &mut escape_view, 34, 11, false);
    assert!(matches!(
        escape_view.pr_key(KeyCode::Esc),
        Some(Action::Close)
    ));

    let tiny = screen(&render_view(&snapshot, &mut view, 34, 11, false));
    assert!(tiny.contains("enlarge terminal"));
    assert!(matches!(view.pr_key(KeyCode::Enter), Some(Action::None)));

    let restored = screen(&render_view(&snapshot, &mut view, 80, 20, false));
    assert!(restored.contains("Select PR checkout"));
    match view.pr_key(KeyCode::Enter) {
        Some(Action::Select(candidate)) => {
            assert_eq!(candidate.agent_id, AgentId::new("lin"));
        }
        _ => panic!("the original explicit selection is usable after redraw"),
    }
}

#[test]
fn pr_snapshot_truthfully_renders_pending_resolved_and_failed_discovery() {
    let mut snapshot = populated();
    let view = selected_view(&snapshot);
    let resolved = screen(&render(&snapshot, &view, 120, 30, false));
    assert!(resolved.contains("PR URL: https://github.com/example/project/pull/42"));
    assert!(resolved.contains("PR root: ."));

    let pr = snapshot.automation.pr.as_mut().unwrap();
    pr.url = None;
    pr.error = None;
    snapshot.automation.pr_enabled = true;
    let pending = screen(&render(&snapshot, &view, 120, 30, false));
    assert!(pending.contains("PR discovery: pending"), "{pending}");
    assert!(pending.contains("PR checkout: ."), "{pending}");
    assert!(!pending.contains("PR root:"));
    assert!(!pending.contains("PR poll:"));

    snapshot.automation.pr_enabled = false;
    let off = screen(&render(&snapshot, &view, 120, 30, false));
    assert!(off.contains("PR discovery: off"), "{off}");
    assert!(!off.contains("PR discovery: pending"));

    snapshot.automation.pr.as_mut().unwrap().error =
        Some("gh could not resolve the current branch".into());
    snapshot.automation.pr_enabled = false;
    let failed = screen(&render(&snapshot, &view, 120, 30, false));
    assert!(failed.contains("PR discovery failed"), "{failed}");
    assert!(
        failed.contains("gh could not resolve the current branch"),
        "{failed}"
    );
    assert!(
        failed.contains("! PR: gh could not resolve the current branch"),
        "{failed}"
    );
}

#[test]
fn no_checkout_error_modal_is_actionable_sanitized_and_plain_without_color() {
    let snapshot = empty_space();
    let mut view = selected_view(&snapshot);
    view.pr_dialog = Some(Dialog::error(
        "No connected checkout.\nJoin from the repository first.".into(),
    ));
    let buffer = render_view(&snapshot, &mut view, 56, 18, false);
    let text = screen(&buffer);
    let unwrapped = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "PR discovery unavailable",
        "No connected checkout.",
        "Join from the repository",
        "first.",
        "Correct the reported cause before retrying p.",
        "Enter/Esc: close",
    ] {
        assert!(
            text.contains(expected) || unwrapped.contains(expected),
            "missing {expected:?}:\n{text}"
        );
    }
    assert!(
        buffer
            .content()
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
    );
    assert!(matches!(
        view.pr_key(KeyCode::Char('p')),
        Some(Action::None)
    ));
    assert!(matches!(view.pr_key(KeyCode::Esc), Some(Action::Close)));
}

#[test]
fn narrow_short_picker_keeps_the_final_long_member_visible_and_selectable() {
    let mut snapshot = empty_space();
    for index in 0..10 {
        snapshot.members.push(Member {
            id: AgentId::new(format!("member-{index}")),
            name: if index == 9 {
                "Zed final checkout with an exceptionally long participant name".into()
            } else {
                format!("Member {index} with a long name")
            },
            role: "long-role-name".into(),
            manager: false,
            pane_id: format!("w1:p{index}-with-a-long-pane-identifier"),
            space_id: SpaceId::new("w1"),
            activity: Activity::Idle,
            last_seen: 0,
            idle_since: 0,
            cwd: Some(std::path::PathBuf::from(format!(
                "/workspace/repository-{index}"
            ))),
        });
    }
    let candidates = eligible(&snapshot, &snapshot.space_id);
    let mut view = selected_view(&snapshot);
    view.pr_dialog = Some(Dialog::choose(candidates));
    for _ in 0..9 {
        assert!(matches!(view.pr_key(KeyCode::Down), Some(Action::None)));
    }
    let text = screen(&render_view(&snapshot, &mut view, 40, 12, false));
    assert!(
        text.contains("Zed final"),
        "final candidate title was clipped:\n{text}"
    );
    assert!(
        text.contains("/workspace/repository-9"),
        "final checkout path was clipped:\n{text}"
    );
    match view.pr_key(KeyCode::Enter) {
        Some(Action::Select(candidate)) => {
            assert_eq!(candidate.agent_id, AgentId::new("member-9"))
        }
        _ => panic!("the final visible candidate must remain selectable"),
    }
}
