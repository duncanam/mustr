//! Scoped Mustr transport through ai-jail; Pi state and sandbox policy belong to ai-jail.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use color_eyre::eyre::{Context, Result, ensure};
use serde_json::json;

use super::{LaunchDescriptor, gateway::Gateway, pi_agent_dir, write_readonly_file};
use crate::{
    paths::{Paths, private_dir, runtime_dir},
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

pub(super) async fn run(manifest: LaunchDescriptor, launch_directory: &Path) -> Result<()> {
    let sandbox = launch_directory.join("sandbox");
    private_dir(&sandbox)?;
    let descriptor_path = sandbox.join("descriptor.json");
    // The host launcher pins PI_CODING_AGENT_DIR even for the standard path.
    // Only a genuinely custom directory needs an explicit state map.
    let selected = pi_agent_dir()?;
    let standard = std::env::var_os("HOME")
        .and_then(|home| PathBuf::from(home).join(".pi/agent").canonicalize().ok());
    let custom_agent_dir = (standard.as_ref() != Some(&selected)).then_some(selected);
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
        // Operational/transport flags only: do not override ai-jail's state,
        // network, environment or other capability policy.
        .args(["--no-save-config", "--forward-port"])
        .arg(gateway.endpoint.port.to_string())
        .arg("--map")
        .arg(&descriptor_path);
    // ai-jail knows Pi's standard ~/.pi state. Preserve an explicitly selected
    // custom agent directory with equivalent read-write sharing semantics.
    if let Some(agent_dir) = custom_agent_dir {
        command
            .arg("--rw-map")
            .arg(&agent_dir)
            .arg("--env")
            .arg(format!("PI_CODING_AGENT_DIR={}", agent_dir.display()));
    }
    command
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
