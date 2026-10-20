use super::*;

fn manager_text(snapshot: &Snapshot, width: u16) -> String {
    manager_line(snapshot, width, false)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn manager_status_uses_admitted_metadata_not_role_names() {
    let mut snapshot = populated();
    snapshot.members[0].manager = false;
    assert!(manager_text(&snapshot, 100).starts_with("Manager: none"));
    snapshot.members[1].manager = true;
    snapshot.members[1].role = "custom-coordinator".into();
    assert_eq!(manager_text(&snapshot, 100), "Manager: Lin · working");
    snapshot.members[1].activity = Activity::Paused;
    assert_eq!(manager_text(&snapshot, 100), "Manager: Lin · paused");
    snapshot.members[1].activity = Activity::Idle;
    assert_eq!(manager_text(&snapshot, 100), "Manager: Lin · idle");
}

#[test]
fn manager_status_ignores_offline_and_foreign_members_without_fallback() {
    let mut snapshot = populated();
    snapshot.members[0].activity = Activity::Offline;
    let mut foreign = snapshot.members[1].clone();
    foreign.id = AgentId::new("foreign-manager");
    foreign.name = "Other space".into();
    foreign.space_id = SpaceId::new("w2");
    foreign.manager = true;
    snapshot.members.push(foreign);
    snapshot.automation.checkin_enabled = true;
    let text = manager_text(&snapshot, 100);
    assert!(text.starts_with("Manager: none"), "{text}");
    assert!(!text.contains("Lin"));
    assert!(!text.contains("Other space"));
}

#[test]
fn malformed_multiple_managers_are_not_silently_selected() {
    let mut snapshot = populated();
    snapshot.members[1].manager = true;
    assert_eq!(
        manager_text(&snapshot, 100),
        "Manager: unavailable (multiple managers)"
    );
}

#[test]
fn manager_status_is_sanitized_and_bounded_in_terminal_columns() {
    let mut snapshot = populated();
    snapshot.members[0].name = "\u{1b}[31mAda\nwith a very long 界界 manager name".into();
    for width in [1, 8, 33, 54, 100] {
        let line = manager_line(&snapshot, width, false);
        assert!(line.width() <= usize::from(width));
        let text = manager_text(&snapshot, width);
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\n'));
    }
}

#[test]
fn long_manager_name_cannot_hide_paused_status_at_supported_sizes() {
    let mut snapshot = populated();
    snapshot.members[0].name = "A very long 界界 manager name ".repeat(8);
    snapshot.members[0].activity = Activity::Paused;
    snapshot.automation.checkin_enabled = true;
    for (width, rows) in [(35, 12), (56, 24), (120, 30)] {
        let line = manager_text(&snapshot, width);
        assert!(line.ends_with(" · paused"), "{line}");
        assert!(Line::raw(&line).width() <= usize::from(width));
        let text = screen(&render(
            &snapshot,
            &selected_view(&snapshot),
            width,
            rows,
            false,
        ));
        let rendered = text.lines().find(|line| line.contains("Manager:")).unwrap();
        assert!(rendered.contains(" · paused"), "{text}");
    }
}

#[test]
fn manager_status_remains_visible_with_controls_at_minimum_size() {
    for mut snapshot in [empty_space(), populated()] {
        snapshot.automation.checkin_enabled = true;
        for (width, rows) in [(35, 12), (56, 24), (120, 30)] {
            let text = screen(&render(
                &snapshot,
                &selected_view(&snapshot),
                width,
                rows,
                false,
            ));
            for expected in ["[t]", "[c]", "[p]", "Manager:", "Tasks"] {
                assert!(text.contains(expected), "missing {expected}:\n{text}");
            }
        }
    }
}

#[test]
fn idle_review_and_blocker_rows_identify_the_responsible_requester() {
    let mut snapshot = populated();
    snapshot.members[1].activity = Activity::Idle;
    snapshot.tasks[0].state = TaskState::Review;
    let mut view = selected_view(&snapshot);
    view.work_tab = WorkTab::Tasks;
    let text = screen(&render(&snapshot, &view, 140, 35, false));
    assert!(text.contains("Review by Ada · Lin · a1b2c3d4 r2"), "{text}");
    assert!(text.contains("Ask Lin · Ada · e5f6a7b8 r1"), "{text}");
    assert!(text.contains("2 tasks · 0 accepted"), "{text}");
    assert!(text.contains("manager · architect"), "{text}");
}
