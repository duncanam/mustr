//! Claude MCP bundle generation and conservative user-scoped registration.

use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use color_eyre::eyre::{Context, Result, ensure, eyre};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

use super::{TRANSPORT, replace};
use crate::paths::{Paths, check_regular, create_file, digest, private_dir};

const ADAPTER: &str = include_str!("../../claude/index.mjs");
const MARKER: &str = "mustr-managed Claude adapter v1";
const LAUNCHER: &str = "mustr.mjs";
const LAUNCHER_MARKER: &str = "// mustr-managed Claude launcher v1";
const LAUNCHER_PREFIX: &str = "// mustr-managed Claude launcher v1\nimport \"./index.";
const MAX_LAUNCHER_BYTES: usize = 4096;
const MAX_ADAPTER_BYTES: usize = 4 * 1024 * 1024;
const MAX_BUNDLE_CONFIG_BYTES: usize = 64 * 1024;
const USER_REGISTRY_FILE: &str = ".claude.json";
const MAX_USER_REGISTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_CLAUDE_CLI_OUTPUT_BYTES: usize = 64 * 1024;
const CLAUDE_CLI_TIMEOUT: Duration = Duration::from_secs(30);

struct ClaudeBundle {
    directory: PathBuf,
    config_path: PathBuf,
    launcher_path: PathBuf,
    adapter_name: String,
    configuration: String,
}

pub(crate) fn install_config_only(paths: &Paths, directory: Option<PathBuf>) -> Result<PathBuf> {
    let bundle = prepare_bundle(paths, directory)?;
    finish_bundle(&bundle)?;
    Ok(bundle.config_path)
}

pub(crate) async fn install_registered(
    paths: &Paths,
    directory: Option<PathBuf>,
) -> Result<PathBuf> {
    let bundle = prepare_bundle(paths, directory)?;
    let registry = user_registry_path()?;
    let expected = registration_value(&bundle)?;
    let already_registered = match read_user_mcp_server(&registry)? {
        None => false,
        Some(actual) => {
            ensure!(
                actual == expected,
                "Claude's user-scoped `mustr` server is already registered with different settings; the native registration and stable launcher were left untouched. Reinstall with its original `--directory` or reconcile it manually, or use `mustr install claude --config-only`."
            );
            true
        }
    };

    finish_bundle(&bundle)?;
    if !already_registered {
        add_user_registration(&expected)
            .await
            .wrap_err_with(|| format!(
                "Claude Code registration failed; the bundle remains at {}. Fix Claude Code or use `mustr install claude --config-only`.",
                bundle.directory.display()
            ))?;
        let actual = read_user_mcp_server(&registry).wrap_err_with(|| format!(
            "Claude Code reported registration success, but its user profile could not be verified; the bundle remains at {}. Inspect the profile manually or use `mustr install claude --config-only`.",
            bundle.directory.display()
        ))?;
        ensure!(
            actual.as_ref() == Some(&expected),
            "Claude Code reported registration success, but the user-scoped `mustr` entry did not match the expected registration. No profile file was edited directly; the bundle remains at {}.",
            bundle.directory.display()
        );
    }
    Ok(bundle.launcher_path)
}

