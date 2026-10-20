//! Versioned JSON-lines protocol. Connections bind a sender at registration.
//! All model text is data, never shell input or privileged instructions.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const VERSION: u32 = 1;
pub(crate) const MAX_FRAME: usize = 256 * 1024;
pub(crate) const MAX_TEXT: usize = 6000;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.as_str().fmt(f)
            }
        }
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }
        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }
    };
}
id_type!(AgentId);
id_type!(SpaceId);
id_type!(TaskId);
id_type!(MessageId);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Request {
    pub id: String,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Join {
        version: u32,
        agent_id: AgentId,
        name: String,
        role: String,
        pane_id: String,
        space_id: SpaceId,
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
        #[serde(default)]
        reset: Option<ResetReceipt>,
    },
    /// Negotiate the single-attempt reset control protocol for this connection.
    ResetCapability,
    ResetClaim {
        plan_id: String,
        token: String,
    },
    ResetStatus {
        plan_id: String,
        token: String,
        status: ResetReport,
        text: String,
    },
    /// Opt in to content-free inbox-ready socket notifications.
    Subscribe,
    Sync {
        activity: Activity,
        receive: bool,
    },
    Send {
        to: AgentId,
        text: String,
        wake: bool,
    },
    Assign {
        to: AgentId,
        title: String,
        text: String,
    },
    Report {
        task_id: TaskId,
        revision: u32,
        kind: ReportKind,
        text: String,
    },
    Guide {
        task_id: TaskId,
        revision: u32,
        text: String,
    },
    Accept {
        task_id: TaskId,
        revision: u32,
        text: String,
    },
    Cancel {
        task_id: TaskId,
        revision: u32,
        text: String,
    },
    Ack {
        message_id: MessageId,
    },
    /// Release an offer only when the adapter has not attempted Pi insertion.
    Defer {
        message_id: MessageId,
    },
    ConfigureTodo {
        path: String,
    },
    ConfigurePr {
        url: String,
        cwd: String,
    },
    Mode {
        mode: Mode,
        enabled: bool,
    },
    Leave,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResetReceipt {
    pub plan_id: String,
    pub token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResetParticipant {
    pub agent_id: AgentId,
    pub name: String,
    pub role: String,
    pub pane_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResetPreview {
    pub space_id: SpaceId,
    pub nonce: String,
    pub participants: Vec<ResetParticipant>,
    pub task_count: usize,
    pub message_count: usize,
    pub offline_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResetPhase {
    Prepared,
    Incomplete,
    Completed,
    Abandoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResetAgentState {
    Pending,
    Claimed,
    Blocked,
    Cancelled,
    Failed,
    Uncertain,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResetReport {
    Blocked,
    Cancelled,
    Failed,
    Uncertain,
}

impl From<ResetReport> for ResetAgentState {
    fn from(value: ResetReport) -> Self {
        match value {
            ResetReport::Blocked => Self::Blocked,
            ResetReport::Cancelled => Self::Cancelled,
            ResetReport::Failed => Self::Failed,
            ResetReport::Uncertain => Self::Uncertain,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResetProgress {
    pub participant: ResetParticipant,
    pub state: ResetAgentState,
    pub new_agent_id: Option<AgentId>,
    pub note: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResetSummary {
    pub plan_id: String,
    pub space_id: SpaceId,
    pub state: ResetPhase,
    pub participants: Vec<ResetProgress>,
    pub note: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ResetControl {
    pub plan_id: String,
    pub token: String,
    pub space_id: SpaceId,
    pub old_agent_id: AgentId,
    pub name: String,
    pub role: String,
    pub pane_id: String,
    pub state: ResetAgentState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Activity {
    Idle,
    Working,
    Paused,
    Offline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportKind {
    Status,
    Progress,
    Blocked,
    Waiting,
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskState {
    Assigned,
    Active,
    Blocked,
    Waiting,
    Review,
    Paused,
    Accepted,
    Cancelled,
}
impl TaskState {
    pub fn finished(self) -> bool {
        matches!(self, Self::Accepted | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Delivery {
    Pending,
    Offered,
    Received,
    Uncertain,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageKind {
    Request,
    Info,
    Assignment,
    Guide,
    Status,
    Blocked,
    Done,
    Checkin,
    Continue,
    Todo,
    Pr,
}
impl MessageKind {
    pub fn automatic(self) -> bool {
        matches!(self, Self::Checkin | Self::Continue | Self::Todo | Self::Pr)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Member {
    pub id: AgentId,
    pub name: String,
    pub role: String,
    pub pane_id: String,
    pub space_id: SpaceId,
    /// Server-validated role metadata for the currently admitted manager.
    #[serde(default)]
    pub manager: bool,
    /// Live checkout context for explicit dashboard PR-discovery selection.
    /// Do not persist or expose it to peer/model snapshots; fresh Join supplies it.
    #[serde(default, skip_serializing)]
    pub cwd: Option<std::path::PathBuf>,
    pub activity: Activity,
    pub last_seen: u64,
    pub idle_since: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Task {
    pub id: TaskId,
    pub space_id: SpaceId,
    pub requester: AgentId,
    pub owner: AgentId,
    pub title: String,
    pub description: String,
    pub state: TaskState,
    pub revision: u32,
    pub updated_at: u64,
    pub checkin_at: u64,
    pub nudges: u32,
    pub note: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Message {
    pub id: MessageId,
    pub space_id: SpaceId,
    pub from: Option<AgentId>,
    pub to: AgentId,
    pub task_id: Option<TaskId>,
    pub revision: Option<u32>,
    pub kind: MessageKind,
    pub text: String,
    pub wake: bool,
    pub delivery: Delivery,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_ack: Option<crate::watchers::PrBaseline>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Todo,
    Checkin,
    Pr,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Automation {
    pub todo_enabled: bool,
    pub checkin_enabled: bool,
    /// Next eligible per-space manager checkpoint; cleared on disable/recovery.
    #[serde(default)]
    pub manager_checkin_at: Option<u64>,
    pub pr_enabled: bool,
    pub generation: u64,
    pub todo: Option<crate::watchers::TodoWatch>,
    pub pr: Option<crate::watchers::PrWatch>,
}

impl Default for Automation {
    fn default() -> Self {
        Self {
            todo_enabled: false,
            checkin_enabled: true,
            manager_checkin_at: None,
            pr_enabled: false,
            generation: 0,
            todo: None,
            pr: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Space {
    pub id: SpaceId,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub space_id: SpaceId,
    /// Live herdr metadata. The identity remains available if the space disappears.
    pub space: Option<Space>,
    pub members: Vec<Member>,
    pub tasks: Vec<Task>,
    pub messages: Vec<Message>,
    /// Actor-internal readiness over the full queue, not the bounded UI preview.
    #[serde(skip)]
    pub ready: std::collections::BTreeMap<AgentId, MessageId>,
    pub automation: Automation,
    pub reset: Option<ResetSummary>,
    pub epoch: u64,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn empty(space_id: SpaceId) -> Self {
        Self {
            space_id,
            space: None,
            members: Vec::new(),
            tasks: Vec::new(),
            messages: Vec::new(),
            ready: std::collections::BTreeMap::new(),
            automation: Automation::default(),
            reset: None,
            epoch: 0,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Present only when live identity could not be checked and the request was
    /// definitely rejected before actor execution. Not a command retry policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
impl Response {
    pub fn success(id: String, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
            error_code: None,
        }
    }
    pub fn failure(id: String, error: impl ToString) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.to_string()),
            error_code: None,
        }
    }

    pub fn identity_unavailable(id: String, error: impl ToString) -> Self {
        Self {
            error_code: Some("identity_unavailable".into()),
            ..Self::failure(id, error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_unavailable_is_an_optional_preexecution_error_code() {
        let legacy: Response =
            serde_json::from_value(json!({"id":"old", "ok":false, "error":"rejected"})).unwrap();
        assert!(legacy.error_code.is_none());
        for response in [
            Response::success("ok".into(), json!({})),
            Response::failure("no".into(), "rejected"),
        ] {
            assert!(
                serde_json::to_value(response)
                    .unwrap()
                    .get("error_code")
                    .is_none()
            );
        }
        let response = Response::identity_unavailable("retry-topology".into(), "herdr timed out");
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["error_code"], "identity_unavailable");
        assert!(!response.ok);
        assert!(response.result.is_none());
        assert_eq!(
            serde_json::from_value::<Response>(value)
                .unwrap()
                .error_code,
            response.error_code
        );
    }

    #[test]
    fn legacy_join_and_member_payloads_default_checkout_context() {
        let request: Request = serde_json::from_value(json!({
            "id":"join",
            "op":"join",
            "version":1,
            "agent_id":"agent",
            "name":"worker",
            "role":"custom",
            "pane_id":"w1:p1",
            "space_id":"w1"
        }))
        .unwrap();
        assert!(matches!(request.command, Command::Join { cwd: None, .. }));

        let member: Member = serde_json::from_value(json!({
            "id":"agent",
            "name":"worker",
            "role":"custom",
            "pane_id":"w1:p1",
            "space_id":"w1",
            "activity":"idle",
            "last_seen":1,
            "idle_since":1
        }))
        .unwrap();
        assert_eq!(member.cwd, None);
        assert!(!member.manager, "legacy member records are not managers");
        let automation = Automation::default();
        assert!(automation.checkin_enabled);
        assert!(!automation.todo_enabled);
        assert!(!automation.pr_enabled);
    }

    #[test]
    fn public_join_cannot_claim_manager_metadata() {
        let request: Result<Request, _> = serde_json::from_value(json!({
            "id":"join",
            "op":"join",
            "version":1,
            "agent_id":"agent",
            "name":"worker",
            "role":"custom",
            "pane_id":"w1:p1",
            "space_id":"w1",
            "manager":true
        }));
        assert!(request.is_err(), "Join denies unrecognized client metadata");
    }

    #[test]
    fn checkout_context_is_not_serialized_to_peers_or_state() {
        let member = Member {
            id: AgentId::new("agent"),
            name: "worker".into(),
            role: "custom".into(),
            pane_id: "w1:p1".into(),
            space_id: SpaceId::new("w1"),
            manager: true,
            cwd: Some("/private/checkout".into()),
            activity: Activity::Idle,
            last_seen: 1,
            idle_since: 1,
        };
        let encoded = serde_json::to_value(member).unwrap();
        assert!(encoded.get("cwd").is_none());
        assert_eq!(
            encoded["manager"], true,
            "manager presence is surfaced to the UI"
        );
    }
}
