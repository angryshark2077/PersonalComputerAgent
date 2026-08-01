use pca_keychain::{
    load_wechat_key_material, CredentialError, CredentialStore, WechatKeyMaterial,
    WECHAT_CREDENTIAL_ACCOUNT, WECHAT_CREDENTIAL_REF, WECHAT_CREDENTIAL_SERVICE,
};

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
    assert_eq!(loaded.raw_key(), &expected_key);
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
