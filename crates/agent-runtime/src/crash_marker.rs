use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::{paths::reject_symlink, RuntimeError};

/// Marks the current process as active and removes that marker after a clean exit.
#[derive(Debug)]
pub struct CrashMarkerGuard {
    marker_file: PathBuf,
    marker_contents: String,
    previous_exit_was_unclean: bool,
    cleanup_attempted: bool,
}

impl CrashMarkerGuard {
    /// Records this process as active and observes whether the previous process exited uncleanly.
    ///
    /// Acquire [`crate::SingleInstanceGuard`] before activating this marker. That ordered
    /// lifecycle prevents concurrent legitimate owners; this type deliberately does not accept
    /// the lock guard so runtime composition remains outside this foundation crate.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker cannot be securely created or synchronized.
    pub fn activate(marker_file: &Path) -> Result<Self, RuntimeError> {
        reject_symlink(marker_file)?;
        let previous_exit_was_unclean = match fs::symlink_metadata(marker_file) {
            Ok(_) => true,
            Err(error) if error.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(RuntimeError::io("inspect crash marker", error)),
        };
        let marker_contents = format!("pca-crash-marker:{}\n", Uuid::new_v4());
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(marker_file)
            .map_err(|error| RuntimeError::io("open crash marker", error))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| RuntimeError::io("secure crash marker", error))?;
        file.write_all(marker_contents.as_bytes())
            .map_err(|error| RuntimeError::io("write crash marker", error))?;
        file.sync_all()
            .map_err(|error| RuntimeError::io("sync crash marker", error))?;

        Ok(Self {
            marker_file: marker_file.to_path_buf(),
            marker_contents,
            previous_exit_was_unclean,
            cleanup_attempted: false,
        })
    }

    #[must_use]
    pub const fn previous_exit_was_unclean(&self) -> bool {
        self.previous_exit_was_unclean
    }

    /// Confirms clean completion by removing only this activation's marker.
    ///
    /// This consumes the guard so a failed cleanup is observable and is not retried by `Drop`.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CrashMarkerOwnershipLost`] if another activation replaced the
    /// marker, or an I/O error if the owned marker cannot be removed.
    pub fn complete_cleanly(mut self) -> Result<(), RuntimeError> {
        self.cleanup_attempted = true;
        self.remove_owned_marker()
    }

    fn remove_owned_marker(&self) -> Result<(), RuntimeError> {
        reject_symlink(&self.marker_file)?;
        let mut contents = String::new();
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.marker_file)
            .map_err(|error| match error.kind() {
                ErrorKind::NotFound => RuntimeError::CrashMarkerOwnershipLost {
                    path: self.marker_file.clone(),
                },
                _ => RuntimeError::io("open crash marker for cleanup", error),
            })?;
        file.read_to_string(&mut contents)
            .map_err(|error| RuntimeError::io("read crash marker for cleanup", error))?;
        if contents != self.marker_contents {
            return Err(RuntimeError::CrashMarkerOwnershipLost {
                path: self.marker_file.clone(),
            });
        }
        fs::remove_file(&self.marker_file)
            .map_err(|error| RuntimeError::io("remove crash marker", error))
    }
}

impl Drop for CrashMarkerGuard {
    fn drop(&mut self) {
        if !self.cleanup_attempted && !std::thread::panicking() {
            drop(self.remove_owned_marker());
        }
    }
}
