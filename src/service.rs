//! Single-writer actor: SQLite and filesystem work stay off async/TUI threads.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, ensure, eyre};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    config::Scheduling,
    hub::State,
    protocol::*,
    store::Store,
    watchers::{PrSnapshot, TodoSnapshot},
};

mod startup_grants;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrSelection {
    pub epoch: u64,
    pub agent_id: AgentId,
    pub name: String,
    pub role: String,
    pub pane_id: String,
    pub cwd: PathBuf,
}

pub(crate) struct IssuedStartupGrant {
    pub handoff: StartupGrant,
    pub issued_at: Instant,
    pub deadline: Instant,
}

pub(crate) enum Event {
    IssueStartupGrant {
        launch_id: LaunchId,
        role: String,
        pane_id: String,
        reply: oneshot::Sender<std::result::Result<IssuedStartupGrant, String>>,
    },
    ConsumeStartupGrant {
        grant: StartupGrant,
        role: String,
        pane_id: String,
        space_id: SpaceId,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    RetireStartupGrant {
        token: StartupGrantToken,
        reply: oneshot::Sender<()>,
    },
    Request {
        connection: u64,
        request: Request,
        /// Server-resolved role metadata; never decoded from the public Join.
        role_manager: Option<bool>,
        reply: oneshot::Sender<Response>,
    },
    Disconnect(u64),
    Space(std::result::Result<Space, String>),
    Tick,
    Mode {
        space: SpaceId,
        mode: Mode,
        enabled: bool,
        epoch: u64,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    EnablePr {
        space: SpaceId,
        selection: PrSelection,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    ResetPreview {
        space: SpaceId,
        reply: oneshot::Sender<std::result::Result<ResetPreview, String>>,
    },
    ResetStart {
        preview: ResetPreview,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    ResetAbandon {
        space: SpaceId,
        plan_id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    Todo {
        space: SpaceId,
        generation: u64,
        result: std::result::Result<TodoSnapshot, String>,
    },
    Pr {
        space: SpaceId,
        generation: u64,
        result: std::result::Result<PrSnapshot, String>,
    },
    Stop(oneshot::Sender<()>),
}

#[derive(Clone)]
pub(crate) struct Service {
    pub space_id: SpaceId,
    pub tx: mpsc::Sender<Event>,
    pub snapshots: watch::Receiver<Snapshot>,
}

impl Service {
    pub async fn start(space_id: SpaceId, path: PathBuf, scheduling: Scheduling) -> Result<Self> {
        let (tx, rx) = mpsc::channel(128);
        let (snapshots, snapshot_rx) = watch::channel(Snapshot::empty(space_id.clone()));
        let (ready_tx, ready_rx) = oneshot::channel();
        let actor_space = space_id.clone();
        tokio::task::spawn_blocking(move || {
            match Actor::new(actor_space, path, scheduling, snapshots) {
                Ok(mut actor) => {
                    actor.publish_snapshot(now());
                    let _ = ready_tx.send(Ok(()));
                    actor.run(rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            }
        });
        ready_rx.await?.map_err(|e| eyre!(e))?;
        Ok(Self {
            space_id,
            tx,
            snapshots: snapshot_rx,
        })
    }

    pub async fn request(&self, connection: u64, request: Request) -> Result<Response> {
        self.request_with_role(connection, request, None).await
    }

    /// Submit a Join with the manager flag resolved by the server from the
    /// selected root role. This metadata never appears in the public protocol.
    pub async fn join(&self, connection: u64, request: Request, manager: bool) -> Result<Response> {
        ensure!(
            matches!(&request.command, Command::Join { .. }),
            "validated role metadata is only valid for Join"
        );
        self.request_with_role(connection, request, Some(manager))
            .await
    }

    pub async fn issue_startup_grant(
        &self,
        launch_id: LaunchId,
        role: String,
        pane_id: String,
    ) -> Result<IssuedStartupGrant> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::IssueStartupGrant {
                launch_id,
                role,
                pane_id,
                reply,
            })
            .await
            .map_err(|_| eyre!("hub stopped before startup grant issuance"))?;
        response
            .await
            .map_err(|_| eyre!("hub stopped before startup grant issuance reply"))?
            .map_err(|error| eyre!(error))
    }

    pub async fn consume_startup_grant(
        &self,
        grant: StartupGrant,
        role: String,
        pane_id: String,
        space_id: SpaceId,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::ConsumeStartupGrant {
                grant,
                role,
                pane_id,
                space_id,
                reply,
            })
            .await
            .map_err(|_| eyre!("hub stopped before startup grant consumption"))?;
        response
            .await
            .map_err(|_| eyre!("hub stopped before startup grant consumption reply"))?
            .map_err(|error| eyre!(error))
    }

    pub async fn retire_startup_grant(&self, token: StartupGrantToken) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::RetireStartupGrant { token, reply })
            .await
            .map_err(|_| eyre!("hub stopped before startup grant retirement"))?;
        response
            .await
            .map_err(|_| eyre!("hub stopped before startup grant retirement reply"))?;
        Ok(())
    }

    async fn request_with_role(
        &self,
        connection: u64,
        request: Request,
        role_manager: Option<bool>,
    ) -> Result<Response> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::Request {
                connection,
                request,
                role_manager,
                reply,
            })
            .await?;
        Ok(response.await?)
    }

