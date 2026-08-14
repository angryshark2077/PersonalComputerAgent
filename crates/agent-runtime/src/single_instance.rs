use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

use fs2::FileExt;

use crate::{paths::reject_symlink, RuntimeError};

/// Holds the operating-system lock that establishes the one local `agentd` owner.
///
/// `lock_file` must live in a trusted `0700` runtime directory. The guard rejects
/// final-component symlinks and verifies the opened inode after lock acquisition,
/// but no pathname API can prevent a same-UID process from replacing that pathname
/// after verification. `RuntimePaths::create_securely` establishes that directory
/// boundary before the runtime creates this guard.
#[derive(Debug)]
pub struct SingleInstanceGuard {
    lock_file: File,
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
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
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock_file)
            .map_err(|error| RuntimeError::io("open instance lock", error))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| RuntimeError::io("secure instance lock", error))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                verify_lock_file_identity(lock_file, &file)?;
                Ok(Self { lock_file: file })
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                Err(RuntimeError::AlreadyRunning)
            }
            Err(error) => Err(RuntimeError::io("acquire instance lock", error)),
        }
    }
}

fn verify_lock_file_identity(lock_file: &Path, file: &File) -> Result<(), RuntimeError> {
    let path_metadata = fs::symlink_metadata(lock_file)
        .map_err(|error| RuntimeError::io("inspect instance lock after acquisition", error))?;
    if path_metadata.file_type().is_symlink() {
        return Err(RuntimeError::UnsafePath {
            path: lock_file.to_path_buf(),
            reason: "was replaced with a symlink after opening",
        });
    }
    let descriptor_metadata = file
        .metadata()
        .map_err(|error| RuntimeError::io("inspect opened instance lock", error))?;
    if descriptor_metadata.dev() != path_metadata.dev()
        || descriptor_metadata.ino() != path_metadata.ino()
    {
        return Err(RuntimeError::UnsafePath {
            path: lock_file.to_path_buf(),
            reason: "was replaced after opening",
        });
    }

    Ok(())
}
