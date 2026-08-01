use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

use crate::{
    delete_bridge_shared_secret, load_bridge_shared_secret, store_bridge_shared_secret,
    validate_bridge_identity, validate_bridge_secret_for_store, validate_loaded_bridge_secret,
    CredentialError, CredentialStore, DeviceCredential, WechatKeyMaterial,
    BRIDGE_CREDENTIAL_ACCOUNT, BRIDGE_CREDENTIAL_SERVICE, BRIDGE_SHARED_SECRET_LENGTH,
    DEVICE_CREDENTIAL_ACCOUNT, DEVICE_CREDENTIAL_SERVICE, WECHAT_CREDENTIAL_ACCOUNT,
    WECHAT_CREDENTIAL_SERVICE,
};

const ITEM_NOT_FOUND_STATUS: i32 = -25_300;
const KEYCHAIN_NOT_AVAILABLE_STATUS: i32 = -25_291;
const INTERACTION_NOT_ALLOWED_STATUS: i32 = -25_308;

/// macOS Keychain adapter dedicated to PCA's fixed Bridge, device, and `WeChat` identities.
///
/// Although [`CredentialStore`] is generic, this adapter rejects every other service/account pair
/// with [`CredentialError::UnsupportedIdentity`]. Device-item creation is intentionally reserved
/// for the Task 6 local IPC adapter, which must create its legacy Keychain ACL for the installed
/// Setup app and `agentd`. This adapter only updates an existing device item so that it does not
/// accidentally create an unrestricted item.
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
        load_for_supported_identity(service, account, || {
            match get_generic_password(service, account) {
                Ok(secret) => Ok(Some(secret)),
                Err(error) if error.code() == ITEM_NOT_FOUND_STATUS => Ok(None),
                Err(error) => Err(map_keychain_error(error.code())),
            }
        })
    }

    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), CredentialError> {
        match identity(service, account)? {
            CredentialIdentity::Bridge => store_for_identity(service, account, secret, || {
                set_generic_password(service, account, secret)
                    .map_err(|error| map_keychain_error(error.code()))
            }),
            CredentialIdentity::Device => {
                store_device_for_identity(service, account, secret, || {
                    match get_generic_password(service, account) {
                        Ok(_) => set_generic_password(service, account, secret)
                            .map_err(|error| map_keychain_error(error.code())),
                        Err(error) if error.code() == ITEM_NOT_FOUND_STATUS => {
                            Err(CredentialError::OperationFailed)
                        }
                        Err(error) => Err(map_keychain_error(error.code())),
                    }
                })
            }
            CredentialIdentity::Wechat => Err(CredentialError::UnsupportedIdentity),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        match identity(service, account)? {
            CredentialIdentity::Bridge | CredentialIdentity::Device => {
                delete_for_supported_identity(service, account, || {
                    match delete_generic_password(service, account) {
                        Ok(()) => Ok(()),
                        Err(error) if error.code() == ITEM_NOT_FOUND_STATUS => Ok(()),
                        Err(error) => Err(map_keychain_error(error.code())),
                    }
                })
            }
            CredentialIdentity::Wechat => Err(CredentialError::UnsupportedIdentity),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CredentialIdentity {
    Bridge,
    Device,
    Wechat,
}

fn identity(service: &str, account: &str) -> Result<CredentialIdentity, CredentialError> {
    if service == BRIDGE_CREDENTIAL_SERVICE && account == BRIDGE_CREDENTIAL_ACCOUNT {
        validate_bridge_identity(service, account)?;
        Ok(CredentialIdentity::Bridge)
    } else if service == DEVICE_CREDENTIAL_SERVICE && account == DEVICE_CREDENTIAL_ACCOUNT {
        Ok(CredentialIdentity::Device)
    } else if service == WECHAT_CREDENTIAL_SERVICE && account == WECHAT_CREDENTIAL_ACCOUNT {
        Ok(CredentialIdentity::Wechat)
    } else {
        Err(CredentialError::UnsupportedIdentity)
    }
}

pub(crate) fn load_for_supported_identity<F>(
    service: &str,
    account: &str,
    backend: F,
) -> Result<Option<Vec<u8>>, CredentialError>
where
    F: FnOnce() -> Result<Option<Vec<u8>>, CredentialError>,
{
    match identity(service, account)? {
        CredentialIdentity::Bridge => load_for_identity(service, account, backend),
        CredentialIdentity::Device => load_device_for_identity(service, account, backend),
        CredentialIdentity::Wechat => load_wechat_for_identity(service, account, backend),
    }
}

pub(crate) fn load_wechat_for_identity<F>(
    service: &str,
    account: &str,
    backend: F,
) -> Result<Option<Vec<u8>>, CredentialError>
where
    F: FnOnce() -> Result<Option<Vec<u8>>, CredentialError>,
{
    if service != WECHAT_CREDENTIAL_SERVICE || account != WECHAT_CREDENTIAL_ACCOUNT {
        return Err(CredentialError::UnsupportedIdentity);
    }
    let record = backend()?;
    if let Some(record) = record.as_deref() {
        WechatKeyMaterial::decode(record)?;
    }
    Ok(record)
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

pub(crate) fn load_device_for_identity<F>(
    service: &str,
    account: &str,
    backend: F,
) -> Result<Option<Vec<u8>>, CredentialError>
where
    F: FnOnce() -> Result<Option<Vec<u8>>, CredentialError>,
{
    guard_device_identity(service, account)?;
    let record = backend()?;
    if let Some(record) = record.as_deref() {
        DeviceCredential::decode(record)?;
    }
    Ok(record)
}

pub(crate) fn store_device_for_identity<F>(
    service: &str,
    account: &str,
    record: &[u8],
    backend: F,
) -> Result<(), CredentialError>
where
    F: FnOnce() -> Result<(), CredentialError>,
{
    guard_device_identity(service, account)?;
    DeviceCredential::decode(record)?;
    backend()
}

pub(crate) fn delete_for_supported_identity<F>(
    service: &str,
    account: &str,
    backend: F,
) -> Result<(), CredentialError>
where
    F: FnOnce() -> Result<(), CredentialError>,
{
    identity(service, account)?;
    backend()
}

fn guard_device_identity(service: &str, account: &str) -> Result<(), CredentialError> {
    if service == DEVICE_CREDENTIAL_SERVICE && account == DEVICE_CREDENTIAL_ACCOUNT {
        Ok(())
    } else {
        Err(CredentialError::UnsupportedIdentity)
    }
}

fn map_keychain_error(status: i32) -> CredentialError {
    match status {
        KEYCHAIN_NOT_AVAILABLE_STATUS | INTERACTION_NOT_ALLOWED_STATUS => {
            CredentialError::Unavailable
        }
        _ => CredentialError::OperationFailed,
    }
}
