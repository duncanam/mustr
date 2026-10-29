//! Install the Pi adapter into its auto-discovered extensions directory.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, Result, ensure, eyre};

use crate::paths::{Paths, check_regular, create_file, private_dir, replace_file};

const ADAPTER: &str = include_str!("../pi/index.ts");
const TRANSPORT: &str = include_str!("../pi/transport.mjs");
const CONTEXT_WINDOW: &str = include_str!("../pi/context-window.ts");
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
    // Native ESM imports can remain cached across Pi /reload. A changed transport
    // must get a new URL, and old entry points must keep their original module.
    let transport_name = format!("transport.{}.mjs", crate::paths::digest(TRANSPORT));
    let context_window_name = format!("context-window.{}.ts", crate::paths::digest(CONTEXT_WINDOW));
    let adapter = ADAPTER
        .replace("\"__MUSTR_BINARY__\"", &serde_json::to_string(binary)?)
        .replace("\"__MUSTR_CONFIG__\"", &serde_json::to_string(config)?)
        .replace(
            "\"./transport.mjs\"",
            &serde_json::to_string(&format!("./{transport_name}"))?,
        )
        .replace(
            "\"./context-window.ts\"",
            &serde_json::to_string(&format!("./{context_window_name}"))?,
        );
    let index = directory.join("index.ts");
    if fs::symlink_metadata(&index).is_ok() {
        check_regular(&index)?;
        let installed = fs::read_to_string(&index)?;
        ensure!(
            installed.contains(MARKER),
            "{} is not a mustr-managed adapter; refusing to overwrite",
            index.display()
        );
    }
    let legacy_transport = directory.join("transport.mjs");
    if fs::symlink_metadata(&legacy_transport).is_ok() {
        check_regular(&legacy_transport)?;
        ensure!(
            index.exists(),
            "unmanaged transport exists; refusing to overwrite"
        );
    }
    let transport = directory.join(transport_name);
    create_file(&transport, TRANSPORT.as_bytes())?;
    check_regular(&transport)?;
    ensure!(
        fs::read(&transport)? == TRANSPORT.as_bytes(),
        "immutable transport contents do not match; refusing to overwrite {}",
        transport.display()
    );
    let context_window = directory.join(context_window_name);
    create_file(&context_window, CONTEXT_WINDOW.as_bytes())?;
    check_regular(&context_window)?;
    ensure!(
        fs::read(&context_window)? == CONTEXT_WINDOW.as_bytes(),
        "immutable context window contents do not match; refusing to overwrite {}",
        context_window.display()
    );
    // Install the immutable dependency first, then atomically switch the entry
    // point. Keep old transports and back up changed entry points for rollback.
    if index.exists() {
        let previous = fs::read(&index)?;
        if previous != adapter.as_bytes() {
            let backup =
                index.with_extension(format!("ts.{}.bak", crate::paths::digest(&previous)));
            create_file(&backup, &previous)?;
            replace(&index, &adapter)?;
        }
    } else {
        create_file(&index, adapter.as_bytes())?;
    }
    crate::profiles::seed_default(paths).wrap_err_with(|| {
        format!(
            "Pi adapter installed at {}, but default profile setup failed",
            index.display()
        )
    })?;
    Ok(index)
}

pub(crate) fn validate_profile_adapter(agent_dir: &Path) -> Result<()> {
    let directory = agent_dir.join("extensions/mustr");
    let index = directory.join("index.ts");
    check_regular(&index).wrap_err(
        "install the current Pi adapter with `mustr install pi` before starting profiles",
    )?;
    let contents = fs::read_to_string(&index)?;
    ensure!(
        contents.contains(MARKER) && contents.contains("registerFlag?.(\"mustr-launch\""),
        "the installed Pi adapter does not support profile startup; run `mustr install pi` explicitly"
    );
    for (prefix, extension) in [("transport.", ".mjs"), ("context-window.", ".ts")] {
        let marker = format!("./{prefix}");
        let relative = contents
            .split(&marker)
            .nth(1)
            .and_then(|suffix| {
                suffix
                    .split_once(extension)
                    .map(|(name, _)| format!("{prefix}{name}{extension}"))
            })
            .ok_or_else(|| {
                eyre!("installed Pi adapter is missing immutable dependency {prefix}*{extension}")
            })?;
        let path = directory.join(relative);
        check_regular(&path)?;
        let bytes = fs::read(&path)?;
        let expected = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| eyre!("invalid immutable dependency filename"))?;
        let content_hash = crate::paths::digest(&bytes);
        ensure!(
            expected.contains(&content_hash),
            "installed immutable Pi dependency hash does not match {}",
            path.display()
        );
    }
    Ok(())
}

fn replace(path: &Path, contents: &str) -> Result<()> {
    replace_file(path, contents.as_bytes()).wrap_err("installing adapter")
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
    fn refuses_dangling_links_without_replacing_them() {
        for name in [
            "index.ts".to_owned(),
            "transport.mjs".to_owned(),
            format!("transport.{}.mjs", crate::paths::digest(TRANSPORT)),
            format!("context-window.{}.ts", crate::paths::digest(CONTEXT_WINDOW)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths {
                config: dir.path().join("config"),
                state: dir.path().join("state"),
            };
            let agent = dir.path().join("agent");
            let destination = agent.join("extensions/mustr");
            fs::create_dir_all(&destination).unwrap();
            let link = destination.join(name);
            std::os::unix::fs::symlink(dir.path().join("absent"), &link).unwrap();
            assert!(install(&paths, Some(agent)).is_err());
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }
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
        let context_window_name = content
            .split("from \"")
            .filter_map(|part| part.split_once("\"").map(|(name, _)| name))
            .find(|name| name.starts_with("./context-window."))
            .unwrap()
            .trim_start_matches("./");
        assert_eq!(
            fs::read(agent.join("extensions/mustr").join(context_window_name)).unwrap(),
            CONTEXT_WINDOW.as_bytes()
        );
        install(&paths, Some(agent.clone())).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(!agent.join("settings.json").exists());
    }
}
