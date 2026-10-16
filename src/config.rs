//! Human-editable configuration. Durations are seconds; unknown keys are errors.

use std::fs;

use color_eyre::eyre::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::paths::{Paths, check_regular, create_file, private_dir};

pub(crate) const DEFAULT_CONFIG: &str = r#"# mustr configuration, version 1
version = 1

[scheduling]
# Status requests only apply to active tasks; heartbeats never invoke a model.
checkin_seconds = 1200
continuation_seconds = 30
max_nudges = 3

[ui]
# Uses terminal default foreground/background plus ANSI palette accents.
# Set false (or NO_COLOR) for monochrome.
color = true
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub version: u32,
    pub scheduling: Scheduling,
    pub ui: Ui,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Scheduling {
    pub checkin_seconds: u64,
    pub continuation_seconds: u64,
    pub max_nudges: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Ui {
    pub color: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            scheduling: Scheduling::default(),
            ui: Ui::default(),
        }
    }
}
impl Default for Scheduling {
    fn default() -> Self {
        Self {
            checkin_seconds: 1200,
            continuation_seconds: 30,
            max_nudges: 3,
        }
    }
}
impl Default for Ui {
    fn default() -> Self {
        Self { color: true }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config.join("config.toml");
        check_regular(&path)?;
        let config: Self = toml::from_str(&fs::read_to_string(&path)?)
            .wrap_err_with(|| format!("parsing {}", path.display()))?;
        ensure!(
            config.version == 1,
            "unsupported configuration version {}",
            config.version
        );
        ensure!(
            (5..=86400).contains(&config.scheduling.checkin_seconds),
            "checkin_seconds must be 5..86400"
        );
        ensure!(
            (1..=3600).contains(&config.scheduling.continuation_seconds),
            "continuation_seconds must be 1..3600"
        );
        ensure!(
            (1..=20).contains(&config.scheduling.max_nudges),
            "max_nudges must be 1..20"
        );
        Ok(config)
    }
}

/// Only initial setup seeds roles. Removing a role later is respected.
pub(crate) fn initialize(paths: &Paths) -> Result<()> {
    private_dir(&paths.config)?;
    private_dir(&paths.roles())?;
    if create_file(&paths.config.join("config.toml"), DEFAULT_CONFIG.as_bytes())? {
        for (name, text) in crate::roles::DEFAULTS {
            create_file(&paths.roles().join(format!("{name}.md")), text.as_bytes())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_preserves_edits_and_deleted_roles() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        fs::write(paths.roles().join("architect.md"), "custom").unwrap();
        fs::remove_file(paths.roles().join("reviewer.md")).unwrap();
        initialize(&paths).unwrap();
        assert_eq!(
            fs::read_to_string(paths.roles().join("architect.md")).unwrap(),
            "custom"
        );
        assert!(!paths.roles().join("reviewer.md").exists());
        assert_eq!(Config::load(&paths).unwrap().version, 1);
    }

    #[test]
    fn rejects_unknown_config_keys() {
        assert!(toml::from_str::<Config>("versoin = 1").is_err());
    }
}
