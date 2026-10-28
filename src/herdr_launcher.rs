use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    path::Path,
    process::{ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize, de::IgnoredAny};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

use crate::{
    herdr::Herdr,
    protocol::{SpaceId, StartupGrant},
    roles,
};

const STARTUP_FLAG_NAME: &str = "mustr-startup";
const STARTUP_FLAG_JSON_LIMIT: usize = 149;
const STARTUP_INTENT_FIXED_BYTES: usize = r#"{"launch_id":"","role":"","token":""}"#.len();
const INVALID_STARTUP_HANDOFF: &str = "invalid startup handoff";
const STARTUP_HANDOFF_TOO_LONG: &str = "startup handoff exceeds 149 UTF-8 bytes";

#[derive(Debug)]
pub(crate) struct HerdrAgentName(String);

impl HerdrAgentName {
    pub(crate) fn random() -> Self {
        let uuid = Uuid::new_v4().simple().to_string();
        Self(format!("mustr-{}", &uuid[..26]))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Serialize)]
struct StartupIntent<'a> {
    launch_id: &'a str,
    role: &'a str,
    token: &'a str,
}

pub(crate) struct StartupFlagArgument(String);

impl StartupFlagArgument {
    pub(crate) fn new(grant: &StartupGrant, role: &str) -> Result<Self, &'static str> {
        if grant.version != 1 || !roles::valid_name(role) {
            return Err(INVALID_STARTUP_HANDOFF);
        }

        let payload_minimum_bytes = STARTUP_INTENT_FIXED_BYTES
            .saturating_add(grant.launch_id.as_str().len())
            .saturating_add(role.len())
            .saturating_add(grant.token.as_str().len());
        if payload_minimum_bytes > STARTUP_FLAG_JSON_LIMIT {
            return Err(STARTUP_HANDOFF_TOO_LONG);
        }
        if !is_compact_uuid32(grant.launch_id.as_str()) || !is_compact_uuid32(grant.token.as_str())
        {
            return Err(INVALID_STARTUP_HANDOFF);
        }

        let payload = serde_json::to_string(&StartupIntent {
            launch_id: grant.launch_id.as_str(),
            role,
            token: grant.token.as_str(),
        })
        .map_err(|_| INVALID_STARTUP_HANDOFF)?;
        if payload.len() > STARTUP_FLAG_JSON_LIMIT {
            return Err(STARTUP_HANDOFF_TOO_LONG);
        }

        Ok(Self(format!("--{STARTUP_FLAG_NAME}={payload}")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StartupFlagArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StartupFlagArgument([REDACTED])")
    }
}

fn is_compact_uuid32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

const SUPPORTED_HERDR_VERSION: &str = "herdr 0.9.5";
const SHORT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const AGENT_COMMAND_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_CAPTURED_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RootPane {
    pane_id: String,
    workspace_id: SpaceId,
}

impl RootPane {
    pub(crate) fn pane_id(&self) -> &str {
        &self.pane_id
    }

    pub(crate) fn workspace_id(&self) -> &SpaceId {
        &self.workspace_id
    }
}

/// The only noncredential environment values forwarded to profile-started shells.
pub(crate) struct TabBootstrap {
    mustr_bin: String,
    config_dir: String,
}

impl TabBootstrap {
    pub(crate) fn new(mustr_bin: &Path, config_dir: &Path) -> Result<Self, &'static str> {
        Ok(Self {
            mustr_bin: absolute_utf8_arg(mustr_bin)?,
            config_dir: absolute_utf8_arg(config_dir)?,
        })
    }

    fn herdr_env_args(&self) -> [OsString; 4] {
        [
            "--env".into(),
            format!("MUSTR_BIN={}", self.mustr_bin).into(),
            "--env".into(),
            format!("MUSTR_CONFIG_DIR={}", self.config_dir).into(),
        ]
    }
}

fn absolute_utf8_arg(path: &Path) -> Result<String, &'static str> {
    if !path.is_absolute() {
        return Err("Herdr launch paths must be absolute UTF-8 arguments without NUL bytes");
    }
    let value = path
        .to_str()
        .filter(|value| !value.contains('\0'))
        .ok_or("Herdr launch paths must be absolute UTF-8 arguments without NUL bytes")?;
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreexecutionRejection {
    AgentNameTaken,
    InvalidAgentName,
    AgentPaneNotFound,
    CanonicalInputTooLong,
    ExecutableUnavailable,
    InvalidArgument,
    VersionNotVerified,
}

impl fmt::Display for PreexecutionRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AgentNameTaken => "Herdr rejected the agent name as already in use",
            Self::InvalidAgentName => "Herdr rejected the generated agent name",
            Self::AgentPaneNotFound => "Herdr could not find the selected pane",
            Self::CanonicalInputTooLong => {
                "Herdr rejected the startup input for the terminal canonical-mode limit"
            }
            Self::ExecutableUnavailable => "Herdr CLI could not be started",
            Self::InvalidArgument => {
                "Herdr launch paths must be absolute UTF-8 arguments without NUL bytes"
            }
            Self::VersionNotVerified => "Herdr CLI version was not verified; no launch command ran",
        })
    }
}

#[must_use = "inspect every Herdr mutation outcome; uncertain outcomes are never safe to retry"]
pub(crate) enum MutationOutcome<T> {
    Completed(T),
    Rejected(PreexecutionRejection),
    Uncertain(Option<PendingCommand>),
}

impl<T: fmt::Debug> fmt::Debug for MutationOutcome<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed(value) => formatter.debug_tuple("Completed").field(value).finish(),
            Self::Rejected(reason) => formatter.debug_tuple("Rejected").field(reason).finish(),
            Self::Uncertain(pending) => formatter
                .debug_struct("Uncertain")
                .field("reconciliation_handle_retained", &pending.is_some())
                .finish(),
        }
    }
}

pub(crate) enum PreflightError {
    Unavailable,
    UnsupportedVersion,
    Uncertain(Option<Box<PendingCommand>>),
}

impl fmt::Debug for PreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("PreflightError::Unavailable"),
            Self::UnsupportedVersion => formatter.write_str("PreflightError::UnsupportedVersion"),
            Self::Uncertain(pending) => formatter
                .debug_struct("PreflightError::Uncertain")
                .field("reconciliation_handle_retained", &pending.is_some())
                .finish(),
        }
    }
}

impl fmt::Display for PreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "Herdr CLI version check could not run",
            Self::UnsupportedVersion => "Herdr CLI version 0.9.5 is required",
            Self::Uncertain(_) => {
                "Herdr CLI version could not be verified; inspect it before starting participants"
            }
        })
    }
}

impl std::error::Error for PreflightError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingCommandError {
    ChildWaitFailed,
    CaptureFailed,
}

impl fmt::Display for PendingCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ChildWaitFailed => "local Herdr child exit could not be confirmed",
            Self::CaptureFailed => "local Herdr output capture did not complete",
        })
    }
}

impl std::error::Error for PendingCommandError {}

#[must_use = "retain and reconcile this local Herdr command; do not retry"]
pub(crate) struct PendingCommand {
    child: Child,
    stdout: CapturedStreamState,
    stderr: CapturedStreamState,
    exit_status: Option<ExitStatus>,
}

impl PendingCommand {
    async fn wait_for_child(&mut self) -> Result<ExitStatus, PendingCommandError> {
        if let Some(status) = self.exit_status {
            return Ok(status);
        }
        let status = self
            .child
            .wait()
            .await
            .map_err(|_| PendingCommandError::ChildWaitFailed)?;
        self.exit_status = Some(status);
        Ok(status)
    }

    async fn collect_output(&mut self) -> Result<CliOutput, PendingCommandError> {
        let status = self.wait_for_child().await?;
        self.stdout.wait().await?;
        self.stderr.wait().await?;
        Ok(CliOutput {
            status,
            stdout: self.stdout.take()?,
            stderr: self.stderr.take()?,
        })
    }

    pub(crate) async fn wait_for_local_exit(&mut self) -> Result<(), PendingCommandError> {
        self.wait_for_child().await?;
        self.stdout.wait().await?;
        self.stderr.wait().await?;
        if self.stdout.failed() || self.stderr.failed() {
            return Err(PendingCommandError::CaptureFailed);
        }
        Ok(())
    }
}

enum CapturedStreamState {
    Pending(JoinHandle<CapturedStream>),
    Complete(Box<CapturedStream>),
    Failed,
    Consumed,
}

impl CapturedStreamState {
    async fn wait(&mut self) -> Result<(), PendingCommandError> {
        match self {
            Self::Pending(task) => match (&mut *task).await {
                Ok(captured) => {
                    *self = Self::Complete(Box::new(captured));
                    Ok(())
                }
                Err(_) => {
                    *self = Self::Failed;
                    Err(PendingCommandError::CaptureFailed)
                }
            },
            Self::Complete(_) => Ok(()),
            Self::Failed | Self::Consumed => Err(PendingCommandError::CaptureFailed),
        }
    }

    fn take(&mut self) -> Result<CapturedStream, PendingCommandError> {
        match std::mem::replace(self, Self::Consumed) {
            Self::Complete(captured) => Ok(*captured),
            Self::Pending(task) => {
                *self = Self::Pending(task);
                Err(PendingCommandError::CaptureFailed)
            }
            Self::Failed => {
                *self = Self::Failed;
                Err(PendingCommandError::CaptureFailed)
            }
            Self::Consumed => {
                *self = Self::Consumed;
                Err(PendingCommandError::CaptureFailed)
            }
        }
    }

    fn failed(&self) -> bool {
        match self {
            Self::Failed => true,
            Self::Complete(captured) => captured.failed,
            Self::Pending(_) | Self::Consumed => false,
        }
    }
}

struct CapturedStream {
    bytes: Vec<u8>,
    over_limit: bool,
    failed: bool,
}

async fn capture_stream<R>(mut stream: R) -> CapturedStream
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut bytes = Vec::new();
    let mut over_limit = false;
    let mut failed = false;
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                let available = MAX_CAPTURED_OUTPUT_BYTES.saturating_sub(bytes.len());
                let retained = count.min(available);
                bytes.extend_from_slice(&buffer[..retained]);
                over_limit |= retained != count;
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    CapturedStream {
        bytes,
        over_limit,
        failed,
    }
}