    pub async fn mode(&self, mode: Mode, enabled: bool) -> Result<()> {
        let (reply, response) = oneshot::channel();
        let epoch = self.snapshots.borrow().epoch;
        self.tx
            .send(Event::Mode {
                space: self.space_id.clone(),
                mode,
                enabled,
                epoch,
                reply,
            })
            .await?;
        response.await?.map_err(|e| eyre!(e))
    }

    /// Enable automatic PR discovery for the exact participant selection shown to the user.
    pub async fn enable_pr(&self, selection: PrSelection) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::EnablePr {
                space: self.space_id.clone(),
                selection,
                reply,
            })
            .await?;
        response.await?.map_err(|error| eyre!(error))
    }

    pub async fn reset_preview(&self) -> Result<ResetPreview> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::ResetPreview {
                space: self.space_id.clone(),
                reply,
            })
            .await?;
        response.await?.map_err(|error| eyre!(error))
    }

    pub async fn reset_start(&self, preview: ResetPreview) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx.send(Event::ResetStart { preview, reply }).await?;
        response.await?.map_err(|error| eyre!(error))
    }

    pub async fn reset_abandon(&self, plan_id: String) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::ResetAbandon {
                space: self.space_id.clone(),
                plan_id,
                reply,
            })
            .await?;
        response.await?.map_err(|error| eyre!(error))
    }

    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Event::Stop(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

#[derive(Clone)]
struct Binding {
    agent: AgentId,
    space: SpaceId,
    epoch: u64,
    revoked: bool,
}

struct Preview {
    approved: ResetPreview,
    fingerprint: String,
    incarnations: BTreeMap<AgentId, u64>,
}

struct Actor {
    space_id: SpaceId,
    state: State,
    store: Store,
    scheduling: Scheduling,
    connections: BTreeMap<u64, Binding>,
    incarnations: BTreeMap<AgentId, u64>,
    preview: Option<Preview>,
    space: Option<Space>,
    error: Option<String>,
    snapshots: watch::Sender<Snapshot>,
    startup_grants: startup_grants::Ledger,
}

impl Actor {
    fn new(
        space_id: SpaceId,
        path: PathBuf,
        scheduling: Scheduling,
        snapshots: watch::Sender<Snapshot>,
    ) -> Result<Self> {
        let mut store = Store::open(&path, space_id.clone())?;
        let mut state = store.load()?;
        state.validate_space(&space_id)?;
        state.recover();
        store.save(&state)?;
        Ok(Self {
            space_id,
            state,
            store,
            scheduling,
            connections: BTreeMap::new(),
            incarnations: BTreeMap::new(),
            preview: None,
            space: None,
            error: None,
            snapshots,
            startup_grants: startup_grants::Ledger::new(),
        })
    }

    fn commit(&mut self, next: State) -> Result<()> {
        if next != self.state {
            self.store.save(&next)?;
            self.state = next;
        }
        Ok(())
    }

    fn publish_snapshot(&self, now: u64) {
        self.snapshots.send_replace(self.state.snapshot(
            self.space.clone(),
            self.error.clone(),
            now,
        ));
    }

    fn binding_valid(&self, connection: u64, binding: &Binding) -> bool {
        !binding.revoked
            && self.incarnations.get(&binding.agent) == Some(&connection)
            && self
                .state
                .connection_epoch_valid(&binding.agent, &binding.space, binding.epoch)
    }

    fn sender(&self, connection: u64) -> Result<Option<AgentId>> {
        self.connections
            .get(&connection)
            .map(|binding| {
                ensure!(
                    self.binding_valid(connection, binding),
                    "connection is retired; rejoin explicitly from the correct session"
                );
                Ok(binding.agent.clone())
            })
            .transpose()
    }

    fn preview(&mut self, space: SpaceId) -> Result<ResetPreview> {
        ensure!(
            space == self.space_id,
            "control event belongs to another space"
        );
        ensure!(
            self.space
                .as_ref()
                .is_some_and(|current| current.id == space),
            "space is unavailable; refresh before confirming"
        );
        let mut approved = self.state.reset_preview(&space)?;
        approved.nonce = uuid::Uuid::new_v4().to_string();
        let incarnations = approved
            .participants
            .iter()
            .map(|p| {
                (
                    p.agent_id.clone(),
                    self.incarnations
                        .get(&p.agent_id)
                        .copied()
                        .unwrap_or_default(),
                )
            })
            .collect();
        self.preview = Some(Preview {
            approved: approved.clone(),
            fingerprint: self.state.reset_fingerprint(&space)?,
            incarnations,
        });
        Ok(approved)
    }

    fn start_reset(&mut self, preview: ResetPreview) -> Result<()> {
        ensure!(
            preview.space_id == self.space_id,
            "reset preview belongs to another space"
        );
        ensure!(
            self.space
                .as_ref()
                .is_some_and(|current| current.id == preview.space_id),
            "space is unavailable; preview again"
        );
        let approved = self
            .preview
            .as_ref()
            .ok_or_else(|| eyre!("preview expired; preview and confirm again"))?;
        ensure!(
            approved.approved == preview
                && approved.fingerprint == self.state.reset_fingerprint(&preview.space_id)?,
            "reset preview changed; preview and confirm again"
        );
        ensure!(
            approved
                .incarnations
                .iter()
                .all(|(agent, connection)| self.incarnations.get(agent) == Some(connection)),
            "participant connection changed; preview again"
        );
        let mut next = self.state.clone();
        next.start_reset(&preview)?;
        self.commit(next)?;
        self.preview = None;
        Ok(())
    }

    fn run(&mut self, mut rx: mpsc::Receiver<Event>) {
        while let Some(event) = rx.blocking_recv() {
            let now = now();
            let monotonic_now = Instant::now();
            match event {
                Event::IssueStartupGrant {
                    launch_id,
                    role,
                    pane_id,
                    reply,
                } => {
                    let result = if self.state.reset_frozen(&self.space_id) {
                        Err("space is frozen; startup grant cannot be issued".to_owned())
                    } else {
                        self.startup_grants
                            .issue(
                                launch_id,
                                role,
                                pane_id,
                                self.space_id.clone(),
                                monotonic_now,
                            )
                            .map_err(str::to_owned)
                    };
                    let _ = reply.send(result);
                }
                Event::ConsumeStartupGrant {
                    grant,
                    role,
                    pane_id,
                    space_id,
                    reply,
                } => {
                    let reset_frozen = self.state.reset_frozen(&space_id);
                    let result = self
                        .startup_grants
                        .consume(
                            grant,
                            &role,
                            &pane_id,
                            &space_id,
                            monotonic_now,
                            reset_frozen,
                        )
                        .map_err(str::to_owned);
                    let _ = reply.send(result);
                }
                Event::RetireStartupGrant { token, reply } => {
                    self.startup_grants.retire(&token);
                    let _ = reply.send(());
                }
                Event::Request {
                    connection,
                    request,
                    role_manager,
                    reply,
                } => {
                    let has_startup_grant = matches!(
                        &request.command,
                        Command::Join {
                            startup_grant: Some(_),
                            ..
                        }
                    );
                    let startup_authorization = match &request.command {
                        Command::Join {
                            role,
                            pane_id,
                            space_id,
                            reset,
                            startup_grant: Some(grant),
                            ..
                        } => self
                            .startup_grants
                            .authorize_join(
                                grant,
                                role,
                                pane_id,
                                space_id,
                                monotonic_now,
                                self.state.reset_frozen(space_id),
                            )
                            .and_then(|()| {
                                if reset.is_some() {
                                    Err("startup grants cannot be combined with a reset receipt")
                                } else {
                                    Ok(())
                                }
                            })
                            .map_err(|error| eyre!(error)),
                        _ => Ok(()),
                    };
                    let mut next = self.state.clone();
                    let result = startup_authorization
                        .and_then(|()| self.sender(connection))
                        .and_then(|sender| match role_manager {
                            Some(manager) => next.apply_validated_join(
                                &request,
                                sender.as_ref(),
                                now,
                                &self.scheduling,
                                manager,
                            ),
                            None => next.apply(&request, sender.as_ref(), now, &self.scheduling),
                        })
                        .and_then(|value| {
                            if matches!(request.command, Command::Sync { .. })
                                && value["inbox"].is_null()
                                && next.messages == self.state.messages
                                && next.tasks == self.state.tasks
                                && next.automation == self.state.automation
                                && next.reset_metadata_equal(&self.state)
                            {
                                // Only presence/idle timestamps may bypass persistence.
                                // A no-inbox sync can still cancel automatic prompts or
                                // refresh a PR deadline: commit those before replying.
                                // Sync cannot change membership or the deduplication cache.
                                self.state = next;
                            } else {
                                self.commit(next)?;
                            }
                            Ok(value)
                        });
                    let response = match result {
                        Ok(mut value) => {
                            if let Command::Join {
                                agent_id, space_id, ..
                            } = &request.command
                            {
                                self.incarnations.insert(agent_id.clone(), connection);
                                self.connections.insert(
                                    connection,
                                    Binding {
                                        agent: agent_id.clone(),
                                        space: space_id.clone(),
                                        epoch: self.state.reset_epoch(space_id),
                                        revoked: false,
                                    },
                                );
                            }
                            if matches!(request.command, Command::Leave)
                                && let Some(binding) = self.connections.remove(&connection)
                                && self.incarnations.get(&binding.agent) == Some(&connection)
                            {
                                self.incarnations.remove(&binding.agent);
                            }
                            if has_startup_grant && let Some(result) = value.as_object_mut() {
                                result.insert("startup_grant_protocol".into(), Value::from(1));
                            }
                            Response::success(request.id, value)
                        }
                        Err(error) => {
                            tracing::debug!(connection, error = %error, "request rejected");
                            Response::failure(request.id, error)
                        }
                    };
                    let _ = reply.send(response);
                }
                Event::Disconnect(connection) => {
                    if let Some(binding) = self.connections.remove(&connection) {
                        if self.binding_valid(connection, &binding) {
                            let mut next = self.state.clone();
                            next.disconnect(&binding.agent);
                            if let Err(error) = self.commit(next) {
                                self.error = Some(error.to_string());
                            }
                        }
                        if self.incarnations.get(&binding.agent) == Some(&connection) {
                            self.incarnations.remove(&binding.agent);
                        }
                    }
                }
                Event::Space(result) => match result {
                    Ok(space) if space.id == self.space_id => {
                        self.space = Some(space);
                        self.error = None;
                    }
                    Ok(_) => {
                        self.space = None;
                        self.preview = None;
                        self.error = Some("herdr returned metadata for another space".into());
                    }
                    Err(error) => {
                        self.space = None;
                        self.preview = None;
                        self.error = Some(error);
                    }
                },
                Event::Mode {
                    space,
                    mode,
                    enabled,
                    epoch,
                    reply,
                } => {
                    let mut next = self.state.clone();
                    let result = (|| {
                        ensure!(
                            space == self.space_id,
                            "mode event belongs to another space"
                        );
                        ensure!(
                            epoch == self.state.reset_epoch(&space),
                            "mode action predates reset; refresh first"
                        );
                        next.set_mode(&space, mode, enabled, now)
                    })()
                    .and_then(|()| self.commit(next))
                    .map_err(|e| e.to_string());
                    let _ = reply.send(result);
                }
                Event::EnablePr {
                    space,
                    selection,
                    reply,
                } => {
                    let mut next = self.state.clone();
                    let result = (|| {
                        ensure!(
                            space == self.space_id,
                            "PR discovery event belongs to another space"
                        );
                        ensure!(
                            selection.epoch == self.state.reset_epoch(&space),
                            "PR participant selection predates reset or space change; refresh first"
                        );
                        ensure!(
                            self.space
                                .as_ref()
                                .is_some_and(|current| current.id == space),
                            "space is unavailable; refresh before enabling PR discovery"
                        );
                        let member = next
                            .members
                            .get(&selection.agent_id)
                            .filter(|member| {
                                member.name == selection.name
                                    && member.role == selection.role
                                    && member.pane_id == selection.pane_id
                                    && member.cwd.as_ref() == Some(&selection.cwd)
                                    && !matches!(
                                        member.activity,
                                        Activity::Offline | Activity::Paused
                                    )
                            })
                            .ok_or_else(|| {
                                eyre!(
                                    "selected participant or checkout changed; reopen the PR picker"
                                )
                            })?;
                        ensure!(
                            member
                                .cwd
                                .as_ref()
                                .is_some_and(|cwd| cwd.is_absolute() && cwd.is_dir()),
                            "selected participant checkout is unavailable; refresh and retry"
                        );
                        next.enable_pr_discovery(&space, &selection.agent_id, now)
                    })()
                    .and_then(|()| self.commit(next))
                    .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                Event::ResetPreview { space, reply } => {
                    let result = self.preview(space).map_err(|e| e.to_string());
                    let _ = reply.send(result);
                }
                Event::ResetStart { preview, reply } => {
                    let result = self.start_reset(preview).map_err(|e| e.to_string());
                    if result.is_ok() {
                        self.startup_grants.clear();
                    }
                    let _ = reply.send(result);
                }
                Event::ResetAbandon {
                    space,
                    plan_id,
                    reply,
                } => {
                    let mut next = self.state.clone();
                    let result = next
                        .abandon_reset(&space, &plan_id)
                        .and_then(|()| self.commit(next))
                        .map_err(|e| e.to_string());
                    if result.is_ok() {
                        let originals: Vec<_> = self
                            .state
                            .snapshot(self.space.clone(), None, now)
                            .reset
                            .into_iter()
                            .flat_map(|reset| reset.participants)
                            .map(|progress| progress.participant.agent_id)
                            .collect();
                        for binding in self
                            .connections
                            .values_mut()
                            .filter(|b| b.space == space && originals.contains(&b.agent))
                        {
                            binding.revoked = true;
                        }
                    }
                    let _ = reply.send(result);
                }
                Event::Stop(reply) => {
                    self.startup_grants.clear();
                    let mut next = self.state.clone();
                    next.recover();
                    if let Err(error) = self.commit(next) {
                        tracing::error!(%error, "shutdown persistence failed");
                    }
                    let _ = reply.send(());
                    break;
                }
                event => {
                    let mut next = self.state.clone();
                    let result = match event {
                        Event::Tick => next.tick(now, &self.scheduling),
                        Event::Todo {
                            space,
                            generation,
                            result,
                        } => next.todo_result(&space, generation, result, now, &self.scheduling),
                        Event::Pr {
                            space,
                            generation,
                            result,
                        } => next.pr_result(&space, generation, result, now),
                        _ => unreachable!("handled above"),
                    }
                    .and_then(|()| self.commit(next));
                    if let Err(error) = result {
                        tracing::error!(%error, "scheduler failed");
                        self.error = Some(error.to_string());
                    }
                }
            }
            self.startup_grants.prune_expired(monotonic_now);
            self.publish_snapshot(now);
        }
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    mod reset;

    #[tokio::test]
    async fn startup_grant_issuance_is_bound_and_never_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let space = SpaceId::new("w1");
        let service = Service::start(space.clone(), path.clone(), Scheduling::default())
            .await
            .unwrap();
        let issued = service
            .issue_startup_grant(
                LaunchId::new("launch-1".into()),
                "reviewer".into(),
                "w1:p9".into(),
            )
            .await
            .unwrap();
        assert_eq!(
            issued.deadline - issued.issued_at,
            std::time::Duration::from_secs(60)
        );
        let token = issued.handoff.token.as_str().to_owned();
        let mismatch = service
            .consume_startup_grant(
                issued.handoff.clone(),
                "researcher".into(),
                "w1:p9".into(),
                space.clone(),
            )
            .await
            .unwrap_err();
        assert!(!mismatch.to_string().contains(&token));
        assert!(
            service
                .consume_startup_grant(issued.handoff, "reviewer".into(), "w1:p9".into(), space,)
                .await
                .is_err(),
            "a mismatched recognized token is burned"
        );
        assert!(
            !serde_json::to_string(&*service.snapshots.borrow())
                .unwrap()
                .contains(&token)
        );
        assert!(
            !serde_json::to_string(
                &Store::open(&path, SpaceId::new("w1"))
                    .unwrap()
                    .load()
                    .unwrap()
            )
            .unwrap()
            .contains(&token)
        );
        service.stop().await;
    }

    #[tokio::test]
    async fn direct_service_join_rejects_a_matching_but_unconsumed_grant() {
        let directory = tempfile::tempdir().unwrap();
        let space = SpaceId::new("w1");
        let service = Service::start(
            space.clone(),
            directory.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .unwrap();
        let issued = service
            .issue_startup_grant(
                LaunchId::new("direct-unconsumed".into()),
                "reviewer".into(),
                "w1:p1".into(),
            )
            .await
            .unwrap();
        let mut snapshots = service.snapshots.clone();
        snapshots.borrow_and_update();
        let response = service
            .join(
                1,
                Request {
                    id: "unconsumed-startup-join".into(),
                    command: Command::Join {
                        version: VERSION,
                        agent_id: AgentId::new("startup-agent"),
                        name: "startup-agent".into(),
                        role: "reviewer".into(),
                        pane_id: "w1:p1".into(),
                        space_id: space.clone(),
                        cwd: None,
                        reset: None,
                        startup_grant: Some(issued.handoff.clone()),
                    },
                },
                false,
            )
            .await
            .unwrap();
        assert!(!response.ok);
        assert!(response.result.is_none());
        assert!(response.error.as_deref().is_some_and(|error| {
            error.contains("startup grant was not consumed at the Join boundary")
        }));
        tokio::time::timeout(std::time::Duration::from_secs(1), snapshots.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(snapshots.borrow().members.is_empty());
        assert!(
            service
                .consume_startup_grant(issued.handoff, "reviewer".into(), "w1:p1".into(), space,)
                .await
                .is_err(),
            "direct Join cannot leave a reusable unconsumed grant"
        );
        service.stop().await;
    }

    #[tokio::test]
    async fn startup_grant_identity_mismatches_burn_recognized_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let space = SpaceId::new("w1");
        let service = Service::start(
            space.clone(),
            directory.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .unwrap();

        for (index, mismatch) in ["version", "launch", "role", "pane", "space"]
            .into_iter()
            .enumerate()
        {
            let issued = service
                .issue_startup_grant(
                    LaunchId::new(format!("launch-{index}")),
                    "reviewer".into(),
                    "w1:p9".into(),
                )
                .await
                .unwrap();
            let mut grant = issued.handoff.clone();
            let mut role = "reviewer".to_owned();
            let mut pane = "w1:p9".to_owned();
            let mut presented_space = space.clone();
            match mismatch {
                "version" => grant.version = 2,
                "launch" => grant.launch_id = LaunchId::new("other-launch".into()),
                "role" => role = "researcher".into(),
                "pane" => pane = "w1:p8".into(),
                "space" => presented_space = SpaceId::new("w2"),
                _ => unreachable!(),
            }
            assert!(
                service
                    .consume_startup_grant(grant, role, pane, presented_space,)
                    .await
                    .is_err(),
                "{mismatch} mismatch must reject"
            );
            assert!(
                service
                    .consume_startup_grant(
                        issued.handoff,
                        "reviewer".into(),
                        "w1:p9".into(),
                        space.clone(),
                    )
                    .await
                    .is_err(),
                "{mismatch} mismatch must spend the recognized token"
            );
        }
        service.stop().await;
    }

    #[tokio::test]
    async fn concurrent_startup_grant_claims_have_one_winner() {
        let directory = tempfile::tempdir().unwrap();
        let space = SpaceId::new("w1");
        let service = Service::start(
            space.clone(),
            directory.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .unwrap();
        let issued = service
            .issue_startup_grant(
                LaunchId::new("launch-race".into()),
                "reviewer".into(),
                "w1:p9".into(),
            )
            .await
            .unwrap();
        let claim = |grant| {
            service.consume_startup_grant(grant, "reviewer".into(), "w1:p9".into(), space.clone())
        };
        let (first, second) = tokio::join!(claim(issued.handoff.clone()), claim(issued.handoff));
        assert_ne!(first.is_ok(), second.is_ok());
        service.stop().await;
    }

    #[tokio::test]
    async fn retired_and_previous_run_startup_grants_never_authorize() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let space = SpaceId::new("w1");
        let service = Service::start(space.clone(), path.clone(), Scheduling::default())
            .await
            .unwrap();
        let retired = service
            .issue_startup_grant(
                LaunchId::new("launch-retired".into()),
                "reviewer".into(),
                "w1:p9".into(),
            )
            .await
            .unwrap();
        service
            .retire_startup_grant(retired.handoff.token.clone())
            .await
            .unwrap();
        assert!(
            service
                .consume_startup_grant(
                    retired.handoff,
                    "reviewer".into(),
                    "w1:p9".into(),
                    space.clone(),
                )
                .await
                .is_err()
        );

        let previous_run = service
            .issue_startup_grant(
                LaunchId::new("launch-old-run".into()),
                "reviewer".into(),
                "w1:p9".into(),
            )
            .await
            .unwrap();
        service.stop().await;
        let next_run = Service::start(space.clone(), path, Scheduling::default())
            .await
            .unwrap();
        assert!(
            next_run
                .consume_startup_grant(
                    previous_run.handoff,
                    "reviewer".into(),
                    "w1:p9".into(),
                    space,
                )
                .await
                .is_err()
        );
        next_run.stop().await;
    }

    #[tokio::test]
    async fn startup_grants_are_spent_when_normal_join_admission_rejects() {
        let directory = tempfile::tempdir().unwrap();
        let space = SpaceId::new("w1");
        let service = Service::start(
            space.clone(),
            directory.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .unwrap();
        let join = |id: &str, agent: &str, name: &str, pane: &str, startup_grant| Request {
            id: id.into(),
            command: Command::Join {
                version: VERSION,
                agent_id: AgentId::new(agent),
                name: name.into(),
                role: "reviewer".into(),
                pane_id: pane.into(),
                space_id: space.clone(),
                cwd: None,
                reset: None,
                startup_grant,
            },
        };
        let existing = service
            .join(
                1,
                join("existing", "existing-agent", "shared", "w1:p1", None),
                true,
            )
            .await
            .unwrap();
        assert!(existing.ok);

        for (id, agent, name, pane, manager, expected) in [
            (
                "duplicate-name",
                "name-agent",
                "shared",
                "w1:p2",
                false,
                "name already",
            ),
            (
                "duplicate-pane",
                "pane-agent",
                "new-name",
                "w1:p1",
                false,
                "already occupies",
            ),
            (
                "second-manager",
                "manager-agent",
                "manager-two",
                "w1:p3",
                true,
                "another manager",
            ),
        ] {
            let grant = service
                .issue_startup_grant(
                    LaunchId::new(format!("launch-{id}")),
                    "reviewer".into(),
                    pane.into(),
                )
                .await
                .unwrap()
                .handoff;
            service
                .consume_startup_grant(grant.clone(), "reviewer".into(), pane.into(), space.clone())
                .await
                .unwrap();
            let rejected = service
                .join(2, join(id, agent, name, pane, Some(grant.clone())), manager)
                .await
                .unwrap();
            assert!(!rejected.ok);
            assert!(
                rejected
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains(expected)),
                "{id}: {:?}",
                rejected.error
            );
            assert!(
                rejected.result.is_none(),
                "rejected startup admission has no capability acknowledgement"
            );
            assert!(
                service
                    .consume_startup_grant(grant, "reviewer".into(), pane.into(), space.clone(),)
                    .await
                    .is_err(),
                "{id} rejection must leave the grant spent"
            );
        }
        assert_eq!(service.snapshots.borrow().members.len(), 1);
        service.stop().await;
    }

    #[tokio::test]
    async fn startup_grant_is_spent_when_participant_capacity_rejects_join() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let space = SpaceId::new("w1");
        let mut state = State::new(space.clone());
        for index in 0..256 {
            let agent_id = AgentId::new(format!("existing-{index}"));
            state.members.insert(
                agent_id.clone(),
                Member {
                    id: agent_id,
                    name: format!("member-{index}"),
                    role: "reviewer".into(),
                    pane_id: format!("w1:p{index}"),
                    space_id: space.clone(),
                    manager: false,
                    cwd: None,
                    activity: Activity::Offline,
                    last_seen: 0,
                    idle_since: 0,
                },
            );
        }
        Store::open(&path, space.clone())
            .unwrap()
            .save(&state)
            .unwrap();
        let service = Service::start(space.clone(), path, Scheduling::default())
            .await
            .unwrap();
        let grant = service
            .issue_startup_grant(
                LaunchId::new("capacity-launch".into()),
                "reviewer".into(),
                "w1:p257".into(),
            )
            .await
            .unwrap()
            .handoff;
        service
            .consume_startup_grant(
                grant.clone(),
                "reviewer".into(),
                "w1:p257".into(),
                space.clone(),
            )
            .await
            .unwrap();
        let response = service
            .join(
                1,
                Request {
                    id: "capacity-reject".into(),
                    command: Command::Join {
                        version: VERSION,
                        agent_id: AgentId::new("new-agent"),
                        name: "new-agent".into(),
                        role: "reviewer".into(),
                        pane_id: "w1:p257".into(),
                        space_id: space.clone(),
                        cwd: None,
                        reset: None,
                        startup_grant: Some(grant.clone()),
                    },
                },
                false,
            )
            .await
            .unwrap();
        assert!(!response.ok);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("capacity"))
        );
        assert!(response.result.is_none());
        assert!(
            service
                .consume_startup_grant(grant, "reviewer".into(), "w1:p257".into(), space,)
                .await
                .is_err()
        );
        service.stop().await;
    }

    #[tokio::test]
    async fn dashboard_pr_enable_binds_the_exact_selected_live_member() {
        let directory = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let space = SpaceId::new("w1");
        let service = Service::start(
            space.clone(),
            directory.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .unwrap();
        service
            .tx
            .send(Event::Space(Ok(Space {
                id: space.clone(),
                label: "Orchard".into(),
            })))
            .await
            .unwrap();
        let agent = AgentId::new("worker");
        let joined = service
            .request(
                1,
                Request {
                    id: "join".into(),
                    command: Command::Join {
                        version: 1,
                        agent_id: agent.clone(),
                        name: "worker".into(),
                        role: "custom".into(),
                        pane_id: "w1:p1".into(),
                        space_id: space.clone(),
                        cwd: Some(checkout.path().to_path_buf()),
                        reset: None,
                        startup_grant: None,
                    },
                },
            )
            .await
            .unwrap();
        assert!(joined.ok);
        service
            .request(
                1,
                Request {
                    id: "idle".into(),
                    command: Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                },
            )
            .await
            .unwrap();
        let mut snapshots = service.snapshots.clone();
        service
            .enable_pr(PrSelection {
                epoch: snapshots.borrow().epoch,
                agent_id: agent.clone(),
                name: "worker".into(),
                role: "custom".into(),
                pane_id: "w1:p1".into(),
                cwd: checkout.path().to_path_buf(),
            })
            .await
            .unwrap();
        let watch = loop {
            let snapshot = snapshots.borrow_and_update().clone();
            if let Some(watch) = snapshot.automation.pr {
                assert!(snapshot.automation.pr_enabled);
                break watch;
            }
            tokio::time::timeout(std::time::Duration::from_secs(1), snapshots.changed())
                .await
                .unwrap()
                .unwrap();
        };
        assert_eq!(watch.driver, agent);
        assert_eq!(watch.cwd, checkout.path());
        assert_eq!(watch.url, None);
        service.stop().await;
    }

    #[tokio::test]
    async fn dashboard_pr_enable_rejects_same_id_rejoined_from_new_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let first_checkout = tempfile::tempdir().unwrap();
        let second_checkout = tempfile::tempdir().unwrap();
        let space = SpaceId::new("w1");
        let service = Service::start(
            space.clone(),
            directory.path().join("hub.sqlite"),
            Scheduling::default(),
        )
        .await
        .unwrap();
        service
            .tx
            .send(Event::Space(Ok(Space {
                id: space.clone(),
                label: "Orchard".into(),
            })))
            .await
            .unwrap();
        let agent = AgentId::new("worker");
        let join = |connection: u64, cwd: &std::path::Path| Request {
            id: format!("join-{connection}"),
            command: Command::Join {
                version: 1,
                agent_id: agent.clone(),
                name: "worker".into(),
                role: "custom".into(),
                pane_id: "w1:p1".into(),
                space_id: space.clone(),
                cwd: Some(cwd.to_path_buf()),
                reset: None,
                startup_grant: None,
            },
        };
        service
            .request(1, join(1, first_checkout.path()))
            .await
            .unwrap();
        service
            .request(
                1,
                Request {
                    id: "idle".into(),
                    command: Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                },
            )
            .await
            .unwrap();
        let stale = PrSelection {
            epoch: service.snapshots.borrow().epoch,
            agent_id: agent.clone(),
            name: "worker".into(),
            role: "custom".into(),
            pane_id: "w1:p1".into(),
            cwd: first_checkout.path().to_path_buf(),
        };
        let mut stale_epoch = stale.clone();
        stale_epoch.epoch += 1;
        assert!(service.enable_pr(stale_epoch).await.is_err());
        service
            .request(
                1,
                Request {
                    id: "leave".into(),
                    command: Command::Leave,
                },
            )
            .await
            .unwrap();
        service
            .request(2, join(2, second_checkout.path()))
            .await
            .unwrap();
        service
            .request(
                2,
                Request {
                    id: "idle-again".into(),
                    command: Command::Sync {
                        activity: Activity::Idle,
                        receive: false,
                    },
                },
            )
            .await
            .unwrap();
        assert!(service.enable_pr(stale).await.is_err());
        assert!(service.snapshots.borrow().automation.pr.is_none());
        service.stop().await;
    }

    #[tokio::test]
    async fn no_inbox_sync_persists_cancellation_before_reply() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let space = SpaceId::new("w1");
        let (snapshots, snapshot_rx) = watch::channel(Snapshot::empty(space.clone()));
        let mut actor = Actor::new(
            space.clone(),
            path.clone(),
            Scheduling::default(),
            snapshots,
        )
        .unwrap();
        let worker = AgentId::new("worker");
        actor.connections.insert(
            1,
            Binding {
                agent: worker.clone(),
                space: SpaceId::new("w1"),
                epoch: 0,
                revoked: false,
            },
        );
        actor.incarnations.insert(worker.clone(), 1);
        actor.state.members.insert(
            worker.clone(),
            Member {
                id: worker.clone(),
                name: "worker".into(),
                role: "implementor".into(),
                pane_id: "w1:p1".into(),
                space_id: SpaceId::new("w1"),
                manager: false,
                cwd: None,
                activity: Activity::Idle,
                last_seen: 0,
                idle_since: 0,
            },
        );
        actor.state.messages.push(Message {
            id: MessageId::new("automatic"),
            space_id: SpaceId::new("w1"),
            from: None,
            to: worker,
            task_id: None,
            revision: None,
            kind: MessageKind::Checkin,
            text: "Queued check-in".into(),
            wake: true,
            delivery: Delivery::Pending,
            created_at: 0,
            pr_ack: None,
        });
        actor.store.save(&actor.state).unwrap();
        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::task::spawn_blocking(move || actor.run(rx));
        let service = Service {
            space_id: SpaceId::new("w1"),
            tx,
            snapshots: snapshot_rx,
        };
        let response = service
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
        assert!(response.ok);
        assert!(response.result.unwrap()["inbox"].is_null());
        // Read before shutdown or any later durable operation can mask the bug.
        let stored = Store::open(&path, SpaceId::new("w1"))
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(stored.messages[0].delivery, Delivery::Cancelled);
        service.stop().await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn assignment_queue_failure_rolls_back_inserted_task_and_heartbeats_are_ephemeral() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let worker = AgentId::new("worker");
        let mut state = State::new(SpaceId::new("w1"));
        for i in 0..1000 {
            state.messages.push(Message {
                id: MessageId::new(format!("m{i}")),
                space_id: SpaceId::new("w1"),
                from: None,
                to: worker.clone(),
                task_id: None,
                revision: None,
                kind: MessageKind::Info,
                text: "Unresolved".into(),
                wake: false,
                delivery: Delivery::Uncertain,
                created_at: 0,
                pr_ack: None,
            });
        }
        Store::open(&path, SpaceId::new("w1"))
            .unwrap()
            .save(&state)
            .unwrap();
        let service = Service::start(SpaceId::new("w1"), path.clone(), Scheduling::default())
            .await
            .unwrap();
        for (connection, name) in [(1, "requester"), (2, "worker")] {
            let response = service
                .request(
                    connection,
                    Request {
                        id: "join".into(),
                        command: Command::Join {
                            version: 1,
                            agent_id: AgentId::new(name),
                            name: name.into(),
                            role: "custom".into(),
                            pane_id: format!("p{connection}"),
                            space_id: SpaceId::new("w1"),
                            cwd: None,
                            reset: None,
                            startup_grant: None,
                        },
                    },
                )
                .await
                .unwrap();
            assert!(response.ok);
        }
        let sync = || Request {
            id: "sync".into(),
            command: Command::Sync {
                activity: Activity::Idle,
                receive: false,
            },
        };
        let before = Store::open(&path, SpaceId::new("w1"))
            .unwrap()
            .load()
            .unwrap();
        service.request(2, sync()).await.unwrap();
        assert_eq!(
            Store::open(&path, SpaceId::new("w1"))
                .unwrap()
                .load()
                .unwrap(),
            before,
            "heartbeat must not rewrite durable state"
        );
        let response = service
            .request(
                1,
                Request {
                    id: "assign".into(),
                    command: Command::Assign {
                        to: worker,
                        title: "Must roll back".into(),
                        text: "Queue is full".into(),
                    },
                },
            )
            .await
            .unwrap();
        assert!(!response.ok);
        let snapshot = service.request(2, sync()).await.unwrap();
        assert_eq!(snapshot.result.unwrap()["tasks"], json!([]));
        assert!(
            Store::open(&path, SpaceId::new("w1"))
                .unwrap()
                .load()
                .unwrap()
                .tasks
                .is_empty()
        );
        service.stop().await;
    }
}
