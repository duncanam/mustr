//! Human-initiated, durable fresh-start coordination. Tokens never enter snapshots.

use std::collections::BTreeSet;

use super::*;

const RECOVERY: &str = "Reset incomplete; old hub history is retained and this space is frozen. Reconcile each pane. Never retry a claimed replacement. The human dashboard can abandon this plan without clearing history; then use manual /new and /role. Already-created sessions cannot be rolled back.";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct ResetPlan {
    plan_id: String,
    space_id: SpaceId,
    epoch: u64,
    state: ResetPhase,
    note: String,
    offline_count: usize,
    agents: Vec<ResetAgent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ResetAgent {
    progress: ResetProgress,
    token: String,
    claimed: bool,
}

impl ResetPlan {
    pub(super) fn belongs_to(&self, space: &SpaceId) -> bool {
        &self.space_id == space
    }

    fn active(&self) -> bool {
        matches!(self.state, ResetPhase::Prepared | ResetPhase::Incomplete)
    }

    pub(super) fn summary(&self) -> ResetSummary {
        ResetSummary {
            plan_id: self.plan_id.clone(),
            space_id: self.space_id.clone(),
            state: self.state,
            participants: self.agents.iter().map(|a| a.progress.clone()).collect(),
            note: self.note.clone(),
        }
    }
}

impl State {
    pub fn reset_metadata_equal(&self, other: &Self) -> bool {
        self.reset == other.reset
            && self.reset_epoch == other.reset_epoch
            && self.retired == other.retired
    }

    pub fn reset_fingerprint(&self, space: &SpaceId) -> Result<String> {
        self.validate_space(space)?;
        Ok(digest(serde_json::to_vec(&(
            &self.tasks,
            &self.messages,
            &self.automation,
            self.reset_epoch,
        ))?))
    }

    pub fn reset_epoch(&self, space: &SpaceId) -> u64 {
        if space == &self.space_id {
            self.reset_epoch
        } else {
            0
        }
    }

    pub fn reset_frozen(&self, space: &SpaceId) -> bool {
        space == &self.space_id && self.reset.as_ref().is_some_and(ResetPlan::active)
    }

    pub fn reset_preview(&self, space: &SpaceId) -> Result<ResetPreview> {
        self.validate_space(space)?;
        ensure!(
            !self.reset_frozen(space),
            "reset already in progress; inspect its recovery status"
        );
        let mut participants = Vec::new();
        let mut offline_count = 0;
        for member in self.members.values() {
            if member.activity == Activity::Offline {
                offline_count += 1;
                continue;
            }
            ensure!(
                member.activity == Activity::Idle,
                "{} is working or paused; reconcile and resume before resetting",
                member.name
            );
            ensure!(
                self.reset_capable.contains(&member.id),
                "{} does not support the reset protocol",
                member.name
            );
            ensure!(
                !self
                    .messages
                    .iter()
                    .any(|m| m.to == member.id && m.delivery == Delivery::Offered),
                "{} has an offered delivery; wait for a settled insertion",
                member.name
            );
            participants.push(ResetParticipant {
                agent_id: member.id.clone(),
                name: member.name.clone(),
                role: member.role.clone(),
                pane_id: member.pane_id.clone(),
            });
        }
        Ok(ResetPreview {
            space_id: space.clone(),
            nonce: String::new(),
            participants,
            task_count: self.tasks.len(),
            message_count: self.messages.len(),
            offline_count,
        })
    }

    pub fn start_reset(&mut self, preview: &ResetPreview) -> Result<()> {
        let mut current = self.reset_preview(&preview.space_id)?;
        current.nonce = preview.nonce.clone();
        ensure!(
            current == *preview,
            "reset inventory changed; preview and confirm again"
        );
        self.validate_space(&preview.space_id)?;
        let space = &preview.space_id;
        let epoch = self
            .reset_epoch
            .checked_add(1)
            .ok_or_else(|| eyre!("reset epoch exhausted"))?;
        let automation = &mut self.automation;
        automation.generation = automation
            .generation
            .checked_add(1)
            .ok_or_else(|| eyre!("watcher generation exhausted"))?;
        // A confirmed fresh start ends the live manager admission. A manager
        // can only claim the slot again through a validated replacement Join.
        for member in self.members.values_mut() {
            member.manager = false;
        }
        automation.todo_enabled = false;
        automation.checkin_enabled = false;
        automation.manager_checkin_at = None;
        automation.pr_enabled = false;
        for message in self
            .messages
            .iter_mut()
            .filter(|message| message.space_id == *space && message.kind == MessageKind::Checkin)
        {
            if message.delivery == Delivery::Pending {
                message.delivery = Delivery::Cancelled;
            } else if message.delivery == Delivery::Offered {
                message.delivery = Delivery::Uncertain;
            }
        }
        self.reset_epoch = epoch;
        self.reset = Some(ResetPlan {
            plan_id: Uuid::new_v4().to_string(),
            space_id: space.clone(),
            epoch,
            state: ResetPhase::Prepared,
            note: "Space frozen; awaiting single-attempt claims and verified fresh-session joins. External jobs are not stopped.".into(),
            offline_count: preview.offline_count,
            agents: preview.participants.iter().map(|participant| ResetAgent {
                progress: ResetProgress {
                    participant: participant.clone(),
                    state: ResetAgentState::Pending,
                    new_agent_id: None,
                    note: "Awaiting adapter safety checks and one persisted claim.".into(),
                },
                token: Uuid::new_v4().to_string(),
                claimed: false,
            }).collect(),
        });
        if preview.participants.is_empty() {
            self.finish_reset();
        }
        Ok(())
    }

    pub fn connection_epoch_valid(&self, agent: &AgentId, space: &SpaceId, epoch: u64) -> bool {
        if space != &self.space_id
            || self.retired.contains(agent)
            || !self
                .members
                .get(agent)
                .is_some_and(|m| m.space_id == self.space_id && m.activity != Activity::Offline)
        {
            return false;
        }
        self.reset_epoch == epoch
            || self.reset.as_ref().is_some_and(|plan| {
                plan.active()
                    && epoch.checked_add(1) == Some(plan.epoch)
                    && plan.agents.iter().any(|a| {
                        &a.progress.participant.agent_id == agent
                            && a.progress.new_agent_id.is_none()
                    })
            })
    }

    pub(super) fn reset_sync(&self, member: &Member) -> Value {
        let plan = self.reset.as_ref().expect("frozen space has reset plan");
        let control = if self.reset_capable.contains(&member.id) {
            plan.agents
                .iter()
                .find(|a| {
                    a.progress.participant.agent_id == member.id
                        && a.progress.new_agent_id.is_none()
                })
                .map(|agent| ResetControl {
                    plan_id: plan.plan_id.clone(),
                    token: agent.token.clone(),
                    space_id: plan.space_id.clone(),
                    old_agent_id: member.id.clone(),
                    name: agent.progress.participant.name.clone(),
                    role: agent.progress.participant.role.clone(),
                    pane_id: agent.progress.participant.pane_id.clone(),
                    state: agent.progress.state,
                })
        } else {
            None
        };
        // A fresh session waiting for peers must not ingest the retained old task
        // history. This is control-plane data, never a normal addressed message.
        json!({"members":[],"tasks":[],"tasks_truncated":false,"inbox":null,"automation":null,"reset_control":control,"reset_summary":plan.summary()})
    }

    pub(super) fn claim_reset(
        &mut self,
        member: &Member,
        plan_id: &str,
        token: &str,
    ) -> Result<Value> {
        ensure!(
            self.reset_capable.contains(&member.id),
            "negotiate reset capability first"
        );
        ensure!(
            member.activity != Activity::Working,
            "participant became busy; use human recovery, not replacement"
        );
        ensure!(
            member.space_id == self.space_id,
            "participant belongs to another space"
        );
        let plan = self
            .reset
            .as_mut()
            .filter(|p| p.plan_id == plan_id && p.active())
            .ok_or_else(|| eyre!("unknown active reset plan"))?;
        let agent = plan
            .agents
            .iter_mut()
            .find(|a| {
                a.progress.participant.agent_id == member.id
                    && a.token == token
                    && a.progress.new_agent_id.is_none()
            })
            .ok_or_else(|| eyre!("invalid participant reset token"))?;
        ensure!(
            !agent.claimed,
            "replacement already claimed; do not retry newSession"
        );
        ensure!(
            agent.progress.state == ResetAgentState::Pending,
            "reset participant is blocked; use human recovery or abandon, not an automatic attempt"
        );
        agent.claimed = true;
        agent.progress.state = ResetAgentState::Claimed;
        agent.progress.note = "One replacement attempt claimed. A lost response is uncertain, not permission to retry.".into();
        Ok(json!({"claimed":true}))
    }

    pub(super) fn report_reset(
        &mut self,
        member: &Member,
        plan_id: &str,
        token: &str,
        status: ResetReport,
        text: &str,
    ) -> Result<Value> {
        validate_text(text)?;
        ensure!(
            member.space_id == self.space_id,
            "participant belongs to another space"
        );
        let plan = self
            .reset
            .as_mut()
            .filter(|p| p.plan_id == plan_id && p.active())
            .ok_or_else(|| eyre!("unknown active reset plan"))?;
        // Adapter error strings are untrusted; never let a copied control
        // envelope leak any participant ticket through public progress notes.
        let note = plan.agents.iter().fold(text.to_owned(), |note, agent| {
            note.replace(&agent.token, "[private reset token]")
        });
        let agent = plan
            .agents
            .iter_mut()
            .find(|a| {
                a.progress.participant.agent_id == member.id
                    && a.token == token
                    && a.progress.new_agent_id.is_none()
            })
            .ok_or_else(|| eyre!("invalid participant reset token"))?;
        agent.progress.state = status.into();
        agent.progress.note = note;
        plan.state = ResetPhase::Incomplete;
        plan.note = RECOVERY.into();
        Ok(json!({"recorded":true}))
    }

    pub(super) fn approved_old_join(
        &self,
        agent: &AgentId,
        name: &str,
        role: &str,
        pane: &str,
        space: &SpaceId,
    ) -> bool {
        space == &self.space_id
            && self.reset.as_ref().is_some_and(|plan| {
                plan.active()
                    && plan.agents.iter().any(|a| {
                        let approved = &a.progress.participant;
                        &approved.agent_id == agent
                            && approved.name == name
                            && approved.role == role
                            && approved.pane_id == pane
                            && a.progress.new_agent_id.is_none()
                    })
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn join_reset(
        &mut self,
        agent: &AgentId,
        name: &str,
        role: &str,
        pane: &str,
        space: &SpaceId,
        cwd: Option<&std::path::PathBuf>,
        receipt: &ResetReceipt,
        now: u64,
        role_manager: bool,
    ) -> Result<Value> {
        self.validate_space(space)?;
        ensure!(
            !self.retired.contains(agent),
            "this session identity is retired; start a genuinely fresh session"
        );
        let plan = self
            .reset
            .as_ref()
            .filter(|p| p.plan_id == receipt.plan_id && p.state != ResetPhase::Abandoned)
            .ok_or_else(|| eyre!("unknown or abandoned reset plan"))?;
        let expected = plan
            .agents
            .iter()
            .find(|a| a.token == receipt.token)
            .ok_or_else(|| eyre!("invalid participant reset token"))?;
        let approved = &expected.progress.participant;
        if role_manager {
            ensure!(
                self.members.values().all(|member| {
                    !member.manager || &member.id == agent || member.id == approved.agent_id
                }),
                "another manager is already admitted in this space"
            );
        }
        ensure!(
            expected.claimed,
            "reset token must be claimed before replacement"
        );
        ensure!(
            agent != &approved.agent_id,
            "replacement must have a fresh, different session ID"
        );
        ensure!(
            approved.name == name && approved.role == role && approved.pane_id == pane,
            "replacement does not match approved role/name/pane/space"
        );
        if let Some(confirmed) = &expected.progress.new_agent_id {
            ensure!(
                confirmed == agent,
                "reset token already confirms another session; do not replace again"
            );
            ensure!(
                self.members.get(agent).is_some_and(|current| {
                    current.id == *agent
                        && current.space_id == *space
                        && current.pane_id == approved.pane_id
                        && current.name == approved.name
                        && current.role == approved.role
                }),
                "confirmed replacement identity moved or changed; its old receipt cannot restore it"
            );
        } else {
            ensure!(
                plan.active() && !self.members.contains_key(agent),
                "replacement session identity already exists"
            );
        }
        for member in self.members.values().filter(|m| {
            m.activity != Activity::Offline && &m.id != agent && m.id != approved.agent_id
        }) {
            ensure!(
                member.pane_id != pane && (member.space_id != *space || member.name != name),
                "replacement pane or name is occupied"
            );
        }
        let old = approved.agent_id.clone();
        self.retired.insert(old.clone());
        self.reset_capable.remove(&old);
        self.reset_capable.remove(agent);
        if let Some(member) = self.members.get_mut(&old) {
            member.activity = Activity::Offline;
            member.manager = false;
            member.cwd = None;
        }
        // A confirmed receipt may be retried after transport loss. Preserve the
        // existing fresh-join behavior: refresh presence and checkout context.
        self.members.insert(
            agent.clone(),
            Member {
                id: agent.clone(),
                name: name.into(),
                role: role.into(),
                pane_id: pane.into(),
                space_id: space.clone(),
                manager: role_manager,
                cwd: cwd.cloned(),
                activity: Activity::Paused,
                last_seen: now,
                idle_since: now,
            },
        );
        let plan = self.reset.as_mut().expect("validated plan");
        let expected = plan
            .agents
            .iter_mut()
            .find(|a| a.token == receipt.token)
            .expect("validated token");
        expected.progress.new_agent_id = Some(agent.clone());
        expected.progress.state = ResetAgentState::Complete;
        expected.progress.note = "Verified fresh-session join; no old context was copied.".into();
        if plan
            .agents
            .iter()
            .all(|a| a.progress.state == ResetAgentState::Complete)
            && plan.active()
        {
            self.finish_reset();
        }
        Ok(
            json!({"protocol":VERSION,"agent_id":agent,"delivery_policy":1,"inbox_notifications":1,"reset_protocol":1,"reset_receipt":true}),
        )
    }

    fn finish_reset(&mut self) {
        let plan = self.reset.as_mut().expect("prepared plan");
        let replacements: BTreeSet<_> = plan
            .agents
            .iter()
            .filter_map(|a| a.progress.new_agent_id.clone())
            .collect();
        plan.state = ResetPhase::Completed;
        plan.note = format!(
            "Fresh start complete: {} fresh session receipt(s); {} offline historical session(s) were retired, not remotely cleared. External jobs and user files are unchanged.",
            replacements.len(),
            plan.offline_count
        );
        let old_ids: BTreeSet<_> = self
            .members
            .keys()
            .filter(|id| !replacements.contains(*id))
            .cloned()
            .collect();
        for id in &old_ids {
            self.retired.insert(id.clone());
            self.reset_capable.remove(id);
        }
        self.members.retain(|id, _| replacements.contains(id));
        self.tasks.clear();
        self.messages.clear();
        self.cache.clear();
        let generation = self.automation.generation;
        self.automation = Automation {
            generation,
            checkin_enabled: false,
            manager_checkin_at: None,
            ..Automation::default()
        };
    }

    pub fn abandon_reset(&mut self, space: &SpaceId, plan_id: &str) -> Result<()> {
        self.validate_space(space)?;
        let plan = self
            .reset
            .as_mut()
            .filter(|p| p.plan_id == plan_id && p.active())
            .ok_or_else(|| eyre!("unknown active reset plan"))?;
        plan.state = ResetPhase::Abandoned;
        plan.note = "Reset abandoned without clearing hub history or bindings. Claimed old identities are retired; use manual /new then /role and reconcile retained tasks. Modes remain off, deliveries uncertain, no work was replayed. Already-issued replacements may still finish but their receipts are rejected; do not retry them.".into();
        for member in self.members.values_mut() {
            member.manager = false;
        }
        for agent in &plan.agents {
            let old = &agent.progress.participant.agent_id;
            if agent.claimed {
                self.retired.insert(old.clone());
            }
            self.reset_capable.remove(old);
            if let Some(member) = self.members.get_mut(old) {
                member.activity = Activity::Offline;
                member.manager = false;
            }
        }
        for message in self
            .messages
            .iter_mut()
            .filter(|m| matches!(m.delivery, Delivery::Pending | Delivery::Offered))
        {
            message.delivery = Delivery::Uncertain;
            message.pr_ack = None;
        }
        for task in self
            .tasks
            .values_mut()
            .filter(|t| matches!(t.state, TaskState::Assigned | TaskState::Active))
        {
            task.state = TaskState::Paused;
            task.note = "Fresh start abandoned. Reconcile files/external jobs before explicit guidance; no work was replayed.".into();
        }
        let automation = &mut self.automation;
        automation.generation = automation
            .generation
            .checked_add(1)
            .ok_or_else(|| eyre!("watcher generation exhausted"))?;
        automation.todo_enabled = false;
        automation.checkin_enabled = false;
        automation.manager_checkin_at = None;
        automation.pr_enabled = false;
        Ok(())
    }

    pub(super) fn recover_resets(&mut self) {
        if let Some(plan) = self.reset.as_mut().filter(|p| p.active()) {
            plan.state = ResetPhase::Incomplete;
            plan.note = RECOVERY.into();
            for agent in &mut plan.agents {
                agent.progress.state = if agent.claimed {
                    ResetAgentState::Uncertain
                } else {
                    ResetAgentState::Blocked
                };
                agent.progress.note = "Hub restarted. Reconcile this pane; a prior claim never authorizes another replacement.".into();
            }
        }
    }

    pub(super) fn disconnect_reset(&mut self, id: &AgentId) {
        if let Some(plan) = self.reset.as_mut().filter(|p| p.active()) {
            for agent in &mut plan.agents {
                let current = agent
                    .progress
                    .new_agent_id
                    .as_ref()
                    .unwrap_or(&agent.progress.participant.agent_id);
                if current == id {
                    agent.progress.state = if agent.claimed {
                        ResetAgentState::Uncertain
                    } else {
                        ResetAgentState::Blocked
                    };
                    agent.progress.note = "Participant disconnected; reconcile its actual session before manual recovery.".into();
                    plan.state = ResetPhase::Incomplete;
                    plan.note = RECOVERY.into();
                }
            }
        }
    }
}
