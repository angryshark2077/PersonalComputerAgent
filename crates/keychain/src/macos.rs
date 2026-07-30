use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

use crate::{
    delete_bridge_shared_secret, load_bridge_shared_secret, store_bridge_shared_secret,
    validate_bridge_identity, validate_bridge_secret_for_store, validate_loaded_bridge_secret,
    CredentialError, CredentialStore, BRIDGE_SHARED_SECRET_LENGTH,
};

const ITEM_NOT_FOUND_STATUS: i32 = -25_300;
const KEYCHAIN_NOT_AVAILABLE_STATUS: i32 = -25_291;
const INTERACTION_NOT_ALLOWED_STATUS: i32 = -25_308;

/// macOS Keychain adapter dedicated to the fixed S1A Bridge credential identity.
///
/// Although [`CredentialStore`] is generic, this adapter rejects every other service/account pair
/// with [`CredentialError::UnsupportedIdentity`].
#[derive(Clone, Copy, Debug, Default)]
pub struct MacOSKeychainStore;

impl MacOSKeychainStore {
    /// Loads the fixed Bridge shared secret and validates its exact length.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::CorruptSecret`] for a stored value of any other length, or a
    /// safe Keychain error.
    pub fn load_shared_secret(
        &self,
    ) -> Result<Option<[u8; BRIDGE_SHARED_SECRET_LENGTH]>, CredentialError> {
        load_bridge_shared_secret(self)
    }

    /// Stores the fixed Bridge shared secret after validating its exact length.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidSecretLength`] for an input of any other length, or a
    /// safe Keychain error.
    pub fn store_shared_secret(&self, secret: &[u8]) -> Result<(), CredentialError> {
        store_bridge_shared_secret(self, secret)
    }

    /// Deletes the fixed Bridge shared secret when present.
    ///
    /// # Errors
    ///
    /// Returns a safe [`CredentialError`] when Keychain cannot complete the operation.
    pub fn delete_shared_secret(&self) -> Result<(), CredentialError> {
        delete_bridge_shared_secret(self)
    }
}

impl CredentialStore for MacOSKeychainStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        load_for_identity(service, account, || {
            match get_generic_password(service, account) {
                Ok(secret) => Ok(Some(secret)),
                Err(error) if error.code() == ITEM_NOT_FOUND_STATUS => Ok(None),
                Err(error) => Err(map_keychain_error(error.code())),
            }
        })
    }

    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), CredentialError> {
        store_for_identity(service, account, secret, || {
            set_generic_password(service, account, secret)
                .map_err(|error| map_keychain_error(error.code()))
        })
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        delete_for_identity(service, account, || {
            match delete_generic_password(service, account) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ITEM_NOT_FOUND_STATUS => Ok(()),
                Err(error) => Err(map_keychain_error(error.code())),
            }
        })
    }
}

pub(crate) fn load_for_identity<F>(
    service: &str,
    account: &str,
    backend: F,
) -> Result<Option<Vec<u8>>, CredentialError>
where
    F: FnOnce() -> Result<Option<Vec<u8>>, CredentialError>,
{
    validate_bridge_identity(service, account)?;
    let secret = backend()?;
    if let Some(secret) = secret.as_deref() {
        validate_loaded_bridge_secret(secret)?;
    }
    Ok(secret)
}

pub(crate) fn store_for_identity<F>(
    service: &str,
    account: &str,
    secret: &[u8],
    backend: F,
) -> Result<(), CredentialError>
where
    F: FnOnce() -> Result<(), CredentialError>,
{
    validate_bridge_identity(service, account)?;
    validate_bridge_secret_for_store(secret)?;
    backend()
}

pub(crate) fn delete_for_identity<F>(
    service: &str,
    account: &str,
    backend: F,
) -> Result<(), CredentialError>
where
    F: FnOnce() -> Result<(), CredentialError>,
{
    validate_bridge_identity(service, account)?;
    backend()
}

fn map_keychain_error(status: i32) -> CredentialError {
    match status {
        KEYCHAIN_NOT_AVAILABLE_STATUS | INTERACTION_NOT_ALLOWED_STATUS => {
            CredentialError::Unavailable
        }
        _ => CredentialError::OperationFailed,
    }
}
