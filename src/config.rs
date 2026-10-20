//! Human-editable configuration. Durations are seconds; unknown keys are errors.

use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::paths::{
    Paths, check_regular, create_file, create_file_atomically, digest, private_dir, replace_file,
};

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

/// Implicit setup seeds roles only when it creates the initial configuration.
pub(crate) fn initialize(paths: &Paths) -> Result<()> {
    private_dir(&paths.config)?;
    private_dir(&paths.roles())?;
    if create_file(&paths.config.join("config.toml"), DEFAULT_CONFIG.as_bytes())? {
        seed_roles(paths)?;
    }
    Ok(())
}

/// Create missing shipped role files without replacing user-owned content.
pub(crate) fn seed_roles(paths: &Paths) -> Result<()> {
    private_dir(&paths.roles())?;
    for (name, text) in crate::roles::DEFAULTS {
        create_file(&paths.roles().join(format!("{name}.md")), text.as_bytes())?;
    }
    Ok(())
}

pub(crate) struct RoleRefreshPlan {
    trusted: TrustedDirectories,
    updates: Vec<RoleRefreshUpdate>,
    unchanged: usize,
}

struct RoleRefreshUpdate {
    path: PathBuf,
    contents: &'static str,
    previous: Option<Vec<u8>>,
    backup: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone)]
struct TrustedDirectory {
    path: PathBuf,
    identity: DirectoryIdentity,
}

#[derive(Clone)]
enum BackupDirectoryState {
    Absent(PathBuf),
    Present(TrustedDirectory),
}

struct TrustedDirectories {
    config: TrustedDirectory,
    roles: TrustedDirectory,
    backup: Option<BackupDirectoryState>,
}

#[derive(Debug)]
pub(crate) struct RoleRefreshReport {
    pub roles_dir: PathBuf,
    pub created: Vec<PathBuf>,
    pub replaced: Vec<RoleReplacement>,
    pub unchanged: usize,
}

#[derive(Debug)]
pub(crate) struct RoleReplacement {
    pub path: PathBuf,
    pub backup: PathBuf,
}

impl RoleRefreshReport {
    pub fn changed_count(&self) -> usize {
        self.created.len() + self.replaced.len()
    }

    fn completed_changes(&self) -> String {
        let mut changes = Vec::new();
        changes.extend(
            self.created
                .iter()
                .map(|path| format!("created {}", path.display())),
        );
        changes.extend(self.replaced.iter().map(|change| {
            format!(
                "updated {} (backup: {})",
                change.path.display(),
                change.backup.display()
            )
        }));
        if changes.is_empty() {
            format!("no files changed; {} already matched", self.unchanged)
        } else {
            changes.join("; ")
        }
    }
}

/// Validate all shipped-role destinations and deterministic backups without changing role files.
pub(crate) fn prepare_role_refresh(paths: &Paths) -> Result<RoleRefreshPlan> {
    private_dir(&paths.config)?;
    let roles_dir = paths.roles();
    private_dir(&roles_dir)?;
    let backup_dir = paths.config.join("role-backups");
    let mut trusted = TrustedDirectories::capture(&paths.config, &roles_dir)?;
    let mut updates = Vec::new();
    let mut unchanged = 0;

    for (name, contents) in crate::roles::DEFAULTS {
        let path = roles_dir.join(format!("{name}.md"));
        let Some(previous) = trusted.read_role_file(&path)? else {
            updates.push(RoleRefreshUpdate {
                path,
                contents,
                previous: None,
                backup: None,
            });
            continue;
        };
        if previous == contents.as_bytes() {
            unchanged += 1;
            continue;
        }
        updates.push(RoleRefreshUpdate {
            path,
            contents,
            backup: Some(role_backup_path(&backup_dir, name, &previous)),
            previous: Some(previous),
        });
    }

    if updates.iter().any(|update| update.backup.is_some()) {
        trusted.capture_backup_directory(&backup_dir)?;
        for update in updates.iter().filter(|update| update.backup.is_some()) {
            let backup = update.backup.as_ref().expect("filtered backup path");
            if let Some(existing) = trusted.read_backup_file(backup)? {
                ensure!(
                    Some(existing.as_slice()) == update.previous.as_deref(),
                    "role backup collision or changed backup; refusing to overwrite {}",
                    backup.display()
                );
            }
        }
    }
    trusted.ensure_all()?;

    Ok(RoleRefreshPlan {
        trusted,
        updates,
        unchanged,
    })
}

