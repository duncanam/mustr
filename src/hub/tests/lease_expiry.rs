use super::*;

#[test]
fn lease_expiry_pauses_work_with_a_distinct_nontermination_note() {
    let (mut state, architect, worker, config) = setup();
    let task = assign(&mut state, &architect, &worker, &config);

    state.tick(16, &config).unwrap();

    let paused = &state.tasks[&task];
    assert_eq!(paused.state, TaskState::Paused);
    assert_eq!(
        paused.note,
        "Participant lease expired. Execution may have continued; reconcile before guidance."
    );
    assert_eq!(state.members[&worker].activity, Activity::Offline);
}

#[test]
fn transport_close_and_explicit_leave_keep_distinct_observed_notes() {
    let (mut closed, architect, worker, config) = setup();
    let closed_task = assign(&mut closed, &architect, &worker, &config);
    closed.disconnect(&worker);
    assert_eq!(
        closed.tasks[&closed_task].note,
        "Participant transport closed. Execution may have continued; reconcile before guidance."
    );

    let (mut left, architect, worker, config) = setup();
    let left_task = assign(&mut left, &architect, &worker, &config);
    left.apply(&request("leave", Command::Leave), Some(&worker), 1, &config)
        .unwrap();
    assert_eq!(
        left.tasks[&left_task].note,
        "Participant left. Execution may have continued; reconcile before guidance."
    );
}
