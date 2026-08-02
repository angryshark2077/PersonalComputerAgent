use std::{
    error::Error,
    fmt,
    path::{Component, Path},
};

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
pub const WECHAT_CREDENTIAL_SERVICE: &str = "com.pca.wechat";
pub const WECHAT_CREDENTIAL_ACCOUNT: &str = "current-v1";
pub const WECHAT_CREDENTIAL_REF: &str = "keychain://com.pca.wechat/current-v1";
const DEVICE_CREDENTIAL_VERSION: u8 = 1;
const WECHAT_KEY_MATERIAL_VERSION: u8 = 3;
const WECHAT_RAW_KEY_LENGTH: usize = 32;
const WECHAT_SQLCIPHER_SALT_LENGTH: usize = 16;

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
            Self::InvalidCredential => "credential is invalid",
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

/// Versioned `SQLCipher` key material retained only in the fixed `WeChat` Keychain item.
///
/// This type deliberately does not implement `Serialize`, `Clone`, or expose its account proof
/// through `Debug`. Only [`Self::encode`] serializes it for Keychain storage; SQLite-facing DTOs,
/// Events, logs, and diagnostics must never receive this value.
///
/// ```compile_fail
/// use pca_keychain::WechatKeyMaterial;
///
/// fn requires_serialize<T: serde::Serialize>(_: &T) {}
/// let material = WechatKeyMaterial::new("account-proof", [7; 32]).unwrap();
/// requires_serialize(&material);
/// ```
#[derive(Eq, PartialEq)]
pub struct WechatKeyMaterial {
    account_id: String,
    database_keys: Vec<WechatDatabaseKeyMaterial>,
}

impl fmt::Debug for WechatKeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WechatKeyMaterial")
            .field("value", &"redacted")
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredWechatKeyMaterial {
    version: u8,
    account_id: String,
    #[serde(default)]
    raw_key: Vec<u8>,
    #[serde(default)]
    salt: Option<Vec<u8>>,
    #[serde(default)]
    database_keys: Vec<StoredWechatDatabaseKeyMaterial>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredWechatDatabaseKeyMaterial {
    database_path: Option<String>,
    raw_key: Vec<u8>,
    salt: Option<Vec<u8>>,
}

/// One database-scoped `SQLCipher` key recovered from the local `WeChat` process.
///
/// The relative path is non-secret routing metadata. Key and salt bytes remain redacted from
/// `Debug` and can only be read by the native read-only database adapter.
#[derive(Eq, PartialEq)]
pub struct WechatDatabaseKeyMaterial {
    database_path: Option<String>,
    raw_key: [u8; WECHAT_RAW_KEY_LENGTH],
    salt: Option<[u8; WECHAT_SQLCIPHER_SALT_LENGTH]>,
}

impl fmt::Debug for WechatDatabaseKeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WechatDatabaseKeyMaterial")
            .field("database_path", &self.database_path)
            .field("raw_key", &"redacted")
            .field("salt", &self.salt.as_ref().map(|_| "redacted"))
            .finish()
    }
}

impl WechatDatabaseKeyMaterial {
    /// Creates one validated database-scoped key entry.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] for a non-canonical relative path, an
    /// all-zero key, or an all-zero salt.
    pub fn new(
        database_path: &str,
        raw_key: [u8; WECHAT_RAW_KEY_LENGTH],
        salt: [u8; WECHAT_SQLCIPHER_SALT_LENGTH],
    ) -> Result<Self, CredentialError> {
        let entry = Self {
            database_path: Some(database_path.to_owned()),
            raw_key,
            salt: Some(salt),
        };
        entry.validate()?;
        Ok(entry)
    }