/// Apply a preflighted refresh one file at a time; failures report completed changes and do not roll back.
pub(crate) fn refresh_shipped_roles(plan: RoleRefreshPlan) -> Result<RoleRefreshReport> {
    let RoleRefreshPlan {
        mut trusted,
        updates,
        unchanged,
    } = plan;
    let mut report = RoleRefreshReport {
        roles_dir: trusted.roles.path.clone(),
        created: Vec::new(),
        replaced: Vec::new(),
        unchanged,
    };
    trusted.ensure_all().wrap_err(
        "trusted config, roles, or backup directory changed after installation; no role-refresh files were modified",
    )?;
    if updates.iter().any(|update| update.backup.is_some()) {
        trusted
            .prepare_backup_directory_for_write()
            .wrap_err("could not secure role-backups before changing any role file")?;
    }

    for update in &updates {
        if let Err(error) = apply_role_refresh(update, &mut trusted) {
            let current = match &update.backup {
                Some(backup) => format!(
                    "while updating {} (original backup path: {})",
                    update.path.display(),
                    backup.display()
                ),
                None => format!("while creating {}", update.path.display()),
            };
            return Err(error).wrap_err_with(|| {
                format!(
                    "shipped-role refresh stopped {current}; completed earlier changes: {}. Inspect the current target and any backup before retrying; no rollback was attempted",
                    report.completed_changes()
                )
            });
        }
        match (&update.previous, &update.backup) {
            (None, _) => report.created.push(update.path.clone()),
            (Some(_), Some(backup)) => report.replaced.push(RoleReplacement {
                path: update.path.clone(),
                backup: backup.clone(),
            }),
            (Some(_), None) => unreachable!("changed roles have a preflighted backup"),
        }
    }
    Ok(report)
}

fn apply_role_refresh(update: &RoleRefreshUpdate, trusted: &mut TrustedDirectories) -> Result<()> {
    trusted.ensure_all()?;
    match &update.previous {
        None => {
            ensure!(
                trusted.read_role_file(&update.path)?.is_none(),
                "role file appeared after refresh preflight; refusing to overwrite {}",
                update.path.display()
            );
            trusted.ensure_all()?;
            ensure!(
                create_file_atomically(&update.path, update.contents.as_bytes())?,
                "role file appeared after refresh preflight; refusing to overwrite {}",
                update.path.display()
            );
        }
        Some(previous) => {
            let current = trusted.read_role_file(&update.path)?;
            ensure!(
                current.as_deref() == Some(previous.as_slice()),
                "role file changed after refresh preflight; refusing to overwrite {}",
                update.path.display()
            );
            let backup = update.backup.as_ref().expect("preflighted backup path");
            trusted.ensure_all()?;
            if !create_file_atomically(backup, previous)? {
                let existing = trusted.read_backup_file(backup)?;
                ensure!(
                    existing.as_deref() == Some(previous.as_slice()),
                    "role backup collision or changed backup; refusing to overwrite {}",
                    backup.display()
                );
            }
            ensure!(
                trusted.read_backup_file(backup)?.as_deref() == Some(previous.as_slice()),
                "role backup contents do not match; refusing to replace {}",
                update.path.display()
            );
            ensure!(
                trusted.read_role_file(&update.path)?.as_deref() == Some(previous.as_slice()),
                "role file changed before replacement; refusing to overwrite {}",
                update.path.display()
            );
            trusted.ensure_all()?;
            replace_file(&update.path, update.contents.as_bytes()).wrap_err_with(|| {
                format!(
                    "replacing {}; inspect its original backup at {}",
                    update.path.display(),
                    backup.display()
                )
            })?;
        }
    }

    trusted.ensure_all()?;
    ensure!(
        trusted.read_role_file(&update.path)?.as_deref() == Some(update.contents.as_bytes()),
        "refreshed role contents do not match {}",
        update.path.display()
    );
    Ok(())
}

