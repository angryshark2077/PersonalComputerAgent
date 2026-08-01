use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod macos;
#[cfg(test)]
mod macos_tests;

pub use macos::MacOSKeychainStore;

pub const BRIDGE_CREDENTIAL_SERVICE: &str = "com.pca.bridge";
pub const BRIDGE_CREDENTIAL_ACCOUNT: &str = "shared-secret-v1";
pub const BRIDGE_SHARED_SECRET_LENGTH: usize = 32;
pub const DEVICE_CREDENTIAL_SERVICE: &str = "com.pca.device";
pub const DEVICE_CREDENTIAL_ACCOUNT: &str = "current-v1";
const DEVICE_CREDENTIAL_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    Unavailable,
    InvalidSecretLength,
    CorruptSecret,
    InvalidCredential,
    OperationFailed,
    UnsupportedIdentity,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unavailable => "credential store unavailable",
            Self::InvalidSecretLength => "invalid credential length",
            Self::CorruptSecret => "stored credential is corrupt",
            Self::InvalidCredential => "device credential is invalid",
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

/// The versioned device credential payload kept in the dedicated macOS Keychain item.
///
/// The value is intentionally opaque to `SQLite`, Events, and logs. Task 6 must add the Agent
/// local-IPC Keychain adapter that creates this item with an ACL for the installed Setup app and
/// `agentd`; this crate deliberately supplies only the versioned codec and fixed identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCredential {
    version: u8,
    device_id: String,
    workspace_id: String,
    credential_generation: u64,
    access_expires_at_ms: i64,
    refresh_expires_at_ms: i64,
    access_credential: String,
    refresh_credential: String,
}

impl DeviceCredential {
    /// Creates a validated credential with initial generation and expiry metadata.
    /// Call [`Self::with_metadata`] with the exchange values before persisting it.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] when either identifier, credential, or
    /// initial expiry metadata is invalid.
    pub fn new(
        device_id: String,
        workspace_id: String,
        access_credential: &str,
        refresh_credential: &str,
    ) -> Result<Self, CredentialError> {
        let credential = Self {
            version: DEVICE_CREDENTIAL_VERSION,
            device_id,
            workspace_id,
            credential_generation: 0,
            access_expires_at_ms: 0,
            refresh_expires_at_ms: 0,
            access_credential: access_credential.to_owned(),
            refresh_credential: refresh_credential.to_owned(),
        };
        credential.validate()?;
        Ok(credential)
    }

    #[must_use]
    pub fn with_metadata(
        mut self,
        credential_generation: u64,
        access_expires_at_ms: i64,
        refresh_expires_at_ms: i64,
    ) -> Self {
        self.credential_generation = credential_generation;
        self.access_expires_at_ms = access_expires_at_ms;
        self.refresh_expires_at_ms = refresh_expires_at_ms;
        self
    }

    /// Returns the Cloud device identifier without exposing either credential value.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Returns the owning Workspace identifier without exposing either credential value.
    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// Returns the current Cloud credential generation.
    #[must_use]
    pub const fn credential_generation(&self) -> u64 {
        self.credential_generation
    }

    /// Returns the access credential only to the authenticated Cloud client.
    #[must_use]
    pub fn access_credential(&self) -> &str {
        &self.access_credential
    }

    /// Returns the refresh credential only to the authenticated Cloud client.
    #[must_use]
    pub fn refresh_credential(&self) -> &str {
        &self.refresh_credential
    }

    /// Serializes the record only after all schema and content checks succeed.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] when validation or serialization fails.
    pub fn encode(&self) -> Result<Vec<u8>, CredentialError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| CredentialError::InvalidCredential)
    }

    /// Decodes a versioned record without exposing its contents through an error.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] when the record is malformed or invalid.
    pub fn decode(value: &[u8]) -> Result<Self, CredentialError> {
        let credential: Self =
            serde_json::from_slice(value).map_err(|_| CredentialError::InvalidCredential)?;
        credential.validate()?;
        Ok(credential)
    }

    fn validate(&self) -> Result<(), CredentialError> {
        let identifiers_valid = self.version == DEVICE_CREDENTIAL_VERSION
            && Uuid::parse_str(&self.device_id).is_ok()
            && Uuid::parse_str(&self.workspace_id).is_ok();
        let expiry_valid = self.access_expires_at_ms >= 0
            && self.refresh_expires_at_ms >= self.access_expires_at_ms;
        if identifiers_valid
            && expiry_valid
            && !self.access_credential.is_empty()
            && !self.refresh_credential.is_empty()
        {
            Ok(())
        } else {
            Err(CredentialError::InvalidCredential)
        }
    }
}

/// Loads and validates the device credential at `com.pca.device/current-v1`.
///
/// # Errors
///
/// Returns a safe backing-store error or [`CredentialError::InvalidCredential`] for a corrupt
/// stored record.
pub fn load_device_credential(
    store: &dyn CredentialStore,
) -> Result<Option<DeviceCredential>, CredentialError> {
    store
        .load(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT)?
        .map(|record| DeviceCredential::decode(&record))
        .transpose()
}

/// Stores a fully validated device credential at `com.pca.device/current-v1`.
///
/// # Errors
///
/// Returns [`CredentialError::InvalidCredential`] when the credential cannot be encoded, or a
/// safe backing-store error.
pub fn store_device_credential(
    store: &dyn CredentialStore,
    credential: &DeviceCredential,
) -> Result<(), CredentialError> {
    store.store(
        DEVICE_CREDENTIAL_SERVICE,
        DEVICE_CREDENTIAL_ACCOUNT,
        &credential.encode()?,
    )
}

/// Deletes the device credential when it is present.
///
/// # Errors
///
/// Returns a safe backing-store error.
pub fn delete_device_credential(store: &dyn CredentialStore) -> Result<(), CredentialError> {
    store.delete(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT)
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
