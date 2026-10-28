//! Command-line surface; clap owns validation and help output.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, Verbosity};
use color_eyre::eyre::{Result, bail, ensure};

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub(crate) struct Cli {
    /// Configuration directory (defaults to $XDG_CONFIG_HOME/mustr or ~/.config/mustr).
    #[arg(long, env = "MUSTR_CONFIG_DIR", global = true)]
    pub config_dir: Option<PathBuf>,

    /// Start with the named profile from profiles.toml (default startup or interactive run only).
    #[arg(long, global = true)]
    pub profile: Option<String>,

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

pub(crate) fn validate_profile_command(cli: &Cli) -> Result<()> {
    let Some(profile) = cli.profile.as_deref() else {
        return Ok(());
    };
    ensure!(
        !profile.trim().is_empty(),
        "--profile name must not be blank"
    );

    match cli.command.as_ref() {
        None | Some(Command::Run { headless: false }) => Ok(()),
        Some(Command::Run { headless: true }) => {
            bail!("--profile cannot be used with `run --headless`")
        }
        Some(_) => bail!("--profile is only valid for default startup or `run`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn profile_option_is_accepted_before_or_after_run() {
        let default = Cli::try_parse_from(["mustr", "--profile", "feature"]).unwrap();
        assert_eq!(default.profile.as_deref(), Some("feature"));
        assert!(default.command.is_none());

        let run = Cli::try_parse_from(["mustr", "run", "--profile", "feature"]).unwrap();
        assert_eq!(run.profile.as_deref(), Some("feature"));
        assert!(matches!(
            run.command,
            Some(Command::Run { headless: false })
        ));
    }

    #[test]
    fn profile_validation_allows_only_default_or_interactive_run() {
        for args in [
            vec!["mustr"],
            vec!["mustr", "--profile", "feature"],
            vec!["mustr", "run", "--profile", "feature"],
            vec!["mustr", "run", "--headless"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(validate_profile_command(&cli).is_ok());
        }

        for args in [
            vec!["mustr", "init", "--profile", "feature"],
            vec!["mustr", "roles", "--profile", "feature"],
            vec!["mustr", "install", "pi", "--profile", "feature"],
            vec!["mustr", "context", "--profile", "feature"],
            vec!["mustr", "run", "--headless", "--profile", "feature"],
            vec!["mustr", "--profile", ""],
            vec!["mustr", "run", "--profile", "   "],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(validate_profile_command(&cli).is_err());
        }
    }

    #[test]
    fn plain_and_plain_headless_commands_have_no_profile() {
        let plain = Cli::try_parse_from(["mustr"]).unwrap();
        assert!(plain.profile.is_none());
        assert!(plain.command.is_none());

        let headless = Cli::try_parse_from(["mustr", "run", "--headless"]).unwrap();
        assert!(headless.profile.is_none());
        assert!(matches!(
            headless.command,
            Some(Command::Run { headless: true })
        ));
    }

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
