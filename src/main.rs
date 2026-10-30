//! mustr: a user-owned, herdr-scoped hub. Agent reasoning remains in its harness.
//! Internal modules deliberately do not form a public library API.

mod ai_help;
mod cli;
mod config;
mod delivery;
mod herdr;
mod hub;
mod install;
mod launcher;
mod paths;
mod profiles;
mod protocol;
mod roles;
mod server;
mod service;
mod store;
mod tui;
mod watchers;

use clap::Parser;
use color_eyre::eyre::{Context, Result};

use crate::{
    cli::{AgentCommand, Cli, Command, InstallTarget},
    config::{Config, initialize, seed_roles},
    herdr::HerdrUnavailable,
    paths::Paths,
};

const CONTEXT_UNAVAILABLE_EXIT_CODE: i32 = 75;

fn validate_profile_cli(command: &Command, profile: Option<&str>, jailed: bool) -> Result<()> {
    match command {
        Command::Run { .. } => {}
        Command::Agent {
            command: AgentCommand::Start { .. } | AgentCommand::List,
        } => {}
        Command::Agent {
            command: AgentCommand::Stop { .. },
        } if !jailed && profile.is_none() => {}
        _ if profile.is_some() || jailed => color_eyre::eyre::bail!(
            "--profile/--jailed are only valid for profile run or agent start/list operations"
        ),
        _ => {}
    }
    if jailed
        && !matches!(
            command,
            Command::Run { .. }
                | Command::Agent {
                    command: AgentCommand::Start { .. }
                }
        )
    {
        color_eyre::eyre::bail!("--jailed applies only to profile run and agent start");
    }
    if matches!(
        command,
        Command::Agent {
            command: AgentCommand::Start { .. }
        }
    ) && profile.is_none()
    {
        color_eyre::eyre::bail!("agent start requires --profile NAME");
    }
    if let Some(name) = profile {
        ensure_profile_name(name)?;
    }
    Ok(())
}

