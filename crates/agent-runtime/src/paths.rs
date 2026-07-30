use std::{
    env, fs,
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use crate::RuntimeError;

/// The fixed per-user paths used by the S1A local runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub root: PathBuf,
    pub app_dir: PathBuf,
    pub data_dir: PathBuf,
    pub run_dir: PathBuf,
    pub database_file: PathBuf,
    pub crash_marker_file: PathBuf,
    pub lock_file: PathBuf,
    pub socket_file: PathBuf,
    pub status_file: PathBuf,
}

impl RuntimePaths {
    /// Builds paths below an explicit root, used by installation and isolated tests.
    #[must_use]
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let app_dir = root.join("App");
        let data_dir = root.join("Data");
        let run_dir = root.join("Run");

        Self {
            database_file: data_dir.join("agent.sqlite3"),
            crash_marker_file: data_dir.join("crash-marker.json"),
            lock_file: run_dir.join("agent.lock"),
            socket_file: run_dir.join("bridge.sock"),
            status_file: run_dir.join("runtime-status.json"),
            root,
            app_dir,
            data_dir,
            run_dir,
        }
    }

    /// Builds the sole production root: `$HOME/Library/Application Support/PersonalComputerAgent`.
    ///
    /// # Errors
    ///
    /// Returns an error when the process has no usable `HOME` directory.
    pub fn for_current_user() -> Result<Self, RuntimeError> {
        let home = env::var_os("HOME").ok_or_else(|| RuntimeError::UnsafePath {
            path: PathBuf::new(),
            reason: "HOME is not set",
        })?;
        if home.is_empty() {
            return Err(RuntimeError::UnsafePath {
                path: PathBuf::new(),
                reason: "HOME is empty",
            });
        }

        Ok(Self::under(
            PathBuf::from(home).join("Library/Application Support/PersonalComputerAgent"),
        ))
    }

    /// Creates and restricts the root and its `App`, `Data`, and `Run` directories.
    ///
    /// Existing symlinks are rejected so sensitive local artifacts are never deliberately
    /// placed through a redirect at one of these ownership boundaries.
    /// Existing ancestors of `root` remain a caller trust boundary; after this method returns,
    /// `root` and its direct children are restricted to the current user with mode `0700`.
    ///
    /// # Errors
    ///
    /// Returns an error when the layout cannot be created, secured, or is unsafe.
    pub fn create_securely(&self) -> Result<(), RuntimeError> {
        ensure_secure_directory(&self.root)?;
        ensure_secure_directory(&self.app_dir)?;
        ensure_secure_directory(&self.data_dir)?;
        ensure_secure_directory(&self.run_dir)
    }
}

fn ensure_secure_directory(path: &Path) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(RuntimeError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "must not be a symlink",
                });
            }
            if !metadata.is_dir() {
                return Err(RuntimeError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "must be a directory",
                });
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| RuntimeError::io("create runtime directory", error))?;
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| RuntimeError::io("inspect runtime directory", error))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RuntimeError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "created path is not a directory",
                });
            }
        }
        Err(error) => return Err(RuntimeError::io("inspect runtime directory", error)),
    }

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| RuntimeError::io("secure runtime directory", error))
}

pub(crate) fn reject_symlink(path: &Path) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(RuntimeError::UnsafePath {
            path: path.to_path_buf(),
            reason: "must not be a symlink",
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RuntimeError::io("inspect runtime path", error)),
    }
}
