use super::super::reset_ui::{Action, Dialog};
use super::*;

fn preview() -> ResetPreview {
    ResetPreview {
        space_id: "w1".into(),
        nonce: "human-preview-only".into(),
        participants: vec![ResetParticipant {
            agent_id: "old-agent".into(),
            name: "Ada".into(),
            role: "architect".into(),
            pane_id: "w1:p1".into(),
        }],
        task_count: 12,
        message_count: 301,
        offline_count: 2,
    }
}

fn type_word(dialog: &mut Dialog, word: &str, snapshot: &Snapshot) {
    for ch in word.chars() {
        assert!(matches!(
            dialog.key(KeyCode::Char(ch), snapshot),
            Action::None
        ));
    }
}

#[test]
fn reset_confirmation_is_cancel_by_default_and_requires_reading_explicit_word() {
    let snapshot = populated();
    let mut dialog = Dialog::confirm(preview());
    assert!(matches!(
        dialog.key(KeyCode::Enter, &snapshot),
        Action::Close
    ));
    let mut dialog = Dialog::confirm(preview());
    type_word(&mut dialog, "reset", &snapshot);
    assert!(matches!(
        dialog.key(KeyCode::Enter, &snapshot),
        Action::None
    ));
    let mut view = selected_view(&snapshot);
    view.reset_dialog = Some(dialog);
    render_view(&snapshot, &mut view, 120, 40, false);
    let dialog = view.reset_dialog.as_mut().unwrap();
    match dialog.key(KeyCode::Enter, &snapshot) {
        Action::Start(preview) => {
            assert_eq!(preview.space_id.as_str(), "w1");
            assert_eq!(preview.message_count, 301);
        }
        _ => panic!("explicit read confirmation must return captured preview"),
    }
    assert!(matches!(dialog.key(KeyCode::Esc, &snapshot), Action::Close));
}

