//! Single-writer actor: SQLite and filesystem work stay off async/TUI threads.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, ensure, eyre};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    config::Scheduling,
    hub::State,
    protocol::*,
    store::Store,
    watchers::{PrSnapshot, TodoSnapshot},
};

pub(crate) enum Event {
    Request {
        connection: u64,
        request: Request,
        reply: oneshot::Sender<Response>,
    },
    Disconnect(u64),
    Spaces(std::result::Result<Vec<Space>, String>),
    Tick,
    Mode {
        space: SpaceId,
        mode: Mode,
        enabled: bool,
        epoch: u64,
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
    pub tx: mpsc::Sender<Event>,
    pub snapshots: watch::Receiver<Snapshot>,
}

impl Service {
    pub async fn start(path: PathBuf, scheduling: Scheduling) -> Result<Self> {
        let (tx, rx) = mpsc::channel(128);
        let (snapshots, snapshot_rx) = watch::channel(Snapshot::default());
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::task::spawn_blocking(move || match Actor::new(path, scheduling, snapshots) {
            Ok(mut actor) => {
                let _ = ready_tx.send(Ok(()));
                actor.run(rx);
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
            }
        });
        ready_rx.await?.map_err(|e| eyre!(e))?;
        Ok(Self {
            tx,
            snapshots: snapshot_rx,
        })
    }

    pub async fn request(&self, connection: u64, request: Request) -> Result<Response> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::Request {
                connection,
                request,
                reply,
            })
            .await?;
        Ok(response.await?)
    }

    pub async fn mode(&self, space: SpaceId, mode: Mode, enabled: bool) -> Result<()> {
        let (reply, response) = oneshot::channel();
        let epoch = self
            .snapshots
            .borrow()
            .space_epochs
            .get(&space)
            .copied()
            .unwrap_or_default();
        self.tx
            .send(Event::Mode {
                space,
                mode,
                enabled,
                epoch,
                reply,
            })
            .await?;
        response.await?.map_err(|e| eyre!(e))
    }

    pub async fn reset_preview(&self, space: SpaceId) -> Result<ResetPreview> {
        let (reply, response) = oneshot::channel();
        self.tx.send(Event::ResetPreview { space, reply }).await?;
        response.await?.map_err(|error| eyre!(error))
    }

    pub async fn reset_start(&self, preview: ResetPreview) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx.send(Event::ResetStart { preview, reply }).await?;
        response.await?.map_err(|error| eyre!(error))
    }

    pub async fn reset_abandon(&self, space: SpaceId, plan_id: String) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Event::ResetAbandon {
                space,
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
    state: State,
    store: Store,
    scheduling: Scheduling,
    connections: BTreeMap<u64, Binding>,
    incarnations: BTreeMap<AgentId, u64>,
    previews: BTreeMap<SpaceId, Preview>,
    spaces: Vec<Space>,
    error: Option<String>,
    snapshots: watch::Sender<Snapshot>,
}

impl Actor {
    fn new(
        path: PathBuf,
        scheduling: Scheduling,
        snapshots: watch::Sender<Snapshot>,
    ) -> Result<Self> {
        let mut store = Store::open(&path)?;
        let mut state = store.load()?;
        state.recover();
        store.save(&state)?;
        Ok(Self {
            state,
            store,
            scheduling,
            connections: BTreeMap::new(),
            incarnations: BTreeMap::new(),
            previews: BTreeMap::new(),
            spaces: Vec::new(),
            error: None,
            snapshots,
        })
    }

    fn commit(&mut self, next: State) -> Result<()> {
        if next != self.state {
            self.store.save(&next)?;
            self.state = next;
        }
        Ok(())
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
            self.spaces.iter().any(|s| s.id == space),
            "space no longer exists; refresh before confirming"
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
        self.previews.insert(
            space.clone(),
            Preview {
                approved: approved.clone(),
                fingerprint: self.state.reset_fingerprint(&space)?,
                incarnations,
            },
        );
        Ok(approved)
    }

    fn start_reset(&mut self, preview: ResetPreview) -> Result<()> {
        ensure!(
            self.spaces.iter().any(|s| s.id == preview.space_id),
            "space no longer exists; preview again"
        );
        let approved = self
            .previews
            .get(&preview.space_id)
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
        self.previews.remove(&preview.space_id);
        Ok(())
    }

    fn run(&mut self, mut rx: mpsc::Receiver<Event>) {
        while let Some(event) = rx.blocking_recv() {
            let now = now();
            match event {
                Event::Request {
                    connection,
                    request,
                    reply,
                } => {
                    let mut next = self.state.clone();
                    let result = self
                        .sender(connection)
                        .and_then(|sender| {
                            next.apply(&request, sender.as_ref(), now, &self.scheduling)
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
                        Ok(value) => {
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
                Event::Spaces(result) => match result {
                    Ok(spaces) => {
                        self.spaces = spaces;
                        self.error = None;
                    }
                    Err(error) => self.error = Some(error),
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
                            epoch == self.state.reset_epoch(&space),
                            "mode action predates reset; refresh first"
                        );
                        next.set_mode(&space, mode, enabled, now)
                    })()
                    .and_then(|()| self.commit(next))
                    .map_err(|e| e.to_string());
                    let _ = reply.send(result);
                }
                Event::ResetPreview { space, reply } => {
                    let result = self.preview(space).map_err(|e| e.to_string());
                    let _ = reply.send(result);
                }
                Event::ResetStart { preview, reply } => {
                    let result = self.start_reset(preview).map_err(|e| e.to_string());
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
                        let originals: Vec<_> = self.state.snapshot(Vec::new(), None, now).resets
                            [&space]
                            .participants
                            .iter()
                            .map(|p| p.participant.agent_id.clone())
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
            self.snapshots.send_replace(self.state.snapshot(
                self.spaces.clone(),
                self.error.clone(),
                now,
            ));
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
    async fn no_inbox_sync_persists_cancellation_before_reply() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let (snapshots, snapshot_rx) = watch::channel(Snapshot::default());
        let mut actor = Actor::new(path.clone(), Scheduling::default(), snapshots).unwrap();
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
        let stored = Store::open(&path).unwrap().load().unwrap();
        assert_eq!(stored.messages[0].delivery, Delivery::Cancelled);
        service.stop().await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn assignment_queue_failure_rolls_back_inserted_task_and_heartbeats_are_ephemeral() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hub.sqlite");
        let worker = AgentId::new("worker");
        let mut state = State::default();
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
        Store::open(&path).unwrap().save(&state).unwrap();
        let service = Service::start(path.clone(), Scheduling::default())
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
                            reset: None,
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
        let before = Store::open(&path).unwrap().load().unwrap();
        service.request(2, sync()).await.unwrap();
        assert_eq!(
            Store::open(&path).unwrap().load().unwrap(),
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
        assert!(Store::open(&path).unwrap().load().unwrap().tasks.is_empty());
        service.stop().await;
    }
}
