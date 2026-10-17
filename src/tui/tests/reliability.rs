use super::*;

#[test]
fn header_separates_navigation_controls_and_reported_values() {
    for width in [35, 56, 80, 120, 240] {
        let mut geometry = None;
        for flags in 0..8 {
            let mut snapshot = populated();
            let automation = snapshot.automation.get_mut(&SpaceId::new("w1")).unwrap();
            automation.todo_enabled = flags & 1 != 0;
            automation.checkin_enabled = flags & 2 != 0;
            automation.pr_enabled = flags & 4 != 0;
            for color in [false, true] {
                let buffer = render(&snapshot, &selected_view(&snapshot), width, 40, color);
                let text = screen(&buffer);
                let brand = position(&text, "mustr").1;
                let navigation = position(&text, "Spaces").1;
                let modes = position(&text, "Automation").1;
                assert!(navigation >= brand + 2, "{text}");
                assert!(modes >= navigation + 2, "{text}");
                let current = [
                    position(&text, "[t]"),
                    position(&text, "[c]"),
                    position(&text, "[p]"),
                    position(&text, "Participants"),
                ];
                if let Some(expected) = geometry {
                    assert_eq!(current, expected, "toggles must not shift the layout");
                } else {
                    geometry = Some(current);
                }
                for (key, name, on) in [
                    ('t', "TODO", flags & 1 != 0),
                    ('c', "Check-in", flags & 2 != 0),
                    ('p', "PR", flags & 4 != 0),
                ] {
                    let (x, y) = position(&text, &format!("[{key}] {name}: "));
                    let key_cell = &buffer[(height(x), height(y))];
                    let label_cell = &buffer[(height(x + 4), height(y))];
                    let value_cell = &buffer[(height(x + 4 + name.len() + 2), height(y))];
                    assert!(key_cell.modifier.contains(Modifier::DIM));
                    assert!(label_cell.modifier.contains(Modifier::BOLD));
                    assert!(!label_cell.modifier.contains(Modifier::DIM));
                    assert!(
                        !value_cell
                            .modifier
                            .intersects(Modifier::BOLD | Modifier::DIM)
                    );
                    assert_eq!(value_cell.symbol(), if on { "O" } else { "o" });
                    assert_eq!(
                        value_cell.fg,
                        if color && on {
                            Color::Green
                        } else {
                            Color::Reset
                        }
                    );
                }
                let (x, y) = position(&text, "TODO open: 2/5");
                assert!(
                    buffer[(height(x), height(y))]
                        .modifier
                        .contains(Modifier::DIM)
                );
                assert!(
                    !buffer[(height(x + "TODO open: ".len()), height(y))]
                        .modifier
                        .contains(Modifier::DIM)
                );
            }
        }
    }
}

#[test]
fn panels_have_complete_dim_outlines_and_normal_contrast_headings() {
    for (width, rows) in [(100, 24), (120, 30), (240, 50), (56, 34)] {
        for mut snapshot in [empty_space(), populated()] {
            // Long wrapped messages must stay inside their own panel.
            if let Some(message) = snapshot.messages.first_mut() {
                message.text = "Long content wrapping inside its panel. ".repeat(20);
            }
            for color in [false, true] {
                let buffer = render(&snapshot, &selected_view(&snapshot), width, rows, color);
                let text = screen(&buffer);
                let mut panels = 0;
                let assert_border = |x, y, symbol| {
                    let cell = &buffer[(x, y)];
                    assert_eq!(cell.symbol(), symbol, "at ({x}, {y}):\n{text}");
                    assert!(cell.modifier.contains(Modifier::DIM));
                    assert_eq!(cell.fg, Color::Reset);
                    assert_eq!(cell.bg, Color::Reset);
                };
                for y in 0..rows {
                    for x in 0..width {
                        if buffer[(x, y)].symbol() != "╭" {
                            continue;
                        }
                        panels += 1;
                        let right = (x + 1..width)
                            .find(|&col| buffer[(col, y)].symbol() == "╮")
                            .expect("top-right corner");
                        let bottom = (y + 1..rows)
                            .find(|&row| buffer[(x, row)].symbol() == "╰")
                            .expect("bottom-left corner");
                        for (col, row, symbol) in [
                            (x, y, "╭"),
                            (right, y, "╮"),
                            (x, bottom, "╰"),
                            (right, bottom, "╯"),
                        ] {
                            assert_border(col, row, symbol);
                        }
                        for row in y + 1..bottom {
                            assert_border(x, row, "│");
                            assert_border(right, row, "│");
                        }
                        for col in x + 1..right {
                            assert_border(col, bottom, "─");
                        }
                    }
                }
                assert_eq!(panels, 3, "{text}");
                for heading in ["Participants", "Tasks", "Messages"] {
                    let (x, y) = position(&text, heading);
                    let cell = &buffer[(height(x), height(y))];
                    assert!(cell.modifier.contains(Modifier::BOLD));
                    assert!(!cell.modifier.contains(Modifier::DIM));
                }
                assert!(text.contains("/role NAME") || text.contains("Refine dashboard"));
            }
        }
    }
}