#[test]
fn reset_modal_contains_scope_and_honest_warnings_without_tokens() {
    let snapshot = populated();
    let mut view = selected_view(&snapshot);
    view.reset_dialog = Some(Dialog::confirm(preview()));
    let text = screen(&render_view(&snapshot, &mut view, 120, 40, false));
    for expected in [
        "Start fresh in Orchard",
        "12 tasks and 301 messages",
        "Ada · architect · w1:p1",
        "TODO/PR",
        "NOT changed",
        "NOT remotely cleared",
        "Old Pi sessions remain saved",
        "NOT stopped",
        "Type RESET, then press Enter to confirm",
        "Confirmation: _",
        "Esc or empty Enter: Cancel",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    assert!(!text.contains("human-preview-only"));
    let bound_space = view.space_id.clone();
    for key in [
        KeyCode::Tab,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Char('t'),
        KeyCode::Char('c'),
        KeyCode::Char('p'),
        KeyCode::Char('f'),
    ] {
        assert!(matches!(view.reset_key(key, &snapshot), Some(Action::None)));
    }
    assert_eq!(view.space_id, bound_space);
    assert_eq!(view.work_tab, WorkTab::Messages);
    view.reset_dialog = None;
    assert!(
        view.reset_key(KeyCode::Char('t'), &snapshot).is_none(),
        "nonmodal events fall through to ordinary routing"
    );
}

#[test]
fn narrow_confirmation_scrolls_and_does_not_confirm_unread_warnings() {
    let snapshot = populated();
    let mut preview = preview();
    for index in 0..25 {
        let mut agent = preview.participants[0].clone();
        agent.name = format!("Agent-{index}");
        preview.participants.push(agent);
    }
    let mut view = selected_view(&snapshot);
    view.reset_dialog = Some(Dialog::confirm(preview));
    let text = screen(&render_view(&snapshot, &mut view, 35, 12, false));
    assert!(text.contains("Read more before confirming"), "{text}");
    let dialog = view.reset_dialog.as_mut().unwrap();
    type_word(dialog, "RESET", &snapshot);
    assert!(matches!(
        dialog.key(KeyCode::Enter, &snapshot),
        Action::None
    ));
    dialog.key(KeyCode::End, &snapshot);
    let text = screen(&render_view(&snapshot, &mut view, 35, 12, false));
    assert!(text.contains("confirming"), "{text}");
    assert!(matches!(
        view.reset_dialog
            .as_mut()
            .unwrap()
            .key(KeyCode::Enter, &snapshot),
        Action::Start(_)
    ));
}

#[test]
fn reset_progress_and_abandon_are_truthful_and_separately_confirmed() {
    let mut snapshot = populated();
    snapshot.reset = Some(ResetSummary {
        plan_id: "plan-1".into(),
        space_id: "w1".into(),
        state: ResetPhase::Incomplete,
        participants: vec![ResetProgress {
            participant: preview().participants.remove(0),
            state: ResetAgentState::Uncertain,
            new_agent_id: None,
            note: "Awaiting actual fresh session receipt".into(),
        }],
        note: "History retained; replacement may have occurred".into(),
    });
    let mut view = selected_view(&snapshot);
    let base = screen(&render_view(&snapshot, &mut view, 120, 30, false));
    assert!(base.contains("Reset incomplete"));
    view.reset_dialog = Some(Dialog::progress("w1".into()));
    let text = screen(&render_view(&snapshot, &mut view, 120, 30, false));
    assert!(text.contains("History retained"));
    assert!(text.contains("Uncertain"));
    assert!(text.contains("Space frozen"));
    let dialog = view.reset_dialog.as_mut().unwrap();
    assert!(matches!(
        dialog.key(KeyCode::Char('a'), &snapshot),
        Action::None
    ));
    assert!(matches!(
        dialog.key(KeyCode::Enter, &snapshot),
        Action::Close
    ));
    type_word(dialog, "ABANDON", &snapshot);
    assert!(matches!(
        dialog.key(KeyCode::Enter, &snapshot),
        Action::None
    ));
    let text = screen(&render_view(&snapshot, &mut view, 120, 30, false));
    assert!(text.contains("does NOT restore old Pi contexts"));
    assert!(text.contains("manual /new"));
    assert!(matches!(
        view.reset_dialog
            .as_mut()
            .unwrap()
            .key(KeyCode::Enter, &snapshot),
        Action::Abandon(_, _)
    ));
}

#[test]
fn shrinking_below_minimum_disables_invisible_reset_and_abandon_confirmation() {
    let mut snapshot = populated();
    snapshot.reset = Some(ResetSummary {
        plan_id: "plan-1".into(),
        space_id: "w1".into(),
        state: ResetPhase::Incomplete,
        participants: vec![],
        note: "Incomplete".into(),
    });
    for abandon in [false, true] {
        let mut view = selected_view(&snapshot);
        let mut dialog = if abandon {
            Dialog::progress("w1".into())
        } else {
            Dialog::confirm(preview())
        };
        if abandon {
            dialog.key(KeyCode::Char('a'), &snapshot);
        }
        type_word(
            &mut dialog,
            if abandon { "ABANDON" } else { "RESET" },
            &snapshot,
        );
        view.reset_dialog = Some(dialog);
        render_view(&snapshot, &mut view, 120, 40, false);
        render_view(&snapshot, &mut view, 20, 6, false);
        assert!(matches!(
            view.reset_key(KeyCode::Enter, &snapshot),
            Some(Action::None)
        ));
        render_view(&snapshot, &mut view, 120, 40, false);
        assert!(matches!(
            view.reset_key(KeyCode::Enter, &snapshot),
            Some(Action::Start(_) | Action::Abandon(_, _))
        ));
    }
}

#[test]
fn reset_errors_do_not_claim_an_attempt_had_no_effect() {
    let snapshot = populated();
    let mut view = selected_view(&snapshot);
    view.reset_dialog = Some(Dialog::error("Acknowledgement uncertain".into()));
    let text = screen(&render_view(&snapshot, &mut view, 120, 30, false));
    assert!(text.contains("Inspect reset status before retry"));
    assert!(!text.contains("nothing was confirmed"));
}

fn confirmation_snapshot() -> Snapshot {
    let mut snapshot = populated();
    snapshot.reset = Some(ResetSummary {
        plan_id: "plan-1".into(),
        space_id: "w1".into(),
        state: ResetPhase::Incomplete,
        participants: vec![],
        note: "History retained; inspect before manual recovery".into(),
    });
    snapshot
}

fn confirmation_dialog(snapshot: &Snapshot, abandon: bool) -> Dialog {
    if abandon {
        let mut dialog = Dialog::progress("w1".into());
        dialog.key(KeyCode::Char('a'), snapshot);
        dialog
    } else {
        Dialog::confirm(preview())
    }
}

#[test]
fn explicit_confirmation_instruction_input_and_cancel_stay_visible_narrow_and_wide() {
    let snapshot = confirmation_snapshot();
    for (width, rows) in [(35, 12), (40, 16), (56, 24), (120, 40)] {
        for (abandon, word) in [(false, "RESET"), (true, "ABANDON")] {
            let mut view = selected_view(&snapshot);
            view.reset_dialog = Some(confirmation_dialog(&snapshot, abandon));
            let text = screen(&render_view(&snapshot, &mut view, width, rows, false));
            let unwrapped = text
                .replace('│', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                unwrapped.contains(&format!("Type {word}, then press Enter to confirm")),
                "{text}"
            );
            assert!(text.contains("Confirmation: _"), "{text}");
            assert!(text.contains("Esc or empty Enter: Cancel"), "{text}");
            let (_, instruction_row) = position(&text, &format!("Type {word}"));
            let (_, input_row) = position(&text, "Confirmation: _");
            let (_, cancel_row) = position(&text, "Esc or empty Enter: Cancel");
            assert!(instruction_row < input_row && input_row < cancel_row);
            assert!(cancel_row < usize::from(rows - 1), "{text}");
            if (width, rows) == (35, 12) {
                assert!(text.contains("Read more before confirming"), "{text}");
                println!("\n--- {word} {width}x{rows} ---\n{text}");
            }
            let dialog = view.reset_dialog.as_mut().unwrap();
            type_word(dialog, word, &snapshot);
            dialog.key(KeyCode::End, &snapshot);
            let typed = screen(&render_view(&snapshot, &mut view, width, rows, false));
            assert!(typed.contains(&format!("Confirmation: {word}")), "{typed}");
            assert!(typed.contains("Esc or empty Enter: Cancel"), "{typed}");
            assert!(typed.contains("all details read"), "{typed}");
            assert!(matches!(
                view.reset_dialog
                    .as_mut()
                    .unwrap()
                    .key(KeyCode::Enter, &snapshot),
                Action::Start(_) | Action::Abandon(_, _)
            ));
        }
    }
}

#[test]
fn explicit_footer_keeps_exact_word_read_and_empty_enter_guards() {
    let snapshot = confirmation_snapshot();
    for (abandon, word) in [(false, "RESET"), (true, "ABANDON")] {
        let mut view = selected_view(&snapshot);
        view.reset_dialog = Some(confirmation_dialog(&snapshot, abandon));
        let dialog = view.reset_dialog.as_mut().unwrap();
        type_word(dialog, &format!("{word}X"), &snapshot);
        assert!(matches!(
            dialog.key(KeyCode::Enter, &snapshot),
            Action::None
        ));
        dialog.key(KeyCode::End, &snapshot);
        render_view(&snapshot, &mut view, 35, 12, false);
        let dialog = view.reset_dialog.as_mut().unwrap();
        assert!(matches!(
            dialog.key(KeyCode::Enter, &snapshot),
            Action::None
        ));
        dialog.key(KeyCode::Backspace, &snapshot);
        assert!(matches!(
            dialog.key(KeyCode::Enter, &snapshot),
            Action::Start(_) | Action::Abandon(_, _)
        ));
        for _ in 0..word.len() {
            dialog.key(KeyCode::Backspace, &snapshot);
        }
        assert!(matches!(
            dialog.key(KeyCode::Enter, &snapshot),
            Action::Close
        ));
        assert!(matches!(dialog.key(KeyCode::Esc, &snapshot), Action::Close));
    }
}

#[test]
fn progress_and_error_footers_only_offer_close_not_typed_confirmation() {
    let snapshot = confirmation_snapshot();
    for (width, rows) in [(35, 12), (120, 40)] {
        for dialog in [
            Dialog::progress("w1".into()),
            Dialog::error("Acknowledgement uncertain".into()),
        ] {
            let mut view = selected_view(&snapshot);
            view.reset_dialog = Some(dialog);
            let text = screen(&render_view(&snapshot, &mut view, width, rows, false));
            assert!(text.contains("Enter/Esc: close"), "{text}");
            assert!(!text.contains("Type RESET") && !text.contains("Type ABANDON"));
            assert!(!text.contains("Confirmation:") && !text.contains("before confirming"));
            assert!(matches!(
                view.reset_dialog
                    .as_mut()
                    .unwrap()
                    .key(KeyCode::Enter, &snapshot),
                Action::Close
            ));
        }
    }
}

#[test]
fn foreign_reset_status_is_not_displayed_or_offered_for_abandonment() {
    let mut snapshot = confirmation_snapshot();
    let reset = snapshot.reset.as_mut().unwrap();
    reset.space_id = SpaceId::new("w2");
    reset.note = "FOREIGN_RESET_STATUS".into();
    let mut view = selected_view(&snapshot);
    let text = screen(&render(&snapshot, &view, 120, 30, false));
    assert!(!text.contains("Reset incomplete"), "{text}");
    view.reset_dialog = Some(Dialog::progress(SpaceId::new("w1")));
    let text = screen(&render_view(&snapshot, &mut view, 120, 30, false));
    assert!(text.contains("No reset status available"), "{text}");
    assert!(!text.contains("FOREIGN_RESET_STATUS"));
    assert!(matches!(
        view.reset_key(KeyCode::Char('a'), &snapshot),
        Some(Action::None)
    ));
    let text = screen(&render_view(&snapshot, &mut view, 120, 30, false));
    assert!(!text.contains("Type ABANDON"), "{text}");
}

#[test]
fn reset_modal_uses_terminal_native_styles_and_sanitizes_external_names() {
    let mut preview = preview();
    preview.participants[0].name = "Bad\u{1b}[31m\nname".into();
    let snapshot = populated();
    for (width, rows) in [(35, 12), (56, 24), (120, 40)] {
        let mut view = selected_view(&snapshot);
        view.reset_dialog = Some(Dialog::confirm(preview.clone()));
        let buffer = render_view(&snapshot, &mut view, width, rows, false);
        let colored = render_view(&snapshot, &mut view, width, rows, true);
        assert_eq!(screen(&buffer), screen(&colored));
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.bg == Color::Reset && cell.fg == Color::Reset)
        );
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| cell.symbol().chars().all(|ch| !ch.is_control()))
        );
    }
}