impl TrustedDirectories {
    fn capture(config_path: &Path, roles_path: &Path) -> Result<Self> {
        let trusted = Self {
            config: TrustedDirectory::capture(config_path)?,
            roles: TrustedDirectory::capture(roles_path)?,
            backup: None,
        };
        trusted.ensure_role_directories()?;
        Ok(trusted)
    }

    fn capture_backup_directory(&mut self, path: &Path) -> Result<()> {
        self.ensure_role_directories()?;
        ensure!(
            path.parent() == Some(self.config.path.as_path()),
            "role-backups must be a direct child of the trusted config directory"
        );
        let state = match fs::symlink_metadata(path) {
            Ok(_) => BackupDirectoryState::Present(TrustedDirectory::capture(path)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                BackupDirectoryState::Absent(path.to_path_buf())
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| format!("reading {}", path.display()));
            }
        };
        self.backup = Some(state);
        self.ensure_all()
    }

    fn ensure_role_directories(&self) -> Result<()> {
        self.config.ensure_current()?;
        self.roles.ensure_current()?;
        Ok(())
    }

    fn ensure_backup_directory(&self) -> Result<()> {
        match &self.backup {
            None => Ok(()),
            Some(BackupDirectoryState::Absent(path)) => ensure_directory_absent(path),
            Some(BackupDirectoryState::Present(directory)) => directory.ensure_current(),
        }
    }

    fn ensure_all(&self) -> Result<()> {
        self.ensure_role_directories()?;
        self.ensure_backup_directory()
    }

    fn read_role_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        ensure!(
            path.parent() == Some(self.roles.path.as_path()),
            "role destination is outside the trusted roles directory: {}",
            path.display()
        );
        self.ensure_all()?;
        let contents = read_regular_if_present(path)?;
        self.ensure_all()?;
        Ok(contents)
    }

    fn read_backup_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let backup_directory = match self.backup.as_ref() {
            Some(BackupDirectoryState::Absent(path)) => path,
            Some(BackupDirectoryState::Present(directory)) => &directory.path,
            None => bail!("role backup directory was not included in refresh preflight"),
        };
        ensure!(
            path.parent() == Some(backup_directory.as_path()),
            "role backup is outside the trusted backup directory: {}",
            path.display()
        );
        self.ensure_all()?;
        let contents = read_regular_if_present(path)?;
        self.ensure_all()?;
        Ok(contents)
    }

    fn prepare_backup_directory_for_write(&mut self) -> Result<()> {
        self.ensure_role_directories()?;
        let Some(state) = self.backup.clone() else {
            bail!("role backup directory was not included in refresh preflight");
        };
        match state {
            BackupDirectoryState::Absent(path) => {
                ensure_directory_absent(&path)?;
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&path)
                    .wrap_err_with(|| format!("creating {}", path.display()))?;
                self.backup = Some(BackupDirectoryState::Present(TrustedDirectory::capture(
                    &path,
                )?));
            }
            BackupDirectoryState::Present(directory) => {
                directory.ensure_current()?;
                fs::set_permissions(&directory.path, fs::Permissions::from_mode(0o700))
                    .wrap_err_with(|| format!("securing {}", directory.path.display()))?;
                directory.ensure_current()?;
            }
        }
        self.ensure_all()
    }
}