    /// Creates one database-scoped key that `WeChat` passes directly to `sqlite3_key`.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] for a non-canonical relative path or an
    /// all-zero key.
    pub fn new_passphrase(
        database_path: &str,
        raw_key: [u8; WECHAT_RAW_KEY_LENGTH],
    ) -> Result<Self, CredentialError> {
        let entry = Self {
            database_path: Some(database_path.to_owned()),
            raw_key,
            salt: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    /// Returns the raw key only to the read-only `SQLCipher` adapter.
    #[must_use]
    pub const fn raw_key(&self) -> &[u8; WECHAT_RAW_KEY_LENGTH] {
        &self.raw_key
    }

    /// Returns the optional database salt only to the read-only `SQLCipher` adapter.
    #[must_use]
    pub const fn salt(&self) -> Option<&[u8; WECHAT_SQLCIPHER_SALT_LENGTH]> {
        self.salt.as_ref()
    }

    /// Distinguishes a database-routed credential from a legacy wildcard credential.
    #[must_use]
    pub const fn is_database_scoped(&self) -> bool {
        self.database_path.is_some()
    }

    fn legacy(
        raw_key: [u8; WECHAT_RAW_KEY_LENGTH],
        salt: Option<[u8; WECHAT_SQLCIPHER_SALT_LENGTH]>,
    ) -> Result<Self, CredentialError> {
        let entry = Self {
            database_path: None,
            raw_key,
            salt,
        };
        entry.validate()?;
        Ok(entry)
    }

    fn validate(&self) -> Result<(), CredentialError> {
        let path_is_valid = self.database_path.as_deref().is_none_or(|value| {
            !value.is_empty()
                && value.len() <= 512
                && !value.chars().any(char::is_control)
                && Path::new(value)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
        });
        if !path_is_valid
            || self.raw_key.iter().all(|byte| *byte == 0)
            || self
                .salt
                .is_some_and(|salt| salt.iter().all(|byte| *byte == 0))
        {
            Err(CredentialError::InvalidCredential)
        } else {
            Ok(())
        }
    }
}

impl WechatKeyMaterial {
    /// Creates validated account-scoped raw `SQLCipher` key material.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] for an empty/control-character account
    /// proof or an all-zero raw key.
    pub fn new(
        account_id: &str,
        raw_key: [u8; WECHAT_RAW_KEY_LENGTH],
    ) -> Result<Self, CredentialError> {
        let material = Self {
            account_id: account_id.to_owned(),
            database_keys: vec![WechatDatabaseKeyMaterial::legacy(raw_key, None)?],
        };
        material.validate()?;
        Ok(material)
    }

    /// Creates validated raw `SQLCipher` material whose database salt was recovered together
    /// with the key during an explicit local repair operation.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] for invalid account proof or key bytes.
    pub fn new_with_salt(
        account_id: &str,
        raw_key: [u8; WECHAT_RAW_KEY_LENGTH],
        salt: [u8; WECHAT_SQLCIPHER_SALT_LENGTH],
    ) -> Result<Self, CredentialError> {
        let material = Self {
            account_id: account_id.to_owned(),
            database_keys: vec![WechatDatabaseKeyMaterial::legacy(raw_key, Some(salt))?],
        };
        material.validate()?;
        Ok(material)
    }

    /// Returns the source account proof only to the read-only `WeChat` source probe.
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// Creates validated account-scoped material for independently keyed databases.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] when the account proof, entries, or paths
    /// are invalid, empty, or duplicated.
    pub fn new_for_databases(
        account_id: &str,
        database_keys: Vec<WechatDatabaseKeyMaterial>,
    ) -> Result<Self, CredentialError> {
        let material = Self {
            account_id: account_id.to_owned(),
            database_keys,
        };
        material.validate()?;
        Ok(material)
    }

    /// Selects the exact key for an absolute source database path. Legacy single-key records are
    /// accepted as a wildcard so existing Keychain items remain readable.
    #[must_use]
    pub fn key_for_database(&self, database_path: &Path) -> Option<&WechatDatabaseKeyMaterial> {
        self.database_keys.iter().find(|entry| {
            entry
                .database_path
                .as_deref()
                .is_none_or(|relative| database_path.ends_with(relative))
        })
    }

    /// Encodes this value solely for storage in the fixed Keychain item.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidCredential`] without exposing secret material when the
    /// value is invalid or cannot be encoded.
    pub fn encode(&self) -> Result<Vec<u8>, CredentialError> {
        self.validate()?;
        serde_json::to_vec(&StoredWechatKeyMaterial {
            version: WECHAT_KEY_MATERIAL_VERSION,
            account_id: self.account_id.clone(),
            raw_key: Vec::new(),
            salt: None,
            database_keys: self
                .database_keys
                .iter()
                .map(|entry| StoredWechatDatabaseKeyMaterial {
                    database_path: entry.database_path.clone(),
                    raw_key: entry.raw_key.to_vec(),
                    salt: entry.salt.map(|salt| salt.to_vec()),
                })
                .collect(),
        })
        .map_err(|_| CredentialError::InvalidCredential)
    }

    pub(crate) fn decode(value: &[u8]) -> Result<Self, CredentialError> {
        let stored: StoredWechatKeyMaterial =
            serde_json::from_slice(value).map_err(|_| CredentialError::InvalidCredential)?;
        match stored.version {
            1 => Self::new(
                &stored.account_id,
                stored
                    .raw_key
                    .try_into()
                    .map_err(|_| CredentialError::InvalidCredential)?,
            ),
            2 => match stored.salt {
                Some(salt) => Self::new_with_salt(
                    &stored.account_id,
                    stored
                        .raw_key
                        .try_into()
                        .map_err(|_| CredentialError::InvalidCredential)?,
                    salt.try_into()
                        .map_err(|_| CredentialError::InvalidCredential)?,
                ),
                None => Self::new(
                    &stored.account_id,
                    stored
                        .raw_key
                        .try_into()
                        .map_err(|_| CredentialError::InvalidCredential)?,
                ),
            },
            WECHAT_KEY_MATERIAL_VERSION => {
                let database_keys = stored
                    .database_keys
                    .into_iter()
                    .map(|entry| {
                        let raw_key = entry
                            .raw_key
                            .try_into()
                            .map_err(|_| CredentialError::InvalidCredential)?;
                        match entry.database_path {
                            Some(database_path) => match entry.salt {
                                Some(salt) => WechatDatabaseKeyMaterial::new(
                                    &database_path,
                                    raw_key,
                                    salt.try_into()
                                        .map_err(|_| CredentialError::InvalidCredential)?,
                                ),
                                None => WechatDatabaseKeyMaterial::new_passphrase(
                                    &database_path,
                                    raw_key,
                                ),
                            },
                            None => WechatDatabaseKeyMaterial::legacy(
                                raw_key,
                                entry
                                    .salt
                                    .map(|salt| {
                                        salt.try_into()
                                            .map_err(|_| CredentialError::InvalidCredential)
                                    })
                                    .transpose()?,
                            ),
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Self::new_for_databases(&stored.account_id, database_keys)
            }
            _ => Err(CredentialError::InvalidCredential),
        }
    }

    fn validate(&self) -> Result<(), CredentialError> {
        let account = self.account_id.trim();
        if account.is_empty()
            || account != self.account_id
            || account.len() > 512
            || account.chars().any(char::is_control)
            || self.database_keys.is_empty()
            || self
                .database_keys
                .iter()
                .any(|entry| entry.validate().is_err())
            || self.database_keys.iter().enumerate().any(|(index, entry)| {
                entry.database_path.is_some()
                    && self.database_keys[index + 1..]
                        .iter()
                        .any(|other| other.database_path == entry.database_path)
            })
        {
            Err(CredentialError::InvalidCredential)
        } else {
            Ok(())
        }
    }
}

/// Loads and validates the fixed account-scoped `WeChat` `KeyMaterial` reference.
///
/// # Errors
///
/// Returns a safe backing-store error or [`CredentialError::InvalidCredential`] for malformed
/// stored material. Error values never include account or key bytes.
pub fn load_wechat_key_material(
    store: &dyn CredentialStore,
) -> Result<Option<WechatKeyMaterial>, CredentialError> {
    store
        .load(WECHAT_CREDENTIAL_SERVICE, WECHAT_CREDENTIAL_ACCOUNT)?
        .map(|record| WechatKeyMaterial::decode(&record))
        .transpose()
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
