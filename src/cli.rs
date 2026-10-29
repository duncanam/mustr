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

    /// Start or manage a host-owned Pi profile.
    #[arg(long, global = true)]
    pub profile: Option<String>,
    /// Require ai-jail for profile-launched agents; never applies to the hub/dashboard.
    #[arg(long, global = true)]
    pub jailed: bool,

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
    /// Create initial configuration and restore missing default roles/profiles without overwriting files.
    Init,
    /// List valid roles or resolve one role before the hub starts.
    Roles {
        /// Resolve only this role and its dependencies.
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Install the Pi adapter and missing default profiles; --overwrite-roles also refreshes shipped role files.
    #[command(subcommand_value_name = "TARGET")]
    Install {
        #[command(subcommand)]
        target: InstallTarget,
    },
    /// Print live caller identity and hub discovery information for adapters.
    Context,
    /// Manage only profile-owned Pi tabs.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Internal bootstrap invoked only by a profile-owned Herdr pane.
    #[command(name = "__profile_bootstrap", hide = true)]
    ProfileBootstrap { launch_id: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentCommand {
    /// Start a configured slot or a distinct named instance.
    Start {
        /// Configured slot key in the selected --profile.
        slot: String,
        /// Create a distinct extra instance instead of using the slot identity.
        #[arg(long)]
        name: Option<String>,
    },
    /// List only tabs recorded in Mustr's private ownership ledger.
    List,
    /// Confirm and close one exact Mustr-owned tab; external work may continue.
    Stop { instance: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum InstallTarget {
    /// Install the Pi adapter into its auto-discovered extensions directory.
    Pi {
        /// Pi agent directory, not its extensions directory.
        #[arg(long, env = "PI_CODING_AGENT_DIR")]
        agent_dir: Option<PathBuf>,
        /// Replace edits under shipped names with embedded defaults for all spaces sharing this config directory; back up exact originals. Active prompt snapshots require explicit refresh/reselection/rejoin.
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
    fn install_selects_pi_and_scopes_its_options() {
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
            matches!(cli.command, Some(Command::Install { target: InstallTarget::Pi { agent_dir: Some(path), overwrite_roles: true } }) if path == std::path::Path::new("/tmp/pi"))
        );
    }

    #[test]
    fn install_rejects_non_pi_targets_and_target_mismatched_options() {
        assert!(Cli::try_parse_from(["mustr", "install", "unsupported"]).is_err());
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
    fn install_requires_a_supported_target() {
        assert!(Cli::try_parse_from(["mustr", "install"]).is_err());
        assert!(Cli::try_parse_from(["mustr", "install", "unknown"]).is_err());
        assert!(Cli::try_parse_from(["mustr", "install", "--agent-dir", "/tmp/pi"]).is_err());
        assert!(Cli::try_parse_from(["mustr", "install-pi"]).is_err());
    }

    #[test]
    fn profile_cli_accepts_global_and_agent_management_options() {
        let cli = Cli::try_parse_from(["mustr", "--profile", "team", "--jailed"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("team"));
        assert!(cli.jailed);
        let cli = Cli::try_parse_from([
            "mustr",
            "agent",
            "start",
            "architect",
            "--profile",
            "team",
            "--name",
            "extra",
        ])
        .unwrap();
        assert_eq!(cli.profile.as_deref(), Some("team"));
        assert!(
            matches!(cli.command, Some(Command::Agent { command: AgentCommand::Start { slot, name: Some(name) } }) if slot == "architect" && name == "extra")
        );
        let cli = Cli::try_parse_from(["mustr", "agent", "stop", "team/architect"]).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Agent { command: AgentCommand::Stop { instance } }) if instance == "team/architect")
        );
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
