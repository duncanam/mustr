//! Fixed ai-jail invocation and per-launch Pi state; never map the host home.

use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
};

use color_eyre::eyre::{Context, Result, ensure};
use serde_json::json;

use super::{LaunchDescriptor, gateway::Gateway, pi_agent_dir, write_readonly_file};
use crate::{
    paths::{Paths, check_regular, private_dir, runtime_dir},
    profiles::Agent,
};

pub(super) fn validate<'a>(paths: &Paths, agents: impl Iterator<Item = &'a Agent>) -> Result<()> {
    let agents: Vec<_> = agents.collect();
    if agents.is_empty() {
        return Ok(());
    }
    ensure!(
        cfg!(any(target_os = "macos", target_os = "linux")),
        "jailed launch requires macOS or Linux"
    );
    let version = Command::new("ai-jail")
        .arg("--version")
        .output()
        .wrap_err("--jailed requires ai-jail 2.12.0 or newer on the host PATH")?;
    let text = String::from_utf8(version.stdout)?;
    let parts: Vec<u64> = text
        .trim()
        .strip_prefix("ai-jail ")
        .unwrap_or_default()
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .wrap_err("cannot verify the installed ai-jail version")?;
    ensure!(
        version.status.success() && parts.len() == 3 && parts.as_slice() >= [2, 12, 0].as_slice(),
        "--jailed requires ai-jail 2.12.0 or newer"
    );
    let agent_dir = pi_agent_dir()?;
    let adapter = fs::read_to_string(agent_dir.join("extensions/mustr/index.ts"))?;
    ensure!(
        adapter.contains("mustr-jail-transport-v1"),
        "install the current jailed-capable adapter with `mustr install pi` first"
    );
    private_dir(&paths.state)?;
    let mut protected = vec![
        paths.state.canonicalize()?,
        paths.config.canonicalize()?,
        agent_dir,
        runtime_dir()?.canonicalize()?,
    ];
    if let Some(socket) = std::env::var_os("HERDR_SOCKET_PATH") {
        protected.push(PathBuf::from(socket).canonicalize()?);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.canonicalize())
        .transpose()?;
    for agent in agents {
        for path in &protected {
            ensure!(
                !path.starts_with(&agent.cwd) && !agent.cwd.starts_with(path),
                "jailed cwd must not overlap host Pi/Mustr configuration, state or control sockets"
            );
        }
        ensure!(
            !home
                .as_ref()
                .is_some_and(|home| home.starts_with(&agent.cwd)),
            "jailed cwd must not contain the host home"
        );
    }
    Ok(())
}

