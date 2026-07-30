use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use crate::{paths::reject_symlink, RuntimeError};

/// Marks the current process as active and removes that marker after a clean exit.
#[derive(Debug)]
pub struct CrashMarkerGuard {
    marker_file: PathBuf,
    previous_exit_was_unclean: bool,
}

impl CrashMarkerGuard {
    /// Records this process as active and observes whether the previous process exited uncleanly.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker cannot be securely created or synchronized.
    pub fn activate(marker_file: &Path) -> Result<Self, RuntimeError> {
        reject_symlink(marker_file)?;
        let previous_exit_was_unclean = marker_file.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(marker_file)
            .map_err(|error| RuntimeError::io("open crash marker", error))?;
        file.write_all(b"active\n")
            .map_err(|error| RuntimeError::io("write crash marker", error))?;
        file.sync_all()
            .map_err(|error| RuntimeError::io("sync crash marker", error))?;
        fs::set_permissions(marker_file, fs::Permissions::from_mode(0o600))
            .map_err(|error| RuntimeError::io("secure crash marker", error))?;

        Ok(Self {
            marker_file: marker_file.to_path_buf(),
            previous_exit_was_unclean,
        })
    }

    #[must_use]
    pub const fn previous_exit_was_unclean(&self) -> bool {
        self.previous_exit_was_unclean
    }
}

impl Drop for CrashMarkerGuard {
    fn drop(&mut self) {
        drop(fs::remove_file(&self.marker_file));
    }
}
