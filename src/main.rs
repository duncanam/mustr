//! mustr: a user-owned, herdr-scoped hub. Agent reasoning remains in Pi.
//! Internal modules deliberately do not form a public library API.

mod cli;
mod config;
mod delivery;
mod herdr;
mod hub;
mod install;
mod paths;
mod protocol;
mod roles;
mod server;
mod service;
mod store;
mod tui;
mod watchers;

use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, sync::Arc};

use clap::Parser;
use color_eyre::eyre::Result;

use crate::{
    cli::{Cli, Command, InstallTarget},
    config::{Config, initialize, seed_roles},
    paths::{Paths, private_dir},
};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
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
            harness: InstallTarget::Pi { agent_dir },
        } => {
            let destination = install::install(&paths, agent_dir)?;
            println!(
                "Installed {}\nRestart Pi or /reload, then /role. Other extensions and settings are unchanged.",
                destination.display()
            );
        }
        Command::Context => println!(
            "{}",
            serde_json::to_string(&herdr::Herdr::from_env()?.discovery().await?)?
        ),
        Command::Run { headless } => {
            private_dir(&paths.state)?;
            let log = paths.state.join("mustr.log");
            if std::fs::metadata(&log).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
                std::fs::rename(&log, log.with_extension("log.1"))?;
            }
            let file = OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .open(&log)?;
            paths::check_regular(&log)?;
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(cli.verbosity)
                .with_writer(Arc::new(file))
                .try_init()
                .map_err(|e| color_eyre::eyre::eyre!("initializing logging: {e}"))?;
            let server = server::Server::start(herdr::Herdr::from_env()?, &paths, &config).await?;
            let result = if headless {
                tracing::info!("headless mode; SIGINT/SIGTERM stops the hub, not its participants");
                wait_for_shutdown().await
            } else {
                tui::run(
                    server.service.clone(),
                    server.initial_space.clone(),
                    config.ui.color && std::env::var_os("NO_COLOR").is_none(),
                )
                .await
            };
            server.shutdown().await;
            result?;
        }
    }
    Ok(())
}

async fn wait_for_shutdown() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    Ok(())
}
