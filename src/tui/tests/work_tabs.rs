use super::*;

pub(super) fn many_tasks(count: usize) -> Snapshot {
    let mut snapshot = populated();
    let template = snapshot.tasks[0].clone();
    snapshot.tasks = (0..count)
        .map(|index| Task {
            id: TaskId::new(format!("task-{index:03}")),
            title: format!("Task {index:03}"),
            ..template.clone()
        })
        .collect();
    snapshot
}

fn tasks_view(snapshot: &Snapshot) -> View {
    let mut view = selected_view(snapshot);
    assert!(view.navigate(KeyCode::Char('f'), snapshot));
    view
}

fn assert_selected_visible(snapshot: &Snapshot, view: &View, text: &str) {
    let task = snapshot
        .tasks
        .iter()
        .find(|task| Some(&task.id) == view.tasks.selected.as_ref())
        .unwrap();
    let (_, title_row) = position(text, &task.title);
    let (_, marker_row) = position(text, "›");
    assert_eq!(title_row, marker_row, "{text}");
}

#[test]
fn messages_default_and_f_toggle_do_not_steal_existing_controls() {
    let snapshot = populated();
    let mut view = selected_view(&snapshot);
    assert_eq!(view.work_tab, WorkTab::Messages);
    assert!(view.navigate(KeyCode::Char('f'), &snapshot));
    assert_eq!(view.work_tab, WorkTab::Tasks);
    assert!(!view.navigate(KeyCode::Tab, &snapshot));
    assert_eq!(view.space_id, SpaceId::new("w1"));
    assert_eq!(view.work_tab, WorkTab::Tasks);
    assert!(!view.navigate(KeyCode::BackTab, &snapshot));
    assert_eq!(view.space_id, SpaceId::new("w1"));
    for key in ['t', 'c', 'p'] {
        assert!(!view.navigate(KeyCode::Char(key), &snapshot));
    }
    assert!(!view.navigate(KeyCode::Enter, &snapshot));
    assert!(view.navigate(KeyCode::Char('f'), &snapshot));
    assert_eq!(view.work_tab, WorkTab::Messages);
}

#[test]
fn accepted_only_counts_are_space_scoped_and_visible_in_either_tab() {
    let mut snapshot = many_tasks(8);
    for (task, state) in snapshot.tasks.iter_mut().zip([
        TaskState::Assigned,
        TaskState::Active,
        TaskState::Blocked,
        TaskState::Waiting,
        TaskState::Review,
        TaskState::Paused,
        TaskState::Accepted,
        TaskState::Cancelled,
    ]) {
        task.state = state;
    }
    let mut foreign = snapshot.tasks[6].clone();
    foreign.id = TaskId::new("foreign");
    foreign.space_id = SpaceId::new("w2");
    snapshot.tasks.push(foreign);
    let mut view = selected_view(&snapshot);
    for tab in [WorkTab::Messages, WorkTab::Tasks] {
        view.work_tab = tab;
        for (width, rows) in [(35, 12), (40, 16), (56, 34), (120, 30)] {
            let text = screen(&render(&snapshot, &view, width, rows, false));
            assert!(text.contains("Tasks 8"), "{text}");
            assert!(text.contains("8 tasks · 1 accepted"), "{text}");
            assert!(!text.contains("2 accepted"));
        }
    }
    for state in [TaskState::Review, TaskState::Cancelled] {
        snapshot.tasks.last_mut().unwrap().state = state;
        assert!(!view.navigate(KeyCode::Tab, &snapshot));
        let text = screen(&render(&snapshot, &view, 120, 30, false));
        assert!(text.contains("Tasks 8"), "{text}");
        assert!(text.contains("8 tasks · 1 accepted"), "{text}");
    }
}

