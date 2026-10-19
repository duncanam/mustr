//! Read-only herdr socket client. Never reads terminal output or session files.

use std::{env, fs, os::unix::fs::MetadataExt, path::PathBuf, time::Duration};

use color_eyre::eyre::{Context, Result, ensure, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

use crate::{
    paths::{digest, runtime_dir},
    protocol::{Space, SpaceId},
};

#[derive(Clone, Debug)]
pub(crate) struct Herdr {
    socket: PathBuf,
    stamp: (u64, u64, i64, i64),
    pub key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Pane {
    pub pane_id: String,
    pub workspace_id: SpaceId,
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Discovery {
    pub protocol: u32,
    pub server_key: String,
    pub socket: PathBuf,
    pub pane_id: String,
    pub space_id: SpaceId,
}

impl Herdr {
    pub fn from_env() -> Result<Self> {
        ensure!(
            env::var("HERDR_ENV").as_deref() == Ok("1"),
            "run mustr inside a herdr-managed pane"
        );
        let socket = env::var_os("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .ok_or_else(|| eyre!("HERDR_SOCKET_PATH is missing"))?;
        Self::new(socket)
    }

    pub fn new(socket: PathBuf) -> Result<Self> {
        let meta = fs::metadata(&socket).wrap_err("herdr socket is unavailable")?;
        let stamp = (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec());
        let key = digest(format!("{}:{stamp:?}", socket.display()));
        Ok(Self { socket, stamp, key })
    }

    pub fn instance_key(&self, space: &SpaceId) -> String {
        digest(format!("{}:{}", self.key, space.as_str()))
    }

    pub fn hub_socket(&self, space: &SpaceId) -> Result<PathBuf> {
        Ok(runtime_dir()?.join(format!("{}.sock", self.instance_key(space))))
    }

    /// The pre-space socket is used only to detect an already-running legacy hub.
    pub fn legacy_hub_socket(&self) -> Result<PathBuf> {
        Ok(runtime_dir()?.join(format!("{}.sock", self.key)))
    }

    pub fn check_instance(&self) -> Result<()> {
        let meta = fs::metadata(&self.socket).wrap_err("herdr server disappeared")?;
        ensure!(
            (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()) == self.stamp,
            "herdr server changed; restart mustr to avoid reusing stale workspace identities"
        );
        Ok(())
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.check_instance()?;
        timeout(Duration::from_secs(3), async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            let request = json!({"id": "mustr", "method": method, "params": params});
            stream.write_all(format!("{request}\n").as_bytes()).await?;
            let mut reader = BufReader::new(stream).take(2 * 1024 * 1024 + 1);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            ensure!(
                line.len() <= 2 * 1024 * 1024 && line.ends_with('\n'),
                "invalid or oversized herdr response"
            );
            let value: Value = serde_json::from_str(&line)?;
            if let Some(error) = value.get("error") {
                return Err(eyre!("herdr {method}: {error}"));
            }
            value
                .get("result")
                .cloned()
                .ok_or_else(|| eyre!("herdr response has no result"))
        })
        .await
        .wrap_err("herdr request timed out")?
    }

    pub async fn pane(&self, caller: &str) -> Result<Pane> {
        let result = self
            .request("pane.current", json!({"caller_pane_id": caller}))
            .await?;
        serde_json::from_value(
            result
                .get("pane")
                .cloned()
                .ok_or_else(|| eyre!("herdr returned no caller pane"))?,
        )
        .wrap_err("invalid herdr pane")
    }

    pub async fn discovery(&self) -> Result<Discovery> {
        let caller = env::var("HERDR_PANE_ID").wrap_err("HERDR_PANE_ID is missing")?;
        let pane = self.pane(&caller).await?;
        Ok(Discovery {
            protocol: crate::protocol::VERSION,
            server_key: self.key.clone(),
            socket: self.hub_socket(&pane.workspace_id)?,
            pane_id: pane.pane_id,
            space_id: pane.workspace_id,
        })
    }

    pub async fn space(&self, space_id: &SpaceId) -> Result<Space> {
        let result = self.request("workspace.list", json!({})).await?;
        let spaces = result
            .get("workspaces")
            .and_then(Value::as_array)
            .ok_or_else(|| eyre!("herdr returned no workspace list"))?;
        let space = spaces
            .iter()
            .find(|space| {
                space.get("workspace_id").and_then(Value::as_str) == Some(space_id.as_str())
            })
            .ok_or_else(|| eyre!("herdr space {space_id} is no longer available"))?;
        Ok(Space {
            id: space_id.clone(),
            label: space
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("Unnamed")
                .to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockets_are_keyed_by_server_incarnation_and_exact_space() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        fs::write(&socket, b"fixture").unwrap();
        let herdr = Herdr::new(socket).unwrap();
        let first = SpaceId::new("w1");
        let second = SpaceId::new("w2");

        let first_key = herdr.instance_key(&first);
        let second_key = herdr.instance_key(&second);
        assert_ne!(first_key, second_key);
        assert_eq!(
            herdr
                .hub_socket(&first)
                .unwrap()
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap(),
            first_key.as_str()
        );
        assert_eq!(
            herdr
                .hub_socket(&second)
                .unwrap()
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap(),
            second_key.as_str()
        );
        assert_eq!(
            herdr
                .legacy_hub_socket()
                .unwrap()
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap(),
            herdr.key.as_str()
        );
    }
}
