use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    macos::{delete_for_supported_identity, load_for_identity, load_for_supported_identity, store_device_for_identity, store_for_identity},
    CredentialError, DeviceCredential, BRIDGE_CREDENTIAL_ACCOUNT, BRIDGE_CREDENTIAL_SERVICE,
    BRIDGE_SHARED_SECRET_LENGTH, DEVICE_CREDENTIAL_ACCOUNT, DEVICE_CREDENTIAL_SERVICE,
};

#[test]
fn trait_store_dispatch_rejects_wrong_length_before_reaching_keychain_backend() {
    for invalid in [vec![0x11; 31], vec![0x22; 33]] {
        let backend_called = AtomicBool::new(false);

        let result = store_for_identity(
            BRIDGE_CREDENTIAL_SERVICE,
            BRIDGE_CREDENTIAL_ACCOUNT,
            &invalid,
            || {
                backend_called.store(true, Ordering::Relaxed);
                Ok(())
            },
        );

        assert_eq!(result, Err(CredentialError::InvalidSecretLength));
        assert!(!backend_called.load(Ordering::Relaxed));
    }
}

#[test]
fn trait_load_dispatch_maps_wrong_length_backend_values_to_corrupt_secret() {
    for invalid in [vec![0x33; 31], vec![0x44; 33]] {
        let result =
            load_for_identity(BRIDGE_CREDENTIAL_SERVICE, BRIDGE_CREDENTIAL_ACCOUNT, || {
                Ok(Some(invalid))
            });

        assert_eq!(result, Err(CredentialError::CorruptSecret));
    }
}

#[test]
fn trait_dispatch_rejects_non_bridge_identity_before_reaching_backend() {
    let store_backend_called = AtomicBool::new(false);
    let load_backend_called = AtomicBool::new(false);
    let delete_backend_called = AtomicBool::new(false);
    let valid = [0x55; BRIDGE_SHARED_SECRET_LENGTH];

    let store_result = store_for_identity("other.service", "other-account", &valid, || {
        store_backend_called.store(true, Ordering::Relaxed);
        Ok(())
    });
    let load_result = load_for_identity("other.service", "other-account", || {
        load_backend_called.store(true, Ordering::Relaxed);
        Ok(Some(valid.to_vec()))
    });
    let delete_result = delete_for_supported_identity("other.service", "other-account", || {
        delete_backend_called.store(true, Ordering::Relaxed);
        Ok(())
    });

    assert_eq!(store_result, Err(CredentialError::UnsupportedIdentity));
    assert_eq!(load_result, Err(CredentialError::UnsupportedIdentity));
    assert_eq!(delete_result, Err(CredentialError::UnsupportedIdentity));
    assert!(!store_backend_called.load(Ordering::Relaxed));
    assert!(!load_backend_called.load(Ordering::Relaxed));
    assert!(!delete_backend_called.load(Ordering::Relaxed));
}

#[test]
fn trait_dispatch_accepts_the_fixed_identity_and_exact_length() {
    let expected = vec![0x66; BRIDGE_SHARED_SECRET_LENGTH];
    let store_backend_called = AtomicBool::new(false);

    store_for_identity(
        BRIDGE_CREDENTIAL_SERVICE,
        BRIDGE_CREDENTIAL_ACCOUNT,
        &expected,
        || {
            store_backend_called.store(true, Ordering::Relaxed);
            Ok(())
        },
    )
    .expect("fixed identity and length");

    assert!(store_backend_called.load(Ordering::Relaxed));
    assert_eq!(
        load_for_identity(BRIDGE_CREDENTIAL_SERVICE, BRIDGE_CREDENTIAL_ACCOUNT, || Ok(
            Some(expected.clone())
        ),),
        Ok(Some(expected))
    );
}

#[test]
fn device_dispatch_uses_its_versioned_identity_without_touching_bridge_validation() {
    let record = DeviceCredential::new(
        "11111111-1111-4111-8111-111111111111".to_owned(),
        "22222222-2222-4222-8222-222222222222".to_owned(),
        "test-access",
        "test-refresh",
    )
    .expect("valid fixture")
    .encode()
    .expect("encoded fixture");
    let store_called = AtomicBool::new(false);

    store_device_for_identity(
        DEVICE_CREDENTIAL_SERVICE,
        DEVICE_CREDENTIAL_ACCOUNT,
        &record,
        || {
            store_called.store(true, Ordering::Relaxed);
            Ok(())
        },
    )
    .expect("device dispatch");

    assert!(store_called.load(Ordering::Relaxed));
    assert_eq!(
        load_for_supported_identity(DEVICE_CREDENTIAL_SERVICE, DEVICE_CREDENTIAL_ACCOUNT, || {
            Ok(Some(record.clone()))
        }),
        Ok(Some(record))
    );
}