#[test]
fn tab_toggles_preserve_independent_message_and_task_positions() {
    let mut snapshot = many_tasks(30);
    for index in 0..30 {
        let mut message = snapshot.messages[0].clone();
        message.text = format!("Older message {index}: {}", "wrapped words ".repeat(8));
        snapshot.messages.push(message);
    }
    let mut view = selected_view(&snapshot);
    for _ in 0..8 {
        view.navigate(KeyCode::Char('j'), &snapshot);
    }
    let before = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_eq!(view.scroll, 8);
    view.navigate(KeyCode::Char('f'), &snapshot);
    for _ in 0..12 {
        view.navigate(KeyCode::Down, &snapshot);
    }
    let tasks = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_selected_visible(&snapshot, &view, &tasks);
    let selected = view.tasks.selected.clone();
    let offset = view.tasks.offset;
    assert!(offset > 0);
    assert_eq!(view.scroll, 8);
    view.navigate(KeyCode::Char('f'), &snapshot);
    let after = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_eq!(before, after);
    assert_eq!(view.scroll, 8);
    view.navigate(KeyCode::Up, &snapshot);
    assert_eq!(view.scroll, 7);
    assert_eq!(view.tasks.selected, selected);
    assert_eq!(view.tasks.offset, offset);
    view.navigate(KeyCode::Char('f'), &snapshot);
    render_view(&snapshot, &mut view, 120, 18, false);
    assert_eq!(view.tasks.selected, selected);
    assert_eq!(view.tasks.offset, offset);
    view.navigate(KeyCode::Char('k'), &snapshot);
    assert_eq!(view.tasks.selected, Some(TaskId::new("task-011")));
    assert_eq!(view.scroll, 7);
}

#[test]
fn final_task_is_reachable_in_wide_narrow_and_short_viewports() {
    let snapshot = many_tasks(70);
    for (width, rows) in [(120, 18), (56, 24), (40, 16)] {
        let mut view = tasks_view(&snapshot);
        for index in 0..70 {
            let text = screen(&render_view(&snapshot, &mut view, width, rows, false));
            assert_selected_visible(&snapshot, &view, &text);
            assert_eq!(view.tasks.selected, Some(snapshot.tasks[index].id.clone()));
            view.navigate(KeyCode::Down, &snapshot);
        }
        for _ in 0..100 {
            view.navigate(KeyCode::Char('j'), &snapshot);
        }
        let text = screen(&render_view(&snapshot, &mut view, width, rows, false));
        assert_eq!(view.tasks.index, 69);
        assert_selected_visible(&snapshot, &view, &text);
        assert!(text.contains("–70/70"), "{text}");
        assert!(!text.contains("more tasks"));
        view.navigate(KeyCode::Up, &snapshot);
        let text = screen(&render_view(&snapshot, &mut view, width, rows, false));
        assert_eq!(view.tasks.index, 68);
        assert_selected_visible(&snapshot, &view, &text);
    }
}

