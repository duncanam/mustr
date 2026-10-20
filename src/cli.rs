//! Command-line surface; clap owns validation and help output.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, Verbosity};

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub(crate) struct Cli {
    /// Configuration directory (defaults to $XDG_CONFIG_HOME/mustr or ~/.config/mustr).
    #[arg(long, env = "MUSTR_CONFIG_DIR", global = true)]
    pub config_dir: Option<PathBuf>,

    #[command(flatten)]
    pub verbosity: Verbosity<InfoLevel>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Start the hub and dashboard for this herdr space (also the default command).
    Run {
        /// Run the hub without a terminal dashboard.
        #[arg(long)]
        headless: bool,
    },
    /// Create initial configuration and restore missing default roles without overwriting files.
    Init,
    /// List valid roles or resolve one role before the hub starts.
    Roles {
        /// Resolve only this role and its dependencies.
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Install a harness adapter; use its --overwrite-roles flag to refresh shipped role files.
    #[command(subcommand_value_name = "HARNESS")]
    Install {
        #[command(subcommand)]
        harness: InstallTarget,
    },
    /// Print live caller identity and hub discovery information for adapters.
    Context,
}

#[derive(Debug, Subcommand)]
pub(crate) enum InstallTarget {
    /// Register the Claude Code MCP adapter for the current user.
    Claude {
        /// Bundle directory (defaults to the mustr config's claude subdirectory; a registered server cannot be moved automatically).
        #[arg(long)]
        directory: Option<PathBuf>,
        /// Generate a standalone MCP config without registering it with Claude Code.
        #[arg(long)]
        config_only: bool,
        /// Replace edits under shipped names with embedded defaults across all harnesses/spaces sharing this config directory; back up exact originals. Active prompt snapshots require explicit refresh/reselection/rejoin.
        #[arg(long)]
        overwrite_roles: bool,
    },
    /// Install the Pi adapter into its auto-discovered extensions directory.
    Pi {
        /// Pi agent directory, not its extensions directory.
        #[arg(long, env = "PI_CODING_AGENT_DIR")]
        agent_dir: Option<PathBuf>,
        /// Replace edits under shipped names with embedded defaults across all harnesses/spaces sharing this config directory; back up exact originals. Active prompt snapshots require explicit refresh/reselection/rejoin.
        #[arg(long)]
        overwrite_roles: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn install_selects_a_harness_and_scopes_its_options() {
        let cli = Cli::try_parse_from([
            "mustr",
            "install",
            "pi",
            "--agent-dir",
            "/tmp/pi",
            "--overwrite-roles",
            "--config-dir",
            "/tmp/mustr",
            "-v",
        ])
        .unwrap();
        assert_eq!(cli.config_dir, Some(PathBuf::from("/tmp/mustr")));
        assert_eq!(cli.verbosity.to_string(), "debug");
        assert!(
            matches!(cli.command, Some(Command::Install { harness: InstallTarget::Pi { agent_dir: Some(path), overwrite_roles: true } }) if path == std::path::Path::new("/tmp/pi"))
        );
    }

    #[test]
    fn claude_install_has_separate_options() {
        assert!(matches!(
            Cli::try_parse_from(["mustr", "install", "claude", "--directory", "/tmp/claude", "--overwrite-roles"])
                .unwrap()
                .command,
            Some(Command::Install { harness: InstallTarget::Claude { directory: Some(path), config_only: false, overwrite_roles: true } })
                if path == std::path::Path::new("/tmp/claude")
        ));
        assert!(matches!(
            Cli::try_parse_from(["mustr", "install", "claude", "--config-only"])
                .unwrap()
                .command,
            Some(Command::Install {
                harness: InstallTarget::Claude {
                    config_only: true,
                    overwrite_roles: false,
                    ..
                }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "mustr",
                "install",
                "claude",
                "--config-only",
                "--overwrite-roles"
            ])
            .unwrap()
            .command,
            Some(Command::Install {
                harness: InstallTarget::Claude {
                    config_only: true,
                    overwrite_roles: true,
                    ..
                }
            })
        ));
        assert!(
            Cli::try_parse_from(["mustr", "install", "claude", "--agent-dir", "/tmp/x"]).is_err()
        );
        assert!(Cli::try_parse_from(["mustr", "install", "pi", "--directory", "/tmp/x"]).is_err());
        assert!(Cli::try_parse_from(["mustr", "init", "--overwrite-roles"]).is_err());
    }

    #[test]
    fn roles_accepts_an_optional_target_name() {
        assert!(matches!(
            Cli::try_parse_from(["mustr", "roles", "architect", "--json"])
                .unwrap()
                .command,
            Some(Command::Roles { name: Some(name), json: true }) if name == "architect"
        ));
        assert!(matches!(
            Cli::try_parse_from(["mustr", "roles"]).unwrap().command,
            Some(Command::Roles {
                name: None,
                json: false
            })
        ));
    }

    #[test]
    fn install_requires_a_supported_harness() {
        assert!(Cli::try_parse_from(["mustr", "install"]).is_err());
        assert!(Cli::try_parse_from(["mustr", "install", "unknown"]).is_err());
        assert!(Cli::try_parse_from(["mustr", "install", "--agent-dir", "/tmp/pi"]).is_err());
        assert!(Cli::try_parse_from(["mustr", "install-pi"]).is_err());
    }

    #[test]
    fn supports_verbosity_and_configuration_after_subcommands() {
        let cli = Cli::try_parse_from([
            "mustr",
            "run",
            "--headless",
            "-vv",
            "--config-dir",
            "/tmp/x",
        ])
        .unwrap();
        assert_eq!(cli.verbosity.to_string(), "trace");
        assert!(matches!(cli.command, Some(Command::Run { headless: true })));
    }
}
