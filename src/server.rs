//! Unix socket host and bounded read-only observers. Owns no agent processes.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

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
    herdr::Herdr,
    paths::{Paths, check_regular, private_dir},
    protocol::*,
    service::{Event, Service, now},
    watchers,
};

pub(crate) struct Server {
    pub service: Service,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    socket: PathBuf,
    _lock: File,
}

impl Server {
    pub async fn start(herdr: Herdr, paths: &Paths, config: &Config) -> Result<Self> {
        herdr.discovery().await?;
        let socket = herdr.hub_socket()?;
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
            .map_err(|_| eyre!("a mustr hub is already running for this herdr session"))?;
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
        let state_dir = paths.state.join(&herdr.key);
        private_dir(&state_dir)?;
        let service =
            Service::start(state_dir.join("hub.sqlite"), config.scheduling.clone()).await?;
        let listener = UnixListener::bind(&socket).wrap_err("binding mustr socket")?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let (stop, stopping) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(accept_loop(
                listener,
                herdr.clone(),
                service.clone(),
                stopping.clone(),
            )),
            tokio::spawn(observe(herdr, service.clone(), stopping)),
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
    service: Service,
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
                    sequence += 1;
                    let id = sequence;
                    let herdr = herdr.clone(); let service = service.clone();
                    clients.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = connection(stream, id, &herdr, &service).await { tracing::debug!(connection = id, %error, "participant disconnected"); }
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

async fn connection(stream: UnixStream, id: u64, herdr: &Herdr, service: &Service) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut identity: Option<(String, SpaceId)> = None;
    loop {
        let mut line = String::new();
        let size = timeout(
            Duration::from_secs(20),
            (&mut reader)
                .take(MAX_FRAME as u64 + 1)
                .read_line(&mut line),
        )
        .await??;
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
        let check = match &request.command {
            Command::Join {
                pane_id, space_id, ..
            } => Some((pane_id.clone(), space_id.clone())),
            _ => identity.clone(),
        };
        if let Some((pane_id, space_id)) = &check {
            let validation = async {
                let pane = herdr.pane(pane_id).await?;
                ensure!(
                    &pane.workspace_id == space_id && pane.agent.as_deref() == Some("pi"),
                    "pane moved, disappeared, or no longer contains Pi; rejoin explicitly"
                );
                Ok::<_, color_eyre::Report>(())
            }
            .await;
            if let Err(error) = validation {
                let response = Response::failure(request.id, error);
                write
                    .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
                    .await?;
                break;
            }
        }
        let joining = matches!(request.command, Command::Join { .. });
        let leaving = matches!(request.command, Command::Leave);
        let response = service.request(id, request).await?;
        if joining && response.ok {
            identity = check;
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

async fn observe(herdr: Herdr, service: Service, mut stop: watch::Receiver<bool>) {
    let mut ticks = interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut topology_at = 0;
    let mut todo_jobs: BTreeMap<SpaceId, (u64, JoinHandle<()>)> = BTreeMap::new();
    let mut pr_jobs: BTreeMap<SpaceId, (u64, JoinHandle<()>)> = BTreeMap::new();
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = ticks.tick() => {
                let time = now();
                if time >= topology_at {
                    let result = herdr.spaces().await.map_err(|e| e.to_string());
                    let _ = service.tx.send(Event::Spaces(result)).await;
                    topology_at = time + 3;
                }
                let _ = service.tx.send(Event::Tick).await;
                let snapshot = service.snapshots.borrow().clone();
                for (space, automation) in &snapshot.automation {
                    if let Some((generation, task)) = todo_jobs.get(space)
                        && (!automation.todo_enabled || *generation != automation.generation) { task.abort(); }
                    if let Some((generation, task)) = pr_jobs.get(space)
                        && (!automation.pr_enabled || *generation != automation.generation) { task.abort(); }
                    if automation.todo_enabled && todo_jobs.get(space).is_none_or(|(_, job)| job.is_finished())
                        && let Some(todo) = &automation.todo {
                        let path = todo.path.clone(); let tx = service.tx.clone(); let space_id = space.clone(); let generation = automation.generation;
                        let task = tokio::spawn(async move {
                            let result = tokio::task::spawn_blocking(move || watchers::read_todo(&path).map_err(|e| e.to_string())).await.unwrap_or_else(|e| Err(e.to_string()));
                            let _ = tx.send(Event::Todo { space: space_id, generation, result }).await;
                        });
                        todo_jobs.insert(space.clone(), (generation, task));
                    }
                    if automation.pr_enabled && pr_jobs.get(space).is_none_or(|(_, job)| job.is_finished())
                        && let Some(pr) = &automation.pr && time >= pr.next_at {
                        let pr = pr.clone(); let tx = service.tx.clone(); let space_id = space.clone(); let generation = automation.generation;
                        let task = tokio::spawn(async move {
                            let result = watchers::collect_pr(&pr).await.map_err(|e| e.to_string());
                            let _ = tx.send(Event::Pr { space: space_id, generation, result }).await;
                        });
                        pr_jobs.insert(space.clone(), (generation, task));
                    }
                }
            }
        }
    }
    for (_, (_, job)) in todo_jobs.into_iter().chain(pr_jobs) {
        job.abort();
    }
}