#[test]
fn task_identity_survives_reordering_updates_and_accepted_state_changes() {
    let mut snapshot = many_tasks(20);
    let mut view = tasks_view(&snapshot);
    for _ in 0..12 {
        view.navigate(KeyCode::Down, &snapshot);
    }
    render_view(&snapshot, &mut view, 120, 18, false);
    let selected = view.tasks.selected.clone();
    snapshot.tasks.reverse();
    view.reconcile(&snapshot);
    let text = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_eq!(view.tasks.selected, selected);
    assert_eq!(view.tasks.index, 7);
    assert_selected_visible(&snapshot, &view, &text);
    let task = snapshot
        .tasks
        .iter_mut()
        .find(|t| Some(&t.id) == selected.as_ref())
        .unwrap();
    task.state = TaskState::Accepted;
    task.updated_at = 12345;
    snapshot.tasks.rotate_left(9);
    view.reconcile(&snapshot);
    let text = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_eq!(view.tasks.selected, selected);
    assert_selected_visible(&snapshot, &view, &text);
    assert!(text.contains("20 tasks · 1 accepted"));
    let before = snapshot
        .tasks
        .iter()
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    view.navigate(KeyCode::Char('f'), &snapshot);
    view.navigate(KeyCode::Char('f'), &snapshot);
    assert_eq!(
        before,
        snapshot
            .tasks
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn task_removals_choose_nearest_survivor_and_empty_snapshots_reset_browsing() {
    let mut snapshot = many_tasks(20);
    let mut view = tasks_view(&snapshot);
    view.scroll = 4;
    for _ in 0..15 {
        view.navigate(KeyCode::Down, &snapshot);
    }
    render_view(&snapshot, &mut view, 120, 18, false);
    snapshot.tasks.remove(4);
    view.reconcile(&snapshot);
    assert_eq!(view.tasks.selected, Some(TaskId::new("task-015")));
    assert_eq!(view.tasks.index, 14);
    snapshot.tasks.remove(14);
    view.reconcile(&snapshot);
    let text = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_eq!(view.tasks.selected, Some(TaskId::new("task-016")));
    assert_selected_visible(&snapshot, &view, &text);
    snapshot.tasks.truncate(3);
    view.reconcile(&snapshot);
    let text = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_eq!(view.tasks.index, 2);
    assert_eq!(view.tasks.offset, 0);
    assert_selected_visible(&snapshot, &view, &text);
    snapshot.tasks.clear();
    view.reconcile(&snapshot);
    view.navigate(KeyCode::Down, &snapshot);
    let text = screen(&render_view(&snapshot, &mut view, 120, 18, false));
    assert_eq!(view.tasks.selected, None);
    assert_eq!(view.tasks.index, 0);
    assert_eq!(view.tasks.offset, 0);
    assert_eq!(view.scroll, 4);
    assert!(text.contains("No tasks assigned."));
    assert!(text.contains("0 tasks · 0 accepted"));
    snapshot.tasks.push(many_tasks(1).tasks.remove(0));
    view.reconcile(&snapshot);
    assert_eq!(view.tasks.selected, Some(TaskId::new("task-000")));
}

#[test]
fn resize_and_paused_row_height_changes_keep_selection_visible_and_ranges_truthful() {
    let mut snapshot = many_tasks(20);
    let mut view = tasks_view(&snapshot);
    for _ in 0..19 {
        view.navigate(KeyCode::Down, &snapshot);
    }
    render_view(&snapshot, &mut view, 120, 40, false);
    snapshot.tasks[7].state = TaskState::Paused;
    snapshot.tasks[7].note = "Reconcile running jobs before guiding".into();
    for (width, rows) in [(120, 40), (40, 16), (240, 50), (56, 20)] {
        let text = screen(&render_view(&snapshot, &mut view, width, rows, false));
        assert_eq!(view.tasks.selected, Some(TaskId::new("task-019")));
        assert_selected_visible(&snapshot, &view, &text);
        assert!(text.contains("–20/20"), "{text}");
    }
    let text = screen(&render_view(&snapshot, &mut view, 35, 12, false));
    assert!(text.contains("0/20 tasks · 0 accepted"), "{text}");
    assert!(
        !text.contains('›'),
        "no task rows fit at this size:\n{text}"
    );
    assert_eq!(view.tasks.selected, Some(TaskId::new("task-019")));
    view.navigate(KeyCode::Up, &snapshot);
    let text = screen(&render_view(&snapshot, &mut view, 120, 30, false));
    assert_eq!(view.tasks.selected, Some(TaskId::new("task-018")));
    assert_selected_visible(&snapshot, &view, &text);
}

#[test]
fn metadata_changes_preserve_positions_without_retargeting_task_identity() {
    let mut snapshot = many_tasks(20);
    let mut foreign = snapshot.tasks[0].clone();
    foreign.id = TaskId::new("foreign-task");
    foreign.title = "Foreign task".into();
    foreign.space_id = SpaceId::new("w2");
    snapshot.tasks.push(foreign);
    let mut view = tasks_view(&snapshot);
    view.scroll = 7;
    for _ in 0..15 {
        view.navigate(KeyCode::Down, &snapshot);
    }
    render_view(&snapshot, &mut view, 120, 18, false);
    let selected = view.tasks.selected.clone();
    let offset = view.tasks.offset;
    let mut renamed = snapshot.space.clone().unwrap();
    renamed.label = "Renamed project".into();
    for metadata in [
        Some(renamed.clone()),
        None,
        Some(Space {
            id: SpaceId::new("w2"),
            label: "Orchard".into(),
        }),
        Some(renamed),
    ] {
        snapshot.space = metadata;
        view.reconcile(&snapshot);
        assert_eq!(view.space_id, SpaceId::new("w1"));
        assert_eq!(view.work_tab, WorkTab::Tasks);
        assert_eq!(view.scroll, 7);
        assert_eq!(view.tasks.offset, offset);
        assert_eq!(view.tasks.selected, selected);
        let text = screen(&render_view(&snapshot, &mut view, 120, 18, false));
        assert!(!text.contains("Foreign task"), "{text}");
        if view.space(&snapshot).is_some() {
            assert_selected_visible(&snapshot, &view, &text);
            assert!(text.contains("Renamed project · w1"), "{text}");
        } else {
            assert!(text.contains("Bound space w1 is unavailable."), "{text}");
        }
    }
}

#[test]
fn active_work_tab_is_explicit_in_both_color_and_monochrome() {
    let snapshot = populated();
    for (width, rows) in [(35, 12), (56, 34), (120, 30)] {
        for tab in [WorkTab::Messages, WorkTab::Tasks] {
            let mut view = selected_view(&snapshot);
            view.work_tab = tab;
            for color in [false, true] {
                let buffer = render(&snapshot, &view, width, rows, color);
                let text = screen(&buffer);
                let message_label = if text.contains("Messages 2") {
                    "Messages 2"
                } else {
                    "Msgs 2"
                };
                for (label, active) in [
                    (message_label, tab == WorkTab::Messages),
                    ("Tasks 2", tab == WorkTab::Tasks),
                ] {
                    let (x, y) = position(&text, label);
                    let cell = &buffer[(height(x), height(y))];
                    assert!(cell.modifier.contains(Modifier::BOLD));
                    assert_eq!(cell.modifier.contains(Modifier::UNDERLINED), active);
                    assert!(!cell.modifier.contains(Modifier::DIM));
                    assert_eq!(
                        cell.fg,
                        if active && color {
                            Color::Cyan
                        } else {
                            Color::Reset
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn shared_layout_is_bounded_and_counts_survive_empty_populated_short_and_narrow_views() {
    for snapshot in [empty_space(), populated(), many_tasks(57)] {
        for (width, rows) in [
            (35, 12),
            (35, 18),
            (40, 13),
            (56, 34),
            (98, 18),
            (120, 30),
            (240, 50),
        ] {
            for tab in [WorkTab::Messages, WorkTab::Tasks] {
                let mut view = selected_view(&snapshot);
                view.work_tab = tab;
                let buffer = render(&snapshot, &view, width, rows, false);
                let text = screen(&buffer);
                assert!(
                    text.contains(&format!("Tasks {}", snapshot.tasks.len())),
                    "{text}"
                );
                assert!(
                    text.contains(&format!("{} tasks · 0 accepted", snapshot.tasks.len())),
                    "{text}"
                );
                assert_eq!(text.matches('╭').count(), 2, "{text}");
                assert_eq!(text.matches('╮').count(), 2, "{text}");
                assert_eq!(text.matches('╰').count(), 2, "{text}");
                assert_eq!(text.matches('╯').count(), 2, "{text}");
                assert!(
                    text.lines()
                        .nth(usize::from(rows - 2))
                        .unwrap()
                        .contains("q quit"),
                    "{text}"
                );
                assert!(
                    buffer
                        .content()
                        .iter()
                        .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
                );
            }
        }
    }
}

#[test]
fn inert_space_keys_do_not_discard_work_navigation() {
    let snapshot = many_tasks(20);
    let mut view = tasks_view(&snapshot);
    view.scroll = 6;
    for _ in 0..10 {
        view.navigate(KeyCode::Down, &snapshot);
    }
    render_view(&snapshot, &mut view, 120, 18, false);
    let selected = view.tasks.selected.clone();
    let offset = view.tasks.offset;
    for key in [
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Char('h'),
        KeyCode::Char('l'),
    ] {
        assert!(!view.navigate(key, &snapshot));
        assert_eq!(view.tasks.selected, selected);
        assert_eq!(view.tasks.offset, offset);
        assert_eq!(view.scroll, 6);
    }
}
