use std::{error::Error, fmt};

mod macos;
#[cfg(test)]
mod macos_tests;

pub use macos::MacOSKeychainStore;

pub const BRIDGE_CREDENTIAL_SERVICE: &str = "com.pca.bridge";
pub const BRIDGE_CREDENTIAL_ACCOUNT: &str = "shared-secret-v1";
pub const BRIDGE_SHARED_SECRET_LENGTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    Unavailable,
    InvalidSecretLength,
    CorruptSecret,
    OperationFailed,
    UnsupportedIdentity,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unavailable => "credential store unavailable",
            Self::InvalidSecretLength => "invalid credential length",
            Self::CorruptSecret => "stored credential is corrupt",
            Self::OperationFailed => "credential operation failed",
            Self::UnsupportedIdentity => "credential identity unsupported",
        };
        formatter.write_str(message)
    }
}

impl Error for CredentialError {}

pub trait CredentialStore: Send + Sync {
    /// Loads a credential from the backing store.
    ///
    /// # Errors
    ///
    /// Returns a safe [`CredentialError`] when the backing store cannot complete the operation.
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError>;

    /// Creates or overwrites a credential in the backing store.
    ///
    /// # Errors
    ///
    /// Returns a safe [`CredentialError`] when the backing store cannot complete the operation.
    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), CredentialError>;

    /// Deletes a credential when present.
    ///
    /// # Errors
    ///
    /// Returns a safe [`CredentialError`] when the backing store cannot complete the operation.
    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError>;
}

/// Loads the fixed Bridge shared secret and validates its exact length.
///
/// # Errors
///
/// Returns [`CredentialError::CorruptSecret`] for a stored value of any other length, or forwards
/// a safe backing-store error.
pub fn load_bridge_shared_secret(
    store: &dyn CredentialStore,
) -> Result<Option<[u8; BRIDGE_SHARED_SECRET_LENGTH]>, CredentialError> {
    store
        .load(BRIDGE_CREDENTIAL_SERVICE, BRIDGE_CREDENTIAL_ACCOUNT)?
        .map(|secret| {
            validate_loaded_bridge_secret(&secret)?;
            secret
                .try_into()
                .map_err(|_| CredentialError::CorruptSecret)
        })
        .transpose()
}

/// Stores the fixed Bridge shared secret after validating its exact length.
///
/// # Errors
///
/// Returns [`CredentialError::InvalidSecretLength`] for an input of any other length, or forwards
/// a safe backing-store error.
pub fn store_bridge_shared_secret(
    store: &dyn CredentialStore,
    secret: &[u8],
) -> Result<(), CredentialError> {
    validate_bridge_secret_for_store(secret)?;
    store.store(BRIDGE_CREDENTIAL_SERVICE, BRIDGE_CREDENTIAL_ACCOUNT, secret)
}

/// Deletes the fixed Bridge shared secret when present.
///
/// # Errors
///
/// Returns a safe [`CredentialError`] when the backing store cannot complete the operation.
pub fn delete_bridge_shared_secret(store: &dyn CredentialStore) -> Result<(), CredentialError> {
    store.delete(BRIDGE_CREDENTIAL_SERVICE, BRIDGE_CREDENTIAL_ACCOUNT)
}

pub(crate) fn validate_bridge_identity(
    service: &str,
    account: &str,
) -> Result<(), CredentialError> {
    if service == BRIDGE_CREDENTIAL_SERVICE && account == BRIDGE_CREDENTIAL_ACCOUNT {
        Ok(())
    } else {
        Err(CredentialError::UnsupportedIdentity)
    }
}

pub(crate) fn validate_bridge_secret_for_store(secret: &[u8]) -> Result<(), CredentialError> {
    if secret.len() == BRIDGE_SHARED_SECRET_LENGTH {
        Ok(())
    } else {
        Err(CredentialError::InvalidSecretLength)
    }
}

pub(crate) fn validate_loaded_bridge_secret(secret: &[u8]) -> Result<(), CredentialError> {
    if secret.len() == BRIDGE_SHARED_SECRET_LENGTH {
        Ok(())
    } else {
        Err(CredentialError::CorruptSecret)
    }
}