struct CliOutput {
    status: ExitStatus,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

impl CliOutput {
    fn is_complete(&self) -> bool {
        !self.stdout.over_limit
            && !self.stdout.failed
            && !self.stderr.over_limit
            && !self.stderr.failed
    }
}

enum CommandRun {
    Completed(CliOutput),
    SpawnFailed,
    Pending(PendingCommand),
}

pub(crate) struct HerdrLauncher {
    program: OsString,
    herdr: Herdr,
    version_verified: AtomicBool,
    short_timeout: Duration,
    agent_timeout: Duration,
    #[cfg(test)]
    test_environment: Vec<(OsString, OsString)>,
    #[cfg(test)]
    test_startup_argument_observer: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    #[cfg(test)]
    test_pending_command_observer: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl HerdrLauncher {
    pub(crate) fn new(herdr: &Herdr) -> Self {
        Self {
            program: OsString::from("herdr"),
            herdr: herdr.clone(),
            version_verified: AtomicBool::new(false),
            short_timeout: SHORT_COMMAND_TIMEOUT,
            agent_timeout: AGENT_COMMAND_TIMEOUT,
            #[cfg(test)]
            test_environment: Vec::new(),
            #[cfg(test)]
            test_startup_argument_observer: None,
            #[cfg(test)]
            test_pending_command_observer: None,
        }
    }

    #[cfg(test)]
    fn with_test_program(
        herdr: &Herdr,
        program: OsString,
        short_timeout: Duration,
        agent_timeout: Duration,
        test_environment: Vec<(OsString, OsString)>,
    ) -> Self {
        Self {
            program,
            herdr: herdr.clone(),
            version_verified: AtomicBool::new(false),
            short_timeout,
            agent_timeout,
            test_environment,
            test_startup_argument_observer: None,
            test_pending_command_observer: None,
        }
    }

    #[cfg(test)]
    fn observe_startup_arguments(
        mut self,
        observer: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Self {
        self.test_startup_argument_observer = Some(observer);
        self
    }

    #[cfg(test)]
    fn observe_pending_commands(
        mut self,
        observer: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Self {
        self.test_pending_command_observer = Some(observer);
        self
    }

    fn command(&self, args: &[OsString]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .env("HERDR_SOCKET_PATH", self.herdr.socket_path().as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        #[cfg(test)]
        for (key, value) in &self.test_environment {
            command.env(key.as_os_str(), value.as_os_str());
        }
        command
    }

    async fn run_command(&self, args: &[OsString], budget: Duration) -> CommandRun {
        let mut child = match self.command(args).spawn() {
            Ok(child) => child,
            Err(_) => return CommandRun::SpawnFailed,
        };
        let stdout = child
            .stdout
            .take()
            .map_or(CapturedStreamState::Failed, |stream| {
                CapturedStreamState::Pending(tokio::spawn(capture_stream(stream)))
            });
        let stderr = child
            .stderr
            .take()
            .map_or(CapturedStreamState::Failed, |stream| {
                CapturedStreamState::Pending(tokio::spawn(capture_stream(stream)))
            });
        let mut pending = PendingCommand {
            child,
            stdout,
            stderr,
            exit_status: None,
        };
        match timeout(budget, pending.collect_output()).await {
            Ok(Ok(output)) => CommandRun::Completed(output),
            Ok(Err(_)) | Err(_) => {
                #[cfg(test)]
                if let Some(observer) = &self.test_pending_command_observer
                    && let Some(command) = args.first()
                {
                    let _ = observer.send(command.to_string_lossy().into_owned());
                }
                CommandRun::Pending(pending)
            }
        }
    }

    pub(crate) async fn preflight(&self) -> Result<(), PreflightError> {
        self.version_verified.store(false, Ordering::Release);
        if self.herdr.check_instance().is_err() {
            return Err(PreflightError::Uncertain(None));
        }
        let args = [OsString::from("--version")];
        let output = match self.run_command(&args, self.short_timeout).await {
            CommandRun::Completed(output) => output,
            CommandRun::SpawnFailed => return Err(PreflightError::Unavailable),
            CommandRun::Pending(pending) => {
                let _ = self.herdr.check_instance();
                return Err(PreflightError::Uncertain(Some(Box::new(pending))));
            }
        };
        if self.herdr.check_instance().is_err() {
            return Err(PreflightError::Uncertain(None));
        }
        match supported_version(&output) {
            Some(true) => {
                self.version_verified.store(true, Ordering::Release);
                Ok(())
            }
            Some(false) => Err(PreflightError::UnsupportedVersion),
            None => Err(PreflightError::Uncertain(None)),
        }
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "retained no-bootstrap tab creation path used by launcher tests"
        )
    )]
    #[cfg(test)]
    pub(crate) async fn create_tab(
        &self,
        workspace: &SpaceId,
        cwd: &Path,
    ) -> MutationOutcome<RootPane> {
        self.create_tab_inner(workspace, cwd, None).await
    }

    pub(crate) async fn create_tab_with_bootstrap(
        &self,
        workspace: &SpaceId,
        cwd: &Path,
        bootstrap: &TabBootstrap,
    ) -> MutationOutcome<RootPane> {
        self.create_tab_inner(workspace, cwd, Some(bootstrap)).await
    }

    async fn create_tab_inner(
        &self,
        workspace: &SpaceId,
        cwd: &Path,
        bootstrap: Option<&TabBootstrap>,
    ) -> MutationOutcome<RootPane> {
        if !self.version_verified.load(Ordering::Acquire) {
            return MutationOutcome::Rejected(PreexecutionRejection::VersionNotVerified);
        }
        if self.herdr.check_instance().is_err() {
            return MutationOutcome::Uncertain(None);
        }
        let Ok(cwd) = absolute_utf8_arg(cwd) else {
            return MutationOutcome::Rejected(PreexecutionRejection::InvalidArgument);
        };
        if workspace.as_str().is_empty() || workspace.as_str().contains('\0') {
            return MutationOutcome::Rejected(PreexecutionRejection::InvalidArgument);
        }
        let mut args = vec![
            "tab".into(),
            "create".into(),
            "--workspace".into(),
            workspace.as_str().into(),
            "--cwd".into(),
            cwd.into(),
        ];
        if let Some(bootstrap) = bootstrap {
            args.extend(bootstrap.herdr_env_args());
        }
        let output = match self.run_command(&args, self.short_timeout).await {
            CommandRun::Completed(output) => output,
            CommandRun::SpawnFailed => {
                return MutationOutcome::Rejected(PreexecutionRejection::ExecutableUnavailable);
            }
            CommandRun::Pending(pending) => {
                let _ = self.herdr.check_instance();
                return MutationOutcome::Uncertain(Some(pending));
            }
        };
        if self.herdr.check_instance().is_err() {
            return MutationOutcome::Uncertain(None);
        }
        let Some(response) = success_response(&output) else {
            return MutationOutcome::Uncertain(None);
        };
        let MutationResult::TabCreated { root_pane, .. } = response.result else {
            return MutationOutcome::Uncertain(None);
        };
        if root_pane.pane_id.is_empty() || root_pane.workspace_id != workspace.as_str() {
            return MutationOutcome::Uncertain(None);
        }
        let root = RootPane {
            pane_id: root_pane.pane_id,
            workspace_id: SpaceId::new(root_pane.workspace_id),
        };
        if !self.root_pane_is_live(&root).await {
            return MutationOutcome::Uncertain(None);
        }
        MutationOutcome::Completed(root)
    }

    pub(crate) async fn start_pi(
        &self,
        name: &HerdrAgentName,
        root: &RootPane,
        startup_argument: &StartupFlagArgument,
    ) -> MutationOutcome<()> {
        if !self.version_verified.load(Ordering::Acquire) {
            return MutationOutcome::Rejected(PreexecutionRejection::VersionNotVerified);
        }
        if !self.root_pane_is_live(root).await {
            return MutationOutcome::Uncertain(None);
        }
        let args = [
            "agent".into(),
            "start".into(),
            name.as_str().into(),
            "--kind".into(),
            "pi".into(),
            "--pane".into(),
            root.pane_id().into(),
            "--timeout".into(),
            "30000".into(),
            "--".into(),
            startup_argument.as_str().into(),
        ];
        #[cfg(test)]
        if let Some(observer) = &self.test_startup_argument_observer {
            let _ = observer.send(startup_argument.as_str().to_owned());
        }
        let output = match self.run_command(&args, self.agent_timeout).await {
            CommandRun::Completed(output) => output,
            CommandRun::SpawnFailed => {
                return MutationOutcome::Rejected(PreexecutionRejection::ExecutableUnavailable);
            }
            CommandRun::Pending(pending) => {
                let _ = self.herdr.check_instance();
                return MutationOutcome::Uncertain(Some(pending));
            }
        };
        if self.herdr.check_instance().is_err() {
            return MutationOutcome::Uncertain(None);
        }
        if let Some(response) = success_response(&output) {
            let MutationResult::AgentStarted { agent, .. } = response.result else {
                return MutationOutcome::Uncertain(None);
            };
            return if agent_matches_root(&agent, name, root) {
                MutationOutcome::Completed(())
            } else {
                MutationOutcome::Uncertain(None)
            };
        }
        match preexecution_rejection(&output) {
            Some(reason) => MutationOutcome::Rejected(reason),
            None => MutationOutcome::Uncertain(None),
        }
    }

    async fn root_pane_is_live(&self, root: &RootPane) -> bool {
        if self.herdr.check_instance().is_err() {
            return false;
        }
        let Ok(live) = self.herdr.pane(root.pane_id()).await else {
            return false;
        };
        live.pane_id == root.pane_id
            && &live.workspace_id == root.workspace_id()
            && live.agent.is_none()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuccessResponse {
    #[serde(rename = "id")]
    _id: IgnoredAny,
    result: MutationResult,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum MutationResult {
    TabCreated {
        #[serde(rename = "tab")]
        _tab: BTreeMap<String, IgnoredAny>,
        root_pane: HerdrRootPane,
    },
    AgentStarted {
        agent: HerdrAgentInfo,
        #[serde(rename = "argv")]
        _argv: Vec<IgnoredAny>,
    },
}

#[derive(Deserialize)]
struct HerdrRootPane {
    pane_id: String,
    workspace_id: String,
}

#[derive(Deserialize)]
struct HerdrAgentInfo {
    name: Option<String>,
    agent: Option<String>,
    pane_id: String,
    terminal_id: String,
    workspace_id: String,
    tab_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorResponse {
    #[serde(rename = "id")]
    _id: IgnoredAny,
    error: HerdrCliError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HerdrCliError {
    code: String,
    message: String,
}

fn success_response(output: &CliOutput) -> Option<SuccessResponse> {
    if !output.status.success() || !output.is_complete() || !output.stderr.bytes.is_empty() {
        return None;
    }
    serde_json::from_slice(&output.stdout.bytes).ok()
}

fn preexecution_rejection(output: &CliOutput) -> Option<PreexecutionRejection> {
    if output.status.code() != Some(1) || !output.is_complete() || !output.stdout.bytes.is_empty() {
        return None;
    }
    let response: ErrorResponse = serde_json::from_slice(&output.stderr.bytes).ok()?;
    match response.error.code.as_str() {
        "agent_name_taken" => Some(PreexecutionRejection::AgentNameTaken),
        "invalid_agent_name" => Some(PreexecutionRejection::InvalidAgentName),
        "agent_pane_not_found" => Some(PreexecutionRejection::AgentPaneNotFound),
        "agent_start_input_failed" if canonical_input_limit_error(&response.error.message) => {
            Some(PreexecutionRejection::CanonicalInputTooLong)
        }
        _ => None,
    }
}

fn canonical_input_limit_error(message: &str) -> bool {
    let Some(rest) = message.strip_prefix("input contains a ") else {
        return false;
    };
    let Some((line_len, rest)) =
        rest.split_once("-byte line but the terminal is in canonical mode with a ")
    else {
        return false;
    };
    let Some(limit) = rest.strip_suffix("-byte limit; wait for the shell prompt and retry") else {
        return false;
    };
    match (line_len.parse::<usize>(), limit.parse::<usize>()) {
        (Ok(line_len), Ok(limit)) => line_len > limit,
        _ => false,
    }
}

fn supported_version(output: &CliOutput) -> Option<bool> {
    if !output.status.success() || !output.is_complete() || !output.stderr.bytes.is_empty() {
        return None;
    }
    let bytes = output
        .stdout
        .bytes
        .strip_suffix(b"\n")
        .unwrap_or(&output.stdout.bytes);
    let version = std::str::from_utf8(bytes).ok()?;
    let version_number = version.strip_prefix("herdr ")?;
    if version_number.is_empty()
        || !version_number.bytes().any(|byte| byte.is_ascii_digit())
        || !version_number
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return None;
    }
    Some(version == SUPPORTED_HERDR_VERSION)
}

fn agent_matches_root(agent: &HerdrAgentInfo, name: &HerdrAgentName, root: &RootPane) -> bool {
    agent.name.as_deref() == Some(name.as_str())
        && agent.agent.as_deref() == Some("pi")
        && agent.pane_id == root.pane_id
        && agent.workspace_id == root.workspace_id().as_str()
        && !agent.terminal_id.is_empty()
        && !agent.tab_id.is_empty()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::protocol::{LaunchId, StartupGrant, StartupGrantToken};

    use super::{HerdrAgentName, StartupFlagArgument};

    const LAUNCH_ID: &str = "0123456789abcdef0123456789abcdef";
    const TOKEN: &str = "fedcba9876543210fedcba9876543210";
    const TOO_LONG: &str = "startup handoff exceeds 149 UTF-8 bytes";

    fn grant(version: u32, launch_id: &str, token: &str) -> StartupGrant {
        StartupGrant {
            version,
            launch_id: LaunchId::new(launch_id.to_owned()),
            token: StartupGrantToken::new(token.to_owned()),
        }
    }

    #[test]
    fn generated_agent_names_have_the_herdr_format_and_are_distinct() {
        let mut names = BTreeSet::new();
        for _ in 0..32 {
            let name = HerdrAgentName::random();
            let value = name.as_str();

            assert_eq!(value.len(), 32);
            assert!(value.starts_with("mustr-"));
            assert!(value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
            }));
            assert!(
                value[6..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            assert!(
                names.insert(value.to_owned()),
                "generated agent names are not reused"
            );
        }
    }

    #[test]
    fn startup_argument_is_one_compact_flag_with_exact_field_order_and_redacted_debug() {
        let grant = grant(1, LAUNCH_ID, TOKEN);
        let argument = StartupFlagArgument::new(&grant, "researcher")
            .expect("valid grant and role produce a startup argument");
        let expected = format!(
            "--mustr-startup={{\"launch_id\":\"{LAUNCH_ID}\",\"role\":\"researcher\",\"token\":\"{TOKEN}\"}}"
        );

        assert!(
            argument.as_str() == expected,
            "the single argument uses compact JSON and the canonical field order"
        );
        assert_eq!(argument.as_str().split_whitespace().count(), 1);
        let debug = format!("{argument:?}");
        assert!(
            debug == "StartupFlagArgument([REDACTED])",
            "Debug output never includes the private argument"
        );
    }

    #[test]
    fn valid_maximum_role_produces_a_149_byte_json_payload() {
        let role = format!("r{}", "a".repeat(47));
        let grant = grant(1, LAUNCH_ID, TOKEN);
        let argument = StartupFlagArgument::new(&grant, &role)
            .expect("the maximum valid role fits the documented payload bound");
        let payload = argument
            .as_str()
            .strip_prefix("--mustr-startup=")
            .expect("startup payload has its single private flag prefix");

        assert_eq!(payload.len(), 149);
        assert!(serde_json::from_str::<serde_json::Value>(payload).is_ok());
    }

    #[test]
    fn payload_over_149_bytes_is_rejected_with_a_static_sanitized_error() {
        let role = format!("r{}", "a".repeat(47));
        let oversized_token = "f".repeat(33);
        let encoded = format!(
            "{{\"launch_id\":\"{LAUNCH_ID}\",\"role\":\"{role}\",\"token\":\"{oversized_token}\"}}"
        );
        assert_eq!(encoded.len(), 150);

        let error = StartupFlagArgument::new(&grant(1, LAUNCH_ID, &oversized_token), &role)
            .expect_err("an oversized private JSON payload is rejected");

        assert_eq!(error, TOO_LONG);
        assert!(!error.contains(&oversized_token));
    }

    #[test]
    fn malformed_grants_and_roles_fail_with_a_static_sanitized_error() {
        let invalid = [
            grant(2, LAUNCH_ID, TOKEN),
            grant(1, "not-a-compact-uuid", TOKEN),
            grant(1, LAUNCH_ID, "not-a-compact-uuid"),
        ];
        for grant in invalid {
            let error = StartupFlagArgument::new(&grant, "researcher")
                .expect_err("invalid grant fields cannot produce a private argument");
            assert_eq!(error, "invalid startup handoff");
        }

        assert!(
            StartupFlagArgument::new(&grant(1, LAUNCH_ID, TOKEN), "Researcher").is_err(),
            "invalid role names cannot produce a private argument"
        );
    }
}

#[cfg(test)]
mod subprocess_tests {
    use std::{
        ffi::OsString,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Stdio,
        time::Duration,
    };

    use serde_json::json;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
        process::Command as TokioCommand,
        sync::{mpsc, oneshot, watch},
        task::JoinHandle,
        time::timeout,
    };

    use crate::{
        config::Scheduling,
        herdr::{Herdr, Pane},
        profile::Profile,
        profile_startup::{EntryState, ProfileLaunch},
        protocol::{
            Activity, AgentId, Command, LaunchId, Request, SpaceId, StartupGrant,
            StartupGrantToken, VERSION,
        },
        roles::Role,
        service::Service,
    };

    use super::{
        AGENT_COMMAND_TIMEOUT, CapturedStream, CapturedStreamState, HerdrAgentName, HerdrLauncher,
        MutationOutcome, PendingCommand, PreexecutionRejection, PreflightError, RootPane,
        SHORT_COMMAND_TIMEOUT, StartupFlagArgument, TabBootstrap,
    };

    const STARTUP_TOKEN: &str = "fedcba9876543210fedcba9876543210";
    const FAKE_HERDR_SCRIPT: &str = r##"#!/bin/sh
set -eu
printf '%s\n' "$HERDR_SOCKET_PATH" >> "$MSTR_FAKE_SOCKET_LOG"
{
  printf '%s\n' '--BEGIN--'
  for arg do
    case "$arg" in
      --mustr-startup=*) printf '%s\n' '--mustr-startup=[REDACTED]' ;;
      *) printf '%s\n' "$arg" ;;
    esac
  done
  printf '%s\n' '--END--'
} >> "$MSTR_FAKE_ARGS_LOG"

error_response() {
  printf '{"id":"opaque","error":{"code":"%s","message":"%s"}}\n' "$1" "$2" >&2
  exit "${3:-1}"
}
signal_error_response() {
  printf '%s\n' '{"id":"opaque","error":{"code":"agent_name_taken","message":"name already exists"}}' >&2
  kill -TERM "$$"
}
tab_success() {
  pane_workspace=w1
  if [ "$MSTR_FAKE_MODE" = "tab-mismatch-workspace" ]; then pane_workspace=w2; fi
  printf '{"id":"not-the-request-id","result":{"type":"tab_created","tab":{"id":"tab-1"},"root_pane":{"pane_id":"w1:p9","workspace_id":"%s","terminal_id":"terminal-1","tab_id":"tab-1","focused":false,"agent_status":null,"revision":1}}}\n' "$pane_workspace"
}
agent_success() {
  startup=
  after_separator=0
  for arg do
    if [ "$arg" = "--" ] && [ "$after_separator" -eq 0 ]; then
      after_separator=1
      continue
    fi
    if [ "$after_separator" -eq 1 ]; then startup=$arg; break; fi
  done
  escaped=$(printf '%s' "$startup" | sed 's/"/\\"/g')
  agent_name=$3
  agent_kind=pi
  pane_id=$7
  workspace_id=w1
  terminal_id=terminal-1
  tab_id=tab-1
  case "$MSTR_FAKE_MODE" in
    agent-mismatch-name) agent_name=wrong-name ;;
    agent-mismatch-kind) agent_kind=claude ;;
    agent-mismatch-pane) pane_id=w1:p-other ;;
    agent-mismatch-space) workspace_id=w2 ;;
  esac
  if [ "$MSTR_FAKE_MODE" = "agent-missing-terminal" ]; then
    printf '{"id":"opaque","result":{"type":"agent_started","agent":{"name":"%s","agent":"%s","pane_id":"%s","workspace_id":"%s","tab_id":"%s","agent_status":"ready","focused":false,"revision":1},"argv":["%s"]}}\n' "$agent_name" "$agent_kind" "$pane_id" "$workspace_id" "$tab_id" "$escaped"
  else
    printf '{"id":"opaque","result":{"type":"agent_started","agent":{"name":"%s","agent":"%s","pane_id":"%s","terminal_id":"%s","workspace_id":"%s","tab_id":"%s","agent_status":"ready","focused":false,"revision":1},"argv":["%s"]}}\n' "$agent_name" "$agent_kind" "$pane_id" "$terminal_id" "$workspace_id" "$tab_id" "$escaped"
  fi
}

if [ "$1" = "--version" ]; then
  if [ "$MSTR_FAKE_MODE" = "wrong-version" ]; then
    printf '%s\n' 'herdr 0.9.4'
  else
    printf '%s\n' 'herdr 0.9.5'
  fi
  if [ "$MSTR_FAKE_MODE" = "remove-after-version" ]; then rm "$0"; fi
  exit 0
fi

if [ "$1" = "tab" ]; then
  case "$MSTR_FAKE_MODE" in
    tab-sleep|tab-agent-sleep)
      trap 'printf "%s\\n" exited > "$MSTR_FAKE_TAB_DONE"' EXIT
      printf '%s\\n' ready > "$MSTR_FAKE_TAB_READY"
      while [ ! -f "$MSTR_FAKE_TAB_RELEASE" ]; do sleep 0.01; done
      ;;
  esac
  if [ "$MSTR_FAKE_MODE" = "tab-drift" ]; then
    rm "$HERDR_SOCKET_PATH"
    printf '%s' replacement > "$HERDR_SOCKET_PATH"
  fi
  case "$MSTR_FAKE_MODE" in
    tab-mismatch-workspace)
      tab_success ;;
    tab-missing-root-field)
      printf '%s\n' '{"id":"opaque","result":{"type":"tab_created","tab":{"id":"tab-1"},"root_pane":{"pane_id":"w1:p9","terminal_id":"terminal-1","tab_id":"tab-1","focused":false,"agent_status":null,"revision":1}}}' ;;
    tab-invalid-shape)
      printf '%s\n' '{"id":"opaque","result":{"type":"tab_created","tab":null,"root_pane":{"pane_id":"w1:p9","workspace_id":"w1","terminal_id":"terminal-1","tab_id":"tab-1","focused":false,"agent_status":null,"revision":1}}}' ;;
    tab-malformed)
      printf '%s\n' '{' ;;
    tab-both-channels)
      tab_success
      error_response tab_error 'contradictory error channel' ;;
    tab-error)
      error_response tab_error 'not source-proven before enqueue' ;;
    *)
      tab_success ;;
  esac
  exit 0
