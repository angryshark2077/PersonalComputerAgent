use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use fs2::FileExt;

use crate::{paths::reject_symlink, RuntimeError};

/// Holds the operating-system lock that establishes the one local `agentd` owner.
#[derive(Debug)]
pub struct SingleInstanceGuard {
    _lock_file: File,
}

impl SingleInstanceGuard {
    /// Opens and exclusively locks `lock_file` until this guard is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::AlreadyRunning`] when another process owns the lock.
    pub fn acquire(lock_file: &Path) -> Result<Self, RuntimeError> {
        reject_symlink(lock_file)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_file)
            .map_err(|error| RuntimeError::io("open instance lock", error))?;
        fs::set_permissions(lock_file, fs::Permissions::from_mode(0o600))
            .map_err(|error| RuntimeError::io("secure instance lock", error))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { _lock_file: file }),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                Err(RuntimeError::AlreadyRunning)
            }
            Err(error) => Err(RuntimeError::io("acquire instance lock", error)),
        }
    }
}
