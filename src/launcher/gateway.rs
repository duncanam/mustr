//! One launch's authenticated loopback entrance to the existing Unix hub.
//! This is not a general proxy: destination and Join scope are host-owned.

use std::{sync::Arc, time::Duration};

use color_eyre::eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixStream},
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use super::{LaunchDescriptor, ReadyReceipt};
use crate::{
    paths::create_file_atomically,
    protocol::{Command, MAX_FRAME, Request, Response},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Endpoint {
    pub port: u16,
    pub token: String,
}

pub(super) struct Gateway {
    pub endpoint: Endpoint,
    task: JoinHandle<()>,
}

impl Gateway {
    pub async fn start(manifest: LaunchDescriptor) -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = Endpoint {
            port: listener.local_addr()?.port(),
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
        };
        let token = endpoint.token.clone();
        let manifest = Arc::new(manifest);
        // A new connection must not steal an existing participation episode.
        let active = Arc::new(Semaphore::new(1));
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break; };
                        if clients.len() >= 8 { continue; }
                        let (manifest, token, active) = (manifest.clone(), token.clone(), active.clone());
                        clients.spawn(async move {
                            // Never log frames, tokens or peer-supplied text.
                            let _ = serve(stream, &token, &manifest, active).await;
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
            // Dropping JoinSet aborts clients and closes their UDS/TCP streams.
        });
        Ok(Self { endpoint, task })
    }

    pub async fn shutdown(mut self) {
        self.task.abort();
        // Connections belong to this task's JoinSet. Wait until it drops them.
        let _ = (&mut self.task).await;
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authentication {
    token: String,
}

fn authenticated(candidate: &str, expected: &str) -> bool {
    candidate.len() == expected.len()
        && candidate
            .as_bytes()
            .iter()
            .zip(expected.as_bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

async fn frame(reader: &mut (impl AsyncBufRead + Unpin), maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut *reader)
        .take(maximum as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(
        bytes.len() <= maximum && bytes.last() == Some(&b'\n'),
        "invalid gateway frame"
    );
    Ok(bytes)
}

fn validate_join(request: &Request, manifest: &LaunchDescriptor) -> Result<String> {
    let Command::Join {
        pane_id,
        space_id,
        role,
        cwd,
        agent_id,
        reset,
        ..
    } = &request.command
    else {
        return Err(eyre!("gateway requires Join"));
    };
    ensure!(
        pane_id == &manifest.pane_id
            && space_id.as_str() == manifest.space_id
            && role == &manifest.role.name
            && cwd.as_ref() == Some(&manifest.cwd)
            && reset.is_none(),
        "Join is outside this launch's scope"
    );
    Ok(agent_id.to_string())
}

fn allowed(command: &Command) -> bool {
    // No host filesystem/watch configuration, reset claims, or second Join.
    // Host-side human controls remain available without crossing the jail.
    matches!(
        command,
        Command::Subscribe
            | Command::Sync { .. }
            | Command::Send { .. }
            | Command::Assign { .. }
            | Command::Report { .. }
            | Command::Guide { .. }
            | Command::Accept { .. }
            | Command::Cancel { .. }
            | Command::CancelOwnerTasks { .. }
            | Command::Ack { .. }
            | Command::Defer { .. }
            | Command::Leave
    )
}

async fn serve(
    stream: TcpStream,
    token: &str,
    manifest: &LaunchDescriptor,
    active: Arc<Semaphore>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let (mut hub_read, mut hub_write, _permit) = timeout(HANDSHAKE_TIMEOUT, async {
        let auth: Authentication = serde_json::from_slice(&frame(&mut reader, 1024).await?)?;
        ensure!(
            authenticated(&auth.token, token),
            "gateway authentication failed"
        );
        let join_bytes = frame(&mut reader, MAX_FRAME).await?;
        let join: Request = serde_json::from_slice(&join_bytes)?;
        let agent_id = validate_join(&join, manifest)?;
        let permit = active.try_acquire_owned()?;
        // No host hub connection or side effect before authentication and scope.
        let hub = UnixStream::connect(&manifest.socket).await?;
        let (read, mut hub_write) = hub.into_split();
        let mut hub_read = BufReader::new(read);
        hub_write.write_all(&join_bytes).await?;
        let reply_bytes = frame(&mut hub_read, MAX_FRAME).await?;
        let reply: Response = serde_json::from_slice(&reply_bytes)?;
        write.write_all(&reply_bytes).await?;
        ensure!(
            reply.id == join.id && reply.ok,
            "hub did not admit this launch"
        );

        // The adapter checks exact model/thinking before Join, and negotiates
        // capabilities before Subscribe. Only confirmed subscription is ready.
        let subscribe_bytes = frame(&mut reader, MAX_FRAME).await?;
        let subscribe: Request = serde_json::from_slice(&subscribe_bytes)?;
        ensure!(
            matches!(subscribe.command, Command::Subscribe),
            "expected launch subscription"
        );
        hub_write.write_all(&subscribe_bytes).await?;
        let reply_bytes = frame(&mut hub_read, MAX_FRAME).await?;
        let reply: Response = serde_json::from_slice(&reply_bytes)?;
        ensure!(
            reply.id == subscribe.id && reply.ok,
            "hub did not confirm subscription"
        );
        let ready = ReadyReceipt {
            version: 1,
            launch_id: manifest.launch_id.clone(),
            server_key: manifest.server_key.clone(),
            space_id: manifest.space_id.clone(),
            pane_id: manifest.pane_id.clone(),
            agent_id,
            role: manifest.role.name.clone(),
            model: manifest.model.clone(),
            thinking: manifest.thinking.clone(),
        };
        create_file_atomically(&manifest.ready_path, &serde_json::to_vec(&ready)?)?;
        write.write_all(&reply_bytes).await?;
        Ok::<_, color_eyre::Report>((hub_read, hub_write, permit))
    })
    .await??;

    let requests = forward_requests(&mut reader, &mut hub_write);
    // Either end closing cancels the other direction; never replay a mutation.
    tokio::select! {
        result = requests => result?,
        result = tokio::io::copy(&mut hub_read, &mut write) => { result?; },
    }
    Ok(())
}

async fn forward_requests(
    reader: &mut (impl AsyncBufRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    loop {
        let bytes = frame(reader, MAX_FRAME).await?;
        let request: Request = serde_json::from_slice(&bytes)?;
        ensure!(
            allowed(&request.command),
            "operation is not available through a jailed launch"
        );
        writer.write_all(&bytes).await?;
    }
}

#[cfg(test)]
mod tests;