fi

if [ "$1" = "agent" ]; then
  case "$MSTR_FAKE_MODE" in
    agent-sleep|tab-agent-sleep)
      printf '%s\n' ready > "$MSTR_FAKE_READY"
      while [ ! -f "$MSTR_FAKE_RELEASE" ]; do sleep 0.01; done
      printf '%s\n' complete > "$MSTR_FAKE_DONE"
      ;;
  esac
  if [ "$MSTR_FAKE_MODE" = "agent-drift" ]; then
    rm "$HERDR_SOCKET_PATH"
    printf '%s' replacement > "$HERDR_SOCKET_PATH"
  fi
  case "$MSTR_FAKE_MODE" in
    agent-name-taken) error_response agent_name_taken 'name already exists' ;;
    agent-name-taken-exit2) error_response agent_name_taken 'name already exists' 2 ;;
    agent-name-taken-signal) signal_error_response ;;
    agent-invalid-name) error_response invalid_agent_name 'name is invalid' ;;
    agent-pane-not-found) error_response agent_pane_not_found 'pane is missing' ;;
    agent-canonical-too-long) error_response agent_start_input_failed 'input contains a 4097-byte line but the terminal is in canonical mode with a 4096-byte limit; wait for the shell prompt and retry' ;;
    agent-canonical-near-miss) error_response agent_start_input_failed 'input contains a 4097-byte line but the terminal is in canonical mode with a 4096-byte limit; retry later' ;;
    agent-not-ready) error_response agent_not_ready 'agent has not started yet' ;;
    agent-unknown-error) error_response future_error_code 'private sentinel error' ;;
    agent-malformed) printf '%s\n' '{' ;;
    agent-both-channels)
      agent_success "$@"
      error_response agent_name_taken 'contradictory error channel' ;;
    agent-error-echo)
      startup=
      after_separator=0
      for arg do
        if [ "$arg" = "--" ] && [ "$after_separator" -eq 0 ]; then after_separator=1; continue; fi
        if [ "$after_separator" -eq 1 ]; then startup=$arg; break; fi
      done
      escaped=$(printf '%s' "$startup" | sed 's/"/\\"/g')
      error_response future_error_code "$escaped" ;;
    agent-wrong-result-type)
      printf '%s\n' '{"id":"opaque","result":{"type":"tab_created","tab":{},"root_pane":{"pane_id":"w1:p9","workspace_id":"w1"}}}' ;;
    *) agent_success "$@" ;;
  esac
  exit 0
