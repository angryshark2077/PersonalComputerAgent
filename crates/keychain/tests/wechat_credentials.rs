use pca_keychain::{
    load_wechat_key_material, CredentialError, CredentialStore, WechatDatabaseKeyMaterial,
    WechatKeyMaterial, WECHAT_CREDENTIAL_ACCOUNT, WECHAT_CREDENTIAL_REF, WECHAT_CREDENTIAL_SERVICE,
};
use std::path::Path;

struct SingleItemStore {
    value: Option<Vec<u8>>,
}

impl CredentialStore for SingleItemStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        if service == WECHAT_CREDENTIAL_SERVICE && account == WECHAT_CREDENTIAL_ACCOUNT {
            Ok(self.value.clone())
        } else {
            Ok(None)
        }
    }

    fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }

    fn delete(&self, _: &str, _: &str) -> Result<(), CredentialError> {
        Err(CredentialError::UnsupportedIdentity)
    }
}

#[test]
fn loads_only_the_fixed_wechat_reference_and_validates_key_material() {
    let expected_key = [0x5a; 32];
    let encoded = WechatKeyMaterial::new("local-account-proof", expected_key)
        .expect("valid key material")
        .encode()
        .expect("keychain encoding");
    let store = SingleItemStore {
        value: Some(encoded),
    };

    let loaded = load_wechat_key_material(&store)
        .expect("safe keychain read")
        .expect("stored key material");

    assert_eq!(
        WECHAT_CREDENTIAL_REF,
        "keychain://com.pca.wechat/current-v1"
    );
    assert_eq!(loaded.account_id(), "local-account-proof");
    assert_eq!(
        loaded
            .key_for_database(Path::new("db_storage/session/session.db"))
            .expect("legacy key matches any database")
            .raw_key(),
        &expected_key
    );
}

#[test]
fn preserves_a_validated_sqlcipher_salt_without_exposing_it_in_debug_output() {
    let expected_key = [0x41; 32];
    let expected_salt = [0x42; 16];
    let encoded =
        WechatKeyMaterial::new_with_salt("local-account-proof", expected_key, expected_salt)
            .expect("valid key material")
            .encode()
            .expect("keychain encoding");
    let store = SingleItemStore {
        value: Some(encoded),
    };

    let loaded = load_wechat_key_material(&store)
        .expect("safe keychain read")
        .expect("stored key material");

    let database_key = loaded
        .key_for_database(Path::new("db_storage/session/session.db"))
        .expect("legacy key matches any database");
    assert_eq!(database_key.raw_key(), &expected_key);
    assert_eq!(database_key.salt(), Some(&expected_salt));
    let debug = format!("{loaded:?}");
    assert!(!debug.contains("65"));
    assert!(!debug.contains("66"));
}

#[test]
fn stores_and_selects_independent_database_keys_by_relative_path() {
    let session_key =
        WechatDatabaseKeyMaterial::new("db_storage/session/session.db", [0x11; 32], [0x21; 16])
            .expect("session key");
    let message_key =
        WechatDatabaseKeyMaterial::new("db_storage/message/message_0.db", [0x12; 32], [0x22; 16])
            .expect("message key");
    let encoded =
        WechatKeyMaterial::new_for_databases("local-account-proof", vec![session_key, message_key])
            .expect("database key set")
            .encode()
            .expect("keychain encoding");
    let loaded = load_wechat_key_material(&SingleItemStore {
        value: Some(encoded),
    })
    .expect("safe keychain read")
    .expect("stored key material");

    assert_eq!(
        loaded
            .key_for_database(Path::new(
                "/private/account/db_storage/message/message_0.db"
            ))
            .expect("exact message key")
            .raw_key(),
        &[0x12; 32]
    );
    assert!(loaded
        .key_for_database(Path::new("db_storage/contact/contact.db"))
        .is_none());
}

#[test]
fn preserves_a_database_scoped_wechat_passphrase_without_a_raw_key_salt() {
    let expected_key = [0x23; 32];
    let entry =
        WechatDatabaseKeyMaterial::new_passphrase("db_storage/session/session.db", expected_key)
            .expect("WeChat passphrase entry");
    let encoded = WechatKeyMaterial::new_for_databases("local-account-proof", vec![entry])
        .expect("database key set")
        .encode()
        .expect("keychain encoding");
    let loaded = load_wechat_key_material(&SingleItemStore {
        value: Some(encoded),
    })
    .expect("safe keychain read")
    .expect("stored key material");

    let database_key = loaded
        .key_for_database(Path::new("/private/db_storage/session/session.db"))
        .expect("exact session key");
    assert_eq!(database_key.raw_key(), &expected_key);
    assert!(database_key.is_database_scoped());
    assert_eq!(database_key.salt(), None);
}

#[test]
fn rejects_noncanonical_or_duplicate_database_paths() {
    assert_eq!(
        WechatDatabaseKeyMaterial::new("../message.db", [0x31; 32], [0x41; 16]).unwrap_err(),
        CredentialError::InvalidCredential
    );
    let first =
        WechatDatabaseKeyMaterial::new("db_storage/message/message_0.db", [0x31; 32], [0x41; 16])
            .expect("first key");
    let duplicate =
        WechatDatabaseKeyMaterial::new("db_storage/message/message_0.db", [0x32; 32], [0x42; 16])
            .expect("duplicate key entry");
    assert_eq!(
        WechatKeyMaterial::new_for_databases("local-account-proof", vec![first, duplicate],)
            .unwrap_err(),
        CredentialError::InvalidCredential
    );
}

#[test]
fn missing_or_malformed_wechat_material_fails_without_exposing_secret_bytes() {
    assert!(load_wechat_key_material(&SingleItemStore { value: None })
        .expect("missing item is not a keychain failure")
        .is_none());

    let secret = "not-a-valid-wechat-key-secret";
    let error = load_wechat_key_material(&SingleItemStore {
        value: Some(secret.as_bytes().to_vec()),
    })
    .unwrap_err();

    assert_eq!(error, CredentialError::InvalidCredential);
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
}

#[test]
fn wechat_key_material_debug_is_redacted() {
    let material =
        WechatKeyMaterial::new("account-must-not-appear", [0xab; 32]).expect("valid key material");
    let output = format!("{material:?}");

    assert!(!output.contains("account-must-not-appear"));
    assert!(!output.contains("171"));
    assert!(output.contains("redacted"));
}

#[test]
fn rejects_noncanonical_account_proof_instead_of_silently_trimming_it() {
    for account_id in [" local-account-proof", "local-account-proof "] {
        assert_eq!(
            WechatKeyMaterial::new(account_id, [0x7c; 32]).unwrap_err(),
            CredentialError::InvalidCredential
        );
    }
}