fn prepare_bundle(paths: &Paths, directory: Option<PathBuf>) -> Result<ClaudeBundle> {
    let directory = directory.unwrap_or_else(|| paths.config.join("claude"));
    ensure!(
        directory.is_absolute(),
        "Claude adapter directory must be absolute"
    );
    private_dir(&directory)?;
    let directory = directory.canonicalize()?;
    let config_path = directory.join("mcp.json");
    validate_configuration(&config_path)?;
    let binary = std::env::current_exe()?.canonicalize()?;
    let binary = binary
        .to_str()
        .ok_or_else(|| eyre!("binary path is not UTF-8"))?;
    let config = paths
        .config
        .to_str()
        .ok_or_else(|| eyre!("config path is not UTF-8"))?;
    let transport_name = format!("transport.{}.mjs", digest(TRANSPORT));
    let adapter = ADAPTER
        .replace("\"__MUSTR_BINARY__\"", &serde_json::to_string(binary)?)
        .replace("\"__MUSTR_CONFIG__\"", &serde_json::to_string(config)?)
        .replace(
            "\"../pi/transport.mjs\"",
            &serde_json::to_string(&format!("./{transport_name}"))?,
        );
    let adapter_name = format!("index.{}.mjs", digest(&adapter));
    for (name, content) in [
        (&transport_name, TRANSPORT),
        (&adapter_name, adapter.as_str()),
    ] {
        let path = directory.join(name);
        create_file(&path, content.as_bytes())?;
        check_regular(&path)?;
        ensure!(
            read_regular_bounded(&path, MAX_ADAPTER_BYTES)? == content.as_bytes(),
            "immutable adapter asset contents do not match; refusing to overwrite {}",
            path.display()
        );
    }
    let launcher_path = directory.join(LAUNCHER);
    let launcher = launcher_path
        .to_str()
        .ok_or_else(|| eyre!("adapter path is not UTF-8"))?;
    let configuration = serde_json::to_string_pretty(&json!({
        "mcpServers": {
            "mustr": {
                "command": "node",
                "args": [launcher],
                "env": { "MUSTR_MANAGED": MARKER }
            }
        }
    }))? + "\n";
    Ok(ClaudeBundle {
        directory,
        config_path,
        launcher_path,
        adapter_name,
        configuration,
    })
}

fn finish_bundle(bundle: &ClaudeBundle) -> Result<()> {
    install_launcher(&bundle.directory, &bundle.adapter_name)?;
    write_configuration(&bundle.config_path, &bundle.configuration)?;
    Ok(())
}

// Official docs name ~/.claude.json as the user MCP registry. An explicit
// CLAUDE_CONFIG_DIR selects the active profile; the CLI 2.1.302 isolated probe
// stores that profile's user registry as <CLAUDE_CONFIG_DIR>/.claude.json.
fn user_registry_path() -> Result<PathBuf> {
    let profile = match env::var_os("CLAUDE_CONFIG_DIR") {
        Some(directory) => {
            let directory = PathBuf::from(directory);
            ensure!(
                directory.is_absolute(),
                "CLAUDE_CONFIG_DIR must be an absolute profile directory; use --config-only if the profile layout is unsupported"
            );
            directory
        }
        None => {
            let home = PathBuf::from(env::var_os("HOME").ok_or_else(|| {
                eyre!("HOME is not set; set CLAUDE_CONFIG_DIR or use --config-only")
            })?);
            ensure!(
                home.is_absolute(),
                "HOME must be an absolute path for Claude Code registration"
            );
            home
        }
    };
    Ok(profile.join(USER_REGISTRY_FILE))
}

fn registration_value(bundle: &ClaudeBundle) -> Result<Value> {
    let launcher = bundle
        .launcher_path
        .to_str()
        .ok_or_else(|| eyre!("Claude launcher path is not UTF-8"))?;
    Ok(json!({
        "type": "stdio",
        "command": "node",
        "args": [launcher],
        "env": { "MUSTR_MANAGED": MARKER }
    }))
}

fn read_user_mcp_server(path: &Path) -> Result<Option<Value>> {
    let bytes = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Claude user profile registry is not a regular file: {}",
                path.display()
            );
            ensure!(
                metadata.len() <= MAX_USER_REGISTRY_BYTES as u64,
                "Claude user profile registry exceeds the {}-byte safety limit: {}",
                MAX_USER_REGISTRY_BYTES,
                path.display()
            );
            read_regular_bounded(path, MAX_USER_REGISTRY_BYTES)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let root: Value = serde_json::from_slice(&bytes).wrap_err_with(|| {
        format!(
            "Claude user profile registry is not valid JSON: {} (contents are not logged)",
            path.display()
        )
    })?;
    let root = root.as_object().ok_or_else(|| {
        eyre!(
            "Claude user profile registry has an unsupported root shape: {}",
            path.display()
        )
    })?;
    let Some(servers) = root.get("mcpServers") else {
        return Ok(None);
    };
    let servers = servers.as_object().ok_or_else(|| {
        eyre!(
            "Claude user profile registry has an unsupported mcpServers shape: {}",
            path.display()
        )
    })?;
    Ok(servers.get("mustr").cloned())
}