#[test]
fn participant_roster_hides_offline_agents_and_restores_reconnected_agents() {
    for (width, rows) in [(140, 40), (56, 34)] {
        let mut snapshot = populated();
        // Isolate participant rows so names cannot match task/message history.
        snapshot.tasks.clear();
        snapshot.messages.clear();
        let view = selected_view(&snapshot);
        snapshot.members[1].activity = Activity::Offline;
        let text = screen(&render(&snapshot, &view, width, rows, false));
        assert!(text.contains("Participants 1"), "{text}");
        assert!(text.contains("Ada"));
        assert!(!text.contains("Lin"));
        assert!(!text.contains("implementor · w1:p2"));
        for activity in [Activity::Idle, Activity::Working, Activity::Paused] {
            snapshot.members[1].activity = activity;
            let text = screen(&render(&snapshot, &view, width, rows, false));
            assert!(text.contains("Participants 2"), "{text}");
            assert!(text.contains("Lin"));
            assert!(text.contains(&label(activity)));
        }
    }
}

#[test]
fn closing_all_agents_keeps_task_recovery_and_named_message_history() {
    let fixture = populated();
    let mut state = crate::hub::State::default();
    state.members = fixture
        .members
        .iter()
        .map(|m| (m.id.clone(), m.clone()))
        .collect();
    state.tasks = fixture
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.clone()))
        .collect();
    state.messages = fixture.messages.clone();
    for member in &fixture.members {
        state.disconnect(&member.id);
    }
    let snapshot = state.snapshot(fixture.spaces.clone(), None, 1);
    let text = screen(&render(
        &snapshot,
        &selected_view(&snapshot),
        160,
        40,
        false,
    ));
    assert!(text.contains("Participants 0"), "{text}");
    assert!(text.contains("No connected participants."));
    assert!(!text.contains("architect · w1:p1"));
    assert!(!text.contains("implementor · w1:p2"));
    assert!(text.contains("paused"));
    assert!(text.contains("Refine dashboard"));
    assert!(text.contains("Ask Ada: reconcile, then guide/cancel"));
    assert!(text.contains("Ada → Lin"));
    assert_eq!(
        snapshot.members.len(),
        2,
        "identity history is retained, not deleted"
    );
    assert_eq!(snapshot.tasks.len(), 2);
    assert_eq!(snapshot.messages.len(), 2);
}

#[test]
fn starts_in_own_space_after_empty_snapshot_and_respects_manual_selection() {
    let mut snapshot = populated();
    let mut view = View::in_space(SpaceId::new("w2"));
    view.reconcile(&Snapshot::default());
    assert!(view.selected.is_none());
    view.reconcile(&snapshot);
    assert_eq!(view.selected, Some(SpaceId::new("w2")));
    view.navigate(KeyCode::Left, &snapshot);
    assert_eq!(view.selected, Some(SpaceId::new("w1")));
    snapshot.spaces.reverse();
    view.reconcile(&snapshot);
    assert_eq!(
        view.selected,
        Some(SpaceId::new("w1")),
        "refresh must not jump home"
    );
    snapshot.spaces.retain(|s| s.id != SpaceId::new("w1"));
    view.reconcile(&snapshot);
    assert_eq!(view.selected, Some(SpaceId::new("w2")));
    snapshot.spaces.clear();
    view.reconcile(&snapshot);
    assert!(view.selected.is_none());
}

