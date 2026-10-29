//! Strict user-owned Pi profile configuration; profile data never becomes shell text.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, Result, ensure, eyre};
use serde::Deserialize;

use crate::{
    paths::{Paths, check_regular, create_file_atomically, private_dir},
    roles,
};

const DEFAULT_PROFILES: &str = include_str!("../profiles/default.toml");

/// Seed a missing file only; never read, merge or follow an existing destination.
pub(crate) fn seed_default(paths: &Paths) -> Result<bool> {
    private_dir(&paths.config)?;
    create_file_atomically(
        &paths.config.join("profiles.toml"),
        DEFAULT_PROFILES.as_bytes(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    version: u32,
    profiles: BTreeMap<String, ProfileConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileConfig {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    jailed: Option<bool>,
    agents: BTreeMap<String, AgentConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentConfig {
    pub tab_name: String,
    pub role: String,
    pub model: String,
    pub thinking: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub jailed: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct Agent {
    pub slot: String,
    pub tab_name: String,
    pub role: roles::Role,
    pub model: String,
    pub thinking: String,
    pub cwd: PathBuf,
    pub jailed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Profile {
    pub name: String,
    pub agents: BTreeMap<String, Agent>,
}

const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub(crate) fn load(paths: &Paths, name: &str) -> Result<Profile> {
    ensure!(roles::valid_name(name), "invalid profile name {name:?}");
    let path = paths.config.join("profiles.toml");
    check_regular(&path)?;
    let contents =
        fs::read_to_string(&path).wrap_err_with(|| format!("reading {}", path.display()))?;
    let file: FileConfig =
        toml::from_str(&contents).wrap_err_with(|| format!("parsing {}", path.display()))?;
    ensure!(
        file.version == 1,
        "unsupported profiles.toml version {}",
        file.version
    );
    let config = file
        .profiles
        .get(name)
        .ok_or_else(|| eyre!("unknown profile {name:?} in {}", path.display()))?;
    ensure!(!config.agents.is_empty(), "profile {name:?} has no agents");

    let default_cwd = match &config.cwd {
        Some(cwd) => resolve_cwd(cwd, &paths.config)?,
        None => std::env::current_dir().wrap_err("resolving current working directory")?,
    };
    let mut agents = BTreeMap::new();
    let mut tab_names = BTreeSet::new();
    let mut managers = 0usize;
    for (slot, agent) in &config.agents {
        ensure!(
            roles::valid_name(slot),
            "invalid slot name {slot:?} in profile {name:?}"
        );
        ensure!(
            roles::valid_name(&agent.role),
            "invalid role name {:?} in profile {name:?}/{slot}",
            agent.role
        );
        validate_tab_name(&agent.tab_name).wrap_err_with(|| format!("profile {name:?}/{slot}"))?;
        ensure!(
            tab_names.insert(agent.tab_name.clone()),
            "profile {name:?} has duplicate tab name {:?}",
            agent.tab_name
        );
        validate_model(&agent.model).wrap_err_with(|| format!("profile {name:?}/{slot}"))?;
        ensure!(
            THINKING_LEVELS.contains(&agent.thinking.as_str()),
            "profile {name:?}/{slot} has unsupported thinking level {:?}",
            agent.thinking
        );
        let cwd = match &agent.cwd {
            Some(cwd) => resolve_cwd(cwd, &paths.config)?,
            None => default_cwd.clone(),
        };
        let role = roles::resolve(&paths.roles(), &agent.role)
            .wrap_err_with(|| format!("validating role for profile {name:?}/{slot}"))?;
        if role.manager {
            managers += 1;
        }
        agents.insert(
            slot.clone(),
            Agent {
                slot: slot.clone(),
                tab_name: agent.tab_name.clone(),
                role,
                model: agent.model.clone(),
                thinking: agent.thinking.clone(),
                cwd,
                jailed: agent.jailed.unwrap_or(config.jailed.unwrap_or(false)),
            },
        );
    }
    ensure!(
        managers <= 1,
        "profile {name:?} configures more than one manager role"
    );
    Ok(Profile {
        name: name.to_owned(),
        agents,
    })
}

pub(crate) fn validate_model(value: &str) -> Result<(&str, &str)> {
    let (provider, model) = value
        .split_once('/')
        .ok_or_else(|| eyre!("model must be an exact provider/id pair"))?;
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && segment.len() <= 128
            && segment.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
            })
    };
    ensure!(
        valid_segment(provider) && !provider.contains(':') && valid_segment(model),
        "model must be an exact provider/id pair using ASCII letters, digits, '.', '_', '-', '/' or ':' in the model id"
    );
    ensure!(
        !provider.contains('/') && !model.starts_with('/') && !model.ends_with('/'),
        "model must contain one nonempty provider and model id"
    );
    Ok((provider, model))
}

fn validate_tab_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 64,
        "tab_name must contain 1..64 bytes"
    );
    ensure!(
        value.chars().all(|ch| !ch.is_control()),
        "tab_name cannot contain control characters"
    );
    Ok(())
}

fn resolve_cwd(value: &Path, config_dir: &Path) -> Result<PathBuf> {
    let path = if value.is_absolute() {
        value.to_path_buf()
    } else {
        config_dir.join(value)
    };
    let canonical = path
        .canonicalize()
        .wrap_err_with(|| format!("resolving profile cwd {}", path.display()))?;
    ensure!(
        canonical.is_dir(),
        "profile cwd is not a directory: {}",
        canonical.display()
    );
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(contents: &str) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let roles_dir = config.join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        for (name, text) in roles::DEFAULTS {
            fs::write(roles_dir.join(format!("{name}.md")), text).unwrap();
        }
        fs::write(config.join("profiles.toml"), contents).unwrap();
        let paths = Paths {
            config,
            state: dir.path().join("state"),
        };
        (dir, paths)
    }

    const VALID: &str = r#"version = 1
[profiles.team]
[profiles.team.agents.architect]
tab_name = "Coordination"
role = "architect"
model = "provider/model-1"
thinking = "high"
"#;

    #[test]
    fn seeds_a_loadable_default_crew_with_exact_models_and_one_manager() {
        let (_dir, paths) = fixture("");
        let path = paths.config.join("profiles.toml");
        fs::remove_file(&path).unwrap();
        assert!(seed_default(&paths).unwrap());
        let profile = load(&paths, "default").unwrap();
        assert_eq!(profile.agents.len(), 4);
        assert_eq!(
            profile
                .agents
                .values()
                .filter(|agent| agent.role.manager)
                .count(),
            1
        );
        for (slot, role, model) in [
            ("arch", "architect", "openai-codex/gpt-6.1-sol"),
            ("impl", "implementor", "openai-codex/gpt-6-luna"),
            ("review", "reviewer", "openai-codex/gpt-6-astra"),
            ("research", "researcher", "openai-codex/gpt-6-luna"),
        ] {
            let agent = &profile.agents[slot];
            assert_eq!(agent.tab_name, slot);
            assert_eq!(agent.role.name, role);
            assert_eq!(agent.role.manager, slot == "arch");
            assert_eq!(agent.model, model);
            assert_eq!(agent.thinking, "high");
            assert_eq!(agent.cwd, std::env::current_dir().unwrap());
            assert!(!agent.jailed);
        }
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!seed_default(&paths).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn seeding_preserves_existing_custom_bytes_and_never_follows_links() {
        let (dir, paths) = fixture("user-owned bytes, not necessarily valid TOML\r\n");
        let path = paths.config.join("profiles.toml");
        let original = fs::read(&path).unwrap();
        assert!(!seed_default(&paths).unwrap());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_file(&path).unwrap();
        let outside = dir.path().join("outside.toml");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(!seed_default(&paths).unwrap());
        assert!(!outside.exists());
        fs::write(&outside, "external sentinel").unwrap();
        assert!(!seed_default(&paths).unwrap());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "external sentinel");
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(!seed_default(&paths).unwrap());
        assert!(path.is_dir());
    }

    #[test]
    fn validates_profile_and_resolves_defaults_without_mutating_files() {
        let (dir, paths) = fixture(VALID);
        let profile = load(&paths, "team").unwrap();
        let agent = &profile.agents["architect"];
        assert_eq!(agent.role.name, "architect");
        assert!(agent.role.manager);
        assert_eq!(agent.model, "provider/model-1");
        assert_eq!(agent.thinking, "high");
        assert_eq!(agent.cwd, std::env::current_dir().unwrap());
        assert!(!agent.jailed);
        assert!(dir.path().join("config/profiles.toml").exists());
    }

    #[test]
    fn preserves_exact_nested_and_versioned_model_ids() {
        for (value, provider, model) in [
            (
                "openrouter/anthropic/claude-sonnet-4",
                "openrouter",
                "anthropic/claude-sonnet-4",
            ),
            (
                "amazon-bedrock/us.anthropic.claude-sonnet-4-20250514-v1:0",
                "amazon-bedrock",
                "us.anthropic.claude-sonnet-4-20250514-v1:0",
            ),
            (
                "custom/region/vendor/model:0",
                "custom",
                "region/vendor/model:0",
            ),
        ] {
            assert_eq!(validate_model(value).unwrap(), (provider, model));
            let (_dir, paths) = fixture(&VALID.replace("provider/model-1", value));
            assert_eq!(
                load(&paths, "team").unwrap().agents["architect"].model,
                value
            );
        }
        for value in [
            "provider/",
            "/model",
            "provider//model",
            "provider/model/",
            "provider/model\n",
            "provider/model name",
        ] {
            assert!(validate_model(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rejects_unknown_fields_and_invalid_model_or_thinking_before_launch() {
        for contents in [
            VALID.replace("version = 1", "version = 1\ncommand = 'sh'"),
            VALID.replace("provider/model-1", "fuzzy model"),
            VALID.replace("thinking = \"high\"", "thinking = \"invalid\""),
        ] {
            let (_dir, paths) = fixture(&contents);
            assert!(load(&paths, "team").is_err());
        }
    }

    #[test]
    fn rejects_multiple_manager_roles_and_duplicate_tab_names() {
        let duplicate_manager = VALID.replace(
            "[profiles.team.agents.architect]",
            "[profiles.team.agents.architect]\n",
        ) + "\n[profiles.team.agents.second]\ntab_name = \"Other\"\nrole = \"architect\"\nmodel = \"provider/model-1\"\nthinking = \"off\"\n";
        let (_dir, paths) = fixture(&duplicate_manager);
        assert!(
            load(&paths, "team")
                .unwrap_err()
                .to_string()
                .contains("more than one manager")
        );
        let duplicate_tabs = VALID.to_owned()
            + "\n[profiles.team.agents.second]\ntab_name = \"Coordination\"\nrole = \"implementor\"\nmodel = \"provider/model-1\"\nthinking = \"off\"\n";
        let (_dir, paths) = fixture(&duplicate_tabs);
        assert!(
            load(&paths, "team")
                .unwrap_err()
                .to_string()
                .contains("duplicate tab name")
        );
    }
}