impl TrustedDirectory {
    fn capture(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            identity: read_directory_identity(path)?,
        })
    }

    fn ensure_current(&self) -> Result<()> {
        let current = read_directory_identity(&self.path)?;
        ensure!(
            current == self.identity,
            "trusted directory identity changed after refresh preflight: {}",
            self.path.display()
        );
        Ok(())
    }
}

fn read_directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    let metadata = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("reading trusted directory {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "expected a real directory, not a link: {}",
        path.display()
    );
    ensure!(
        metadata.uid() == rustix::process::getuid().as_raw(),
        "directory is not owned by this user: {}",
        path.display()
    );
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn ensure_directory_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!(
            "role-backups directory appeared or changed after refresh preflight: {}",
            path.display()
        ),
        Err(error) => Err(error).wrap_err_with(|| format!("reading {}", path.display())),
    }
}

fn read_regular_if_present(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "expected a regular file, not a link: {}",
                path.display()
            );
            check_regular(path)?;
            Ok(Some(
                fs::read(path).wrap_err_with(|| format!("reading {}", path.display()))?,
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).wrap_err_with(|| format!("reading {}", path.display())),
    }
}

fn role_backup_path(backup_dir: &Path, name: &str, previous: &[u8]) -> PathBuf {
    backup_dir.join(format!("{name}.md.{}.bak", digest(previous)))
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
    fn fresh_setup_seeds_base_without_overwriting_existing_role_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        private_dir(&paths.config).unwrap();
        private_dir(&paths.roles()).unwrap();
        let customized = "+++\nextends = \"implementor\"\n+++\nKeep my prototype role.";
        fs::write(paths.roles().join("reviewer.md"), customized).unwrap();
        initialize(&paths).unwrap();
        assert_eq!(
            fs::read_to_string(paths.roles().join("base.md")).unwrap(),
            include_str!("../roles/base.md")
        );
        assert_eq!(
            fs::read_to_string(paths.roles().join("reviewer.md")).unwrap(),
            customized
        );
        fs::write(paths.roles().join("base.md"), "My shared instructions.").unwrap();
        initialize(&paths).unwrap();
        assert_eq!(
            fs::read_to_string(paths.roles().join("base.md")).unwrap(),
            "My shared instructions."
        );
        fs::remove_file(paths.roles().join("base.md")).unwrap();
        initialize(&paths).unwrap();
        assert!(!paths.roles().join("base.md").exists());
    }

    #[test]
    fn existing_configuration_does_not_implicitly_migrate_roles() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        private_dir(&paths.config).unwrap();
        fs::write(paths.config.join("config.toml"), DEFAULT_CONFIG).unwrap();
        initialize(&paths).unwrap();
        assert_eq!(fs::read_dir(paths.roles()).unwrap().count(), 0);
    }

    #[test]
    fn refresh_rejects_roles_parent_symlink_substitutions_after_preflight() {
        for redirect_to_saved in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths {
                config: dir.path().join("config"),
                state: dir.path().join("state"),
            };
            initialize(&paths).unwrap();
            let roles = paths.roles();
            let architect = roles.join("architect.md");
            let original = b"role bytes captured before directory substitution\n";
            fs::write(&architect, original).unwrap();
            let plan = prepare_role_refresh(&paths).unwrap();

            let saved_roles = paths.config.join("roles-saved");
            fs::rename(&roles, &saved_roles).unwrap();
            let redirected_roles = if redirect_to_saved {
                saved_roles.clone()
            } else {
                let redirected = dir.path().join("redirected-roles");
                fs::create_dir(&redirected).unwrap();
                fs::write(redirected.join("architect.md"), original).unwrap();
                redirected
            };
            std::os::unix::fs::symlink(&redirected_roles, &roles).unwrap();

            assert!(refresh_shipped_roles(plan).is_err());
            assert_eq!(
                fs::read(saved_roles.join("architect.md")).unwrap(),
                original
            );
            assert_eq!(
                fs::read(redirected_roles.join("architect.md")).unwrap(),
                original
            );
            assert!(!paths.config.join("role-backups").exists());
        }
    }

    #[test]
    fn refresh_rejects_roles_directory_identity_replacement_after_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let roles = paths.roles();
        let architect = roles.join("architect.md");
        let original = b"role bytes captured before directory replacement\n";
        fs::write(&architect, original).unwrap();
        let plan = prepare_role_refresh(&paths).unwrap();

        let saved_roles = paths.config.join("roles-saved");
        fs::rename(&roles, &saved_roles).unwrap();
        fs::create_dir(&roles).unwrap();
        fs::write(roles.join("architect.md"), original).unwrap();

        assert!(refresh_shipped_roles(plan).is_err());
        assert_eq!(
            fs::read(saved_roles.join("architect.md")).unwrap(),
            original
        );
        assert_eq!(fs::read(roles.join("architect.md")).unwrap(), original);
        assert!(!paths.config.join("role-backups").exists());
    }

    #[test]
    fn refresh_rejects_config_directory_symlink_substitution_after_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"role bytes captured before config substitution\n";
        fs::write(&architect, original).unwrap();
        let plan = prepare_role_refresh(&paths).unwrap();

        let saved_config = dir.path().join("config-saved");
        fs::rename(&paths.config, &saved_config).unwrap();
        std::os::unix::fs::symlink(&saved_config, &paths.config).unwrap();

        assert!(refresh_shipped_roles(plan).is_err());
        assert_eq!(
            fs::read(saved_config.join("roles/architect.md")).unwrap(),
            original
        );
        assert!(!saved_config.join("role-backups").exists());
    }

    #[test]
    fn refresh_rejects_backup_directory_substitution_after_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"role bytes captured before backup substitution\n";
        fs::write(&architect, original).unwrap();
        let backup_dir = paths.config.join("role-backups");
        fs::create_dir(&backup_dir).unwrap();
        let plan = prepare_role_refresh(&paths).unwrap();
        let backup = plan
            .updates
            .iter()
            .find(|update| update.path == architect)
            .unwrap()
            .backup
            .clone()
            .unwrap();

        let saved_backup_dir = paths.config.join("role-backups-saved");
        fs::rename(&backup_dir, &saved_backup_dir).unwrap();
        let redirected_backup_dir = dir.path().join("redirected-backups");
        fs::create_dir(&redirected_backup_dir).unwrap();
        std::os::unix::fs::symlink(&redirected_backup_dir, &backup_dir).unwrap();

        assert!(refresh_shipped_roles(plan).is_err());
        assert_eq!(fs::read(&architect).unwrap(), original);
        assert!(!saved_backup_dir.join(backup.file_name().unwrap()).exists());
        assert!(
            !redirected_backup_dir
                .join(backup.file_name().unwrap())
                .exists()
        );
    }

    #[test]
    fn refresh_rejects_backup_directory_appearing_after_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"role bytes captured before backup path creation\n";
        fs::write(&architect, original).unwrap();
        let plan = prepare_role_refresh(&paths).unwrap();
        fs::create_dir(paths.config.join("role-backups")).unwrap();

        assert!(refresh_shipped_roles(plan).is_err());
        assert_eq!(fs::read(&architect).unwrap(), original);
        assert_eq!(
            fs::read_dir(paths.config.join("role-backups"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn refresh_shipped_roles_backs_up_exact_bytes_and_is_idempotent() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let config_path = paths.config.join("config.toml");
        let config_before = fs::read(&config_path).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"hand-edited role bytes\r\nexactly preserved\n";
        fs::write(&architect, original).unwrap();
        let missing = paths.roles().join("reviewer.md");
        fs::remove_file(&missing).unwrap();
        let custom_role = paths.roles().join("custom.md");
        fs::write(&custom_role, "custom role stays\n").unwrap();
        private_dir(&paths.state).unwrap();
        let history = paths.state.join("history.sqlite");
        fs::write(&history, b"state sentinel").unwrap();

        let plan = prepare_role_refresh(&paths).unwrap();
        assert!(!paths.config.join("role-backups").exists());
        let report = refresh_shipped_roles(plan).unwrap();
        assert_eq!(report.created, vec![missing.clone()]);
        assert_eq!(report.replaced.len(), 1);
        assert_eq!(report.replaced[0].path, architect);
        assert_eq!(
            report.replaced[0].backup,
            role_backup_path(&paths.config.join("role-backups"), "architect", original)
        );
        assert_eq!(fs::read(&report.replaced[0].backup).unwrap(), original);
        assert_eq!(
            fs::read(&architect).unwrap(),
            include_str!("../roles/architect.md").as_bytes()
        );
        assert_eq!(
            fs::read(&missing).unwrap(),
            include_str!("../roles/reviewer.md").as_bytes()
        );
        assert_eq!(fs::read(custom_role).unwrap(), b"custom role stays\n");
        assert_eq!(fs::read(&config_path).unwrap(), config_before);
        assert_eq!(fs::read(history).unwrap(), b"state sentinel");
        assert_eq!(report.unchanged, 3);

        let role_metadata = fs::metadata(&architect).unwrap();
        let backup_count = fs::read_dir(paths.config.join("role-backups"))
            .unwrap()
            .count();
        let repeated = refresh_shipped_roles(prepare_role_refresh(&paths).unwrap()).unwrap();
        let repeated_metadata = fs::metadata(&architect).unwrap();
        assert_eq!(repeated.changed_count(), 0);
        assert_eq!(repeated.unchanged, crate::roles::DEFAULTS.len());
        assert_eq!(repeated_metadata.ino(), role_metadata.ino());
        assert_eq!(repeated_metadata.mtime(), role_metadata.mtime());
        assert_eq!(
            fs::read_dir(paths.config.join("role-backups"))
                .unwrap()
                .count(),
            backup_count
        );
    }

    #[test]
    fn role_refresh_rejects_unsafe_role_and_backup_destinations() {
        for unsafe_destination in ["symlink", "directory"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths {
                config: dir.path().join("config"),
                state: dir.path().join("state"),
            };
            initialize(&paths).unwrap();
            let architect = paths.roles().join("architect.md");
            fs::remove_file(&architect).unwrap();
            let target = dir.path().join("outside-role.md");
            fs::write(&target, b"outside sentinel").unwrap();
            if unsafe_destination == "symlink" {
                std::os::unix::fs::symlink(&target, &architect).unwrap();
            } else {
                fs::create_dir(&architect).unwrap();
            }
            assert!(prepare_role_refresh(&paths).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"outside sentinel");
        }

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"custom architect\n";
        fs::write(&architect, original).unwrap();
        let backup_dir = paths.config.join("role-backups");
        fs::write(&backup_dir, b"not a directory").unwrap();
        assert!(prepare_role_refresh(&paths).is_err());
        assert_eq!(fs::read(&architect).unwrap(), original);
        assert_eq!(fs::read(&backup_dir).unwrap(), b"not a directory");

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"symlink backup guard\n";
        fs::write(&architect, original).unwrap();
        let external_backup_dir = dir.path().join("external-backups");
        fs::create_dir(&external_backup_dir).unwrap();
        std::os::unix::fs::symlink(&external_backup_dir, paths.config.join("role-backups"))
            .unwrap();
        assert!(prepare_role_refresh(&paths).is_err());
        assert_eq!(fs::read(&architect).unwrap(), original);
    }

    #[test]
    fn role_refresh_reuses_an_exact_existing_backup_without_rewriting_it() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"original retained before a prior interrupted refresh\n";
        fs::write(&architect, original).unwrap();
        let backup_dir = paths.config.join("role-backups");
        fs::create_dir(&backup_dir).unwrap();
        let backup = role_backup_path(&backup_dir, "architect", original);
        fs::write(&backup, original).unwrap();
        let backup_inode = fs::metadata(&backup).unwrap().ino();

        let report = refresh_shipped_roles(prepare_role_refresh(&paths).unwrap()).unwrap();
        assert_eq!(report.replaced.len(), 1);
        assert_eq!(report.replaced[0].backup, backup);
        assert_eq!(fs::metadata(&backup).unwrap().ino(), backup_inode);
        assert_eq!(fs::read(&backup).unwrap(), original);
        assert_eq!(
            fs::read(&architect).unwrap(),
            include_str!("../roles/architect.md").as_bytes()
        );
    }

    #[test]
    fn role_refresh_refuses_backup_collision_and_preserves_original() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"collision source bytes\n";
        fs::write(&architect, original).unwrap();
        let backup_dir = paths.config.join("role-backups");
        fs::create_dir(&backup_dir).unwrap();
        let backup = role_backup_path(&backup_dir, "architect", original);
        fs::write(&backup, b"different backup content").unwrap();

        assert!(prepare_role_refresh(&paths).is_err());
        assert_eq!(fs::read(&architect).unwrap(), original);
        assert_eq!(fs::read(backup).unwrap(), b"different backup content");
    }

    #[test]
    fn backup_creation_failure_does_not_replace_original_role() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let original = b"backup creation failure sentinel\n";
        fs::write(&architect, original).unwrap();
        fs::create_dir(paths.config.join("role-backups")).unwrap();
        let plan = prepare_role_refresh(&paths).unwrap();
        let backup = plan
            .updates
            .iter()
            .find(|update| update.path == architect)
            .unwrap()
            .backup
            .clone()
            .unwrap();
        fs::create_dir_all(backup.parent().unwrap()).unwrap();
        fs::create_dir(&backup).unwrap();

        let error = refresh_shipped_roles(plan).unwrap_err().to_string();
        assert!(error.contains("role refresh stopped"));
        assert_eq!(fs::read(&architect).unwrap(), original);
        assert!(fs::metadata(backup).unwrap().is_dir());
    }

    #[test]
    fn partial_refresh_error_reports_completed_paths_and_backup_locations() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config"),
            state: dir.path().join("state"),
        };
        initialize(&paths).unwrap();
        let architect = paths.roles().join("architect.md");
        let architect_original = b"architect original bytes\n";
        fs::write(&architect, architect_original).unwrap();
        let base = paths.roles().join("base.md");
        let base_original = b"base original bytes\n";
        fs::write(&base, base_original).unwrap();
        fs::create_dir(paths.config.join("role-backups")).unwrap();

        let plan = prepare_role_refresh(&paths).unwrap();
        let base_backup = plan
            .updates
            .iter()
            .find(|update| update.path == base)
            .unwrap()
            .backup
            .clone()
            .unwrap();
        let architect_backup = plan
            .updates
            .iter()
            .find(|update| update.path == architect)
            .unwrap()
            .backup
            .clone()
            .unwrap();
        fs::create_dir_all(base_backup.parent().unwrap()).unwrap();
        fs::create_dir(&base_backup).unwrap();

        let error = refresh_shipped_roles(plan).unwrap_err().to_string();
        assert!(error.contains("completed earlier changes"));
        assert!(error.contains(&architect.display().to_string()));
        assert!(error.contains(&architect_backup.display().to_string()));
        assert!(error.contains(&base.display().to_string()));
        assert!(error.contains(&base_backup.display().to_string()));
        assert_eq!(
            fs::read(&architect).unwrap(),
            include_str!("../roles/architect.md").as_bytes()
        );
        assert_eq!(fs::read(&architect_backup).unwrap(), architect_original);
        assert_eq!(fs::read(&base).unwrap(), base_original);
    }

    #[test]
    fn rejects_unknown_config_keys() {
        assert!(toml::from_str::<Config>("versoin = 1").is_err());
    }
}