fn ensure_profile_name(name: &str) -> Result<()> {
    color_eyre::eyre::ensure!(roles::valid_name(name), "invalid profile name {name:?}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    if cli.ai_help {
        ai_help::print()?;
        return Ok(());
    }
    let profile_name = cli.profile.clone();
    let jailed = cli.jailed;
    let paths = Paths::discover(cli.config_dir)?;
    initialize(&paths)?;
    let config = Config::load(&paths)?;
    let command = cli.command.unwrap_or(Command::Run { headless: false });
    validate_profile_cli(&command, profile_name.as_deref(), jailed)?;
    match command {
        Command::Init => {
            seed_roles(&paths)?;
            profiles::seed_default(&paths)?;
            println!(
                "Configuration: {}\nRoles: {}\nProfiles: {} (existing files preserved)",
                paths.config.display(),
                paths.roles().display(),
                paths.config.join("profiles.toml").display()
            );
        }
        Command::Roles { name, json } => {
            let roles = match name {
                Some(name) => vec![roles::resolve(&paths.roles(), &name)?],
                None => roles::load(&paths.roles())?,
            };
            if json {
                println!("{}", serde_json::to_string(&roles)?);
            } else {
                for role in roles {
                    println!("{}\t{}", role.name, role.description);
                }
            }
        }
        Command::Install {
            target:
                InstallTarget::Pi {
                    agent_dir,
                    overwrite_roles,
                },
        } => {
            let role_plan = prepare_role_refresh(&paths, overwrite_roles)?;
            let destination = install::install(&paths, agent_dir)?;
            let role_report =
                apply_role_refresh_after_install(role_plan, &destination, "Pi adapter")?;
            println!(
                "Installed {}\nProfiles: {} (existing files preserved)\nRestart Pi or /reload, then /role. Other extensions and settings are unchanged.",
                destination.display(),
                paths.config.join("profiles.toml").display()
            );
            if let Some(report) = role_report {
                print_role_refresh(&report);
            }
        }
        Command::Context { wait_for_pi } => {
            let herdr = herdr::Herdr::from_env()?;
            let result = if wait_for_pi {
                herdr.discovery_wait_for_pi().await
            } else {
                herdr.discovery().await
            };
            match result {
                Ok(discovery) => println!("{}", serde_json::to_string(&discovery)?),
                Err(error) => {
                    if let Some(unavailable) = error.downcast_ref::<HerdrUnavailable>() {
                        // This command-only status is the adapter's narrow transient
                        // context contract. Avoid color-eyre chains/backtraces here.
                        eprintln!("{unavailable}");
                        std::process::exit(CONTEXT_UNAVAILABLE_EXIT_CODE);
                    }
                    return Err(error);
                }
            }
        }
        Command::Run { headless } => {
            let profile = profile_name
                .as_deref()
                .map(|name| profiles::load(&paths, name))
                .transpose()?;
            if let Some(profile) = &profile {
                launcher::validate_jail_request(&paths, profile, jailed)?;
            }
            let herdr = herdr::Herdr::from_env()?;
            let discovery = herdr.discovery().await?;
            if let Some(profile) = &profile
                && server::is_running(&herdr, &discovery.space_id)?
            {
                let started = launcher::ensure_profile(&paths, profile, jailed).await?;
                println!(
                    "Profile {} ensured; the existing dashboard was left unchanged ({} new agent(s)).",
                    profile.name,
                    started.len()
                );
                return Ok(());
            }
            let server = match server::Server::start(herdr.clone(), &paths, &config, cli.verbosity)
                .await
            {
                Ok(server) => server,
                Err(_error)
                    if profile.is_some() && server::is_running(&herdr, &discovery.space_id)? =>
                {
                    let profile = profile.as_ref().expect("checked above");
                    let started = launcher::ensure_profile(&paths, profile, jailed).await?;
                    println!(
                        "Profile {} ensured; the existing dashboard was left unchanged ({} new agent(s)).",
                        profile.name,
                        started.len()
                    );
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if let Some(profile) = &profile
                && let Err(error) = launcher::ensure_profile(&paths, profile, jailed).await
            {
                server.shutdown().await;
                return Err(error);
            }
            let result = if headless {
                tracing::info!("headless mode; SIGINT/SIGTERM stops the hub, not its participants");
                wait_for_shutdown().await
            } else {
                tui::run(
                    server.service.clone(),
                    config.ui.color && std::env::var_os("NO_COLOR").is_none(),
                )
                .await
            };
            server.shutdown().await;
            result?;
        }
        Command::Agent { command } => match command {
            AgentCommand::Start { slot, name } => {
                let profile_name = profile_name.as_deref().ok_or_else(|| {
                    color_eyre::eyre::eyre!("agent start requires --profile NAME")
                })?;
                let profile = profiles::load(&paths, profile_name)?;
                let instance =
                    launcher::start_one_instance(&paths, &profile, &slot, name.as_deref(), jailed)
                        .await?;
                println!("Started or confirmed profile instance {instance}.");
            }
            AgentCommand::List => launcher::list(&paths, profile_name.as_deref()).await?,
            AgentCommand::Stop { instance } => launcher::stop(&paths, &instance).await?,
        },
        Command::ProfileBootstrap { launch_id } => launcher::bootstrap(&paths, &launch_id).await?,
    }
    Ok(())
}

fn prepare_role_refresh(
    paths: &Paths,
    overwrite_roles: bool,
) -> Result<Option<config::RoleRefreshPlan>> {
    overwrite_roles
        .then(|| {
            config::prepare_role_refresh(paths)
                .wrap_err("shipped-role refresh preflight failed; installer was not run")
        })
        .transpose()
}

fn apply_role_refresh_after_install(
    plan: Option<config::RoleRefreshPlan>,
    installed: &std::path::Path,
    target: &str,
) -> Result<Option<config::RoleRefreshReport>> {
    plan.map(|plan| {
        config::refresh_shipped_roles(plan).wrap_err_with(|| {
            format!(
                "{target} installation succeeded at {}; requested role refresh may be partial. Inspect the reported target and backup paths; no automatic rollback or live prompt refresh was attempted",
                installed.display()
            )
        })
    })
    .transpose()
}

fn print_role_refresh(report: &config::RoleRefreshReport) {
    if report.changed_count() == 0 {
        println!(
            "Shipped roles in {} already match embedded defaults; no files changed or backups created.",
            report.roles_dir.display()
        );
    } else {
        println!("Refreshed shipped roles in {}:", report.roles_dir.display());
        for path in &report.created {
            println!("  Created {}", path.display());
        }
        for replacement in &report.replaced {
            println!(
                "  Updated {} (backup: {})",
                replacement.path.display(),
                replacement.backup.display()
            );
        }
        if report.unchanged > 0 {
            println!(
                "  {} shipped role file(s) were already identical.",
                report.unchanged
            );
        }
    }
    println!(
        "Role files are on disk only; existing prompt snapshots were not refreshed. After reconciling work, explicitly reselect or rejoin roles to adopt them."
    );
}

async fn wait_for_shutdown() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    Ok(())
}
