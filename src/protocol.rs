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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Automation {
    pub todo_enabled: bool,
    pub checkin_enabled: bool,
    pub pr_enabled: bool,
    pub generation: u64,
    pub todo: Option<crate::watchers::TodoWatch>,
    pub pr: Option<crate::watchers::PrWatch>,
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
}
impl Response {
    pub fn success(id: String, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn failure(id: String, error: impl ToString) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.to_string()),
        }
    }
}