#[test]
fn missing_startup_space_falls_back_once_without_future_selection_theft() {
    let mut snapshot = empty_space();
    let mut view = View::in_space(SpaceId::new("w2"));
    view.reconcile(&snapshot);
    assert_eq!(view.selected, Some(SpaceId::new("w1")));
    snapshot.spaces.push(Space {
        id: SpaceId::new("w2"),
        label: "Returned".into(),
    });
    view.reconcile(&snapshot);
    assert_eq!(view.selected, Some(SpaceId::new("w1")));
    view.navigate(KeyCode::Right, &snapshot);
    assert_eq!(view.selected, Some(SpaceId::new("w2")));
}

#[test]
fn paused_work_shows_reason_requester_and_explicit_recovery_not_auto_resume() {
    let mut snapshot = populated();
    snapshot.tasks[0].state = TaskState::Paused;
    snapshot.tasks[0].note = "Participant disconnected. Reconcile external jobs.".into();
    snapshot.tasks[1].state = TaskState::Accepted;
    let mut view = selected_view(&snapshot);
    let text = screen(&render(&snapshot, &view, 140, 40, true));
    for expected in [
        "paused",
        "accepted",
        "Participant disconnected.",
        "Ask Ada: reconcile, then guide/cancel",
        "automation will not resume it",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    let narrow = screen(&render(&snapshot, &view, 40, 16, false));
    assert!(narrow.contains("Paused task: ask requester to guide"));
    view.advance(&snapshot, true);
    let text = screen(&render(&snapshot, &view, 140, 40, true));
    assert!(
        !text.contains("Paused work:"),
        "other spaces must not show this recovery warning"
    );
}

#[test]
fn cancelled_completion_notification_is_not_presented_as_cancelled_task() {
    let mut snapshot = populated();
    snapshot.tasks[0].state = TaskState::Accepted;
    snapshot.messages[0].kind = MessageKind::Done;
    snapshot.messages[0].delivery = Delivery::Cancelled;
    let text = screen(&render(
        &snapshot,
        &selected_view(&snapshot),
        160,
        40,
        false,
    ));
    assert!(text.contains("accepted"));
    assert!(text.contains("done · cancelled notification (not task)"));
}

#[test]
fn previews_mark_truncation_and_handle_unicode() {
    assert_eq!(message_preview("short"), "short");
    assert_eq!(message_preview(&"界".repeat(240)), "界".repeat(240));
    assert_eq!(
        message_preview(&"界".repeat(241)),
        format!("{}… [truncated]", "界".repeat(240))
    );
}

#[test]
fn overscroll_clamps_and_one_up_key_immediately_moves_after_resize() {
    let mut snapshot = populated();
    for index in 0..20 {
        let mut message = snapshot.messages[0].clone();
        message.text = format!("Older message {index}: {}", "界 word ".repeat(40));
        snapshot.messages.push(message);
    }
    let mut view = selected_view(&snapshot);
    view.scroll = u16::MAX;
    for (width, rows) in [(120, 30), (50, 24), (160, 40)] {
        let mut terminal = Terminal::new(TestBackend::new(width, rows)).unwrap();
        terminal
            .draw(|frame| draw(frame, &snapshot, &mut view, false))
            .unwrap();
        assert!(view.scroll < u16::MAX);
        let before = view.scroll;
        assert!(
            screen(terminal.backend().buffer()).contains('界'),
            "last wrapped message must remain visible"
        );
        view.navigate(KeyCode::Up, &snapshot);
        terminal
            .draw(|frame| draw(frame, &snapshot, &mut view, false))
            .unwrap();
        assert_eq!(view.scroll, before - 1);
        view.scroll = u16::MAX;
    }
}
