//! Unix socket host and bounded read-only observers. Owns no agent processes.

use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use clap_verbosity_flag::{InfoLevel, Verbosity};
use color_eyre::eyre::{Context, Result, ensure, eyre};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::{interval, timeout},
};

use crate::{
    config::Config,
    herdr::{Herdr, HerdrUnavailable},
    paths::{Paths, check_regular, private_dir},
    protocol::*,
    roles,
    service::{Event, Service, now},
    watchers,
};

pub(crate) fn is_running(herdr: &Herdr, space_id: &SpaceId) -> Result<bool> {
    let lock_is_held = |path: PathBuf| -> Result<bool> {
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
            Ok(_) => check_regular(&path)?,
        }
        let lock = OpenOptions::new().read(true).write(true).open(&path)?;
        check_regular(&path)?;
        match lock.try_lock() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    };
    let current = herdr.hub_socket(space_id)?.with_extension("lock");
    lock_is_held(current)
}

pub(crate) struct Server {
    pub service: Service,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    socket: PathBuf,
    _lock: File,
}

impl Server {
    pub async fn start(
        herdr: Herdr,
        paths: &Paths,
        config: &Config,
        verbosity: Verbosity<InfoLevel>,
    ) -> Result<Self> {
        let discovery = herdr.discovery().await?;
        let space_id = discovery.space_id;
        let instance_key = herdr.instance_key(&space_id);
        let socket = herdr.hub_socket(&space_id)?;
        let lock_path = socket.with_extension("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        check_regular(&lock_path)?;
        lock.try_lock()
            .map_err(|_| eyre!("a mustr hub is already running for this herdr space"))?;
        reject_running_legacy_hub(&herdr)?;
        private_dir(&paths.state)?;
        let server_dir = paths.state.join(&herdr.key);
        private_dir(&server_dir)?;
        let state_dir = paths.instance_state(&herdr.key, &instance_key);
        private_dir(&state_dir)?;
        init_logging(&state_dir, verbosity)?;
        match fs::symlink_metadata(&socket) {
            Ok(meta) => {
                ensure!(
                    meta.file_type().is_socket(),
                    "refusing to remove non-socket runtime file"
                );
                fs::remove_file(&socket)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let service = Service::start(
            space_id.clone(),
            state_dir.join("hub.sqlite"),
            config.scheduling.clone(),
        )
        .await?;
        let listener = UnixListener::bind(&socket).wrap_err("binding mustr socket")?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let (stop, stopping) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(accept_loop(
                listener,
                herdr.clone(),
                space_id.clone(),
                service.clone(),
                paths.roles(),
                stopping.clone(),
            )),
            tokio::spawn(observe(herdr, space_id, service.clone(), stopping)),
        ];
        tracing::info!(socket = %socket.display(), "hub started");
        Ok(Self {
            service,
            stop,
            tasks,
            socket,
            _lock: lock,
        })
    }

    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        self.service.stop().await;
        if let Err(error) = fs::remove_file(&self.socket) {
            tracing::warn!(%error, "socket cleanup failed");
        }
    }
}

fn reject_running_legacy_hub(herdr: &Herdr) -> Result<()> {
    let legacy_socket = herdr.legacy_hub_socket()?;
    let lock_path = legacy_socket.with_extension("lock");
    match fs::symlink_metadata(&lock_path) {
        Ok(meta) => ensure!(
            meta.is_file(),
            "refusing to inspect non-regular legacy lock"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
    check_regular(&lock_path)?;
    match lock.try_lock_shared() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(eyre!(
                "a legacy session-wide mustr hub is already running; stop it before starting per-space hubs"
            ));
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    Ok(())
}

fn init_logging(state_dir: &Path, verbosity: Verbosity<InfoLevel>) -> Result<()> {
    let log = state_dir.join("mustr.log");
    match fs::symlink_metadata(&log) {
        Ok(meta) => {
            ensure!(meta.is_file(), "refusing to use non-regular log file");
            if meta.len() > 5 * 1024 * 1024 {
                fs::rename(&log, log.with_extension("log.1"))?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&log)?;
    check_regular(&log)?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(verbosity)
        .with_writer(Arc::new(file))
        .try_init()
        .map_err(|error| eyre!("initializing logging: {error}"))
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        for task in &self.tasks {
            task.abort();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

async fn accept_loop(
    listener: UnixListener,
    herdr: Herdr,
    bound_space: SpaceId,
    service: Service,
    roles_dir: PathBuf,
    mut stop: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(128));
    let mut clients = JoinSet::new();
    let mut sequence = 0u64;
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            Some(_) = clients.join_next(), if !clients.is_empty() => {},
            result = listener.accept() => match result {
                Ok((stream, _)) => {
                    let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                    let Some(next) = sequence.checked_add(1) else { break; };
                    sequence = next;
                    let id = sequence;
                    let herdr = herdr.clone(); let bound_space = bound_space.clone(); let service = service.clone(); let roles_dir = roles_dir.clone();
                    clients.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = connection(stream, id, &herdr, &bound_space, &service, &roles_dir).await { tracing::debug!(connection = id, %error, "participant disconnected"); }
                        let _ = service.tx.send(Event::Disconnect(id)).await;
                    });
                }
                Err(error) => { tracing::error!(%error, "accept failed"); break; }
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
}

async fn connection(
    stream: UnixStream,
    id: u64,
    herdr: &Herdr,
    bound_space: &SpaceId,
    service: &Service,
    roles_dir: &std::path::Path,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut identity: Option<(String, SpaceId)> = None;
    let mut recipient: Option<AgentId> = None;
    let mut subscribed = false;
    let mut notified: Option<MessageId> = None;
    let mut snapshots = service.snapshots.clone();
    loop {
        let mut line = String::new();
        let size = {
            let mut limited = (&mut reader).take(MAX_FRAME as u64 + 1);
            let reading = timeout(Duration::from_secs(20), limited.read_line(&mut line));
            tokio::pin!(reading);
            loop {
                if subscribed {
                    let ready = recipient
                        .as_ref()
                        .and_then(|agent| snapshots.borrow_and_update().ready.get(agent).cloned());
                    if ready != notified {
                        notified = ready.clone();
                        if ready.is_some() {
                            // Only a content-free, recipient-scoped hint. Normal sync
                            // still checks identity, activity and transactional offering.
                            timeout(
                                Duration::from_secs(5),
                                write.write_all(b"{\"event\":\"inbox_ready\"}\n"),
                            )
                            .await??;
                        }
                    }
                }
                tokio::select! {
                    result = &mut reading => break result??,
                    changed = snapshots.changed(), if subscribed => {
                        ensure!(changed.is_ok(), "hub stopped");
                    }
                }
                // Keep the SAME pinned read future across notifications: cancelling
                // read_line and restarting it would lose partial client frames.
            }
        };
        if size == 0 {
            break;
        }
        ensure!(
            size <= MAX_FRAME && line.ends_with('\n'),
            "oversized or incomplete frame"
        );
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => {
                let response = Response::failure(String::new(), "invalid request JSON/schema");
                write
                    .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
                    .await?;
                break;
            }
        };
        let role_manager = if let Command::Join { role, .. } = &request.command {
            let roles_dir = roles_dir.to_path_buf();
            let role = role.clone();
            let validation = tokio::task::spawn_blocking(move || roles::resolve(&roles_dir, &role))
                .await
                .map_err(|error| eyre!("role validation worker failed: {error}"))?;
            match validation {
                Ok(resolved) => Some(resolved.manager),
                Err(error) => {
                    let response = Response::failure(request.id, error);
                    write
                        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
                        .await?;
                    break;
                }
            }
        } else {
            None
        };
        let check = match &request.command {
            Command::Join {
                pane_id, space_id, ..
            } => Some((pane_id.clone(), space_id.clone())),
            _ => identity.clone(),
        };
        let mut checked_harness = None;
        if let Some((pane_id, space_id)) = &check {
            let validation = async {
                ensure!(
                    space_id == bound_space,
                    "wrong mustr endpoint for this herdr space"
                );
                let pane = herdr.pane(pane_id).await?;
                ensure!(
                    &pane.workspace_id == space_id
                        && pane.pane_id == *pane_id
                        && pane.agent.as_deref() == Some("pi"),
                    "pane moved, disappeared, or is not running Pi; rejoin explicitly"
                );
                Ok::<_, color_eyre::Report>(pane.agent)
            }
            .await;
            match validation {
                Ok(agent) => {
                    checked_harness =
                        agent.and_then(|agent| ClientHarness::from_verified_agent(&agent))
                }
                Err(error) => {
                    // Only an already-authenticated connection can survive an
                    // unavailable lookup. No actor request (including sync or
                    // ACK) has executed; every later command validates afresh.
                    let unavailable = identity.is_some()
                        && !matches!(request.command, Command::Join { .. })
                        && error.downcast_ref::<HerdrUnavailable>().is_some();
                    let response = if unavailable {
                        Response::identity_unavailable(request.id, error)
                    } else {
                        Response::failure(request.id, error)
                    };
                    timeout(
                        Duration::from_secs(5),
                        write.write_all(
                            format!("{}\n", serde_json::to_string(&response)?).as_bytes(),
                        ),
                    )
                    .await??;
                    if unavailable {
                        continue;
                    }
                    break;
                }
            }
        }
        let joining = match &request.command {
            Command::Join { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        };
        let subscribing = matches!(request.command, Command::Subscribe);
        let leaving = matches!(request.command, Command::Leave);
        let response = match role_manager {
            Some(manager) => {
                let harness = checked_harness
                    .ok_or_else(|| eyre!("Join has no verified supported pane harness"))?;
                service.join(id, request, manager, harness).await?
            }
            None => service.request(id, request).await?,
        };
        if response.ok {
            if let Some(agent) = joining {
                identity = check;
                recipient = Some(agent);
            }
            if subscribing {
                subscribed = true;
            }
        }
        let bytes = format!("{}\n", serde_json::to_string(&response)?);
        ensure!(bytes.len() <= MAX_FRAME, "response exceeds frame limit");
        timeout(Duration::from_secs(5), write.write_all(bytes.as_bytes())).await??;
        if leaving {
            break;
        }
    }
    Ok(())
}

fn retire_job(job: &mut Option<(u64, JoinHandle<()>)>, generation: u64, enabled: bool) {
    if job
        .as_ref()
        .is_some_and(|(running_generation, _)| !enabled || *running_generation != generation)
        && let Some((_, job)) = job.take()
    {
        // A blocking read can outlive abort; the actor also checks generation.
        job.abort();
    }
}

async fn observe(
    herdr: Herdr,
    bound_space_id: SpaceId,
    service: Service,
    mut stop: watch::Receiver<bool>,
) {
    let mut ticks = interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut topology_at = 0;
    let mut todo_job: Option<(u64, JoinHandle<()>)> = None;
    let mut pr_job: Option<(u64, JoinHandle<()>)> = None;
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = ticks.tick() => {
                let time = now();
                if time >= topology_at {
                    let result = herdr.space(&bound_space_id).await
                        .map_err(|error| error.to_string());
                    let _ = service.tx.send(Event::Space(result)).await;
                    topology_at = time + 3;
                }
                let _ = service.tx.send(Event::Tick).await;
                let snapshot = service.snapshots.borrow().clone();
                let automation = &snapshot.automation;
                retire_job(&mut todo_job, automation.generation, automation.todo_enabled);
                retire_job(&mut pr_job, automation.generation, automation.pr_enabled);
                if automation.todo_enabled
                    && todo_job.as_ref().is_none_or(|(_, job)| job.is_finished())
                    && let Some(todo) = &automation.todo {
                    let path = todo.path.clone();
                    let tx = service.tx.clone();
                    let space_id = bound_space_id.clone();
                    let generation = automation.generation;
                    let task = tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || watchers::read_todo(&path).map_err(|e| e.to_string()))
                            .await.unwrap_or_else(|e| Err(e.to_string()));
                        let _ = tx.send(Event::Todo { space: space_id, generation, result }).await;
                    });
                    todo_job = Some((generation, task));
                }
                if automation.pr_enabled
                    && pr_job.as_ref().is_none_or(|(_, job)| job.is_finished())
                    && let Some(pr) = &automation.pr
                    && time >= pr.next_at {
                    let pr = pr.clone();
                    let tx = service.tx.clone();
                    let space_id = bound_space_id.clone();
                    let generation = automation.generation;
                    let task = tokio::spawn(async move {
                        let result = watchers::collect_pr(&pr).await.map_err(|e| e.to_string());
                        let _ = tx.send(Event::Pr { space: space_id, generation, result }).await;
                    });
                    pr_job = Some((generation, task));
                }
            }
        }
    }
    if let Some((_, job)) = todo_job {
        job.abort();
    }
    if let Some((_, job)) = pr_job {
        job.abort();
    }
}

#[cfg(test)]
mod tests;
