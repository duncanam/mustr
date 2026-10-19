//! mustr: a user-owned, herdr-scoped hub. Agent reasoning remains in its harness.
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

use clap::Parser;
use color_eyre::eyre::Result;

use crate::{
    cli::{Cli, Command, InstallTarget},
    config::{Config, initialize, seed_roles},
    paths::Paths,
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
        Command::Install {
            harness:
                InstallTarget::Claude {
                    directory,
                    config_only,
                },
        } => {
            if config_only {
                let destination = install::install_claude_config_only(&paths, directory)?;
                println!(
                    "Generated {}\nLaunch Claude Code in herdr with --mcp-config pointing to this file.\nAsk Claude to join mustr with a role, then wait for work. Claude settings and permissions are unchanged.",
                    destination.display()
                );
            } else {
                let launcher = install::install_claude(&paths, directory).await?;
                println!(
                    "Installed {}\nRegistered the user-scoped mustr MCP server. Restart Claude Code to use it; tool permissions and managed-policy checks are unchanged.",
                    launcher.display()
                );
            }
        }
        Command::Context => println!(
            "{}",
            serde_json::to_string(&herdr::Herdr::from_env()?.discovery().await?)?
        ),
        Command::Run { headless } => {
            let server =
                server::Server::start(herdr::Herdr::from_env()?, &paths, &config, cli.verbosity)
                    .await?;
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
    }
    Ok(())
}

async fn wait_for_shutdown() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    Ok(())
}
