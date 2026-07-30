use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use pca_domain::RuntimeStatusEnvelope;

use crate::{paths::reject_symlink, RuntimeError};

static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Writes one canonical local heartbeat; the runtime owns its two-second scheduling.
#[derive(Debug, Clone)]
pub struct LocalHeartbeatWriter {
    status_file: PathBuf,
}

impl LocalHeartbeatWriter {
    #[must_use]
    pub fn new(status_file: &Path) -> Self {
        Self {
            status_file: status_file.to_path_buf(),
        }
    }

    /// Atomically replaces the local status file with the canonical status envelope.
    ///
    /// # Errors
    ///
    /// Returns an error when the status cannot be serialized or atomically written.
    pub fn write(&self, status: &RuntimeStatusEnvelope) -> Result<(), RuntimeError> {
        reject_symlink(&self.status_file)?;
        let serialized = serde_json::to_vec(status).map_err(RuntimeError::Serialization)?;
        let parent = self
            .status_file
            .parent()
            .ok_or_else(|| RuntimeError::UnsafePath {
                path: self.status_file.clone(),
                reason: "must have a parent directory",
            })?;
        let (temporary_path, mut temporary_file) = create_unique_temporary_file(parent)?;
        let result = write_and_replace(
            &mut temporary_file,
            &temporary_path,
            &self.status_file,
            &serialized,
        );
        if result.is_err() {
            drop(fs::remove_file(&temporary_path));
        }
        result
    }
}

fn create_unique_temporary_file(parent: &Path) -> Result<(PathBuf, File), RuntimeError> {
    for _ in 0..16 {
        let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".runtime-status.tmp.{}.{}",
            process::id(),
            sequence
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(RuntimeError::io("create heartbeat temporary file", error)),
        }
    }

    Err(RuntimeError::UnsafePath {
        path: parent.to_path_buf(),
        reason: "could not allocate a unique heartbeat temporary file",
    })
}

fn write_and_replace(
    temporary_file: &mut File,
    temporary_path: &Path,
    status_file: &Path,
    serialized: &[u8],
) -> Result<(), RuntimeError> {
    temporary_file
        .write_all(serialized)
        .map_err(|error| RuntimeError::io("write heartbeat temporary file", error))?;
    temporary_file
        .sync_all()
        .map_err(|error| RuntimeError::io("sync heartbeat temporary file", error))?;
    fs::rename(temporary_path, status_file)
        .map_err(|error| RuntimeError::io("replace heartbeat status", error))?;
    fs::set_permissions(status_file, fs::Permissions::from_mode(0o600))
        .map_err(|error| RuntimeError::io("secure heartbeat status", error))
}
