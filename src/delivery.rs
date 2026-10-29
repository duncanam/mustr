//! Delivery policy and consuming transition capabilities.
//!
//! JSON/SQLite state must be checked at runtime. Once checked, private-field,
//! non-Clone guards prevent offering a received message or deferring a receipt.
//! These guards mutate only the actor's candidate state; persistence remains the
//! transaction boundary. They do not claim to prove what an external Pi executed.

use color_eyre::eyre::{Result, ensure};
use serde::Serialize;

use std::collections::BTreeMap;

use crate::{
    protocol::{
        Activity, AgentId, Automation, Delivery, Message, MessageKind, SpaceId, Task, TaskId,
        TaskState,
    },
    watchers::PrBaseline,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InsertionPolicy {
    IdleOnly,
    Live,
}

impl InsertionPolicy {
    pub fn for_kind(kind: MessageKind) -> Self {
        match kind {
            MessageKind::Request
            | MessageKind::Info
            | MessageKind::Status
            | MessageKind::Blocked
            | MessageKind::Done => Self::Live,
            MessageKind::Assignment
            | MessageKind::Guide
            | MessageKind::Checkin
            | MessageKind::Continue
            | MessageKind::Todo
            | MessageKind::Pr => Self::IdleOnly,
        }
    }

    pub fn allows(self, activity: Activity) -> bool {
        match activity {
            Activity::Idle => true,
            Activity::Working => self == Self::Live,
            Activity::Paused | Activity::Offline => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Pending<'a>(&'a mut Message);
#[derive(Debug)]
pub(crate) struct Offered<'a>(&'a mut Message);
/// Receipt-only effects. Cannot be constructed without consuming an offer;
/// applying it consumes the capability, so activation/baselines happen once.
#[derive(Debug)]
pub(crate) struct Received {
    assignment: Option<(TaskId, u32)>,
    baseline: Option<PrBaseline>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Deferral {
    Requeue,
    Discard,
}

impl<'a> Pending<'a> {
    pub fn eligible(
        message: &Message,
        recipient: &AgentId,
        space: &SpaceId,
        activity: Activity,
        now: u64,
    ) -> bool {
        &message.to == recipient
            && &message.space_id == space
            && message.delivery == Delivery::Pending
            && InsertionPolicy::for_kind(message.kind).allows(activity)
            && (message.kind != MessageKind::Pr || now.saturating_sub(message.created_at) <= 5)
    }

    pub fn new(message: &'a mut Message) -> Result<Self> {
        ensure!(
            message.delivery == Delivery::Pending,
            "message is not pending"
        );
        Ok(Self(message))
    }

    #[must_use]
    pub fn offer(self) -> Offered<'a> {
        self.0.delivery = Delivery::Offered;
        Offered(self.0)
    }
}

impl<'a> Offered<'a> {
    /// Validate recipient ownership before constructing a transition capability.
    pub fn for_recipient(
        message: &'a mut Message,
        recipient: &AgentId,
        space: &SpaceId,
    ) -> Result<Self> {
        ensure!(
            &message.to == recipient && &message.space_id == space,
            "unknown recipient message"
        );
        ensure!(
            message.delivery == Delivery::Offered,
            "message was not offered on this session"
        );
        Ok(Self(message))
    }

    pub fn envelope(&self) -> Message {
        let mut message = self.0.clone();
        message.pr_ack = None;
        message
    }

    #[must_use]
    pub fn acknowledge(self) -> Received {
        self.0.delivery = Delivery::Received;
        let assignment = if matches!(self.0.kind, MessageKind::Assignment | MessageKind::Guide) {
            self.0.task_id.clone().zip(self.0.revision)
        } else {
            None
        };
        Received {
            assignment,
            baseline: self.0.pr_ack.take(),
        }
    }

    /// Only the adapter that has NOT attempted Pi insertion may call defer.
    /// An uncertain message cannot acquire this capability and is never replayed.
    pub fn defer(self, disposition: Deferral) {
        self.0.delivery = match disposition {
            Deferral::Requeue => Delivery::Pending,
            Deferral::Discard => {
                self.0.pr_ack = None;
                Delivery::Cancelled
            }
        };
    }
}

impl Received {
    pub fn apply(
        self,
        tasks: &mut BTreeMap<TaskId, Task>,
        automation: Option<&mut Automation>,
        now: u64,
    ) {
        if let Some((id, revision)) = self.assignment
            && let Some(task) = tasks.get_mut(&id)
            && task.state == TaskState::Assigned
            && task.revision == revision
        {
            task.state = TaskState::Active;
            task.updated_at = now;
        }
        if let Some(baseline) = self.baseline
            && let Some(pr) = automation.and_then(|a| a.pr.as_mut())
        {
            pr.baseline = Some(baseline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MessageId;

    fn message(delivery: Delivery) -> Message {
        Message {
            id: MessageId::new("m"),
            space_id: SpaceId::new("s"),
            from: None,
            to: AgentId::new("a"),
            task_id: None,
            revision: None,
            kind: MessageKind::Info,
            text: "hello".into(),
            wake: false,
            delivery,
            created_at: 0,
            pr_ack: None,
            context: None,
        }
    }

    #[test]
    fn every_kind_has_an_explicit_activity_policy() {
        for kind in [
            MessageKind::Request,
            MessageKind::Info,
            MessageKind::Status,
            MessageKind::Blocked,
            MessageKind::Done,
            MessageKind::Assignment,
            MessageKind::Guide,
            MessageKind::Checkin,
            MessageKind::Continue,
            MessageKind::Todo,
            MessageKind::Pr,
        ] {
            let policy = InsertionPolicy::for_kind(kind);
            assert!(policy.allows(Activity::Idle));
            assert!(!policy.allows(Activity::Paused));
            assert!(!policy.allows(Activity::Offline));
            assert_eq!(
                policy.allows(Activity::Working),
                matches!(
                    kind,
                    MessageKind::Request
                        | MessageKind::Info
                        | MessageKind::Status
                        | MessageKind::Blocked
                        | MessageKind::Done
                )
            );
        }
    }

    #[test]
    fn transition_capabilities_validate_wire_states_and_recipient() {
        for state in [
            Delivery::Pending,
            Delivery::Offered,
            Delivery::Received,
            Delivery::Uncertain,
            Delivery::Cancelled,
        ] {
            let mut m = message(state);
            assert_eq!(Pending::new(&mut m).is_ok(), state == Delivery::Pending);
            assert_eq!(
                Offered::for_recipient(&mut m, &AgentId::new("a"), &SpaceId::new("s")).is_ok(),
                state == Delivery::Offered
            );
            assert!(
                Offered::for_recipient(&mut m, &AgentId::new("other"), &SpaceId::new("s")).is_err()
            );
            assert!(
                Offered::for_recipient(&mut m, &AgentId::new("a"), &SpaceId::new("other")).is_err()
            );
        }
        let mut m = message(Delivery::Pending);
        Pending::new(&mut m)
            .unwrap()
            .offer()
            .defer(Deferral::Requeue);
        assert_eq!(m.delivery, Delivery::Pending);
        let received = Pending::new(&mut m).unwrap().offer().acknowledge();
        assert_eq!(m.delivery, Delivery::Received);
        received.apply(&mut BTreeMap::new(), None, 1);
    }
}
