//! Host-managed Pi tab ownership and the fixed per-launch bootstrap.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Context, Result, bail, ensure, eyre};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    herdr::{Discovery, Herdr},
    install,
    paths::{Paths, check_regular, create_file, digest, private_dir, replace_file},
    profiles::{Agent, Profile},
    roles, server,
};

mod gateway;
mod jail;

const LEDGER_VERSION: u32 = 1;
const MANIFEST_VERSION: u32 = 1;
const LAUNCH_WAIT: Duration = Duration::from_secs(20);

pub(crate) fn validate_jail_request(
    paths: &Paths,
    profile: &Profile,
    jail_override: bool,
) -> Result<()> {
    jail::validate(
        paths,
        profile
            .agents
            .values()
            .filter(|agent| jail_override || agent.jailed),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    server_key: String,
    space_id: String,
    records: Vec<Record>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    launch_id: String,
    profile: String,
    slot: String,
    instance: String,
    spec: String,
    #[serde(default)]
    tab_id: Option<String>,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    ready: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchDescriptor {
    version: u32,
    launch_id: String,
    server_key: String,
    space_id: String,
    pane_id: String,
    tab_id: String,
    socket: PathBuf,
    profile: String,
    slot: String,
    instance: String,
    tab_name: String,
    role: roles::Role,
    model: String,
    thinking: String,
    cwd: PathBuf,
    config_dir: PathBuf,
    state_dir: PathBuf,
    binary: PathBuf,
    descriptor_path: PathBuf,
    ready_path: PathBuf,
    spec_digest: String,
    #[serde(default)]
    jailed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyReceipt {
    version: u32,
    launch_id: String,
    server_key: String,
    space_id: String,
    pane_id: String,
    agent_id: String,
    role: String,
    model: String,
    thinking: String,
}

#[derive(Clone, Debug)]
struct LiveTab {
    tab_id: String,
    workspace_id: String,
    pane_count: Option<u64>,
}

struct LedgerFiles {
    directory: PathBuf,
    ledger: PathBuf,
    lock: Option<File>,
}

impl LedgerFiles {
    fn open(paths: &Paths, discovery: &Discovery) -> Result<Self> {
        let directory = paths
            .state
            .join("profile-launches")
            .join(&discovery.server_key)
            .join(digest(discovery.space_id.as_str()));
        private_dir(&directory)?;
        let lock_path = directory.join("ledger.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)?;
        check_regular(&lock_path)?;
        lock.try_lock()
            .map_err(|_| eyre!("profile ownership ledger is already in use"))?;
        Ok(Self {
            ledger: directory.join("ownership.json"),
            directory,
            lock: Some(lock),
        })
    }

    fn unlock(&mut self) {
        self.lock.take();
    }

    fn relock(&mut self) -> Result<()> {
        ensure!(self.lock.is_none(), "profile ledger lock is already held");
        let lock_path = self.directory.join("ledger.lock");
        let lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        check_regular(&lock_path)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match lock.try_lock() {
                Ok(()) => {
                    self.lock = Some(lock);
                    return Ok(());
                }
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    bail!("timed out waiting for profile ownership ledger")
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    fn load(&self, discovery: &Discovery) -> Result<Ledger> {
        match fs::symlink_metadata(&self.ledger) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Ledger {
                version: LEDGER_VERSION,
                server_key: discovery.server_key.clone(),
                space_id: discovery.space_id.to_string(),
                records: Vec::new(),
            }),
            Err(error) => Err(error.into()),
            Ok(_) => {
                check_regular(&self.ledger)?;
                let ledger: Ledger = serde_json::from_slice(&fs::read(&self.ledger)?)
                    .wrap_err_with(|| format!("parsing {}", self.ledger.display()))?;
                ensure!(
                    ledger.version == LEDGER_VERSION,
                    "unsupported profile ownership ledger version"
                );
                ensure!(
                    ledger.server_key == discovery.server_key
                        && ledger.space_id == discovery.space_id.as_str(),
                    "profile ownership ledger belongs to a different herdr incarnation or space"
                );
                Ok(ledger)
            }
        }
    }

    fn save(&self, ledger: &Ledger) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(ledger)?;
        match fs::symlink_metadata(&self.ledger) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                crate::paths::create_file_atomically(&self.ledger, &bytes)?;
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {
                check_regular(&self.ledger)?;
                replace_file(&self.ledger, &bytes)?;
            }
        }
        Ok(())
    }

    fn launch_directory(&self, launch_id: &str) -> Result<PathBuf> {
        validate_launch_id(launch_id)?;
        let path = self.directory.join(launch_id);
        private_dir(&path)?;
        Ok(path)
    }
}

pub(crate) async fn ensure_profile(
    paths: &Paths,
    profile: &Profile,
    jail_override: bool,
) -> Result<Vec<String>> {
    validate_jail_request(paths, profile, jail_override)?;
    install::validate_profile_adapter(&pi_agent_dir()?)?;
    let herdr = Herdr::from_env()?;
    let discovery = herdr.discovery().await?;
    ensure!(
        server::is_running(&herdr, &discovery.space_id)?,
        "start the Mustr hub in this herdr space before launching profile agents"
    );
    let mut files = LedgerFiles::open(paths, &discovery)?;
    let mut ledger = files.load(&discovery)?;
    let tabs = list_tabs(&discovery.space_id.to_string())?;
    let mut planned = Vec::new();

    for agent in profile.agents.values() {
        let mut agent = agent.clone();
        agent.jailed |= jail_override;
        let instance = format!("{}/{}", profile.name, agent.slot);
        match existing_instance(
            &mut ledger,
            &files,
            &discovery,
            &tabs,
            &profile.name,
            &instance,
            &agent,
        )? {
            Existing::Ready => {}
            Existing::Missing => planned.push((agent, instance)),
        }
    }
    files.save(&ledger)?;

    let mut started = Vec::new();
    for (agent, instance) in planned {
        if start_one(
            paths,
            &discovery,
            &mut files,
            &mut ledger,
            &profile.name,
            &agent,
            &instance,
        )? {
            started.push(instance);
        }
    }
    Ok(started)
}

pub(crate) async fn start_one_instance(
    paths: &Paths,
    profile: &Profile,
    slot: &str,
    extra: Option<&str>,
    jail_override: bool,
) -> Result<String> {
    install::validate_profile_adapter(&pi_agent_dir()?)?;
    let agent = profile
        .agents
        .get(slot)
        .ok_or_else(|| eyre!("profile {:?} has no slot {slot:?}", profile.name))?;
    let instance = match extra {
        Some(extra) => {
            validate_logical_name(extra)?;
            format!("{}/{}/{}", profile.name, slot, extra)
        }
        None => format!("{}/{}", profile.name, slot),
    };
    let mut agent = agent.clone();
    if let Some(extra) = extra {
        agent.tab_name = format!("{} ({extra})", agent.tab_name);
        ensure!(
            agent.tab_name.len() <= 64,
            "extra instance makes tab label exceed 64 bytes"
        );
    }
    agent.jailed |= jail_override;
    jail::validate(paths, std::iter::once(&agent).filter(|agent| agent.jailed))?;
    let herdr = Herdr::from_env()?;
    let discovery = herdr.discovery().await?;
    ensure!(
        server::is_running(&herdr, &discovery.space_id)?,
        "start the Mustr hub in this herdr space before launching profile agents"
    );
    let mut files = LedgerFiles::open(paths, &discovery)?;
    let mut ledger = files.load(&discovery)?;
    let tabs = list_tabs(&discovery.space_id.to_string())?;
    match existing_instance(
        &mut ledger,
        &files,
        &discovery,
        &tabs,
        &profile.name,
        &instance,
        &agent,
    )? {
        Existing::Ready if extra.is_none() => return Ok(format!("{instance} is already managed")),
        Existing::Ready => {
            bail!("extra instance {instance:?} already exists; choose a different --name")
        }
        Existing::Missing => {}
    }
    files.save(&ledger)?;
    start_one(
        paths,
        &discovery,
        &mut files,
        &mut ledger,
        &profile.name,
        &agent,
        &instance,
    )?;
    Ok(instance)
}

pub(crate) async fn list(paths: &Paths, profile: Option<&str>) -> Result<()> {
    let herdr = Herdr::from_env()?;
    let discovery = herdr.discovery().await?;
    let files = LedgerFiles::open(paths, &discovery)?;
    let ledger = files.load(&discovery)?;
    let tabs = list_tabs(&discovery.space_id.to_string())?;
    for record in ledger
        .records
        .iter()
        .filter(|record| profile.is_none_or(|p| p == record.profile))
    {
        let state = match (&record.tab_id, &record.pane_id) {
            (None, _) => "unresolved-create".to_owned(),
            (Some(tab), _) if !tabs.iter().any(|live| live.tab_id == *tab) => "stale".to_owned(),
            (Some(_), _) if record.ready => "ready".to_owned(),
            _ => "launch-unconfirmed".to_owned(),
        };
        println!(
            "{}\t{}\t{}\t{}",
            record.instance,
            state,
            record.tab_id.as_deref().unwrap_or("-"),
            record.pane_id.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

pub(crate) async fn stop(paths: &Paths, instance: &str) -> Result<()> {
    let herdr = Herdr::from_env()?;
    let discovery = herdr.discovery().await?;
    let files = LedgerFiles::open(paths, &discovery)?;
    let ledger = files.load(&discovery)?;
    let record = ledger
        .records
        .iter()
        .rev()
        .find(|record| record.instance == instance)
        .ok_or_else(|| {
            eyre!("no profile-owned instance {instance:?} in this herdr incarnation/space")
        })?;
    let tab_id = record.tab_id.as_deref().ok_or_else(|| eyre!("instance {instance:?} has unresolved tab-create outcome; inspect Herdr before editing its ownership record"))?;
    validate_stop_target(&herdr, &discovery, record).await?;
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "agent stop requires an interactive terminal for confirmation"
    );
    eprint!(
        "Close managed tab {tab_id} for {instance}? Unfinished tasks and external jobs may continue. Type 'yes' to close: "
    );
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim() != "yes" {
        println!("Cancelled; no tab closed.");
        return Ok(());
    }
    // Confirmation can take arbitrarily long; it is not ownership evidence.
    validate_stop_target(&herdr, &discovery, record).await?;
    run_herdr(&["tab", "close", tab_id])?;
    println!(
        "Closed managed tab {tab_id}. This did not stop external jobs or cancel/reassign tasks."
    );
    Ok(())
}

async fn validate_stop_target(herdr: &Herdr, discovery: &Discovery, record: &Record) -> Result<()> {
    herdr.check_instance()?;
    let tab_id = record
        .tab_id
        .as_deref()
        .ok_or_else(|| eyre!("owned tab is unknown"))?;
    let pane_id = record
        .pane_id
        .as_deref()
        .ok_or_else(|| eyre!("owned root pane is unknown; no close attempted"))?;
    let live = list_tabs(discovery.space_id.as_str())?;
    let tab = live
        .iter()
        .find(|tab| tab.tab_id == tab_id)
        .ok_or_else(|| {
            eyre!("owned tab {tab_id} is stale or no longer in this space; no close attempted")
        })?;
    ensure!(
        tab.workspace_id == discovery.space_id.as_str() && tab.pane_count == Some(1),
        "owned tab is not in the expected space with exactly one pane; no close attempted"
    );
    let result = herdr
        .request("pane.get", serde_json::json!({ "pane_id": pane_id }))
        .await
        .wrap_err("cannot validate the recorded root pane; no close attempted")?;
    let pane = result
        .get("pane")
        .ok_or_else(|| eyre!("recorded root pane is missing; no close attempted"))?;
    ensure!(
        pane.get("pane_id").and_then(Value::as_str) == Some(pane_id)
            && pane.get("tab_id").and_then(Value::as_str) == Some(tab_id)
            && pane.get("workspace_id").and_then(Value::as_str)
                == Some(discovery.space_id.as_str()),
        "recorded root pane no longer belongs to the owned tab/space; no close attempted"
    );
    Ok(())
}

pub(crate) async fn bootstrap(paths: &Paths, launch_id: &str) -> Result<()> {
    validate_launch_id(launch_id)?;
    let herdr = Herdr::from_env()?;
    let discovery = herdr.discovery().await?;
    let files = LedgerFiles::open(paths, &discovery)?;
    let ledger = files.load(&discovery)?;
    let record = ledger
        .records
        .iter()
        .find(|record| record.launch_id == launch_id)
        .ok_or_else(|| eyre!("launch id is not owned in this herdr incarnation/space"))?;
    let tab_id = record
        .tab_id
        .as_deref()
        .ok_or_else(|| eyre!("tab-create outcome is unresolved; bootstrap refused"))?;
    let pane_id = record
        .pane_id
        .as_deref()
        .ok_or_else(|| eyre!("owned tab has no recorded root pane"))?;
    ensure!(
        pane_id == discovery.pane_id && record.profile.len() <= 48,
        "bootstrap ran in a different pane or invalid profile scope"
    );
    let launch_directory = files.launch_directory(launch_id)?;
    let manifest_path = launch_directory.join("manifest.json");
    check_regular(&manifest_path)?;
    let manifest: LaunchDescriptor =
        serde_json::from_slice(&read_private_file(&manifest_path, 256 * 1024)?)
            .wrap_err("parsing the private launch manifest")?;
    validate_descriptor(&manifest, launch_id, &discovery, tab_id, pane_id)?;
    ensure!(
        manifest.profile == record.profile
            && manifest.slot == record.slot
            && manifest.instance == record.instance
            && manifest.spec_digest == record.spec,
        "launch manifest does not match its ownership record"
    );
    drop(files);
    if manifest.jailed {
        return jail::run(manifest, &launch_directory).await;
    }
    let descriptor_path = launch_directory.join("descriptor.json");
    let mut descriptor = serde_json::to_value(&manifest)?;
    descriptor
        .as_object_mut()
        .expect("manifest object")
        .remove("jailed");
    write_readonly_file(&descriptor_path, &serde_json::to_vec(&descriptor)?)?;
    let child = Command::new("pi")
        .args([
            "--model",
            &manifest.model,
            "--thinking",
            &manifest.thinking,
            "--name",
            &manifest.tab_name,
            "--mustr-launch",
            descriptor_path
                .to_str()
                .ok_or_else(|| eyre!("launch descriptor path is not UTF-8"))?,
        ])
        .current_dir(&manifest.cwd)
        .env("MUSTR_LAUNCH_DESCRIPTOR", &descriptor_path)
        .env("MUSTR_BIN", &manifest.binary)
        .env("MUSTR_CONFIG_DIR", &manifest.config_dir)
        .env("MUSTR_STATE_DIR", &manifest.state_dir)
        .status()
        .wrap_err("starting Pi from the fixed host bootstrap")?;
    ensure!(
        child.success(),
        "Pi exited with {child}; launch remains owned but did not complete"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Existing {
    Ready,
    Missing,
}

fn existing_instance(
    ledger: &mut Ledger,
    files: &LedgerFiles,
    discovery: &Discovery,
    tabs: &[LiveTab],
    profile: &str,
    instance: &str,
    agent: &Agent,
) -> Result<Existing> {
    let spec = spec_digest(profile, &agent.slot, agent)?;
    let index = ledger
        .records
        .iter()
        .rposition(|record| record.instance == instance);
    let Some(index) = index else {
        return Ok(Existing::Missing);
    };
    let record = &mut ledger.records[index];
    let Some(tab_id) = record.tab_id.as_deref() else {
        bail!(
            "instance {instance:?} has unresolved tab-create outcome; inspect Herdr and the ownership ledger before retrying"
        );
    };
    let Some(_) = tabs
        .iter()
        .find(|tab| tab.tab_id == tab_id && tab.workspace_id == discovery.space_id.as_str())
    else {
        return Ok(Existing::Missing);
    };
    ensure!(
        record.spec == spec,
        "profile drift for live instance {instance:?}; stop it explicitly before changing model/role/thinking/cwd/tab/jail settings"
    );
    if record.ready {
        return Ok(Existing::Ready);
    }
    let ready_path = files
        .launch_directory(&record.launch_id)?
        .join("ready.json");
    if let Ok(bytes) = read_private_file(&ready_path, 16 * 1024) {
        let receipt: ReadyReceipt =
            serde_json::from_slice(&bytes).wrap_err("parsing launch readiness receipt")?;
        validate_ready(&receipt, record, discovery, agent)?;
        record.ready = true;
        return Ok(Existing::Ready);
    }
    bail!(
        "instance {instance:?} has an owned tab but startup/join is unconfirmed; no automatic relaunch attempted"
    )
}

fn start_one(
    paths: &Paths,
    discovery: &Discovery,
    files: &mut LedgerFiles,
    ledger: &mut Ledger,
    profile: &str,
    agent: &Agent,
    instance: &str,
) -> Result<bool> {
    let agent_dir = pi_agent_dir()?;
    install::validate_profile_adapter(&agent_dir)?;
    // Earlier slots release the lock while Pi starts. An individual start may
    // have filled this planned slot meanwhile; refresh under the held lock
    // immediately before persisting intent or creating anything in Herdr.
    *ledger = files.load(discovery)?;
    let tabs = list_tabs(discovery.space_id.as_str())?;
    if existing_instance(ledger, files, discovery, &tabs, profile, instance, agent)?
        == Existing::Ready
    {
        files.save(ledger)?;
        return Ok(false);
    }
    let launch_id = uuid::Uuid::new_v4().to_string();
    let launch_directory = files.launch_directory(&launch_id)?;
    let record = Record {
        launch_id: launch_id.clone(),
        profile: profile.to_owned(),
        slot: agent.slot.clone(),
        instance: instance.to_owned(),
        spec: spec_digest(profile, &agent.slot, agent)?,
        tab_id: None,
        pane_id: None,
        ready: false,
    };
    ledger.records.push(record);
    files
        .save(ledger)
        .wrap_err("persisting the launch intent before Herdr side effects")?;

    let binary = std::env::current_exe()?.canonicalize()?;
    // Preserve the exact configured spelling so the bootstrap reopens the same
    // per-space ownership ledger as the parent launcher (not a symlink alias).
    let state_dir = paths.state.clone();
    let mut args = vec![
        "tab".to_owned(),
        "create".to_owned(),
        "--workspace".to_owned(),
        discovery.space_id.to_string(),
        "--cwd".to_owned(),
        agent.cwd.to_string_lossy().into_owned(),
        "--label".to_owned(),
        agent.tab_name.clone(),
        "--env".to_owned(),
        "HERDR_AGENT=pi".to_owned(),
        "--env".to_owned(),
        format!("MUSTR_CONFIG_DIR={}", paths.config.display()),
        "--env".to_owned(),
        format!("MUSTR_STATE_DIR={}", state_dir.display()),
        "--env".to_owned(),
        format!("MUSTR_BIN={}", binary.display()),
        "--no-focus".to_owned(),
    ];
    args.extend([
        "--env".to_owned(),
        format!("PI_CODING_AGENT_DIR={}", agent_dir.display()),
    ]);
    let output = run_herdr_args(args)
        .wrap_err("tab-create outcome is unresolved; no retry will be attempted")?;
    let (tab, pane) = parse_created_tab(&output, &discovery.space_id.to_string())?;
    let index = ledger.records.len() - 1;
    ledger.records[index].tab_id = Some(tab.tab_id.clone());
    ledger.records[index].pane_id = Some(pane.clone());
    files.save(ledger).wrap_err_with(|| {
        format!(
            "Herdr created tab {} but ownership-ledger update failed; do not recreate it",
            tab.tab_id
        )
    })?;

    let descriptor_path = launch_directory.join("descriptor.json");
    let ready_path = launch_directory.join("ready.json");
    let manifest = LaunchDescriptor {
        version: MANIFEST_VERSION,
        launch_id: launch_id.clone(),
        server_key: discovery.server_key.clone(),
        space_id: discovery.space_id.to_string(),
        pane_id: pane.clone(),
        tab_id: tab.tab_id.clone(),
        socket: discovery.socket.clone(),
        profile: profile.to_owned(),
        slot: agent.slot.clone(),
        instance: instance.to_owned(),
        tab_name: agent.tab_name.clone(),
        role: agent.role.clone(),
        model: agent.model.clone(),
        thinking: agent.thinking.clone(),
        cwd: agent.cwd.clone(),
        config_dir: paths.config.clone(),
        state_dir,
        binary,
        descriptor_path: descriptor_path.clone(),
        ready_path: ready_path.clone(),
        spec_digest: spec_digest(profile, &agent.slot, agent)?,
        jailed: agent.jailed,
    };
    write_readonly_file(
        &launch_directory.join("manifest.json"),
        &serde_json::to_vec(&manifest)?,
    )?;
    let command = format!(
        "{} --config-dir {} __profile_bootstrap {}",
        shell_quote(
            manifest
                .binary
                .to_str()
                .ok_or_else(|| eyre!("mustr path is not UTF-8"))?
        ),
        shell_quote(
            paths
                .config
                .to_str()
                .ok_or_else(|| eyre!("config path is not UTF-8"))?
        ),
        shell_quote(&launch_id)
    );
    files.unlock();
    if let Err(error) = run_herdr(&["pane", "run", &pane, &command]) {
        files.relock()?;
        *ledger = files.load(discovery)?;
        return Err(error).wrap_err_with(|| {
            format!(
                "tab {} is owned but bootstrap launch outcome is uncertain; no retry attempted",
                tab.tab_id
            )
        });
    }

    let deadline = Instant::now() + LAUNCH_WAIT;
    loop {
        if let Ok(bytes) = read_private_file(&ready_path, 16 * 1024) {
            let receipt: ReadyReceipt =
                serde_json::from_slice(&bytes).wrap_err("parsing launch readiness receipt")?;
            files.relock()?;
            *ledger = files.load(discovery)?;
            let index = ledger
                .records
                .iter()
                .rposition(|record| record.launch_id == launch_id)
                .ok_or_else(|| eyre!("launch record disappeared while Pi was starting"))?;
            validate_ready(&receipt, &ledger.records[index], discovery, agent)?;
            ledger.records[index].ready = true;
            files.save(ledger)?;
            return Ok(true);
        }
        if Instant::now() >= deadline {
            files.relock()?;
            *ledger = files.load(discovery)?;
            bail!(
                "profile launch {instance:?} created owned tab {} but Pi startup/join was not confirmed within {} seconds; no retry or cleanup attempted",
                tab.tab_id,
                LAUNCH_WAIT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn validate_ready(
    receipt: &ReadyReceipt,
    record: &Record,
    discovery: &Discovery,
    agent: &Agent,
) -> Result<()> {
    ensure!(
        receipt.version == 1 && receipt.launch_id == record.launch_id,
        "readiness receipt does not match launch id"
    );
    ensure!(
        receipt.server_key == discovery.server_key
            && receipt.space_id == discovery.space_id.as_str()
            && receipt.pane_id == record.pane_id.as_deref().unwrap_or_default(),
        "readiness receipt identity differs from the recorded live pane/space"
    );
    ensure!(
        receipt.role == agent.role.name
            && receipt.model == agent.model
            && receipt.thinking == agent.thinking,
        "Pi startup did not honor the requested role/model/thinking level"
    );
    ensure!(
        !receipt.agent_id.is_empty() && receipt.agent_id.len() <= 128,
        "invalid Pi identity in readiness receipt"
    );
    Ok(())
}

fn validate_descriptor(
    value: &LaunchDescriptor,
    launch_id: &str,
    discovery: &Discovery,
    tab_id: &str,
    pane_id: &str,
) -> Result<()> {
    ensure!(
        value.version == MANIFEST_VERSION && value.launch_id == launch_id,
        "launch manifest schema or id mismatch"
    );
    ensure!(
        value.server_key == discovery.server_key
            && value.space_id == discovery.space_id.as_str()
            && value.pane_id == pane_id
            && value.tab_id == tab_id
            && value.socket == discovery.socket,
        "launch manifest does not match current herdr context"
    );
    ensure!(
        value.binary.is_absolute()
            && value.config_dir.is_absolute()
            && value.state_dir.is_absolute()
            && value.cwd.is_absolute(),
        "manifest paths must be absolute"
    );
    ensure!(
        roles::valid_name(&value.profile) && roles::valid_name(&value.slot),
        "invalid launch profile or slot"
    );
    ensure!(
        value.instance.len() <= 160 && !value.instance.chars().any(char::is_control),
        "invalid launch instance name"
    );
    Ok(())
}

fn spec_digest(profile: &str, slot: &str, agent: &Agent) -> Result<String> {
    Ok(digest(serde_json::to_vec(&(
        profile,
        slot,
        &agent.tab_name,
        &agent.role.name,
        &agent.model,
        &agent.thinking,
        &agent.cwd,
        agent.jailed,
    ))?))
}

fn pi_agent_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return Ok(PathBuf::from(path).canonicalize()?);
    }
    let home = std::env::var_os("HOME").ok_or_else(|| eyre!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".pi/agent").canonicalize()?)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn validate_launch_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 36
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "invalid launch id"
    );
    Ok(())
}

fn validate_logical_name(value: &str) -> Result<()> {
    ensure!(
        (1..=48).contains(&value.len())
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "extra instance names use 1..48 lowercase letters, digits, and hyphens"
    );
    Ok(())
}

fn read_private_file(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).wrap_err_with(|| format!("inspecting {}", path.display()))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= maximum as u64,
        "expected a bounded regular launch file: {}",
        path.display()
    );
    ensure!(
        metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.permissions().mode() & 0o077 == 0,
        "launch file is not private and user-owned: {}",
        path.display()
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && opened.len() <= maximum as u64,
        "launch file changed during open: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= maximum, "launch file exceeds size limit");
    Ok(bytes)
}

fn write_readonly_file(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        create_file(path, bytes)?,
        "launch file already exists: {}",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o400))?;
    check_regular(path)?;
    Ok(())
}

fn run_herdr(args: &[&str]) -> Result<String> {
    run_herdr_args(args.iter().map(|value| (*value).to_owned()).collect())
}

fn run_herdr_args(args: Vec<String>) -> Result<String> {
    let output = Command::new("herdr")
        .args(&args)
        .output()
        .wrap_err_with(|| format!("running herdr {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "herdr {} failed ({}): {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).wrap_err("herdr output was not UTF-8")
}

fn list_tabs(workspace: &str) -> Result<Vec<LiveTab>> {
    let output = run_herdr(&["tab", "list", "--workspace", workspace])?;
    let value: Value =
        serde_json::from_str(output.trim()).wrap_err("parsing herdr tab list JSON")?;
    let mut tabs = Vec::new();
    collect_tabs(&value, &mut tabs);
    Ok(tabs)
}

fn collect_tabs(value: &Value, output: &mut Vec<LiveTab>) {
    match value {
        Value::Object(map) => {
            if let (Some(tab_id), Some(workspace_id)) = (
                map.get("tab_id").and_then(Value::as_str),
                map.get("workspace_id").and_then(Value::as_str),
            ) {
                output.push(LiveTab {
                    tab_id: tab_id.to_owned(),
                    workspace_id: workspace_id.to_owned(),
                    pane_count: map.get("pane_count").and_then(Value::as_u64),
                });
            }
            for child in map.values() {
                collect_tabs(child, output);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_tabs(child, output);
            }
        }
        _ => {}
    }
}

fn parse_created_tab(output: &str, workspace: &str) -> Result<(LiveTab, String)> {
    let value: Value =
        serde_json::from_str(output.trim()).wrap_err("parsing herdr tab-create JSON")?;
    let tab_value =
        find_key(&value, "tab").ok_or_else(|| eyre!("tab-create response has no tab"))?;
    let tab_id = find_key(tab_value, "tab_id")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("tab-create response has no tab_id"))?
        .to_owned();
    let workspace_id = find_key(tab_value, "workspace_id")
        .and_then(Value::as_str)
        .unwrap_or(workspace)
        .to_owned();
    ensure!(
        workspace_id == workspace,
        "herdr created tab in a different space"
    );
    let root = find_key(&value, "root_pane")
        .or_else(|| find_key(&value, "root_pane_id"))
        .ok_or_else(|| eyre!("tab-create response has no root pane identity"))?;
    let pane_id = if let Some(value) = root.as_str() {
        value.to_owned()
    } else {
        find_key(root, "pane_id")
            .or_else(|| find_key(root, "root_pane_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| eyre!("tab-create response has no root pane id"))?
            .to_owned()
    };
    ensure!(
        !tab_id.is_empty() && !pane_id.is_empty() && tab_id.len() <= 128 && pane_id.len() <= 128,
        "invalid tab or pane id from herdr"
    );
    Ok((
        LiveTab {
            tab_id,
            workspace_id,
            pane_count: Some(1),
        },
        pane_id,
    ))
}

fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|child| find_key(child, key))),
        Value::Array(items) => items.iter().find_map(|child| find_key(child, key)),
        _ => None,
    }
}
