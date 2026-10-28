//! Strict, read-only named-profile parsing and role preflight.

use std::{collections::BTreeMap, fs};

use color_eyre::eyre::{Context, Result, ensure};
use serde::Deserialize;

use crate::{
    paths::{Paths, check_regular},
    roles::{self, Role},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilesFile {
    profiles: BTreeMap<String, ProfileEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileEntry {
    roles: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct Profile {
    pub name: String,
    pub roles: Vec<Role>,
}

pub(crate) fn load(paths: &Paths, name: &str) -> Result<Profile> {
    ensure!(!name.trim().is_empty(), "--profile name must not be blank");

    let profile_path = paths.profiles();
    check_regular(&profile_path)
        .wrap_err_with(|| format!("loading profile file {}", profile_path.display()))?;
    let source = fs::read_to_string(&profile_path)
        .wrap_err_with(|| format!("reading profile file {}", profile_path.display()))?;
    let profiles: ProfilesFile = toml::from_str(&source)
        .wrap_err_with(|| format!("parsing profile file {}", profile_path.display()))?;
    let entry = profiles
        .profiles
        .get(name)
        .ok_or_else(|| color_eyre::eyre::eyre!("unknown profile `{name}`"))?;
    ensure!(
        !entry.roles.is_empty(),
        "profile `{name}` must list at least one role"
    );

    let role_directory = paths.roles();
    let mut resolved_roles = Vec::with_capacity(entry.roles.len());
    let mut errors = Vec::new();
    for role_name in &entry.roles {
        match roles::resolve(&role_directory, role_name) {
            Ok(role) => resolved_roles.push(role),
            Err(error) => errors.push(format!("{role_name}: {error:#}")),
        }
    }

    let manager_count = resolved_roles.iter().filter(|role| role.manager).count();
    if manager_count > 1 {
        errors.push(format!(
            "profile `{name}` resolves {manager_count} manager entries; only one manager role is allowed"
        ));
    }
    ensure!(
        errors.is_empty(),
        "profile `{name}` failed role preflight: {}",
        errors.join("; ")
    );

    Ok(Profile {
        name: name.to_owned(),
        roles: resolved_roles,
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use tempfile::{TempDir, tempdir};

    use crate::{paths::Paths, profile::load};

    fn test_paths() -> (TempDir, Paths) {
        let directory = tempdir().unwrap();
        let paths = Paths {
            config: directory.path().join("config"),
            state: directory.path().join("state"),
        };
        fs::create_dir_all(paths.roles()).unwrap();
        (directory, paths)
    }

    fn write_profile(paths: &Paths, contents: &str) {
        fs::write(paths.profiles(), contents).unwrap();
    }

    fn write_role(paths: &Paths, name: &str, contents: &str) {
        fs::write(paths.roles().join(format!("{name}.md")), contents).unwrap();
    }

    fn role(name: &str) -> String {
        format!("+++\ndescription = \"{name}\"\n+++\n{name} prompt.\n")
    }

    #[test]
    fn resolves_the_selected_literal_profile_in_order_and_keeps_repeated_roles() {
        let (_directory, paths) = test_paths();
        write_role(
            &paths,
            "manager",
            "+++\nmanager = true\n+++\nManager prompt.\n",
        );
        write_role(&paths, "reviewer", &role("reviewer"));
        write_role(&paths, "researcher", &role("researcher"));
        write_profile(
            &paths,
            "[profiles.\"team/../debug\"]\nroles = [\"manager\", \"researcher\", \"reviewer\", \"researcher\"]\n\n[profiles.unrelated]\nroles = [\"missing-role\"]\n",
        );

        let profile = load(&paths, "team/../debug").unwrap();
        assert_eq!(profile.name, "team/../debug");
        assert_eq!(
            profile
                .roles
                .iter()
                .map(|role| role.name.as_str())
                .collect::<Vec<_>>(),
            ["manager", "researcher", "reviewer", "researcher"]
        );
        assert_eq!(
            profile
                .roles
                .iter()
                .map(|role| role.manager)
                .collect::<Vec<_>>(),
            [true, false, false, false]
        );
        assert!(load(&paths, "Team/../debug").is_err());
    }

    #[test]
    fn rejects_blank_unknown_and_missing_profiles_without_creating_the_file() {
        let (_directory, paths) = test_paths();
        let profile_path = paths.profiles();

        assert!(load(&paths, "   ").is_err());
        assert!(load(&paths, "missing").is_err());
        assert!(!profile_path.exists());

        write_profile(&paths, "[profiles.other]\nroles = [\"reviewer\"]\n");
        assert!(load(&paths, "missing").is_err());
    }

    #[test]
    fn rejects_malformed_toml_unknown_fields_and_empty_role_lists() {
        let cases = [
            ("profiles = [", "feature"),
            ("[profiles.feature]\nroles = []\n", "feature"),
        ];

        for (contents, name) in cases {
            let (_directory, paths) = test_paths();
            write_profile(&paths, contents);
            assert!(
                load(&paths, name).is_err(),
                "accepted profile TOML: {contents}"
            );
        }
    }

    #[test]
    fn rejects_unknown_top_level_fields_with_otherwise_valid_profile_data() {
        let (_directory, paths) = test_paths();
        write_role(&paths, "reviewer", &role("reviewer"));
        write_profile(
            &paths,
            "[profiles.feature]\nroles = [\"reviewer\"]\n[unexpected]\nvalue = true\n",
        );

        let error = load(&paths, "feature").unwrap_err();
        assert!(format!("{error:#}").contains("unexpected"));
    }

    #[test]
    fn rejects_unknown_selected_profile_fields_with_valid_role_data() {
        let (_directory, paths) = test_paths();
        write_role(&paths, "reviewer", &role("reviewer"));
        write_profile(
            &paths,
            "[profiles.feature]\nroles = [\"reviewer\"]\nunexpected = true\n",
        );

        let error = load(&paths, "feature").unwrap_err();
        assert!(format!("{error:#}").contains("unexpected"));
    }

    #[test]
    fn reports_all_selected_role_graph_errors_and_ignores_unselected_graphs() {
        let (_directory, paths) = test_paths();
        write_profile(
            &paths,
            "[profiles.feature]\nroles = [\"missing-one\", \"missing-two\"]\n[profiles.unrelated]\nroles = [\"missing-three\"]\n",
        );

        let error = load(&paths, "feature").unwrap_err().to_string();
        assert!(error.contains("missing-one"));
        assert!(error.contains("missing-two"));
        assert!(!error.contains("missing-three"));
    }

    #[test]
    fn rejects_invalid_role_graphs_and_more_than_one_manager_entry() {
        let (_directory, paths) = test_paths();
        write_role(
            &paths,
            "broken",
            "+++\ncompose = [\"missing\"]\n+++\nBroken role.\n",
        );
        write_role(&paths, "manager-one", "+++\nmanager = true\n+++\nOne.\n");
        write_role(&paths, "manager-two", "+++\nmanager = true\n+++\nTwo.\n");
        write_profile(
            &paths,
            "[profiles.broken]\nroles = [\"broken\"]\n[profiles.repeated]\nroles = [\"manager-one\", \"manager-one\"]\n[profiles.multiple]\nroles = [\"manager-one\", \"manager-two\"]\n",
        );

        assert!(
            load(&paths, "broken")
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
        assert!(
            load(&paths, "repeated")
                .unwrap_err()
                .to_string()
                .contains("only one manager role")
        );
        assert!(
            load(&paths, "multiple")
                .unwrap_err()
                .to_string()
                .contains("only one manager role")
        );
    }

    #[test]
    fn loading_a_profile_does_not_change_configuration_or_profile_files() {
        let (_directory, paths) = test_paths();
        write_role(&paths, "reviewer", &role("reviewer"));
        write_profile(&paths, "[profiles.feature]\nroles = [\"reviewer\"]\n");
        let config_path = paths.config.join("config.toml");
        let config_before = b"version = 1\n[ui]\ncolor = false\n";
        fs::write(&config_path, config_before).unwrap();
        let profile_before = fs::read(paths.profiles()).unwrap();

        assert!(load(&paths, "feature").is_ok());

        assert_eq!(fs::read(config_path).unwrap(), config_before);
        assert_eq!(fs::read(paths.profiles()).unwrap(), profile_before);
    }

    #[test]
    fn rejects_a_symlinked_profile_file() {
        let (directory, paths) = test_paths();
        let target = directory.path().join("profiles-target.toml");
        fs::write(&target, "[profiles.feature]\nroles = [\"reviewer\"]\n").unwrap();
        symlink(&target, paths.profiles()).unwrap();

        assert!(load(&paths, "feature").is_err());
    }

    #[test]
    fn profiles_path_stays_under_the_resolved_configuration_directory() {
        let (_directory, paths) = test_paths();
        assert_eq!(paths.profiles(), paths.config.join("profiles.toml"));
        assert!(paths.profiles().starts_with(&paths.config));
    }
}
