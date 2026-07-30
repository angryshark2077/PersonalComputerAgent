use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::{paths::reject_symlink, RuntimeError};

/// Marks the current process as active and removes that marker after a clean exit.
#[derive(Debug)]
pub struct CrashMarkerGuard {
    marker_path: PathBuf,
    previous_exit_was_unclean: bool,
    cleanup_attempted: bool,
}

impl CrashMarkerGuard {
    /// Records this process as active and observes whether the previous process exited uncleanly.
    ///
    /// `marker_file` is the stable base of a marker family. Each activation creates an exclusive
    /// sibling named `base.UUID`, so an earlier guard can never target a later activation's path.
    /// Acquire [`crate::SingleInstanceGuard`] before activating this marker. That ordered
    /// lifecycle prevents concurrent legitimate owners; this type deliberately does not accept
    /// the lock guard so runtime composition remains outside this foundation crate.
    ///
    /// # Errors
    ///
    /// Returns an error when the marker family cannot be securely scanned, cleaned, or created.
    pub fn activate(marker_file: &Path) -> Result<Self, RuntimeError> {
        reject_symlink(marker_file)?;
        let stale_markers = marker_family_members(marker_file)?;
        let previous_exit_was_unclean = !stale_markers.is_empty();
        for stale_marker in stale_markers {
            fs::remove_file(&stale_marker)
                .map_err(|error| RuntimeError::io("remove stale crash marker", error))?;
        }

        let (marker_path, mut file) = create_marker_file(marker_file)?;
        let marker_contents = format!("pca-crash-marker:{}\n", Uuid::new_v4());
        let initialization = (|| {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| RuntimeError::io("secure crash marker", error))?;
            file.write_all(marker_contents.as_bytes())
                .map_err(|error| RuntimeError::io("write crash marker", error))?;
            file.sync_all()
                .map_err(|error| RuntimeError::io("sync crash marker", error))
        })();
        if let Err(error) = initialization {
            drop(fs::remove_file(&marker_path));
            return Err(error);
        }

        Ok(Self {
            marker_path,
            previous_exit_was_unclean,
            cleanup_attempted: false,
        })
    }

    #[must_use]
    pub const fn previous_exit_was_unclean(&self) -> bool {
        self.previous_exit_was_unclean
    }

    /// Confirms clean completion by removing only this activation's unique marker path.
    ///
    /// This consumes the guard so a failed cleanup is observable and is not retried by `Drop`.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CrashMarkerOwnershipLost`] when a later activation already
    /// removed this guard's stale marker, or an I/O error if cleanup otherwise fails.
    pub fn complete_cleanly(mut self) -> Result<(), RuntimeError> {
        self.cleanup_attempted = true;
        self.remove_owned_marker()
    }

    fn remove_owned_marker(&self) -> Result<(), RuntimeError> {
        fs::remove_file(&self.marker_path).map_err(|error| match error.kind() {
            ErrorKind::NotFound => RuntimeError::CrashMarkerOwnershipLost {
                path: self.marker_path.clone(),
            },
            _ => RuntimeError::io("remove crash marker", error),
        })
    }
}

impl Drop for CrashMarkerGuard {
    fn drop(&mut self) {
        if !self.cleanup_attempted && !std::thread::panicking() {
            drop(self.remove_owned_marker());
        }
    }
}

fn marker_family_members(marker_file: &Path) -> Result<Vec<PathBuf>, RuntimeError> {
    let (parent, base_name) = marker_family_context(marker_file)?;
    let prefix = format!("{base_name}.");
    let entries = fs::read_dir(parent)
        .map_err(|error| RuntimeError::io("scan crash marker directory", error))?;
    let mut markers = Vec::new();

    for entry in entries {
        let entry =
            entry.map_err(|error| RuntimeError::io("read crash marker directory", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let is_legacy_base = name == base_name;
        let is_token_marker = name.strip_prefix(&prefix).is_some_and(is_canonical_uuid);
        if !is_legacy_base && !is_token_marker {
            continue;
        }

        let file_type = entry
            .file_type()
            .map_err(|error| RuntimeError::io("inspect crash marker family member", error))?;
        if file_type.is_symlink() {
            return Err(RuntimeError::UnsafePath {
                path: entry.path(),
                reason: "crash marker family member must not be a symlink",
            });
        }
        if !file_type.is_file() {
            return Err(RuntimeError::UnsafePath {
                path: entry.path(),
                reason: "crash marker family member must be a regular file",
            });
        }
        markers.push(entry.path());
    }

    Ok(markers)
}

fn create_marker_file(marker_file: &Path) -> Result<(PathBuf, File), RuntimeError> {
    let (parent, base_name) = marker_family_context(marker_file)?;
    for _ in 0..16 {
        let marker_path = parent.join(format!("{base_name}.{}", Uuid::new_v4()));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker_path)
        {
            Ok(file) => return Ok((marker_path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(RuntimeError::io("create crash marker", error)),
        }
    }

    Err(RuntimeError::UnsafePath {
        path: marker_file.to_path_buf(),
        reason: "could not allocate a unique crash marker path",
    })
}

fn marker_family_context(marker_file: &Path) -> Result<(&Path, &str), RuntimeError> {
    let parent = marker_file
        .parent()
        .ok_or_else(|| RuntimeError::UnsafePath {
            path: marker_file.to_path_buf(),
            reason: "crash marker base must have a parent directory",
        })?;
    let base_name = marker_file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| RuntimeError::UnsafePath {
            path: marker_file.to_path_buf(),
            reason: "crash marker base name must be UTF-8",
        })?;

    Ok((parent, base_name))
}

fn is_canonical_uuid(candidate: &str) -> bool {
    Uuid::parse_str(candidate).is_ok_and(|token| token.hyphenated().to_string() == candidate)
}
