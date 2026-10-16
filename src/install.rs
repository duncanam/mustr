//! Explicit, non-destructive Pi adapter installation. Never edits settings.json.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, Result, ensure, eyre};

use crate::paths::{Paths, check_regular, create_file, private_dir};

const ADAPTER: &str = include_str!("../pi/index.ts");
const TRANSPORT: &str = include_str!("../pi/transport.mjs");
const MARKER: &str = "mustr-managed adapter v1";

pub(crate) fn install(paths: &Paths, agent_dir: Option<PathBuf>) -> Result<PathBuf> {
    let agent = match agent_dir {
        Some(path) => path,
        None => PathBuf::from(env::var_os("HOME").ok_or_else(|| eyre!("HOME is not set"))?)
            .join(".pi/agent"),
    };
    ensure!(agent.is_absolute(), "Pi agent directory must be absolute");
    let directory = agent.join("extensions/mustr");
    private_dir(&directory)?;
    let binary = env::current_exe()?.canonicalize()?;
    let binary = binary
        .to_str()
        .ok_or_else(|| eyre!("binary path is not UTF-8"))?;
    let config = paths
        .config
        .to_str()
        .ok_or_else(|| eyre!("config path is not UTF-8"))?;
    let adapter = ADAPTER
        .replace("\"__MUSTR_BINARY__\"", &serde_json::to_string(binary)?)
        .replace("\"__MUSTR_CONFIG__\"", &serde_json::to_string(config)?);
    let index = directory.join("index.ts");
    if index.exists() {
        check_regular(&index)?;
        let installed = fs::read_to_string(&index)?;
        ensure!(
            installed.contains(MARKER),
            "{} is not a mustr-managed adapter; refusing to overwrite",
            index.display()
        );
    }
    let transport = directory.join("transport.mjs");
    if transport.exists() {
        check_regular(&transport)?;
        ensure!(
            index.exists(),
            "unmanaged transport exists; refusing to overwrite"
        );
    }
    // Preserve the previous installed adapter for review before replacing it.
    for (path, content) in [(&transport, TRANSPORT), (&index, adapter.as_str())] {
        if path.exists() {
            let previous = fs::read(path)?;
            if previous == content.as_bytes() {
                continue;
            }
            let backup = path.with_extension(format!(
                "{}.{}.bak",
                path.extension().and_then(|s| s.to_str()).unwrap_or("file"),
                crate::paths::digest(&previous)
            ));
            create_file(&backup, &previous)?;
            replace(path, content)?;
        } else {
            create_file(path, content.as_bytes())?;
        }
    }
    Ok(index)
}

fn replace(path: &Path, contents: &str) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    create_file(&temporary, contents.as_bytes())?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).wrap_err("installing adapter");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn does_not_replace_unmanaged_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        let agent = dir.path().join("agent");
        let destination = agent.join("extensions/mustr");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("index.ts"), "user code").unwrap();
        assert!(install(&paths, Some(agent)).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("index.ts")).unwrap(),
            "user code"
        );
    }

    #[test]
    fn installs_idempotently_without_settings_changes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        let agent = dir.path().join("agent");
        let path = install(&paths, Some(agent.clone())).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(MARKER));
        assert!(!content.contains("\"__MUSTR_BINARY__\""));
        install(&paths, Some(agent.clone())).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(!agent.join("settings.json").exists());
    }
}
