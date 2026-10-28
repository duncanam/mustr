//! mustr: a user-owned, herdr-scoped hub. Agent reasoning remains in its harness.
//! Internal modules deliberately do not form a public library API.

mod cli;
mod config;
mod delivery;
mod herdr;
mod herdr_launcher;
mod hub;
mod install;
mod paths;
mod profile;
mod profile_startup;
mod protocol;
mod roles;
mod server;
mod service;
mod store;
mod tui;
mod watchers;

use clap::Parser;
use color_eyre::eyre::{Context, Result, bail};

use crate::{
    cli::{Cli, Command, InstallTarget, validate_profile_command},
    config::{Config, initialize, seed_roles},
    paths::Paths,
};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    validate_profile_command(&cli)?;
    let paths = Paths::discover(cli.config_dir)?;
    initialize(&paths)?;
    let config = Config::load(&paths)?;
    match cli.command.unwrap_or(Command::Run { headless: false }) {
        Command::Init => {
            seed_roles(&paths)?;
            println!(
                "Configuration: {}\nRoles: {}",
                paths.config.display(),
                paths.roles().display()
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
            harness:
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
                "Installed {}\nRestart Pi or /reload, then /role. Other extensions and settings are unchanged.",
                destination.display()
            );
            if let Some(report) = role_report {
                print_role_refresh(&report);
            }
        }
        Command::Install {
            harness:
                InstallTarget::Claude {
                    directory,
                    config_only,
                    overwrite_roles,
                },
        } => {
            let role_plan = prepare_role_refresh(&paths, overwrite_roles)?;
            let installed = if config_only {
                install::install_claude_config_only(&paths, directory)?
            } else {
                install::install_claude(&paths, directory).await?
            };
            let install_label = if config_only {
                "Claude config-only bundle"
            } else {
                "Claude adapter"
            };
            let role_report =
                apply_role_refresh_after_install(role_plan, &installed, install_label)?;
            if config_only {
                println!(
                    "Generated {}\nLaunch Claude Code in herdr with --mcp-config pointing to this file.\nAsk Claude to join mustr with a role, then wait for work. Claude settings and permissions are unchanged.",
                    installed.display()
                );
            } else {
                println!(
                    "Installed {}\nRegistered the user-scoped mustr MCP server. Restart Claude Code to use it; tool permissions and managed-policy checks are unchanged.",
                    installed.display()
                );
            }
            if let Some(report) = role_report {
                print_role_refresh(&report);
            }
        }
        Command::Context => println!(
            "{}",
            serde_json::to_string(&herdr::Herdr::from_env()?.discovery().await?)?
        ),
        Command::Run { headless } => {
            if let Some(profile_name) = cli.profile.as_deref() {
                let profile = profile::load(&paths, profile_name)?;
                run_profile_startup(profile, &paths, &config, cli.verbosity).await?;
            } else {
                let server = server::Server::start(
                    herdr::Herdr::from_env()?,
                    &paths,
                    &config,
                    cli.verbosity,
                )
                .await?;
                let result = if headless {
                    tracing::info!(
                        "headless mode; SIGINT/SIGTERM stops the hub, not its participants"
                    );
                    wait_for_shutdown().await
                } else {
                    tui::run(
                        server.service.clone(),
                        config.ui.color && std::env::var_os("NO_COLOR").is_none(),
                        None,
                    )
                    .await
                };
                server.shutdown().await;
                result?;
            }
        }
    }
    Ok(())
}

async fn run_profile_startup(
    profile: profile::Profile,
    paths: &Paths,
    config: &Config,
    verbosity: clap_verbosity_flag::Verbosity<clap_verbosity_flag::InfoLevel>,
) -> Result<()> {
    let herdr = herdr::Herdr::from_env()?;
    let preflight_discovery = herdr.discovery().await?;
    let binary = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;
    if !cwd.is_absolute() || cwd.to_str().is_none_or(|value| value.contains('\0')) {
        bail!("Herdr launch paths must be absolute UTF-8 arguments without NUL bytes");
    }
    let config_dir = paths
        .config
        .canonicalize()
        .wrap_err("resolving the profile configuration directory")?;
    let bootstrap = herdr_launcher::TabBootstrap::new(&binary, &config_dir)
        .map_err(|message| color_eyre::eyre::eyre!(message))?;
    let launcher = herdr_launcher::HerdrLauncher::new(&herdr);
    launcher
        .preflight()
        .await
        .wrap_err("Herdr capability preflight failed; no profile participants were launched")?;
    tui::ensure_terminal()?;

    let server = server::Server::start(herdr.clone(), paths, config, verbosity).await?;
    if server.service.space_id != preflight_discovery.space_id {
        server.shutdown().await;
        bail!(
            "Herdr space identity changed during profile preflight; no profile participants were launched"
        );
    }

    tracing::info!(profile = %profile.name, entries = profile.roles.len(), "profile startup coordinator started");
    let (progress_tx, progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let coordinator = tokio::spawn(profile_startup::run(profile_startup::ProfileLaunch {
        profile,
        service: server.service.clone(),
        herdr,
        launcher,
        cwd,
        bootstrap,
        progress: progress_tx,
        cancel: cancel_rx,
    }));
    let result = tui::run(
        server.service.clone(),
        config.ui.color && std::env::var_os("NO_COLOR").is_none(),
        Some(tui::ProfileUi {
            progress: progress_rx,
            cancel: cancel_tx,
            coordinator: Some(coordinator),
        }),
    )
    .await;
    server.shutdown().await;
    result?;
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