fi
exit 99
"##;

    struct PaneServer {
        _directory: TempDir,
        herdr: Herdr,
        task: JoinHandle<()>,
    }

    impl PaneServer {
        async fn new(pane_id: &str, workspace_id: &str, agent: Option<&str>) -> Self {
            let directory = tempfile::tempdir().expect("temporary Herdr socket directory");
            let socket = directory.path().join("herdr.sock");
            let listener = UnixListener::bind(&socket).expect("bind read-only Herdr fixture");
            let pane = Pane {
                pane_id: pane_id.to_owned(),
                workspace_id: SpaceId::new(workspace_id),
                agent: agent.map(str::to_owned),
            };
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let pane = pane.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(stream);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.is_ok() {
                            let mut stream = reader.into_inner();
                            let response = json!({
                                "id": "not-an-identity-proof",
                                "result": { "pane": pane }
                            });
                            let _ = stream.write_all(format!("{response}\n").as_bytes()).await;
                        }
                    });
                }
            });
            let herdr = Herdr::new(socket).expect("capture fixture Herdr identity");
            Self {
                _directory: directory,
                herdr,
                task,
            }
        }

        fn herdr(&self) -> &Herdr {
            &self.herdr
        }

        fn stop(&self) {
            self.task.abort();
        }
    }

    impl Drop for PaneServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct FakeCli {
        _directory: TempDir,
        program: PathBuf,
        args_log: PathBuf,
        socket_log: PathBuf,
        done: PathBuf,
        ready: PathBuf,
        release: PathBuf,
        tab_done: PathBuf,
        tab_ready: PathBuf,
        tab_release: PathBuf,
        mode: OsString,
    }

    impl FakeCli {
        fn new(mode: &str) -> Self {
            let directory = tempfile::tempdir().expect("temporary fake CLI directory");
            let program = directory.path().join("herdr");
            fs::write(&program, FAKE_HERDR_SCRIPT).expect("write fake Herdr executable");
            let mut permissions = fs::metadata(&program)
                .expect("stat fake Herdr executable")
                .permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&program, permissions).expect("make fake Herdr executable");
            Self {
                args_log: directory.path().join("argv.redacted"),
                socket_log: directory.path().join("socket-paths"),
                done: directory.path().join("child-completed"),
                ready: directory.path().join("child-ready"),
                release: directory.path().join("child-release"),
                tab_done: directory.path().join("tab-child-exited"),
                tab_ready: directory.path().join("tab-child-ready"),
                tab_release: directory.path().join("tab-child-release"),
                _directory: directory,
                program,
                mode: mode.into(),
            }
        }

        fn launcher(
            &self,
            herdr: &Herdr,
            short_timeout: Duration,
            agent_timeout: Duration,
        ) -> HerdrLauncher {
            HerdrLauncher::with_test_program(
                herdr,
                self.program.as_os_str().to_os_string(),
                short_timeout,
                agent_timeout,
                vec![
                    ("MSTR_FAKE_MODE".into(), self.mode.clone()),
                    (
                        "MSTR_FAKE_ARGS_LOG".into(),
                        self.args_log.as_os_str().to_os_string(),
                    ),
                    (
                        "MSTR_FAKE_SOCKET_LOG".into(),
                        self.socket_log.as_os_str().to_os_string(),
                    ),
                    (
                        "MSTR_FAKE_DONE".into(),
                        self.done.as_os_str().to_os_string(),
                    ),
                    (
                        "MSTR_FAKE_READY".into(),
                        self.ready.as_os_str().to_os_string(),
                    ),
                    (
                        "MSTR_FAKE_RELEASE".into(),
                        self.release.as_os_str().to_os_string(),
                    ),
                    (
                        "MSTR_FAKE_TAB_DONE".into(),
                        self.tab_done.as_os_str().to_os_string(),
                    ),
                    (
                        "MSTR_FAKE_TAB_READY".into(),
                        self.tab_ready.as_os_str().to_os_string(),
                    ),
                    (
                        "MSTR_FAKE_TAB_RELEASE".into(),
                        self.tab_release.as_os_str().to_os_string(),
                    ),
                ],
            )
        }

        fn args(&self) -> String {
            fs::read_to_string(&self.args_log).unwrap_or_default()
        }

        fn socket_paths(&self) -> String {
            fs::read_to_string(&self.socket_log).unwrap_or_default()
        }

        fn release_child(&self) {
            fs::write(&self.release, "release").expect("release test-owned agent child");
            fs::write(&self.tab_release, "release").expect("release test-owned tab child");
        }
    }

    impl Drop for FakeCli {
        fn drop(&mut self) {
            let _ = fs::write(&self.release, "release");
            let _ = fs::write(&self.tab_release, "release");
        }
    }

    async fn wait_for_file(path: &Path) -> bool {
        timeout(Duration::from_secs(2), async {
            while !path.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
    }

    async fn wait_for_snapshot(
        service: &Service,
        description: &'static str,
        predicate: impl Fn(&crate::protocol::Snapshot) -> bool,
    ) -> Result<crate::protocol::Snapshot, String> {
        let mut snapshots = service.snapshots.clone();
        match timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return Ok(snapshot);
                }
                snapshots.changed().await.map_err(|_| {
                    format!("service snapshot stream closed while waiting for {description}")
                })?;
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(format!(
                "timed out waiting for {description} in the published service snapshot"
            )),
        }
    }

    fn startup_flag() -> StartupFlagArgument {
        let grant = StartupGrant {
            version: 1,
            launch_id: LaunchId::new("0123456789abcdef0123456789abcdef".to_owned()),
            token: StartupGrantToken::new(STARTUP_TOKEN.to_owned()),
        };
        StartupFlagArgument::new(&grant, "researcher").expect("valid private startup argument")
    }

    #[derive(serde::Deserialize)]
    struct CapturedStartupIntent {
        launch_id: String,
        role: String,
        token: String,
    }

    fn captured_startup_grant(argument: &str, role: &str) -> Option<StartupGrant> {
        let intent = argument
            .strip_prefix("--mustr-startup=")
            .and_then(|payload| serde_json::from_str::<CapturedStartupIntent>(payload).ok())?;
        if intent.role != role {
            return None;
        }
        Some(StartupGrant {
            version: 1,
            launch_id: LaunchId::new(intent.launch_id),
            token: StartupGrantToken::new(intent.token),
        })
    }

    async fn verified_root(launcher: &HerdrLauncher) -> RootPane {
        assert!(
            launcher.preflight().await.is_ok(),
            "fake Herdr version is supported"
        );
        match launcher
            .create_tab(&SpaceId::new("w1"), Path::new("/tmp/profile startup cwd"))
            .await
        {
            MutationOutcome::Completed(root) => root,
            _ => panic!("valid tab response and matching read-only pane lookup complete"),
        }
    }

    fn profile_with_roles(roles: &[(&str, bool)]) -> Profile {
        Profile {
            name: "coordinator-test".into(),
            roles: roles
                .iter()
                .map(|(name, manager)| Role {
                    name: (*name).into(),
                    description: (*name).into(),
                    prompt: "test role".into(),
                    manager: *manager,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn production_launcher_uses_herdr_with_bounded_command_budgets() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let launcher = HerdrLauncher::new(server.herdr());

        assert_eq!(launcher.program, OsString::from("herdr"));
        assert_eq!(launcher.short_timeout, Duration::from_secs(15));
        assert_eq!(launcher.agent_timeout, Duration::from_secs(45));
    }

    #[tokio::test]
    async fn agent_started_accepts_full_095_agent_info_and_checks_identity() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert!(launcher.preflight().await.is_ok());
        let root = RootPane {
            pane_id: "w1:p9".to_owned(),
            workspace_id: SpaceId::new("w1"),
        };
        let name = HerdrAgentName::random();

        assert!(matches!(
            launcher.start_pi(&name, &root, &startup_flag()).await,
            MutationOutcome::Completed(())
        ));
        assert_eq!(fake.args().matches("--BEGIN--").count(), 2);
    }

    #[tokio::test]
    async fn profile_tabs_forward_only_the_two_noncredential_bootstrap_values_as_two_arg_env() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        let bootstrap = TabBootstrap::new(
            Path::new("/tmp/mustr bin=debug"),
            Path::new("/tmp/config directory=selected"),
        )
        .expect("absolute UTF-8 runtime paths are valid");
        assert!(launcher.preflight().await.is_ok());

        assert!(matches!(
            launcher
                .create_tab_with_bootstrap(
                    &SpaceId::new("w1"),
                    Path::new("/tmp/work tree=one"),
                    &bootstrap,
                )
                .await,
            MutationOutcome::Completed(_)
        ));
        let args = fake.args();
        assert!(args.contains(
            "--workspace\nw1\n--cwd\n/tmp/work tree=one\n--env\nMUSTR_BIN=/tmp/mustr bin=debug\n--env\nMUSTR_CONFIG_DIR=/tmp/config directory=selected\n"
        ));
        assert!(!args.contains("--env=MUSTR_"));
        assert!(!args.contains("--focus\n"));
        assert!(!args.contains("--no-focus\n"));
        assert!(!args.contains("OPENAI_API_KEY"));
        assert!(!args.contains("PI_MODEL"));
    }

    #[tokio::test]
    async fn profile_tab_rejects_invalid_argv_paths_before_spawning_herdr() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        let bootstrap = TabBootstrap::new(Path::new("/tmp/mustr"), Path::new("/tmp/config"))
            .expect("valid bootstrap paths");
        assert!(launcher.preflight().await.is_ok());
        let before = fake.args();

        assert!(matches!(
            launcher
                .create_tab_with_bootstrap(
                    &SpaceId::new("w1"),
                    Path::new("/tmp/bad\0cwd"),
                    &bootstrap,
                )
                .await,
            MutationOutcome::Rejected(PreexecutionRejection::InvalidArgument)
        ));
        assert_eq!(fake.args(), before, "invalid argv never reaches Herdr");
    }

    #[test]
    fn profile_bootstrap_rejects_relative_or_nul_containing_paths() {
        assert!(TabBootstrap::new(Path::new("relative/mustr"), Path::new("/tmp/config")).is_err());
        assert!(TabBootstrap::new(Path::new("/tmp/mustr\0bad"), Path::new("/tmp/config")).is_err());
        assert!(TabBootstrap::new(Path::new("/tmp/mustr"), Path::new("relative/config")).is_err());
    }

    #[tokio::test]
    async fn preflight_whitelists_only_095_and_blocks_tabs_until_verified() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("wrong-version");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        assert!(matches!(
            launcher.preflight().await,
            Err(PreflightError::UnsupportedVersion)
        ));
        assert!(matches!(
            launcher
                .create_tab(&SpaceId::new("w1"), Path::new("/tmp/cwd"))
                .await,
            MutationOutcome::Rejected(PreexecutionRejection::VersionNotVerified)
        ));
        assert_eq!(fake.args(), "--BEGIN--\n--version\n--END--\n");
    }

    #[tokio::test]
    async fn commands_use_exact_argv_and_captured_socket_and_discard_echoed_startup_argv() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert!(launcher.preflight().await.is_ok());
        let root = match launcher
            .create_tab(&SpaceId::new("w1"), Path::new("/tmp/profile startup cwd"))
            .await
        {
            MutationOutcome::Completed(root) => root,
            _ => panic!("tab success includes a verified root pane"),
        };
        assert_eq!(
            root,
            RootPane {
                pane_id: "w1:p9".to_owned(),
                workspace_id: SpaceId::new("w1"),
            }
        );
        let name = HerdrAgentName::random();
        let outcome = launcher.start_pi(&name, &root, &startup_flag()).await;
        assert!(
            matches!(&outcome, MutationOutcome::Completed(())),
            "agent start outcome {outcome:?}; redacted argv log:\n{}",
            fake.args()
        );
        let expected = format!(
            concat!(
                "--BEGIN--\n--version\n--END--\n",
                "--BEGIN--\ntab\ncreate\n--workspace\nw1\n--cwd\n/tmp/profile startup cwd\n--END--\n",
                "--BEGIN--\nagent\nstart\n{}\n--kind\npi\n--pane\nw1:p9\n--timeout\n30000\n--\n--mustr-startup=[REDACTED]\n--END--\n"
            ),
            name.as_str()
        );
        assert_eq!(fake.args(), expected);
        let socket = server.herdr().socket_path().to_string_lossy().into_owned();
        assert_eq!(
            fake.socket_paths(),
            format!("{socket}\n{socket}\n{socket}\n")
        );
        assert!(!fake.args().contains(STARTUP_TOKEN));
        assert!(!format!("{outcome:?}").contains(STARTUP_TOKEN));
    }

    #[tokio::test]
    async fn tab_schema_workspace_and_live_pane_mismatches_are_uncertain() {
        for (mode, pane_id, workspace_id, agent) in [
            ("tab-mismatch-workspace", "w1:p9", "w1", None),
            ("success", "w1:p-other", "w1", None),
            ("success", "w1:p9", "w2", None),
            ("success", "w1:p9", "w1", Some("pi")),
            ("tab-missing-root-field", "w1:p9", "w1", None),
            ("tab-invalid-shape", "w1:p9", "w1", None),
            ("tab-malformed", "w1:p9", "w1", None),
            ("tab-both-channels", "w1:p9", "w1", None),
            ("tab-error", "w1:p9", "w1", None),
        ] {
            let server = PaneServer::new(pane_id, workspace_id, agent).await;
            let fake = FakeCli::new(mode);
            let launcher = fake.launcher(
                server.herdr(),
                Duration::from_secs(5),
                Duration::from_secs(5),
            );
            assert!(launcher.preflight().await.is_ok());
            assert!(matches!(
                launcher
                    .create_tab(&SpaceId::new("w1"), Path::new("/tmp/cwd"))
                    .await,
                MutationOutcome::Uncertain(None)
            ));
            assert_eq!(fake.args().matches("--BEGIN--").count(), 2);
        }
    }

    #[tokio::test]
    async fn unavailable_read_only_pane_lookup_stops_after_tab_without_agent_start() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        server.stop();
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );

        assert!(launcher.preflight().await.is_ok());
        assert!(matches!(
            launcher
                .create_tab(&SpaceId::new("w1"), Path::new("/tmp/cwd"))
                .await,
            MutationOutcome::Uncertain(None)
        ));
        assert_eq!(fake.args().matches("--BEGIN--").count(), 2);
    }

    #[tokio::test]
    async fn socket_fence_drift_after_tab_or_agent_is_uncertain_and_never_retried() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("tab-drift");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert!(launcher.preflight().await.is_ok());
        assert!(matches!(
            launcher
                .create_tab(&SpaceId::new("w1"), Path::new("/tmp/cwd"))
                .await,
            MutationOutcome::Uncertain(None)
        ));
        assert_eq!(fake.args().matches("--BEGIN--").count(), 2);

        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("agent-drift");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        let root = verified_root(&launcher).await;
        assert!(matches!(
            launcher
                .start_pi(&HerdrAgentName::random(), &root, &startup_flag())
                .await,
            MutationOutcome::Uncertain(None)
        ));
        assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
    }

    #[tokio::test]
    async fn agent_identity_mismatches_and_wrong_success_discriminants_are_uncertain() {
        for mode in [
            "agent-mismatch-name",
            "agent-mismatch-kind",
            "agent-mismatch-pane",
            "agent-mismatch-space",
            "agent-missing-terminal",
            "agent-wrong-result-type",
        ] {
            let server = PaneServer::new("w1:p9", "w1", None).await;
            let fake = FakeCli::new(mode);
            let launcher = fake.launcher(
                server.herdr(),
                Duration::from_secs(5),
                Duration::from_secs(5),
            );
            let root = verified_root(&launcher).await;
            let outcome = launcher
                .start_pi(&HerdrAgentName::random(), &root, &startup_flag())
                .await;
            assert!(matches!(outcome, MutationOutcome::Uncertain(None)));
            assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
        }
    }

    #[tokio::test]
    async fn source_proven_agent_rejections_are_typed_and_ambiguous_errors_stay_uncertain() {
        for (mode, expected) in [
            ("agent-name-taken", PreexecutionRejection::AgentNameTaken),
            (
                "agent-invalid-name",
                PreexecutionRejection::InvalidAgentName,
            ),
            (
                "agent-pane-not-found",
                PreexecutionRejection::AgentPaneNotFound,
            ),
            (
                "agent-canonical-too-long",
                PreexecutionRejection::CanonicalInputTooLong,
            ),
        ] {
            let server = PaneServer::new("w1:p9", "w1", None).await;
            let fake = FakeCli::new(mode);
            let launcher = fake.launcher(
                server.herdr(),
                Duration::from_secs(5),
                Duration::from_secs(5),
            );
            let root = verified_root(&launcher).await;
            assert!(matches!(
                launcher.start_pi(&HerdrAgentName::random(), &root, &startup_flag()).await,
                MutationOutcome::Rejected(actual) if actual == expected
            ));
            assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
        }

        for mode in [
            "agent-canonical-near-miss",
            "agent-name-taken-exit2",
            "agent-name-taken-signal",
            "agent-not-ready",
            "agent-unknown-error",
            "agent-malformed",
            "agent-both-channels",
            "agent-error-echo",
        ] {
            let server = PaneServer::new("w1:p9", "w1", None).await;
            let fake = FakeCli::new(mode);
            let launcher = fake.launcher(
                server.herdr(),
                Duration::from_secs(5),
                Duration::from_secs(5),
            );
            let root = verified_root(&launcher).await;
            let outcome = launcher
                .start_pi(&HerdrAgentName::random(), &root, &startup_flag())
                .await;
            assert!(matches!(outcome, MutationOutcome::Uncertain(None)));
            assert!(!fake.args().contains(STARTUP_TOKEN));
            assert!(!format!("{outcome:?}").contains(STARTUP_TOKEN));
            assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
        }
    }

    #[tokio::test]
    async fn timed_out_capture_retains_completed_stdout_for_stderr_reconciliation() {
        use std::{
            future::{Future, poll_fn},
            task::Poll,
        };

        let mut child = TokioCommand::new("/usr/bin/true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn self-completing test child");
        let status = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(status) = child.try_wait().expect("check self-completing test child") {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("test child exits within the bounded test budget");
        assert!(status.success());

        let stdout = tokio::spawn(async {
            CapturedStream {
                bytes: b"bounded stdout".to_vec(),
                over_limit: false,
                failed: false,
            }
        });
        while !stdout.is_finished() {
            tokio::task::yield_now().await;
        }
        let (release_stderr, stderr_release) = oneshot::channel();
        let stderr = tokio::spawn(async move {
            let _ = stderr_release.await;
            CapturedStream {
                bytes: b"bounded stderr".to_vec(),
                over_limit: false,
                failed: false,
            }
        });
        let mut pending = PendingCommand {
            child,
            stdout: CapturedStreamState::Pending(stdout),
            stderr: CapturedStreamState::Pending(stderr),
            exit_status: None,
        };
        let mut collecting = Box::pin(pending.collect_output());
        let reached_delayed_stderr = poll_fn(|context| {
            Poll::Ready(matches!(collecting.as_mut().poll(context), Poll::Pending))
        })
        .await;
        let timed_out = timeout(Duration::from_millis(10), collecting.as_mut())
            .await
            .is_err();
        drop(collecting);
        let stdout_was_captured = matches!(&pending.stdout, CapturedStreamState::Complete(_));
        let status_was_captured = pending.exit_status.is_some();
        release_stderr
            .send(())
            .expect("release the test-owned stderr barrier");
        let reconciled = timeout(Duration::from_secs(2), pending.wait_for_local_exit()).await;

        assert!(
            reached_delayed_stderr,
            "stdout completed before delayed stderr"
        );
        assert!(
            timed_out,
            "the held stderr stream outlives the bounded wait"
        );
        assert!(stdout_was_captured, "stdout is retained after timeout");
        assert!(
            status_was_captured,
            "the reaped child status is retained after timeout"
        );
        assert!(
            matches!(reconciled, Ok(Ok(()))),
            "reconciliation keeps completed stdout and finishes after stderr release"
        );
    }

    #[tokio::test]
    async fn timed_out_agent_child_remains_uncertain_and_is_reconciled_without_kill_or_retry() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("agent-sleep");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(5),
            Duration::from_millis(10),
        );
        let root = verified_root(&launcher).await;
        let name = HerdrAgentName::random();
        let startup = startup_flag();
        let start = launcher.start_pi(&name, &root, &startup);
        tokio::pin!(start);
        let outcome = tokio::select! {
            outcome = start.as_mut() => outcome,
            ready = wait_for_file(&fake.ready) => {
                assert!(ready, "fake child reached its explicit startup barrier");
                start.as_mut().await
            }
        };
        assert!(
            wait_for_file(&fake.ready).await,
            "fake child reached its explicit startup barrier"
        );
        let pending = match outcome {
            MutationOutcome::Uncertain(Some(pending)) => Some(pending),
            _ => None,
        };
        fake.release_child();
        let mut pending =
            pending.expect("an agent CLI timeout retains its child for reconciliation");
        assert!(matches!(
            timeout(Duration::from_secs(2), pending.wait_for_local_exit()).await,
            Ok(Ok(()))
        ));
        assert!(
            fake.done.exists(),
            "the fake mutating child completed naturally"
        );
        assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
    }

    #[tokio::test]
    async fn full_profile_run_retires_authority_on_herdr_identity_drift_while_agent_cli_is_held() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("agent-sleep");
        let launcher = fake.launcher(
            server.herdr(),
            Duration::from_secs(15),
            Duration::from_secs(45),
        );
        assert!(launcher.preflight().await.is_ok());

        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel) = watch::channel(false);
        let (argument_tx, mut arguments) = mpsc::unbounded_channel();
        let launcher = launcher.observe_startup_arguments(argument_tx);
        let profile = Profile {
            name: "identity-check".into(),
            roles: vec![Role {
                name: "researcher".into(),
                description: "researcher".into(),
                prompt: "test role".into(),
                manager: false,
            }],
        };
        let mut run = tokio::spawn(crate::profile_startup::run(ProfileLaunch {
            profile,
            service: service.clone(),
            herdr: server.herdr().clone(),
            launcher,
            cwd: PathBuf::from("/tmp"),
            bootstrap: TabBootstrap::new(
                Path::new("/tmp/mustr-bin"),
                Path::new("/tmp/mustr-config"),
            )
            .expect("valid test bootstrap paths"),
            progress,
            cancel,
        }));

        let ready = wait_for_file(&fake.ready).await;
        let argument = timeout(Duration::from_secs(2), arguments.recv())
            .await
            .ok()
            .flatten();
        let grant = argument
            .as_deref()
            .and_then(|argument| captured_startup_grant(argument, "researcher"));
        let grant_observed = grant.is_some();
        let identity_replaced = if ready && grant.is_some() {
            fs::remove_file(server.herdr().socket_path())
                .and_then(|()| fs::write(server.herdr().socket_path(), b"replacement identity"))
                .is_ok()
        } else {
            false
        };
        let late_join = if identity_replaced {
            if let Some(grant) = grant {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let consumed = service
                    .consume_startup_grant(
                        grant.clone(),
                        "researcher".into(),
                        "w1:p9".into(),
                        SpaceId::new("w1"),
                    )
                    .await;
                Some(match consumed {
                    Err(error) => Err(error),
                    Ok(()) => {
                        service
                            .join(
                                71,
                                Request {
                                    id: "late-join".into(),
                                    command: Command::Join {
                                        version: VERSION,
                                        agent_id: AgentId::new("late-identity-agent"),
                                        name: "researcher-late-identity".into(),
                                        role: "researcher".into(),
                                        pane_id: "w1:p9".into(),
                                        space_id: SpaceId::new("w1"),
                                        cwd: None,
                                        reset: None,
                                        startup_grant: Some(grant),
                                    },
                                },
                                false,
                            )
                            .await
                    }
                })
            } else {
                None
            }
        } else {
            None
        };

        fake.release_child();
        let timely = timeout(Duration::from_secs(60), &mut run).await;
        let (settled_in_budget, completed) = match timely {
            Ok(completed) => (true, completed),
            Err(_) => (false, run.await),
        };
        let child_exited = fake.done.exists();
        let members = service.snapshots.borrow().members.clone();
        service.stop().await;

        assert!(
            ready,
            "the fake mutating child reached its explicit barrier"
        );
        assert!(
            identity_replaced,
            "the captured Herdr identity was replaced while the CLI was held"
        );
        assert!(
            argument.is_some(),
            "the test-only memory channel observed the actual issued flag"
        );
        assert!(
            grant_observed,
            "the Join correlation used the actual issued token from an in-memory channel"
        );
        assert!(child_exited, "the fake child exited after explicit release");
        assert!(
            settled_in_budget,
            "the profile run settled within its bounded cleanup budget"
        );
        let outcomes = completed
            .expect("profile coordinator task did not panic")
            .expect("profile coordinator returned outcomes");
        let late_join =
            late_join.expect("the real service actor checked the actual issued startup grant");
        match late_join {
            Err(error) => assert!(
                format!("{error:#}").contains("unknown or expired startup grant"),
                "consumption rejection should identify retired startup authority"
            ),
            Ok(response) => {
                assert!(
                    !response.ok,
                    "retired authority rejects a direct actor Join"
                );
                assert!(
                    response.error.as_deref().is_some_and(|error| {
                        error.contains("unknown or expired startup grant")
                    })
                );
            }
        }
        assert!(
            members.is_empty(),
            "no member was admitted after identity drift"
        );
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].state, EntryState::Uncertain);
        assert!(outcomes[0].reason.contains("Herdr identity changed"));
        assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
    }

    #[tokio::test]
    async fn full_profile_run_preflights_an_existing_manager_through_the_real_actor() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        let preflight = launcher.preflight().await;
        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let manager_join = service
            .join(
                31,
                Request {
                    id: "existing-manager".into(),
                    command: Command::Join {
                        version: VERSION,
                        agent_id: AgentId::new("existing-manager"),
                        name: "architect-existing".into(),
                        role: "architect".into(),
                        pane_id: "w1:p1".into(),
                        space_id: SpaceId::new("w1"),
                        cwd: None,
                        reset: None,
                        startup_grant: None,
                    },
                },
                true,
            )
            .await;
        let manager_snapshot = if matches!(&manager_join, Ok(response) if response.ok) {
            Some(
                wait_for_snapshot(&service, "the accepted manager membership", |snapshot| {
                    snapshot.members.iter().any(|member| {
                        member.id == AgentId::new("existing-manager")
                            && member.space_id == snapshot.space_id
                            && member.role == "architect"
                            && member.manager
                    })
                })
                .await,
            )
        } else {
            None
        };
        let outcomes = if manager_snapshot.as_ref().is_some_and(Result::is_ok) && preflight.is_ok()
        {
            let (progress, _progress_rx) = mpsc::unbounded_channel();
            let (_cancel_tx, cancel) = watch::channel(false);
            Some(
                crate::profile_startup::run(ProfileLaunch {
                    profile: profile_with_roles(&[("architect", true)]),
                    service: service.clone(),
                    herdr: server.herdr().clone(),
                    launcher,
                    cwd: PathBuf::from("/tmp"),
                    bootstrap: TabBootstrap::new(
                        Path::new("/tmp/mustr-bin"),
                        Path::new("/tmp/mustr-config"),
                    )
                    .expect("valid test bootstrap paths"),
                    progress,
                    cancel,
                })
                .await,
            )
        } else {
            None
        };
        service.stop().await;

        assert!(
            preflight.is_ok(),
            "production Herdr version preflight passes"
        );
        assert!(matches!(manager_join, Ok(response) if response.ok));
        assert!(
            manager_snapshot.as_ref().is_some_and(Result::is_ok),
            "accepted manager membership was not published: {}",
            manager_snapshot
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .map_or("manager Join did not publish".to_owned(), Clone::clone)
        );
        let outcomes = outcomes
            .expect("profile run executed after the manager was seeded")
            .expect("profile coordinator returned outcomes");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].state, EntryState::Failed);
        assert!(
            outcomes[0]
                .reason
                .contains("manager role is already present")
        );
        assert_eq!(fake.args().matches("--BEGIN--").count(), 1);
        assert!(fake.args().contains("--version"));
    }

    #[tokio::test]
    async fn full_profile_run_preflights_a_prepared_reset_through_the_real_actor() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        let preflight = launcher.preflight().await;
        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let setup = async {
            service
                .tx
                .send(crate::service::Event::Space(Ok(crate::protocol::Space {
                    id: SpaceId::new("w1"),
                    label: "Test space".into(),
                })))
                .await
                .map_err(|error| format!("publish bound-space availability: {error}"))?;
            let joined = service
                .join(
                    32,
                    Request {
                        id: "reset-participant".into(),
                        command: Command::Join {
                            version: VERSION,
                            agent_id: AgentId::new("reset-participant"),
                            name: "researcher-reset-participant".into(),
                            role: "researcher".into(),
                            pane_id: "w1:p2".into(),
                            space_id: SpaceId::new("w1"),
                            cwd: None,
                            reset: None,
                            startup_grant: None,
                        },
                    },
                    false,
                )
                .await
                .map_err(|error| format!("Join failed: {error:#}"))?;
            if !joined.ok {
                return Err(format!("Join rejected: {:?}", joined.error));
            }
            let idle = service
                .request(
                    32,
                    Request {
                        id: "idle".into(),
                        command: Command::Sync {
                            activity: Activity::Idle,
                            receive: false,
                        },
                    },
                )
                .await
                .map_err(|error| format!("idle sync failed: {error:#}"))?;
            if !idle.ok {
                return Err(format!("idle sync rejected: {:?}", idle.error));
            }
            let capability = service
                .request(
                    32,
                    Request {
                        id: "reset-capability".into(),
                        command: Command::ResetCapability,
                    },
                )
                .await
                .map_err(|error| format!("ResetCapability failed: {error:#}"))?;
            if !capability.ok {
                return Err(format!("ResetCapability rejected: {:?}", capability.error));
            }
            let preview = service
                .reset_preview()
                .await
                .map_err(|error| format!("reset preview failed: {error:#}"))?;
            service
                .reset_start(preview)
                .await
                .map_err(|error| format!("reset start failed: {error:#}"))?;
            Ok::<(), String>(())
        }
        .await;
        let prepared_snapshot = if setup.is_ok() && preflight.is_ok() {
            Some(
                wait_for_snapshot(&service, "reset phase Prepared", |snapshot| {
                    snapshot.reset.as_ref().is_some_and(|reset| {
                        reset.state == crate::protocol::ResetPhase::Prepared
                            && reset.space_id == snapshot.space_id
                    })
                })
                .await,
            )
        } else {
            None
        };
        let outcomes = if prepared_snapshot.as_ref().is_some_and(Result::is_ok) {
            let (progress, _progress_rx) = mpsc::unbounded_channel();
            let (_cancel_tx, cancel) = watch::channel(false);
            Some(
                crate::profile_startup::run(ProfileLaunch {
                    profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
                    service: service.clone(),
                    herdr: server.herdr().clone(),
                    launcher,
                    cwd: PathBuf::from("/tmp"),
                    bootstrap: TabBootstrap::new(
                        Path::new("/tmp/mustr-bin"),
                        Path::new("/tmp/mustr-config"),
                    )
                    .expect("valid test bootstrap paths"),
                    progress,
                    cancel,
                })
                .await,
            )
        } else {
            None
        };
        service.stop().await;

        assert!(
            preflight.is_ok(),
            "production Herdr version preflight passes"
        );
        assert!(
            setup.is_ok(),
            "the real actor accepted idle participant and reset: {setup:?}"
        );
        assert!(
            prepared_snapshot.as_ref().is_some_and(Result::is_ok),
            "prepared reset was not published: {}",
            prepared_snapshot
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .map_or("reset setup did not run".to_owned(), Clone::clone)
        );
        let outcomes = outcomes
            .expect("profile run executed after the actor entered prepared reset")
            .expect("profile coordinator returned outcomes");
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes
                .iter()
                .all(|outcome| outcome.state == EntryState::Failed)
        );
        assert!(outcomes[0].reason.contains("prepared or incomplete reset"));
        assert_eq!(fake.args().matches("--BEGIN--").count(), 1);
        assert!(fake.args().contains("--version"));
    }

    #[tokio::test]
    async fn full_profile_run_stops_after_preexecution_tab_rejection() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("success");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        let preflight = launcher.preflight().await;
        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel) = watch::channel(false);
        let run = if preflight.is_ok() {
            Some(
                crate::profile_startup::run(ProfileLaunch {
                    profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
                    service: service.clone(),
                    herdr: server.herdr().clone(),
                    launcher,
                    cwd: PathBuf::from("relative-working-directory"),
                    bootstrap: TabBootstrap::new(
                        Path::new("/tmp/mustr-bin"),
                        Path::new("/tmp/mustr-config"),
                    )
                    .expect("valid test bootstrap paths"),
                    progress,
                    cancel,
                })
                .await,
            )
        } else {
            None
        };
        service.stop().await;

        assert!(
            preflight.is_ok(),
            "production Herdr version preflight passes"
        );
        let outcomes = run
            .expect("profile run executed after version preflight")
            .expect("profile coordinator returned outcomes");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].state, EntryState::Failed);
        assert!(outcomes[0].reason.contains("absolute UTF-8"));
        assert!(
            outcomes[1]
                .reason
                .contains("not attempted after the preceding tab create was rejected")
        );
        assert_eq!(fake.args().matches("--BEGIN--").count(), 1);
        assert!(fake.args().contains("--version"));
    }

    #[tokio::test]
    async fn full_profile_run_stops_after_herdr_tab_spawn_rejection() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("remove-after-version");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        assert!(launcher.preflight().await.is_ok());

        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel) = watch::channel(false);
        let outcomes = crate::profile_startup::run(ProfileLaunch {
            profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
            service: service.clone(),
            herdr: server.herdr().clone(),
            launcher,
            cwd: PathBuf::from("/tmp"),
            bootstrap: TabBootstrap::new(
                Path::new("/tmp/mustr-bin"),
                Path::new("/tmp/mustr-config"),
            )
            .expect("valid test bootstrap paths"),
            progress,
            cancel,
        })
        .await;
        service.stop().await;

        let outcomes = outcomes.expect("profile coordinator returned outcomes");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].state, EntryState::Failed);
        assert_eq!(outcomes[0].reason, "Herdr CLI could not be started");
        assert!(
            outcomes[1]
                .reason
                .contains("not attempted after the preceding tab create was rejected")
        );
        assert_eq!(fake.args().matches("--BEGIN--").count(), 1);
        assert!(fake.args().contains("--version"));
    }

    #[tokio::test]
    async fn full_profile_run_retains_a_cancelled_held_tab_until_its_valid_reply() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("tab-agent-sleep");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        let preflight = launcher.preflight().await;
        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        let (argument_tx, mut arguments) = mpsc::unbounded_channel();
        let launcher = launcher.observe_startup_arguments(argument_tx);
        let mut run = tokio::spawn(crate::profile_startup::run(ProfileLaunch {
            profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
            service: service.clone(),
            herdr: server.herdr().clone(),
            launcher,
            cwd: PathBuf::from("/tmp"),
            bootstrap: TabBootstrap::new(
                Path::new("/tmp/mustr-bin"),
                Path::new("/tmp/mustr-config"),
            )
            .expect("valid test bootstrap paths"),
            progress,
            cancel,
        }));

        let ready = wait_for_file(&fake.tab_ready).await;
        let cancellation_sent = cancel_tx.send(true).is_ok();
        let run_still_owns_cli = !run.is_finished();
        let child_still_held = !fake.tab_done.exists();
        fake.release_child();
        let timely = timeout(Duration::from_secs(8), &mut run).await;
        let (settled_in_budget, completed) = match timely {
            Ok(completed) => (true, completed),
            Err(_) => {
                let _ = cancel_tx.send(true);
                fake.release_child();
                (false, run.await)
            }
        };
        let child_exited = wait_for_file(&fake.tab_done).await;
        let startup_argument = arguments.try_recv().ok();
        let members = service.snapshots.borrow().members.clone();
        service.stop().await;

        assert!(
            preflight.is_ok(),
            "production Herdr version preflight passes"
        );
        assert!(ready, "the fake tab command reached its explicit barrier");
        assert!(
            cancellation_sent,
            "the run's cancellation watch is still owned"
        );
        assert!(
            run_still_owns_cli && child_still_held,
            "cancellation must not drop or kill the in-flight tab command"
        );
        assert!(
            child_exited,
            "the fake tab child exited after explicit release"
        );
        assert!(
            settled_in_budget,
            "the run settled after its held tab was released"
        );
        assert!(
            startup_argument.is_none(),
            "no startup grant reached Pi start"
        );
        assert!(members.is_empty(), "no profile member was admitted");
        let outcomes = completed
            .expect("profile coordinator task did not panic")
            .expect("profile coordinator returned outcomes");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].state, EntryState::Failed);
        assert_eq!(outcomes[0].pane_id.as_deref(), Some("w1:p9"));
        assert_eq!(
            outcomes[0].reason,
            "tab created; Pi start not attempted after launch cancelled"
        );
        assert_eq!(outcomes[1].reason, "not attempted after launch cancelled");
        assert_eq!(fake.args().matches("--BEGIN--").count(), 2);
    }

    #[tokio::test]
    async fn full_profile_run_retains_a_held_tab_through_active_reset_freeze() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("tab-agent-sleep");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        let preflight = launcher.preflight().await;
        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let setup = async {
            service
                .tx
                .send(crate::service::Event::Space(Ok(crate::protocol::Space {
                    id: SpaceId::new("w1"),
                    label: "Test space".into(),
                })))
                .await
                .map_err(|error| format!("publish bound-space availability: {error}"))?;
            let joined = service
                .join(
                    33,
                    Request {
                        id: "freeze-participant".into(),
                        command: Command::Join {
                            version: VERSION,
                            agent_id: AgentId::new("freeze-participant"),
                            name: "researcher-freeze-participant".into(),
                            role: "researcher".into(),
                            pane_id: "w1:p2".into(),
                            space_id: SpaceId::new("w1"),
                            cwd: None,
                            reset: None,
                            startup_grant: None,
                        },
                    },
                    false,
                )
                .await
                .map_err(|error| format!("Join failed: {error:#}"))?;
            if !joined.ok {
                return Err(format!("Join rejected: {:?}", joined.error));
            }
            let idle = service
                .request(
                    33,
                    Request {
                        id: "freeze-idle".into(),
                        command: Command::Sync {
                            activity: Activity::Idle,
                            receive: false,
                        },
                    },
                )
                .await
                .map_err(|error| format!("idle sync failed: {error:#}"))?;
            if !idle.ok {
                return Err(format!("idle sync rejected: {:?}", idle.error));
            }
            let capability = service
                .request(
                    33,
                    Request {
                        id: "freeze-capability".into(),
                        command: Command::ResetCapability,
                    },
                )
                .await
                .map_err(|error| format!("ResetCapability failed: {error:#}"))?;
            if !capability.ok {
                return Err(format!("ResetCapability rejected: {:?}", capability.error));
            }
            Ok::<(), String>(())
        }
        .await;
        if setup.is_err() || preflight.is_err() {
            service.stop().await;
            assert!(setup.is_ok(), "reset fixture setup failed: {setup:?}");
            assert!(
                preflight.is_ok(),
                "production Herdr version preflight passes"
            );
        }

        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        let (argument_tx, mut arguments) = mpsc::unbounded_channel();
        let launcher = launcher.observe_startup_arguments(argument_tx);
        let mut run = tokio::spawn(crate::profile_startup::run(ProfileLaunch {
            profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
            service: service.clone(),
            herdr: server.herdr().clone(),
            launcher,
            cwd: PathBuf::from("/tmp"),
            bootstrap: TabBootstrap::new(
                Path::new("/tmp/mustr-bin"),
                Path::new("/tmp/mustr-config"),
            )
            .expect("valid test bootstrap paths"),
            progress,
            cancel,
        }));

        let ready = wait_for_file(&fake.tab_ready).await;
        let reset = match service.reset_preview().await {
            Ok(preview) => service
                .reset_start(preview)
                .await
                .map_err(|error| format!("reset start failed: {error:#}")),
            Err(error) => Err(format!("reset preview failed: {error:#}")),
        };
        let published_reset = if reset.is_ok() {
            wait_for_snapshot(
                &service,
                "reset phase Prepared while the tab is held",
                |snapshot| {
                    snapshot.reset.as_ref().is_some_and(|summary| {
                        summary.state == crate::protocol::ResetPhase::Prepared
                            && summary.space_id == snapshot.space_id
                    })
                },
            )
            .await
        } else {
            Err("reset start did not succeed".into())
        };
        let actively_frozen = published_reset.as_ref().is_ok_and(|snapshot| {
            snapshot.reset.as_ref().is_some_and(|summary| {
                summary.state == crate::protocol::ResetPhase::Prepared
                    && summary.space_id == snapshot.space_id
            })
        });
        let run_still_owns_cli = !run.is_finished();
        let child_still_held = !fake.tab_done.exists();
        if !actively_frozen {
            let _ = cancel_tx.send(true);
        }
        fake.release_child();
        let timely = timeout(Duration::from_secs(8), &mut run).await;
        let (settled_in_budget, completed) = match timely {
            Ok(completed) => (true, completed),
            Err(_) => {
                let _ = cancel_tx.send(true);
                fake.release_child();
                (false, run.await)
            }
        };
        let child_exited = wait_for_file(&fake.tab_done).await;
        let startup_argument = arguments.try_recv().ok();
        let members = service.snapshots.borrow().members.clone();
        service.stop().await;

        assert!(
            preflight.is_ok(),
            "production Herdr version preflight passes"
        );
        assert!(setup.is_ok(), "the real actor fixture is ready: {setup:?}");
        assert!(ready, "the fake tab command reached its explicit barrier");
        assert!(reset.is_ok(), "the real actor entered reset: {reset:?}");
        assert!(
            published_reset.is_ok(),
            "prepared reset was not published while the tab was held: {}",
            published_reset
                .as_ref()
                .err()
                .map_or("snapshot wait succeeded".to_owned(), Clone::clone)
        );
        assert!(
            actively_frozen,
            "reset is actively prepared/incomplete while tab is held"
        );
        assert!(
            run_still_owns_cli && child_still_held,
            "a frozen space must not drop or kill the in-flight tab command"
        );
        assert!(
            child_exited,
            "the fake tab child exited after explicit release"
        );
        assert!(
            settled_in_budget,
            "the run settled after its held tab was released"
        );
        assert!(
            startup_argument.is_none(),
            "no startup grant reached Pi start"
        );
        assert_eq!(
            members.len(),
            1,
            "only the fixture reset participant remains"
        );
        assert!(
            members
                .iter()
                .all(|member| member.id.as_str() == "freeze-participant")
        );
        let outcomes = completed
            .expect("profile coordinator task did not panic")
            .expect("profile coordinator returned outcomes");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].state, EntryState::Failed);
        assert_eq!(outcomes[0].pane_id.as_deref(), Some("w1:p9"));
        assert_eq!(
            outcomes[0].reason,
            "tab created; Pi start not attempted after space entered a prepared or incomplete reset"
        );
        assert_eq!(
            outcomes[1].reason,
            "not attempted after space entered a prepared or incomplete reset"
        );
        assert_eq!(fake.args().matches("--BEGIN--").count(), 2);
    }

    #[tokio::test]
    async fn full_profile_run_reconciles_a_timed_out_held_tab_without_starting_later_slots() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("tab-sleep");
        let tab_budget = Duration::from_millis(500);
        let launcher = fake.launcher(server.herdr(), tab_budget, AGENT_COMMAND_TIMEOUT);
        let preflight = launcher.preflight().await;
        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        let (argument_tx, mut arguments) = mpsc::unbounded_channel();
        let (pending_tx, mut pending_commands) = mpsc::unbounded_channel();
        let launcher = launcher
            .observe_startup_arguments(argument_tx)
            .observe_pending_commands(pending_tx);
        let mut run = tokio::spawn(crate::profile_startup::run(ProfileLaunch {
            profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
            service: service.clone(),
            herdr: server.herdr().clone(),
            launcher,
            cwd: PathBuf::from("/tmp"),
            bootstrap: TabBootstrap::new(
                Path::new("/tmp/mustr-bin"),
                Path::new("/tmp/mustr-config"),
            )
            .expect("valid test bootstrap paths"),
            progress,
            cancel,
        }));

        let ready = wait_for_file(&fake.tab_ready).await;
        let pending_command = timeout(Duration::from_secs(3), pending_commands.recv())
            .await
            .ok()
            .flatten();
        let tab_timed_out = pending_command.as_deref() == Some("tab");
        let run_still_owns_pending_cli = !run.is_finished();
        let child_still_held = !fake.tab_done.exists();
        if !tab_timed_out {
            let _ = cancel_tx.send(true);
        }
        fake.release_child();
        let timely = timeout(Duration::from_secs(8), &mut run).await;
        let (settled_in_budget, completed) = match timely {
            Ok(completed) => (true, completed),
            Err(_) => {
                let _ = cancel_tx.send(true);
                fake.release_child();
                (false, run.await)
            }
        };
        let child_exited = wait_for_file(&fake.tab_done).await;
        let startup_argument = arguments.try_recv().ok();
        let members = service.snapshots.borrow().members.clone();
        service.stop().await;

        assert!(
            preflight.is_ok(),
            "production Herdr version preflight passes"
        );
        assert!(ready, "the fake tab command reached its explicit barrier");
        assert_eq!(pending_command.as_deref(), Some("tab"));
        assert!(
            tab_timed_out,
            "the test observed the actual tab command become pending"
        );
        assert!(run_still_owns_pending_cli && child_still_held);
        assert!(
            child_exited,
            "the fake tab child exited after explicit release"
        );
        assert!(
            settled_in_budget,
            "pending tab reconciliation settled after release"
        );
        assert!(
            startup_argument.is_none(),
            "no startup grant reached Pi start"
        );
        assert!(members.is_empty(), "no profile member was admitted");
        let outcomes = completed
            .expect("profile coordinator task did not panic")
            .expect("profile coordinator returned outcomes");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].state, EntryState::Uncertain);
        assert_eq!(outcomes[0].pane_id, None);
        assert_eq!(
            outcomes[0].reason,
            "tab create result or root identity is uncertain; no Pi was started; local Herdr command exited; remote mutation result remains uncertain"
        );
        assert_eq!(outcomes[1].state, EntryState::Failed);
        assert_eq!(
            outcomes[1].reason,
            "not attempted after tab create was ambiguous"
        );
        assert_eq!(fake.args().matches("--BEGIN--").count(), 2);
    }

    #[tokio::test]
    async fn full_profile_run_keeps_an_ambiguous_start_until_the_original_sixty_second_grant_deadline()
     {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("agent-sleep");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        assert_eq!(launcher.short_timeout, SHORT_COMMAND_TIMEOUT);
        assert_eq!(launcher.agent_timeout, AGENT_COMMAND_TIMEOUT);
        assert!(launcher.preflight().await.is_ok());

        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (_cancel_tx, cancel) = watch::channel(false);
        let (argument_tx, mut arguments) = mpsc::unbounded_channel();
        let launcher = launcher.observe_startup_arguments(argument_tx);
        let mut run = tokio::spawn(crate::profile_startup::run(ProfileLaunch {
            profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
            service: service.clone(),
            herdr: server.herdr().clone(),
            launcher,
            cwd: PathBuf::from("/tmp"),
            bootstrap: TabBootstrap::new(
                Path::new("/tmp/mustr-bin"),
                Path::new("/tmp/mustr-config"),
            )
            .expect("valid test bootstrap paths"),
            progress,
            cancel,
        }));

        let ready = wait_for_file(&fake.ready).await;
        let argument = timeout(Duration::from_secs(2), arguments.recv())
            .await
            .ok()
            .flatten();
        let observed_at = std::time::Instant::now();
        let (
            still_running_after_48_seconds,
            child_held_after_48_seconds,
            no_member_after_48_seconds,
            still_running_before_60_seconds,
            no_member_before_60_seconds,
        ) = if ready && argument.is_some() {
            tokio::time::sleep(Duration::from_secs(48)).await;
            let running_48 = !run.is_finished();
            let held_48 = !fake.done.exists();
            let empty_48 = service.snapshots.borrow().members.is_empty();
            tokio::time::sleep(Duration::from_secs(10)).await;
            let running_58 = !run.is_finished();
            let empty_58 = service.snapshots.borrow().members.is_empty();
            (running_48, held_48, empty_48, running_58, empty_58)
        } else {
            (false, false, false, false, false)
        };

        fake.release_child();
        let timely = timeout(Duration::from_secs(20), &mut run).await;
        let (settled_in_budget, completed) = match timely {
            Ok(completed) => (true, completed),
            Err(_) => (false, run.await),
        };
        let child_exited = wait_for_file(&fake.done).await;
        let elapsed = observed_at.elapsed();
        let members = service.snapshots.borrow().members.clone();
        service.stop().await;

        assert!(ready, "the fake agent reached its held startup barrier");
        assert!(
            argument.is_some(),
            "the test-only memory channel observed the actual issued flag"
        );
        assert!(
            still_running_after_48_seconds,
            "the profile run outlived the 45-second agent CLI budget"
        );
        assert!(
            child_held_after_48_seconds,
            "the fake mutation remained owned and held past 45 seconds"
        );
        assert!(
            no_member_after_48_seconds,
            "no matching member appeared before the original grant deadline"
        );
        assert!(
            still_running_before_60_seconds,
            "the run must wait for the original grant deadline, not restart a fresh window"
        );
        assert!(
            no_member_before_60_seconds,
            "there was no admission before the shared deadline"
        );
        assert!(child_exited, "the fake child exited after explicit release");
        assert!(
            settled_in_budget,
            "the run settled after child release within the bounded cleanup interval"
        );
        assert!(members.is_empty(), "no live hub member was admitted");
        let outcomes = completed
            .expect("profile coordinator task did not panic")
            .expect("profile coordinator returned outcomes");
        assert!(
            (Duration::from_secs(58)..=Duration::from_secs(75)).contains(&elapsed),
            "completion should use the original 60-second grant deadline after the 45-second CLI limit; observed {elapsed:?}"
        );
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].state, EntryState::Uncertain);
        assert!(outcomes[0].reason.contains("shared grant deadline"));
        assert!(outcomes[0].reason.contains("was not retried"));
        assert!(
            outcomes[0]
                .reason
                .contains("local Herdr command exited; remote mutation result remains uncertain"),
            "the run must report the pending child reconciliation result: {}",
            outcomes[0].reason
        );
        assert_eq!(outcomes[1].state, EntryState::Failed);
        assert!(outcomes[1].reason.contains("not attempted"));
        assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
    }

    #[tokio::test]
    async fn full_profile_run_describes_admission_present_when_cancellation_halts_agent_start() {
        let server = PaneServer::new("w1:p9", "w1", None).await;
        let fake = FakeCli::new("agent-sleep");
        let launcher = fake.launcher(server.herdr(), SHORT_COMMAND_TIMEOUT, AGENT_COMMAND_TIMEOUT);
        let preflight = launcher.preflight().await;
        let state = tempfile::tempdir().expect("temporary profile run state");
        let service = Service::start(
            SpaceId::new("w1"),
            state.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .expect("start the real service actor");
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        let (argument_tx, mut arguments) = mpsc::unbounded_channel();
        let launcher = launcher.observe_startup_arguments(argument_tx);
        let mut run = if preflight.is_ok() {
            Some(tokio::spawn(crate::profile_startup::run(ProfileLaunch {
                profile: profile_with_roles(&[("researcher", false), ("reviewer", false)]),
                service: service.clone(),
                herdr: server.herdr().clone(),
                launcher,
                cwd: PathBuf::from("/tmp"),
                bootstrap: TabBootstrap::new(
                    Path::new("/tmp/mustr-bin"),
                    Path::new("/tmp/mustr-config"),
                )
                .expect("valid test bootstrap paths"),
                progress,
                cancel,
            })))
        } else {
            None
        };

        let ready = wait_for_file(&fake.ready).await;
        let argument = timeout(Duration::from_secs(2), arguments.recv())
            .await
            .ok()
            .flatten();
        let grant = argument
            .as_deref()
            .and_then(|argument| captured_startup_grant(argument, "researcher"));
        let admitted = if let Some(grant) = grant {
            let consumed = service
                .consume_startup_grant(
                    grant.clone(),
                    "researcher".into(),
                    "w1:p9".into(),
                    SpaceId::new("w1"),
                )
                .await;
            Some(match consumed {
                Err(error) => Err(error),
                Ok(()) => {
                    service
                        .join(
                            72,
                            Request {
                                id: "admit-before-cancel".into(),
                                command: Command::Join {
                                    version: VERSION,
                                    agent_id: AgentId::new("researcher-before-cancel"),
                                    name: "researcher-before-cancel".into(),
                                    role: "researcher".into(),
                                    pane_id: "w1:p9".into(),
                                    space_id: SpaceId::new("w1"),
                                    cwd: None,
                                    reset: None,
                                    startup_grant: Some(grant),
                                },
                            },
                            false,
                        )
                        .await
                }
            })
        } else {
            None
        };
        let admission_snapshot = if matches!(&admitted, Some(Ok(response)) if response.ok) {
            Some(
                wait_for_snapshot(
                    &service,
                    "the matching admitted member before cancellation",
                    |snapshot| {
                        snapshot.members.iter().any(|member| {
                            member.id == AgentId::new("researcher-before-cancel")
                                && member.space_id == SpaceId::new("w1")
                                && member.role == "researcher"
                                && member.pane_id == "w1:p9"
                                && member.activity != Activity::Offline
                        })
                    },
                )
                .await,
            )
        } else {
            None
        };
        cancel_tx.send_replace(true);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let still_owned_after_cancel = run.as_ref().is_some_and(|run| !run.is_finished());
        let child_still_held = !fake.done.exists();

        fake.release_child();
        let completion = if let Some(run) = run.as_mut() {
            let timely = timeout(Duration::from_secs(20), &mut *run).await;
            Some(match timely {
                Ok(completed) => (true, completed),
                Err(_) => (false, run.await),
            })
        } else {
            None
        };
        let child_exited = wait_for_file(&fake.done).await;
        let post_run_sync_confirmed = matches!(
            service
                .request(
                    72,
                    Request {
                        id: "post-run-membership-fence".into(),
                        command: Command::Sync {
                            activity: Activity::Working,
                            receive: false,
                        },
                    },
                )
                .await,
            Ok(response) if response.ok
        );
        let post_run_snapshot = if post_run_sync_confirmed {
            Some(
                wait_for_snapshot(
                    &service,
                    "the matching member in a fresh post-run snapshot",
                    |snapshot| {
                        snapshot.members.iter().any(|member| {
                            member.id == AgentId::new("researcher-before-cancel")
                                && member.space_id == SpaceId::new("w1")
                                && member.role == "researcher"
                                && member.pane_id == "w1:p9"
                                && member.activity == Activity::Working
                        })
                    },
                )
                .await,
            )
        } else {
            None
        };
        let members = post_run_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.as_ref().ok())
            .map(|snapshot| snapshot.members.clone())
            .unwrap_or_default();
        service.stop().await;

        assert!(
            preflight.is_ok(),
            "production Herdr version preflight passes"
        );
        assert!(ready, "the fake agent reached its held startup barrier");
        assert!(
            still_owned_after_cancel,
            "cancellation retains the in-flight agent command"
        );
        assert!(
            child_still_held,
            "cancellation does not release or kill the CLI child"
        );
        assert!(child_exited, "the fake child exits after explicit release");
        let (_, completed) = completion.expect("profile run started after preflight");
        assert!(completed.is_ok(), "profile coordinator task did not panic");
        let outcomes = completed
            .expect("bounded profile cleanup")
            .expect("profile coordinator returned outcomes");
        let admitted = admitted
            .expect("the real service actor checked the observed startup grant")
            .expect("service actor replied to Join");
        assert!(
            admitted.ok,
            "the matching member was admitted before cancellation"
        );
        assert!(
            admission_snapshot.as_ref().is_some_and(Result::is_ok),
            "matching admission was not published before cancellation: {}",
            admission_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.as_ref().err())
                .map_or("Join did not publish".to_owned(), Clone::clone)
        );
        assert!(post_run_sync_confirmed, "post-run actor fence was rejected");
        assert!(
            post_run_snapshot.as_ref().is_some_and(Result::is_ok),
            "fresh post-run membership snapshot was not published: {}",
            post_run_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.as_ref().err())
                .map_or("post-run Sync did not publish".to_owned(), Clone::clone)
        );
        assert!(members.iter().any(|member| {
            member.id == AgentId::new("researcher-before-cancel")
                && member.role == "researcher"
                && member.pane_id == "w1:p9"
        }));
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].state, EntryState::Joined);
        assert!(
            outcomes[0].reason.contains(
                "matching live hub member was present when launch cancelled was observed"
            )
        );
        assert_eq!(outcomes[1].state, EntryState::Failed);
        assert!(
            outcomes[1]
                .reason
                .contains("not attempted after launch cancelled")
        );
        assert_eq!(fake.args().matches("--BEGIN--").count(), 3);
    }
}