async fn add_user_registration(registration: &Value) -> Result<()> {
    let configuration = serde_json::to_string(registration)?;
    ensure!(
        configuration.len() <= MAX_CLAUDE_CLI_OUTPUT_BYTES,
        "Claude registration argument exceeds the safety limit"
    );
    let mut child = Command::new("claude")
        .args(["mcp", "add-json", "--scope", "user", "mustr"])
        .arg(configuration)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .wrap_err(
            "Claude Code CLI could not be started; install Claude Code or use --config-only",
        )?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("Claude Code CLI stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| eyre!("Claude Code CLI stderr was not captured"))?;
    let output = timeout(CLAUDE_CLI_TIMEOUT, async {
        tokio::try_join!(read_cli_output(stdout), read_cli_output(stderr), async {
            child
                .wait()
                .await
                .map_err(|error| eyre!("waiting for Claude Code CLI: {error}"))
        })
    })
    .await;
    match output {
        Err(_) => {
            let _ = child.kill().await;
            Err(eyre!(
                "Claude Code CLI timed out after {} seconds; registration was not confirmed",
                CLAUDE_CLI_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(error)) => {
            let _ = child.kill().await;
            Err(error.wrap_err("capturing bounded Claude Code CLI output"))
        }
        Ok(Ok((_stdout, _stderr, status))) if status.success() => Ok(()),
        Ok(Ok((_stdout, _stderr, status))) => Err(eyre!(
            "Claude Code CLI exited with {status}; user registration was not confirmed"
        )),
    }
}

async fn read_cli_output<R>(reader: R) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    reader
        .take(MAX_CLAUDE_CLI_OUTPUT_BYTES as u64 + 1)
        .read_to_end(&mut output)
        .await?;
    ensure!(
        output.len() <= MAX_CLAUDE_CLI_OUTPUT_BYTES,
        "Claude Code CLI output exceeded the {}-byte safety limit",
        MAX_CLAUDE_CLI_OUTPUT_BYTES
    );
    Ok(output)
}

fn validate_configuration(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            check_regular(path)?;
            let existing: Value =
                serde_json::from_slice(&read_regular_bounded(path, MAX_BUNDLE_CONFIG_BYTES)?)?;
            ensure!(
                existing.as_object().is_some_and(|root| root.len() == 1)
                    && existing["mcpServers"]
                        .as_object()
                        .is_some_and(|servers| servers.len() == 1)
                    && existing["mcpServers"]["mustr"]["env"]["MUSTR_MANAGED"] == MARKER,
                "{} is not an exclusively mustr-managed MCP configuration; refusing to overwrite",
                path.display()
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_configuration(path: &Path, configuration: &str) -> Result<()> {
    validate_configuration(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let previous = read_regular_bounded(path, MAX_BUNDLE_CONFIG_BYTES)?;
            if previous != configuration.as_bytes() {
                let backup = path.with_extension(format!("json.{}.bak", digest(&previous)));
                create_file(&backup, &previous)?;
                check_regular(&backup)?;
                ensure!(
                    read_regular_bounded(&backup, MAX_BUNDLE_CONFIG_BYTES)? == previous,
                    "backup contents do not match; refusing to replace MCP configuration"
                );
                replace(path, configuration)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_file(path, configuration.as_bytes())?;
        }
        Err(error) => return Err(error.into()),
    }
    check_regular(path)?;
    ensure!(
        read_regular_bounded(path, MAX_BUNDLE_CONFIG_BYTES)? == configuration.as_bytes(),
        "MCP configuration contents do not match; refusing to use {}",
        path.display()
    );
    Ok(())
}

fn launcher_contents(adapter_name: &str) -> String {
    format!("{LAUNCHER_MARKER}\nimport \"./{adapter_name}\";\n")
}

fn managed_launcher_hash(contents: &[u8]) -> Option<&str> {
    let contents = std::str::from_utf8(contents).ok()?;
    let digest = contents
        .strip_prefix(LAUNCHER_PREFIX)?
        .strip_suffix(".mjs\";\n")?;
    (digest.len() == 24 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digest)
}

#[cfg(test)]
fn managed_launcher(contents: &[u8]) -> bool {
    managed_launcher_hash(contents).is_some()
}

fn read_regular_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    check_regular(path)?;
    ensure!(
        fs::metadata(path)?.len() <= limit as u64,
        "{} exceeds the {}-byte safety limit",
        path.display(),
        limit
    );
    let mut contents = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut contents)?;
    ensure!(
        contents.len() <= limit,
        "{} exceeds the {}-byte safety limit",
        path.display(),
        limit
    );
    Ok(contents)
}

fn verify_previous_launcher(directory: &Path, contents: &[u8]) -> Result<()> {
    let hash = managed_launcher_hash(contents)
        .ok_or_else(|| eyre!("launcher is not a valid mustr-managed Claude launcher"))?;
    let entry = directory.join(format!("index.{hash}.mjs"));
    let adapter = read_regular_bounded(&entry, MAX_ADAPTER_BYTES)?;
    ensure!(
        digest(&adapter) == hash,
        "launcher target is not an intact immutable adapter asset; refusing to replace it"
    );
    Ok(())
}

fn install_launcher(directory: &Path, adapter_name: &str) -> Result<PathBuf> {
    let path = directory.join(LAUNCHER);
    let contents = launcher_contents(adapter_name);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let previous = read_regular_bounded(&path, MAX_LAUNCHER_BYTES)?;
            if previous != contents.as_bytes() {
                verify_previous_launcher(directory, &previous).wrap_err_with(|| {
                    format!(
                        "{} is not a valid mustr-managed Claude launcher; refusing to overwrite",
                        path.display()
                    )
                })?;
                let backup = path.with_extension(format!("mjs.{}.bak", digest(&previous)));
                create_file(&backup, &previous)?;
                check_regular(&backup)?;
                ensure!(
                    read_regular_bounded(&backup, MAX_LAUNCHER_BYTES)? == previous,
                    "launcher backup contents do not match; refusing to replace {}",
                    path.display()
                );
                replace(&path, &contents)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_file(&path, contents.as_bytes())?;
        }
        Err(error) => return Err(error.into()),
    }
    check_regular(&path)?;
    ensure!(
        read_regular_bounded(&path, MAX_LAUNCHER_BYTES)? == contents.as_bytes(),
        "Claude launcher contents do not match; refusing to use {}",
        path.display()
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(directory: &std::path::Path) -> Paths {
        Paths {
            config: directory.join("config"),
            state: directory.join("state"),
        }
    }

    #[test]
    fn installs_idempotent_standalone_configuration_and_immutable_assets() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let installed = install_config_only(&paths, None).unwrap();
        assert_eq!(
            installed,
            paths.config.join("claude/mcp.json").canonicalize().unwrap()
        );
        let original = fs::read(&installed).unwrap();
        let config: Value = serde_json::from_slice(&original).unwrap();
        let launcher = PathBuf::from(config["mcpServers"]["mustr"]["args"][0].as_str().unwrap());
        assert_eq!(
            launcher,
            paths
                .config
                .join("claude")
                .canonicalize()
                .unwrap()
                .join(LAUNCHER)
        );
        let launcher_contents = fs::read_to_string(&launcher).unwrap();
        assert!(launcher_contents.starts_with(&format!("{LAUNCHER_MARKER}\nimport \"./index.")));
        let adapter_name = launcher_contents
            .split_once("import \"./")
            .unwrap()
            .1
            .trim_end_matches("\";\n");
        let entry = launcher.parent().unwrap().join(adapter_name);
        let adapter = fs::read_to_string(&entry).unwrap();
        assert!(!adapter.contains("\"__MUSTR_BINARY__\""));
        assert!(!adapter.contains("\"__MUSTR_CONFIG__\""));
        assert!(adapter.contains(&format!("./transport.{}.mjs", digest(TRANSPORT))));
        assert_eq!(install_config_only(&paths, None).unwrap(), installed);
        assert_eq!(fs::read(&installed).unwrap(), original);
        assert!(!paths.config.join("claude/settings.json").exists());
        fs::write(&entry, "corrupt").unwrap();
        assert!(
            install_config_only(&paths, None)
                .unwrap_err()
                .to_string()
                .contains("immutable")
        );
        assert_eq!(fs::read(&installed).unwrap(), original);
        assert_eq!(fs::read_to_string(&entry).unwrap(), "corrupt");
    }

    #[test]
    fn refuses_unmanaged_config_additional_servers_and_symlinks() {
        for text in [
            "{}",
            r#"{"mcpServers":{"other":{}}}"#,
            r#"{"mcpServers":{"mustr":{"env":{"MUSTR_MANAGED":"mustr-managed Claude adapter v1"}},"other":{}}}"#,
        ] {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("adapter");
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("mcp.json"), text).unwrap();
            assert!(install_config_only(&paths(root.path()), Some(directory.clone())).is_err());
            assert_eq!(
                fs::read_to_string(directory.join("mcp.json")).unwrap(),
                text
            );
        }
        for name in [
            "mcp.json".to_owned(),
            LAUNCHER.to_owned(),
            format!("transport.{}.mjs", digest(TRANSPORT)),
        ] {
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join("adapter");
            fs::create_dir(&directory).unwrap();
            let link = directory.join(name);
            std::os::unix::fs::symlink(root.path().join("absent"), &link).unwrap();
            assert!(install_config_only(&paths(root.path()), Some(directory)).is_err());
            assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        }
    }

    #[test]
    fn backs_up_changed_managed_config_and_keeps_old_assets() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let destination = install_config_only(&paths, None).unwrap();
        let original = fs::read(&destination).unwrap();
        let mut config: Value = serde_json::from_slice(&original).unwrap();
        config["mcpServers"]["mustr"]["args"] = json!(["old.mjs"]);
        let previous = serde_json::to_vec(&config).unwrap();
        fs::write(&destination, &previous).unwrap();
        install_config_only(&paths, None).unwrap();
        let backup = destination.with_extension(format!("json.{}.bak", digest(&previous)));
        assert_eq!(fs::read(backup).unwrap(), previous);
        assert_eq!(fs::read(destination).unwrap(), original);
    }

    #[test]
    fn backs_up_managed_launcher_before_switching_immutable_entry() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let directory = paths.config.join("claude");
        fs::create_dir_all(&directory).unwrap();
        let old_adapter = b"old adapter";
        let old_entry = format!("index.{}.mjs", digest(old_adapter));
        fs::write(directory.join(&old_entry), old_adapter).unwrap();
        let previous = launcher_contents(&old_entry);
        let launcher = directory.join(LAUNCHER);
        fs::write(&launcher, &previous).unwrap();

        install_config_only(&paths, None).unwrap();

        let current = fs::read(&launcher).unwrap();
        assert_ne!(current, previous.as_bytes());
        assert!(managed_launcher(&current));
        let backup = launcher.with_extension(format!("mjs.{}.bak", digest(&previous)));
        assert_eq!(fs::read(backup).unwrap(), previous.as_bytes());
        assert_eq!(
            fs::read_to_string(directory.join(old_entry)).unwrap(),
            "old adapter"
        );
        install_config_only(&paths, None).unwrap();
        assert_eq!(fs::read(&launcher).unwrap(), current);
    }

    #[test]
    fn refuses_unmanaged_launcher_without_overwriting_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let directory = paths.config.join("claude");
        fs::create_dir_all(&directory).unwrap();
        let launcher = directory.join(LAUNCHER);
        fs::write(&launcher, "user launcher").unwrap();
        assert!(install_config_only(&paths, None).is_err());
        assert_eq!(fs::read_to_string(launcher).unwrap(), "user launcher");
    }

    #[test]
    fn rejects_relative_destination() {
        let root = tempfile::tempdir().unwrap();
        assert!(install_config_only(&paths(root.path()), Some(PathBuf::from("relative"))).is_err());
    }
}