fn prepare_pi(source: &Path, destination: &Path) -> Result<Vec<PathBuf>> {
    private_dir(destination)?;
    let mut maps = Vec::new();
    // Preserve installed code/policy resources, not private session archives or
    // host credentials. External symlink targets still need explicit jail grants.
    for name in ["extensions", "skills", "prompts", "themes", "npm", "git"] {
        let path = source.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Pi resource root {} must be a real directory for jailed launch",
                    path.display()
                );
                symlink(&path, destination.join(name))?;
                maps.push(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    for name in [
        "settings.json",
        "keybindings.json",
        "models.json",
        "mcp.json",
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
        "SYSTEM.md",
        "APPEND_SYSTEM.md",
    ] {
        let path = source.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                check_regular(&path)?;
                ensure!(
                    metadata.len() <= 1024 * 1024,
                    "Pi configuration file is too large: {}",
                    path.display()
                );
                crate::paths::create_file(&destination.join(name), &fs::read(path)?)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(maps)
}

pub(super) async fn run(manifest: LaunchDescriptor, launch_directory: &Path) -> Result<()> {
    let sandbox = launch_directory.join("sandbox");
    private_dir(&sandbox)?;
    let pi_dir = sandbox.join("pi");
    let maps = prepare_pi(&pi_agent_dir()?, &pi_dir)?;
    let descriptor_path = sandbox.join("descriptor.json");
    let gateway = Gateway::start(manifest.clone()).await?;
    // No host binary/config/state/socket paths or writable readiness route.
    let descriptor = json!({
        "version": 2, "launch_id": manifest.launch_id, "server_key": manifest.server_key,
        "space_id": manifest.space_id, "pane_id": manifest.pane_id, "tab_id": manifest.tab_id,
        "socket": format!("tcp://127.0.0.1:{}", gateway.endpoint.port),
        "profile": manifest.profile, "slot": manifest.slot, "instance": manifest.instance,
        "tab_name": manifest.tab_name, "role": manifest.role, "model": manifest.model,
        "thinking": manifest.thinking, "cwd": manifest.cwd, "spec_digest": manifest.spec_digest,
        "descriptor_path": descriptor_path, "gateway": gateway.endpoint,
    });
    write_readonly_file(&descriptor_path, &serde_json::to_vec(&descriptor)?)?;
    let mut command = tokio::process::Command::new("ai-jail");
    command
        .args([
            "--no-save-config",
            "--private-home",
            "--no-agent-state",
            "--no-inherit-env",
            "--no-macos-host-ipc",
            "--no-docker",
            "--no-browser",
            "--no-github",
            "--no-aws",
            "--no-kube",
            "--no-gcloud",
            "--no-docker-config",
            "--no-worktree",
            "--no-gpu",
            "--no-display",
            "--no-audio",
            "--no-x11",
            "--no-pictures",
            "--no-tailscale",
            "--no-ssh",
            "--no-systemd-user",
            "--no-host-shm",
            "--no-network",
            "--no-toolchains",
            "--no-mise",
            "--no-update-check",
            "--no-audit-log",
            "--no-terminal-passthrough",
            "--exec",
            "--forward-port",
        ])
        .arg(gateway.endpoint.port.to_string())
        .arg("--map")
        .arg(&descriptor_path)
        .arg("--rw-map")
        .arg(&pi_dir);
    for path in maps {
        command.arg("--map").arg(path);
    }
    command
        .arg("--env")
        .arg(format!("PI_CODING_AGENT_DIR={}", pi_dir.display()))
        .arg("--env")
        .arg(format!(
            "MUSTR_LAUNCH_DESCRIPTOR={}",
            descriptor_path.display()
        ))
        .args([
            "--",
            "pi",
            "--model",
            &manifest.model,
            "--thinking",
            &manifest.thinking,
            "--name",
            &manifest.tab_name,
            "--mustr-launch",
        ])
        .arg(&descriptor_path)
        .current_dir(&manifest.cwd)
        .env_remove("MUSTR_BIN")
        .env_remove("MUSTR_CONFIG_DIR")
        .env_remove("MUSTR_STATE_DIR")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_AGENT")
        .kill_on_drop(true);
    let result = command
        .status()
        .await
        .wrap_err("starting jailed Pi; no unjailed fallback attempted");
    gateway.shutdown().await;
    let status = result?;
    ensure!(
        status.success(),
        "jailed Pi exited with {status}; no retry or unjailed fallback attempted"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_policy_resources_without_host_auth_or_history() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("extensions")).unwrap();
        fs::create_dir_all(source.join("sessions")).unwrap();
        fs::write(source.join("extensions/policy.ts"), "policy sentinel").unwrap();
        fs::write(source.join("AGENTS.md"), "user policy sentinel").unwrap();
        fs::write(
            source.join("settings.json"),
            "{\"extensions\":[\"./extensions/policy.ts\"]}",
        )
        .unwrap();
        fs::write(source.join("auth.json"), "PRIVATE AUTH").unwrap();
        fs::write(source.join("sessions/private.jsonl"), "PRIVATE HISTORY").unwrap();
        let maps = prepare_pi(&source, &target).unwrap();
        assert_eq!(maps, [source.join("extensions")]);
        assert_eq!(
            fs::read_to_string(target.join("extensions/policy.ts")).unwrap(),
            "policy sentinel"
        );
        assert_eq!(
            fs::read_to_string(target.join("AGENTS.md")).unwrap(),
            "user policy sentinel"
        );
        assert_eq!(
            fs::read(target.join("settings.json")).unwrap(),
            fs::read(source.join("settings.json")).unwrap()
        );
        assert!(!target.join("auth.json").exists());
        assert!(!target.join("sessions").exists());
    }
}
