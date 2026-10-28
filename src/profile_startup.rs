use std::{
    collections::{BTreeSet, HashSet},
    future::Future,
    path::PathBuf,
    time::Duration,
};

use color_eyre::eyre::Result;

use crate::{
    herdr::Herdr,
    herdr_launcher::{
        HerdrAgentName, HerdrLauncher, MutationOutcome, PendingCommand, PendingCommandError,
        PreexecutionRejection, StartupFlagArgument, TabBootstrap,
    },
    profile::Profile,
    protocol::{
        Activity, AgentId, LaunchId, Member, ResetPhase, Snapshot, SpaceId, StartupGrantToken,
    },
    service::Service,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    Frozen,
    ResetEpochChanged,
    SpaceChanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryState {
    Joined,
    Failed,
    Uncertain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EntryOutcome {
    pub role: String,
    pub pane_id: Option<String>,
    pub state: EntryState,
    pub reason: String,
}

pub(crate) enum ProfileProgress {
    Starting {
        index: usize,
        total: usize,
        role: String,
    },
    Finished(EntryOutcome),
}

pub(crate) struct ProfileLaunch {
    pub profile: Profile,
    pub service: Service,
    pub herdr: Herdr,
    pub launcher: HerdrLauncher,
    pub cwd: PathBuf,
    pub bootstrap: TabBootstrap,
    pub progress: tokio::sync::mpsc::UnboundedSender<ProfileProgress>,
    pub cancel: tokio::sync::watch::Receiver<bool>,
}

fn snapshot_stop_reason(
    snapshot: &Snapshot,
    baseline_epoch: u64,
    expected_space: &SpaceId,
) -> Option<StopReason> {
    if &snapshot.space_id != expected_space
        || snapshot
            .space
            .as_ref()
            .is_some_and(|space| &space.id != expected_space)
    {
        return Some(StopReason::SpaceChanged);
    }
    if snapshot
        .reset
        .as_ref()
        .is_some_and(|reset| matches!(reset.state, ResetPhase::Prepared | ResetPhase::Incomplete))
    {
        return Some(StopReason::Frozen);
    }
    if snapshot.epoch != baseline_epoch {
        return Some(StopReason::ResetEpochChanged);
    }
    None
}

#[derive(Clone)]
struct AdmissionContext {
    service: Service,
    herdr: Herdr,
    expected_space: SpaceId,
    baseline_epoch: u64,
    baseline_ids: BTreeSet<AgentId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MembershipWait {
    Joined,
    Deadline,
    Stopped(StopReason),
    Cancelled,
    IdentityChanged,
    HubStopped,
}

async fn observe_membership(
    context: AdmissionContext,
    role: String,
    pane_id: String,
    deadline: tokio::time::Instant,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> MembershipWait {
    let mut snapshots = context.service.snapshots.clone();
    loop {
        if context.herdr.check_instance().is_err() {
            return MembershipWait::IdentityChanged;
        }
        let snapshot = snapshots.borrow().clone();
        if let Some(reason) =
            snapshot_stop_reason(&snapshot, context.baseline_epoch, &context.expected_space)
        {
            return MembershipWait::Stopped(reason);
        }
        if matching_member(
            &snapshot,
            &context.expected_space,
            &role,
            &pane_id,
            &context.baseline_ids,
        )
        .is_some()
        {
            return MembershipWait::Joined;
        }
        if *cancel.borrow() {
            return MembershipWait::Cancelled;
        }
        if tokio::time::Instant::now() >= deadline {
            // Read the latest watch value once more at the shared deadline.
            let final_snapshot = snapshots.borrow_and_update().clone();
            if context.herdr.check_instance().is_err() {
                return MembershipWait::IdentityChanged;
            }
            if let Some(reason) = snapshot_stop_reason(
                &final_snapshot,
                context.baseline_epoch,
                &context.expected_space,
            ) {
                return MembershipWait::Stopped(reason);
            }
            return if matching_member(
                &final_snapshot,
                &context.expected_space,
                &role,
                &pane_id,
                &context.baseline_ids,
            )
            .is_some()
            {
                MembershipWait::Joined
            } else {
                MembershipWait::Deadline
            };
        }
        tokio::select! {
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return MembershipWait::HubStopped;
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return MembershipWait::Cancelled;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {},
            _ = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
    }
}

fn existing_manager_conflict(profile: &Profile, snapshot: &Snapshot, space: &SpaceId) -> bool {
    profile.roles.iter().any(|role| role.manager)
        && snapshot
            .members
            .iter()
            .any(|member| &member.space_id == space && member.manager)
}

fn matching_member<'a>(
    snapshot: &'a Snapshot,
    space: &SpaceId,
    role: &str,
    pane_id: &str,
    baseline_ids: &BTreeSet<AgentId>,
) -> Option<&'a Member> {
    snapshot.members.iter().find(|member| {
        &member.space_id == space
            && member.role == role
            && member.pane_id == pane_id
            && member.activity != Activity::Offline
            && !baseline_ids.contains(&member.id)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HaltReason {
    Cancelled,
    HerdrIdentityChanged,
    HubStopped,
    Snapshot(StopReason),
}

impl HaltReason {
    fn description(self) -> &'static str {
        match self {
            Self::Cancelled => "launch cancelled",
            Self::HerdrIdentityChanged => "Herdr identity changed or became unavailable",
            Self::HubStopped => "hub stopped during profile launch",
            Self::Snapshot(StopReason::Frozen) => "space entered a prepared or incomplete reset",
            Self::Snapshot(StopReason::ResetEpochChanged) => "reset epoch changed during launch",
            Self::Snapshot(StopReason::SpaceChanged) => {
                "bound space identity changed during launch"
            }
        }
    }
}

fn current_halt(
    service: &Service,
    herdr: &Herdr,
    expected_space: &SpaceId,
    baseline_epoch: u64,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> Option<HaltReason> {
    if herdr.check_instance().is_err() {
        return Some(HaltReason::HerdrIdentityChanged);
    }
    let snapshot = service.snapshots.borrow().clone();
    if let Some(reason) = snapshot_stop_reason(&snapshot, baseline_epoch, expected_space) {
        return Some(HaltReason::Snapshot(reason));
    }
    if *cancel.borrow() {
        return Some(HaltReason::Cancelled);
    }
    None
}

async fn retain_agent_start(
    start: impl Future<Output = MutationOutcome<()>>,
    context: AdmissionContext,
    token: StartupGrantToken,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> (MutationOutcome<()>, Option<HaltReason>, bool) {
    let mut snapshots = context.service.snapshots.clone();
    tokio::pin!(start);

    loop {
        if let Some(halt) = current_halt(
            &context.service,
            &context.herdr,
            &context.expected_space,
            context.baseline_epoch,
            &cancel,
        ) {
            let _ = context.service.retire_startup_grant(token.clone()).await;
            return (start.as_mut().await, Some(halt), true);
        }

        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    let halt = current_halt(
                        &context.service,
                        &context.herdr,
                        &context.expected_space,
                        context.baseline_epoch,
                        &cancel,
                    ).unwrap_or(HaltReason::Cancelled);
                    let _ = context.service.retire_startup_grant(token.clone()).await;
                    return (start.as_mut().await, Some(halt), true);
                }
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    let _ = context.service.retire_startup_grant(token.clone()).await;
                    return (start.as_mut().await, Some(HaltReason::HubStopped), true);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            outcome = &mut start => {
                if let Some(halt) = current_halt(
                    &context.service,
                    &context.herdr,
                    &context.expected_space,
                    context.baseline_epoch,
                    &cancel,
                ) {
                    let _ = context.service.retire_startup_grant(token.clone()).await;
                    return (outcome, Some(halt), true);
                }
                return (outcome, None, false);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalReconciliation {
    Exited,
    OutputIncomplete,
    Unreconciled,
}

async fn reconcile_pending(pending: &mut PendingCommand) -> LocalReconciliation {
    match tokio::time::timeout(Duration::from_secs(5), pending.wait_for_local_exit()).await {
        Ok(Ok(())) => LocalReconciliation::Exited,
        Ok(Err(PendingCommandError::CaptureFailed)) => LocalReconciliation::OutputIncomplete,
        Ok(Err(PendingCommandError::ChildWaitFailed)) | Err(_) => LocalReconciliation::Unreconciled,
    }
}

fn local_reconciliation_note(result: LocalReconciliation) -> &'static str {
    match result {
        LocalReconciliation::Exited => {
            "local Herdr command exited; remote mutation result remains uncertain"
        }
        LocalReconciliation::OutputIncomplete => {
            "local Herdr child exited but output capture was incomplete; remote mutation remains uncertain"
        }
        LocalReconciliation::Unreconciled => "local Herdr child exit remains unconfirmed",
    }
}

fn membership_invalidation_reason(observation: MembershipWait, halt: HaltReason) -> String {
    if observation == MembershipWait::Joined {
        format!(
            "matching live hub member was observed; launch stopped after {}",
            halt.description()
        )
    } else {
        format!(
            "no matching live hub member was confirmed before {}; launch stopped after {}",
            if observation == MembershipWait::Deadline {
                "the shared grant deadline"
            } else {
                "invalidation"
            },
            halt.description()
        )
    }
}

fn publish_starting(
    progress: &tokio::sync::mpsc::UnboundedSender<ProfileProgress>,
    index: usize,
    total: usize,
    role: &str,
) {
    let _ = progress.send(ProfileProgress::Starting {
        index,
        total,
        role: role.to_owned(),
    });
}

fn publish_finished(
    progress: &tokio::sync::mpsc::UnboundedSender<ProfileProgress>,
    outcomes: &mut Vec<EntryOutcome>,
    outcome: EntryOutcome,
) {
    let _ = progress.send(ProfileProgress::Finished(outcome.clone()));
    outcomes.push(outcome);
}

fn entry_outcome(
    role: &str,
    pane_id: Option<String>,
    state: EntryState,
    reason: impl Into<String>,
) -> EntryOutcome {
    EntryOutcome {
        role: role.to_owned(),
        pane_id,
        state,
        reason: reason.into(),
    }
}

fn publish_unattempted(
    roles: &[crate::roles::Role],
    from: usize,
    progress: &tokio::sync::mpsc::UnboundedSender<ProfileProgress>,
    outcomes: &mut Vec<EntryOutcome>,
    reason: &str,
) {
    for (offset, role) in roles.iter().enumerate().skip(from) {
        publish_starting(progress, offset + 1, roles.len(), &role.name);
        publish_finished(
            progress,
            outcomes,
            entry_outcome(
                &role.name,
                None,
                EntryState::Failed,
                format!("not attempted after {reason}"),
            ),
        );
    }
}

fn next_agent_name(names: &mut HashSet<String>) -> HerdrAgentName {
    loop {
        let name = HerdrAgentName::random();
        if names.insert(name.as_str().to_owned()) {
            return name;
        }
    }
}

fn rejected_outcome(
    role: &str,
    pane_id: Option<String>,
    reason: PreexecutionRejection,
) -> EntryOutcome {
    entry_outcome(role, pane_id, EntryState::Failed, reason.to_string())
}

fn member_is_present(
    service: &Service,
    expected_space: &SpaceId,
    baseline_epoch: u64,
    baseline_ids: &BTreeSet<AgentId>,
    role: &str,
    pane_id: &str,
) -> bool {
    let snapshot = service.snapshots.borrow().clone();
    snapshot_stop_reason(&snapshot, baseline_epoch, expected_space).is_none()
        && matching_member(&snapshot, expected_space, role, pane_id, baseline_ids).is_some()
}

/// Launch profile slots serially. Only a new authoritative hub member proves admission.
pub(crate) async fn run(launch: ProfileLaunch) -> Result<Vec<EntryOutcome>> {
    let ProfileLaunch {
        profile,
        service,
        herdr,
        launcher,
        cwd,
        bootstrap,
        progress,
        cancel,
    } = launch;
    let mut snapshots = service.snapshots.clone();
    let baseline = snapshots.borrow_and_update().clone();
    let expected_space = service.space_id.clone();
    let baseline_epoch = baseline.epoch;
    let baseline_ids = baseline
        .members
        .iter()
        .map(|member| member.id.clone())
        .collect::<BTreeSet<_>>();
    let admission_context = AdmissionContext {
        service: service.clone(),
        herdr: herdr.clone(),
        expected_space: expected_space.clone(),
        baseline_epoch,
        baseline_ids: baseline_ids.clone(),
    };
    let mut outcomes = Vec::with_capacity(profile.roles.len());

    let current_snapshot = service.snapshots.borrow().clone();
    let initial_halt = current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel)
        .map(|reason| reason.description().to_owned())
        .or_else(|| {
            existing_manager_conflict(&profile, &current_snapshot, &expected_space)
                .then(|| "a manager role is already present in the bound space".to_owned())
        });
    if let Some(reason) = initial_halt {
        publish_unattempted(&profile.roles, 0, &progress, &mut outcomes, &reason);
        return Ok(outcomes);
    }

    let launch_id = LaunchId::new(uuid::Uuid::new_v4().simple().to_string());
    let mut agent_names = HashSet::new();
    for (offset, role) in profile.roles.iter().enumerate() {
        let index = offset + 1;
        publish_starting(&progress, index, profile.roles.len(), &role.name);
        if let Some(halt) = current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel)
        {
            let reason = halt.description();
            publish_finished(
                &progress,
                &mut outcomes,
                entry_outcome(
                    &role.name,
                    None,
                    EntryState::Failed,
                    format!("not attempted after {reason}"),
                ),
            );
            publish_unattempted(&profile.roles, index, &progress, &mut outcomes, reason);
            return Ok(outcomes);
        }

        let tab = launcher
            .create_tab_with_bootstrap(&expected_space, &cwd, &bootstrap)
            .await;
        let after_tab_halt =
            current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel);
        match tab {
            MutationOutcome::Rejected(reason) => {
                publish_finished(
                    &progress,
                    &mut outcomes,
                    rejected_outcome(&role.name, None, reason),
                );
                publish_unattempted(
                    &profile.roles,
                    index,
                    &progress,
                    &mut outcomes,
                    "the preceding tab create was rejected",
                );
                return Ok(outcomes);
            }
            MutationOutcome::Uncertain(mut pending) => {
                let local = match pending.as_mut() {
                    Some(pending) => Some(reconcile_pending(pending).await),
                    None => None,
                };
                let reason = match local {
                    Some(result) => format!(
                        "tab create result or root identity is uncertain; no Pi was started; {}",
                        local_reconciliation_note(result)
                    ),
                    None => {
                        "tab create result or root identity is uncertain; no Pi was started".into()
                    }
                };
                publish_finished(
                    &progress,
                    &mut outcomes,
                    entry_outcome(&role.name, None, EntryState::Uncertain, reason),
                );
                let stop_reason = after_tab_halt
                    .map(HaltReason::description)
                    .unwrap_or("tab create was ambiguous");
                publish_unattempted(&profile.roles, index, &progress, &mut outcomes, stop_reason);
                return Ok(outcomes);
            }
            MutationOutcome::Completed(root) => {
                if let Some(halt) = after_tab_halt {
                    let reason = halt.description();
                    publish_finished(
                        &progress,
                        &mut outcomes,
                        entry_outcome(
                            &role.name,
                            Some(root.pane_id().to_owned()),
                            EntryState::Failed,
                            format!("tab created; Pi start not attempted after {reason}"),
                        ),
                    );
                    publish_unattempted(&profile.roles, index, &progress, &mut outcomes, reason);
                    return Ok(outcomes);
                }
                if let Some(halt) =
                    current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel)
                {
                    let reason = halt.description();
                    publish_finished(
                        &progress,
                        &mut outcomes,
                        entry_outcome(
                            &role.name,
                            Some(root.pane_id().to_owned()),
                            EntryState::Failed,
                            format!("tab created; Pi start not attempted after {reason}"),
                        ),
                    );
                    publish_unattempted(&profile.roles, index, &progress, &mut outcomes, reason);
                    return Ok(outcomes);
                }

                let issued = service
                    .issue_startup_grant(
                        launch_id.clone(),
                        role.name.clone(),
                        root.pane_id().to_owned(),
                    )
                    .await;
                let after_issue_halt =
                    current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel);
                let issued = match issued {
                    Ok(issued) => issued,
                    Err(_) => {
                        publish_finished(
                            &progress,
                            &mut outcomes,
                            entry_outcome(
                                &role.name,
                                Some(root.pane_id().to_owned()),
                                EntryState::Failed,
                                "one-use startup authority could not be confirmed; Pi start was not attempted",
                            ),
                        );
                        let reason = after_issue_halt
                            .map(HaltReason::description)
                            .unwrap_or("startup authority issuance failed");
                        publish_unattempted(
                            &profile.roles,
                            index,
                            &progress,
                            &mut outcomes,
                            reason,
                        );
                        return Ok(outcomes);
                    }
                };
                if let Some(halt) = after_issue_halt {
                    let reason = halt.description();
                    let _ = service
                        .retire_startup_grant(issued.handoff.token.clone())
                        .await;
                    publish_finished(
                        &progress,
                        &mut outcomes,
                        entry_outcome(
                            &role.name,
                            Some(root.pane_id().to_owned()),
                            EntryState::Failed,
                            format!("Pi start not attempted after {reason}"),
                        ),
                    );
                    publish_unattempted(&profile.roles, index, &progress, &mut outcomes, reason);
                    return Ok(outcomes);
                }
                if issued.deadline <= issued.issued_at {
                    let _ = service
                        .retire_startup_grant(issued.handoff.token.clone())
                        .await;
                    let reason =
                        current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel)
                            .map(HaltReason::description)
                            .unwrap_or("startup grant deadline did not follow its issue time");
                    publish_finished(
                        &progress,
                        &mut outcomes,
                        entry_outcome(
                            &role.name,
                            Some(root.pane_id().to_owned()),
                            EntryState::Failed,
                            "startup grant had no valid observation window; Pi start was not attempted",
                        ),
                    );
                    publish_unattempted(&profile.roles, index, &progress, &mut outcomes, reason);
                    return Ok(outcomes);
                }
                let startup_argument = match StartupFlagArgument::new(&issued.handoff, &role.name) {
                    Ok(argument) => argument,
                    Err(reason) => {
                        let _ = service
                            .retire_startup_grant(issued.handoff.token.clone())
                            .await;
                        publish_finished(
                            &progress,
                            &mut outcomes,
                            entry_outcome(
                                &role.name,
                                Some(root.pane_id().to_owned()),
                                EntryState::Failed,
                                format!(
                                    "private startup handoff was rejected before Pi start: {reason}"
                                ),
                            ),
                        );
                        publish_unattempted(
                            &profile.roles,
                            index,
                            &progress,
                            &mut outcomes,
                            "private startup handoff validation failed",
                        );
                        return Ok(outcomes);
                    }
                };
                if let Some(halt) =
                    current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel)
                {
                    let reason = halt.description();
                    let _ = service
                        .retire_startup_grant(issued.handoff.token.clone())
                        .await;
                    publish_finished(
                        &progress,
                        &mut outcomes,
                        entry_outcome(
                            &role.name,
                            Some(root.pane_id().to_owned()),
                            EntryState::Failed,
                            format!("Pi start not attempted after {reason}"),
                        ),
                    );
                    publish_unattempted(&profile.roles, index, &progress, &mut outcomes, reason);
                    return Ok(outcomes);
                }

                let name = next_agent_name(&mut agent_names);
                let start = launcher.start_pi(&name, &root, &startup_argument);
                let (mutation, after_start_halt, grant_retired) = retain_agent_start(
                    start,
                    admission_context.clone(),
                    issued.handoff.token.clone(),
                    cancel.clone(),
                )
                .await;
                let (uncertain_mutation, mut pending) = match mutation {
                    MutationOutcome::Rejected(reason) => {
                        if !grant_retired {
                            let _ = service
                                .retire_startup_grant(issued.handoff.token.clone())
                                .await;
                        }
                        publish_finished(
                            &progress,
                            &mut outcomes,
                            rejected_outcome(&role.name, Some(root.pane_id().to_owned()), reason),
                        );
                        let stop_reason = after_start_halt
                            .map(HaltReason::description)
                            .unwrap_or("agent start was rejected before execution");
                        publish_unattempted(
                            &profile.roles,
                            index,
                            &progress,
                            &mut outcomes,
                            stop_reason,
                        );
                        return Ok(outcomes);
                    }
                    MutationOutcome::Completed(()) => (false, None),
                    MutationOutcome::Uncertain(pending) => (true, pending),
                };

                if let Some(halt) = after_start_halt {
                    let joined = matches!(halt, HaltReason::Cancelled)
                        && member_is_present(
                            &service,
                            &expected_space,
                            baseline_epoch,
                            &baseline_ids,
                            &role.name,
                            root.pane_id(),
                        );
                    if !grant_retired {
                        let _ = service
                            .retire_startup_grant(issued.handoff.token.clone())
                            .await;
                    }
                    let mut reason = if joined {
                        format!(
                            "matching live hub member was present when {} was observed; no later profile mutation was attempted",
                            halt.description()
                        )
                    } else {
                        format!(
                            "agent start was not fully reconciled after {}; no later profile mutation was attempted",
                            halt.description()
                        )
                    };
                    let local = match pending.as_mut() {
                        Some(pending) => Some(reconcile_pending(pending).await),
                        None => None,
                    };
                    if let Some(local) = local {
                        reason.push_str("; ");
                        reason.push_str(local_reconciliation_note(local));
                    }
                    publish_finished(
                        &progress,
                        &mut outcomes,
                        entry_outcome(
                            &role.name,
                            Some(root.pane_id().to_owned()),
                            if joined {
                                EntryState::Joined
                            } else {
                                EntryState::Uncertain
                            },
                            reason,
                        ),
                    );
                    publish_unattempted(
                        &profile.roles,
                        index,
                        &progress,
                        &mut outcomes,
                        halt.description(),
                    );
                    return Ok(outcomes);
                }

                let deadline = tokio::time::Instant::from_std(issued.deadline);
                let observation = observe_membership(
                    admission_context.clone(),
                    role.name.clone(),
                    root.pane_id().to_owned(),
                    deadline,
                    cancel.clone(),
                )
                .await;
                let after_observation_halt =
                    current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel);
                let _ = service
                    .retire_startup_grant(issued.handoff.token.clone())
                    .await;
                let after_retire_halt =
                    current_halt(&service, &herdr, &expected_space, baseline_epoch, &cancel);
                let local = match pending.as_mut() {
                    Some(pending) => Some(reconcile_pending(pending).await),
                    None => None,
                };
                let final_halt = after_observation_halt.or(after_retire_halt);
                let joined = observation == MembershipWait::Joined
                    && final_halt.is_none_or(|halt| matches!(halt, HaltReason::Cancelled));
                let state = if joined {
                    EntryState::Joined
                } else {
                    EntryState::Uncertain
                };
                let reason = if joined {
                    match local {
                        Some(LocalReconciliation::OutputIncomplete) => {
                            "new matching hub member confirmed; local Herdr output capture was incomplete".to_owned()
                        }
                        Some(LocalReconciliation::Unreconciled) => {
                            "new matching hub member confirmed; local Herdr command exit remains unconfirmed".to_owned()
                        }
                        Some(LocalReconciliation::Exited) => {
                            "new matching hub member confirmed; local Herdr command exited".to_owned()
                        }
                        None => "new matching hub member confirmed".to_owned(),
                    }
                } else if let Some(halt) = final_halt {
                    membership_invalidation_reason(observation, halt)
                } else {
                    let mut reason = match observation {
                        MembershipWait::Deadline => {
                            "no matching live hub member appeared before the shared grant deadline"
                                .to_owned()
                        }
                        MembershipWait::Stopped(reason) => format!(
                            "launch stopped after {}",
                            HaltReason::Snapshot(reason).description()
                        ),
                        MembershipWait::Cancelled => {
                            "launch cancelled before matching live membership was observed"
                                .to_owned()
                        }
                        MembershipWait::IdentityChanged => {
                            "Herdr identity changed before matching live membership was observed"
                                .to_owned()
                        }
                        MembershipWait::HubStopped => {
                            "hub stopped before matching live membership was observed".to_owned()
                        }
                        MembershipWait::Joined => unreachable!("joined observation handled above"),
                    };
                    if uncertain_mutation {
                        reason
                            .push_str("; Herdr agent start remained ambiguous and was not retried");
                    }
                    if let Some(local) = local {
                        reason.push_str("; ");
                        reason.push_str(local_reconciliation_note(local));
                    }
                    reason
                };
                publish_finished(
                    &progress,
                    &mut outcomes,
                    entry_outcome(&role.name, Some(root.pane_id().to_owned()), state, reason),
                );

                if !joined || uncertain_mutation || final_halt.is_some() {
                    let stop_reason = final_halt
                        .map(HaltReason::description)
                        .unwrap_or("the preceding agent start or admission was uncertain");
                    publish_unattempted(
                        &profile.roles,
                        index,
                        &progress,
                        &mut outcomes,
                        stop_reason,
                    );
                    return Ok(outcomes);
                }
            }
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, time::Duration};

    use crate::{
        herdr::Herdr,
        profile::Profile,
        protocol::{
            Activity, AgentId, Command, Member, Request, ResetPhase, ResetSummary, Snapshot,
            SpaceId, VERSION,
        },
        roles::Role,
        service::Service,
    };

    use super::{
        AdmissionContext, EntryState, HaltReason, MembershipWait, StopReason,
        existing_manager_conflict, matching_member, membership_invalidation_reason,
        observe_membership, snapshot_stop_reason,
    };

    fn snapshot() -> Snapshot {
        Snapshot::empty(SpaceId::new("w1"))
    }

    fn reset(state: ResetPhase) -> ResetSummary {
        ResetSummary {
            plan_id: "plan".into(),
            space_id: SpaceId::new("w1"),
            state,
            participants: Vec::new(),
            note: String::new(),
        }
    }

    fn role(name: &str, manager: bool) -> Role {
        Role {
            name: name.into(),
            description: name.into(),
            prompt: String::new(),
            manager,
        }
    }

    fn profile(manager: bool) -> Profile {
        Profile {
            name: "test".into(),
            roles: vec![role("architect", manager)],
        }
    }

    fn member(
        id: &str,
        role: &str,
        pane_id: &str,
        space_id: &str,
        activity: Activity,
        manager: bool,
    ) -> Member {
        Member {
            id: AgentId::new(id),
            name: id.into(),
            role: role.into(),
            pane_id: pane_id.into(),
            space_id: SpaceId::new(space_id),
            manager,
            cwd: None,
            activity,
            last_seen: 0,
            idle_since: 0,
        }
    }

    #[test]
    fn invalidation_wording_preserves_an_already_observed_member() {
        let frozen = HaltReason::Snapshot(StopReason::Frozen);
        assert_eq!(
            membership_invalidation_reason(MembershipWait::Joined, frozen),
            "matching live hub member was observed; launch stopped after space entered a prepared or incomplete reset"
        );
        assert_eq!(
            membership_invalidation_reason(MembershipWait::Deadline, frozen),
            "no matching live hub member was confirmed before the shared grant deadline; launch stopped after space entered a prepared or incomplete reset"
        );
    }

    #[test]
    fn missing_space_metadata_and_completed_reset_are_not_frozen() {
        let mut snapshot = snapshot();
        snapshot.reset = Some(reset(ResetPhase::Completed));

        assert_eq!(
            snapshot_stop_reason(&snapshot, 0, &SpaceId::new("w1")),
            None
        );
    }

    #[test]
    fn only_prepared_and_incomplete_reset_phases_are_frozen() {
        for phase in [ResetPhase::Prepared, ResetPhase::Incomplete] {
            let mut snapshot = snapshot();
            snapshot.reset = Some(reset(phase));
            assert_eq!(
                snapshot_stop_reason(&snapshot, 0, &SpaceId::new("w1")),
                Some(super::StopReason::Frozen)
            );
        }

        for phase in [ResetPhase::Completed, ResetPhase::Abandoned] {
            let mut snapshot = snapshot();
            snapshot.reset = Some(reset(phase));
            assert_eq!(
                snapshot_stop_reason(&snapshot, 0, &SpaceId::new("w1")),
                None
            );
        }
    }

    #[test]
    fn reset_epoch_change_is_detected_without_observing_a_frozen_phase() {
        let mut snapshot = snapshot();
        snapshot.epoch = 1;
        snapshot.reset = Some(reset(ResetPhase::Completed));

        assert_eq!(
            snapshot_stop_reason(&snapshot, 0, &SpaceId::new("w1")),
            Some(super::StopReason::ResetEpochChanged)
        );
    }

    #[test]
    fn snapshot_identity_drift_is_detected_but_missing_metadata_is_allowed() {
        let mut snapshot = snapshot();
        assert_eq!(
            snapshot_stop_reason(&snapshot, 0, &SpaceId::new("w1")),
            None
        );
        snapshot.space_id = SpaceId::new("w2");
        assert_eq!(
            snapshot_stop_reason(&snapshot, 0, &SpaceId::new("w1")),
            Some(super::StopReason::SpaceChanged)
        );

        snapshot.space_id = SpaceId::new("w1");
        snapshot.space = Some(crate::protocol::Space {
            id: SpaceId::new("w2"),
            label: "Moved".into(),
        });
        assert_eq!(
            snapshot_stop_reason(&snapshot, 0, &SpaceId::new("w1")),
            Some(super::StopReason::SpaceChanged)
        );
    }

    #[test]
    fn manager_preflight_counts_every_manager_bit_in_the_bound_space() {
        let mut snapshot = snapshot();
        snapshot.members.push(member(
            "offline-manager",
            "architect",
            "w1:p1",
            "w1",
            Activity::Offline,
            true,
        ));
        assert!(existing_manager_conflict(
            &profile(true),
            &snapshot,
            &SpaceId::new("w1")
        ));
        assert!(!existing_manager_conflict(
            &profile(false),
            &snapshot,
            &SpaceId::new("w1")
        ));
        assert!(!existing_manager_conflict(
            &profile(true),
            &snapshot,
            &SpaceId::new("w2")
        ));
    }

    #[test]
    fn admission_requires_a_new_exact_live_member_and_accepts_paused() {
        let space = SpaceId::new("w1");
        let baseline = BTreeSet::from([AgentId::new("old")]);
        let mut snapshot = snapshot();
        snapshot.members = vec![
            member("old", "reviewer", "w1:p9", "w1", Activity::Idle, false),
            member(
                "wrong-pane",
                "reviewer",
                "w1:p8",
                "w1",
                Activity::Idle,
                false,
            ),
            member(
                "wrong-role",
                "researcher",
                "w1:p9",
                "w1",
                Activity::Idle,
                false,
            ),
            member(
                "wrong-space",
                "reviewer",
                "w1:p9",
                "w2",
                Activity::Idle,
                false,
            ),
            member(
                "offline",
                "reviewer",
                "w1:p9",
                "w1",
                Activity::Offline,
                false,
            ),
            member("paused", "reviewer", "w1:p9", "w1", Activity::Paused, false),
        ];

        let joined = matching_member(&snapshot, &space, "reviewer", "w1:p9", &baseline);
        assert_eq!(joined.map(|member| member.id.as_str()), Some("paused"));
        assert_eq!(joined.map(|_| EntryState::Joined), Some(EntryState::Joined));
    }

    #[tokio::test(start_paused = true)]
    async fn membership_observation_uses_the_supplied_absolute_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        fs::write(&socket, b"test identity").unwrap();
        let herdr = Herdr::new(socket).unwrap();
        let service = Service::start(
            SpaceId::new("w1"),
            directory.path().join("hub.sqlite"),
            crate::config::Scheduling::default(),
        )
        .await
        .unwrap();
        let baseline_epoch = service.snapshots.borrow().epoch;
        let context = AdmissionContext {
            service: service.clone(),
            herdr,
            expected_space: SpaceId::new("w1"),
            baseline_epoch,
            baseline_ids: BTreeSet::new(),
        };
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let waiting = tokio::spawn(observe_membership(
            context,
            "reviewer".into(),
            "w1:p9".into(),
            deadline,
            cancel_rx,
        ));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "the absolute deadline has not elapsed"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(waiting.await.unwrap(), MembershipWait::Deadline);
        cancel.send_replace(true);
        service.stop().await;
    }

    #[tokio::test]
    async fn membership_observation_accepts_a_paused_new_member() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        fs::write(&socket, b"test identity").unwrap();
        let herdr = Herdr::new(socket).unwrap();
        let service = Service::start(
            SpaceId::new("w1"),
            directory.path().join("hub.sqlite"),
            crate::config::Scheduling::default(),
        )
        .await
        .unwrap();
        let baseline_epoch = service.snapshots.borrow().epoch;
        let context = AdmissionContext {
            service: service.clone(),
            herdr,
            expected_space: SpaceId::new("w1"),
            baseline_epoch,
            baseline_ids: BTreeSet::new(),
        };
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        let waiting = tokio::spawn(observe_membership(
            context,
            "reviewer".into(),
            "w1:p9".into(),
            tokio::time::Instant::now() + Duration::from_secs(60),
            cancel_rx,
        ));
        tokio::task::yield_now().await;

        let space = SpaceId::new("w1");
        let joined = service
            .join(
                1,
                Request {
                    id: "join".into(),
                    command: Command::Join {
                        version: VERSION,
                        agent_id: AgentId::new("new-reviewer"),
                        name: "reviewer-00000001".into(),
                        role: "reviewer".into(),
                        pane_id: "w1:p9".into(),
                        space_id: space.clone(),
                        cwd: None,
                        reset: None,
                        startup_grant: None,
                    },
                },
                false,
            )
            .await
            .unwrap();
        assert!(joined.ok);
        service
            .request(
                1,
                Request {
                    id: "pause".into(),
                    command: Command::Sync {
                        activity: Activity::Paused,
                        receive: false,
                    },
                },
            )
            .await
            .unwrap();

        assert_eq!(waiting.await.unwrap(), MembershipWait::Joined);
        cancel.send_replace(true);
        service.stop().await;
    }
}
