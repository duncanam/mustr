//! Private configuration/state/runtime paths. No repository-local discovery.

use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn discover(config: Option<PathBuf>) -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| color_eyre::eyre::eyre!("HOME is not set"))?;
        let config = config.unwrap_or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("mustr")
        });
        let state = env::var_os("MUSTR_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/state"))
                    .join("mustr")
            });
        ensure!(
            config.is_absolute() && state.is_absolute(),
            "configuration and state paths must be absolute"
        );
        Ok(Self { config, state })
    }

    pub fn roles(&self) -> PathBuf {
        self.config.join("roles")
    }
}

pub(crate) fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .wrap_err_with(|| format!("creating {}", path.display()))?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "expected a real directory: {}",
        path.display()
    );
    ensure!(
        meta.uid() == rustix::process::getuid().as_raw(),
        "directory is not owned by this user: {}",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Create, never truncate. Initial defaults and installer assets use this path.
pub(crate) fn create_file(path: &Path, bytes: &[u8]) -> Result<bool> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).wrap_err_with(|| format!("creating {}", path.display())),
    }
}

pub(crate) fn digest(value: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(value.as_ref()))[..24].to_owned()
}

/// Short paths avoid macOS's Unix-domain socket path limit. The parent is private.
pub(crate) fn runtime_dir() -> Result<PathBuf> {
    let path = PathBuf::from(format!("/tmp/mustr-{}", rustix::process::getuid().as_raw()));
    private_dir(&path)?;
    Ok(path)
}

pub(crate) fn check_regular(path: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).wrap_err_with(|| format!("reading {}", path.display()))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        bail!("expected a regular file, not a link: {}", path.display());
    }
    Ok(())
}
