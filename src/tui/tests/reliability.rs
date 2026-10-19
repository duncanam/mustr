use super::*;

#[test]
fn header_separates_fixed_identity_controls_and_reported_values() {
    for width in [35, 56, 80, 120, 240] {
        let mut geometry = None;
        for flags in 0..8 {
            let mut snapshot = populated();
            let automation = &mut snapshot.automation;
            automation.todo_enabled = flags & 1 != 0;
            automation.checkin_enabled = flags & 2 != 0;
            automation.pr_enabled = flags & 4 != 0;
            for color in [false, true] {
                let buffer = render(&snapshot, &selected_view(&snapshot), width, 40, color);
                let text = screen(&buffer);
                let brand = position(&text, "mustr").1;
                let navigation = position(&text, "Space").1;
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
            for (color, tab) in [
                (false, WorkTab::Messages),
                (true, WorkTab::Messages),
                (false, WorkTab::Tasks),
                (true, WorkTab::Tasks),
            ] {
                let mut view = selected_view(&snapshot);
                view.work_tab = tab;
                let buffer = render(&snapshot, &view, width, rows, color);
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
                            // The shared pane's bottom border carries counts/range.
                            if buffer[(col, bottom)].symbol() == "─" {
                                assert_border(col, bottom, "─");
                            }
                        }
                    }
                }
                assert_eq!(panels, 2, "{text}");
                for heading in ["Participants", "Tasks", "Messages"] {
                    let (x, y) = position(&text, heading);
                    let cell = &buffer[(height(x), height(y))];
                    assert!(cell.modifier.contains(Modifier::BOLD));
                    assert!(!cell.modifier.contains(Modifier::DIM));
                }
                assert!(text.contains("/role NAME") || text.contains("Ada"));
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
    let mut state = crate::hub::State::new(SpaceId::new("w1"));
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
    let snapshot = state.snapshot(fixture.space.clone(), None, 1);
    let mut view = selected_view(&snapshot);
    let messages = screen(&render(&snapshot, &view, 160, 40, false));
    view.navigate(KeyCode::Char('f'), &snapshot);
    let text = screen(&render(&snapshot, &view, 160, 40, false));
    assert!(text.contains("Participants 0"), "{text}");
    assert!(text.contains("No connected participants."));
    assert!(!text.contains("architect · w1:p1"));
    assert!(!text.contains("implementor · w1:p2"));
    assert!(text.contains("paused"));
    assert!(text.contains("Refine dashboard"));
    assert!(text.contains("Ask Ada: reconcile, then guide/cancel"));
    assert!(messages.contains("Ada → Lin"));
    assert_eq!(
        snapshot.members.len(),
        2,
        "identity history is retained, not deleted"
    );
    assert_eq!(snapshot.tasks.len(), 2);
    assert_eq!(snapshot.messages.len(), 2);
}

#[test]
fn bound_identity_survives_initial_missing_metadata_rename_and_disappearance() {
    let mut snapshot = populated();
    let mut view = View::in_space(SpaceId::new("w1"));
    let unavailable = Snapshot::empty(SpaceId::new("w1"));
    view.reconcile(&unavailable);
    assert_eq!(view.space_id, SpaceId::new("w1"));
    let text = screen(&render(&unavailable, &view, 120, 30, false));
    assert!(text.contains("Bound space w1 is unavailable."), "{text}");
    view.reconcile(&snapshot);
    snapshot.space.as_mut().unwrap().label = "Renamed project".into();
    view.reconcile(&snapshot);
    let text = screen(&render(&snapshot, &view, 120, 30, false));
    assert!(text.contains("Renamed project · w1"), "{text}");
    assert!(!text.contains("Orchard"));
    snapshot.space = None;
    view.reconcile(&snapshot);
    let text = screen(&render(&snapshot, &view, 120, 30, false));
    assert_eq!(view.space_id, SpaceId::new("w1"));
    assert!(text.contains("w1 · unavailable"), "{text}");
    assert!(text.contains("This dashboard never switches spaces."));
    // A different space reusing the old label is not a replacement for w1.
    snapshot.space = Some(Space {
        id: SpaceId::new("w2"),
        label: "Orchard".into(),
    });
    view.reconcile(&snapshot);
    let text = screen(&render(&snapshot, &view, 120, 30, false));
    assert_eq!(view.space_id, SpaceId::new("w1"));
    assert!(text.contains("Bound space w1 is unavailable."), "{text}");
    assert!(!text.contains("Orchard"));
    assert!(!text.contains("w2"));
}

#[test]
fn missing_bound_space_never_falls_back_and_same_identity_can_return() {
    let mut snapshot = empty_space();
    snapshot.space_id = SpaceId::new("w2");
    let mut view = View::in_space(SpaceId::new("w2"));
    view.reconcile(&snapshot);
    let text = screen(&render(&snapshot, &view, 120, 30, false));
    assert_eq!(view.space_id, SpaceId::new("w2"));
    assert!(text.contains("Bound space w2 is unavailable."), "{text}");
    assert!(!text.contains("Orchard"));
    for key in [
        KeyCode::Tab,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Char('h'),
        KeyCode::Char('l'),
    ] {
        assert!(!view.navigate(key, &snapshot));
    }
    snapshot.space = Some(Space {
        id: SpaceId::new("w2"),
        label: "Returned".into(),
    });
    view.reconcile(&snapshot);
    let text = screen(&render(&snapshot, &view, 120, 30, false));
    assert_eq!(view.space_id, SpaceId::new("w2"));
    assert!(text.contains("Returned · w2"), "{text}");
    assert!(!text.contains("unavailable"));
}

#[test]
fn paused_work_shows_reason_requester_and_explicit_recovery_not_auto_resume() {
    let mut snapshot = populated();
    snapshot.tasks[0].state = TaskState::Paused;
    snapshot.tasks[0].note = "Participant disconnected. Reconcile external jobs.".into();
    snapshot.tasks[1].state = TaskState::Accepted;
    let mut view = selected_view(&snapshot);
    view.navigate(KeyCode::Char('f'), &snapshot);
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
    snapshot.tasks[0].space_id = SpaceId::new("w2");
    let text = screen(&render(&snapshot, &view, 140, 40, true));
    assert!(
        !text.contains("Paused work:"),
        "foreign tasks must not show this recovery warning"
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
    assert!(text.contains("2 tasks · 1 accepted"));
    assert!(text.contains("done · cancelled notification (not task)"));
    let mut view = selected_view(&snapshot);
    view.navigate(KeyCode::Char('f'), &snapshot);
    let tasks = screen(&render(&snapshot, &view, 160, 40, false));
    assert!(tasks.contains("accepted  Refine dashboard"), "{tasks}");
    assert!(!tasks.contains("cancelled"));
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
