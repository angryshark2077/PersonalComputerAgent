use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_agentd::pairing_ipc::{
    PairingIpcRequest, PairingIpcServer, PairingSocket, PairingSocketError,
};
use pca_agentd::{cloud_control::CloudControlOwner, communication::CommunicationAuthorization};
use pca_bridge_client::auth::create_proof;
use pca_bridge_client::framing::{read_frame, write_frame};
use pca_db_local::DbActorHandle;
use pca_keychain::{
    CredentialError, CredentialStore, BRIDGE_CREDENTIAL_ACCOUNT, BRIDGE_CREDENTIAL_SERVICE,
};
use tempfile::TempDir;
use tokio::{
    net::UnixStream,
    sync::watch,
    time::{timeout, Duration},
};

#[derive(Default)]
struct MemoryStore(Mutex<BTreeMap<(String, String), Vec<u8>>>);

impl CredentialStore for MemoryStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Ok(self
            .0
            .lock()
            .expect("store lock")
            .get(&(service.to_owned(), account.to_owned()))
            .cloned())
    }

    fn store(&self, service: &str, account: &str, value: &[u8]) -> Result<(), CredentialError> {
        self.0
            .lock()
            .expect("store lock")
            .insert((service.to_owned(), account.to_owned()), value.to_vec());
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        self.0
            .lock()
            .expect("store lock")
            .remove(&(service.to_owned(), account.to_owned()));
        Ok(())
    }
}

#[tokio::test]
async fn pairing_socket_is_private_and_refuses_a_regular_file_target() {
    let directory = TempDir::new().expect("temporary runtime root");
    let socket_path = directory.path().join("pairing.sock");

    let socket = PairingSocket::bind(&socket_path)
        .await
        .expect("bind pairing socket");
    let permissions = std::fs::metadata(&socket_path)
        .expect("inspect socket")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(permissions, 0o600);
    socket.shutdown().await.expect("remove socket");

    std::fs::write(&socket_path, b"not a socket").expect("regular file fixture");
    assert!(matches!(
        PairingSocket::bind(&socket_path).await,
        Err(PairingSocketError::UnsafePath)
    ));
}

#[test]
fn pairing_ipc_rejects_unknown_fields_and_wrong_protocol_versions() {
    let valid = br#"{
        "protocol_version": 1,
        "request_id": "01982222-7222-8222-8222-222222222222",
        "operation": "status",
        "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "proof": "proof"
    }"#;
    assert!(PairingIpcRequest::decode(valid).is_ok());

    let unknown = br#"{
        "protocol_version": 1,
        "request_id": "01982222-7222-8222-8222-222222222222",
        "operation": "status",
        "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "proof": "proof",
        "cloud_api_origin": "https://pca-cloud-api-production.up.railway.app"
    }"#;
    assert!(PairingIpcRequest::decode(unknown).is_err());

    let wrong_version = br#"{
        "protocol_version": 2,
        "request_id": "01982222-7222-8222-8222-222222222222",
        "operation": "status",
        "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "proof": "proof"
    }"#;
    assert!(PairingIpcRequest::decode(wrong_version).is_err());

    let begin_without_payload = br#"{
        "protocol_version": 1,
        "request_id": "01982222-7222-8222-8222-222222222222",
        "operation": "begin",
        "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "proof": "proof"
    }"#;
    assert!(PairingIpcRequest::decode(begin_without_payload).is_err());
}

#[test]
fn pairing_ipc_proof_is_bound_to_its_request_id_and_operation() {
    let secret = [0x42; 32];
    let nonce = [0x24; 32];
    let request_id = "01982222-7222-8222-8222-222222222222";
    let proof = create_proof(
        &secret,
        &nonce,
        1,
        "pca-setup-pairing-v1:01982222-7222-8222-8222-222222222222:status",
    );
    let valid = format!(
        r#"{{"protocol_version":1,"request_id":"{request_id}","operation":"status","nonce":"{}","proof":"{proof}"}}"#,
        URL_SAFE_NO_PAD.encode(nonce),
    );
    assert!(PairingIpcRequest::decode(valid.as_bytes())
        .expect("decode request")
        .authenticate(&secret)
        .is_ok());

    let replayed_as_begin = format!(
        r#"{{"protocol_version":1,"request_id":"{request_id}","operation":"begin","nonce":"{}","proof":"{proof}","payload":{{"callback_uri":"http://127.0.0.1:49152/pca/pair/callback","cloud_api_origin":"https://pca-cloud-api-production.up.railway.app"}}}}"#,
        URL_SAFE_NO_PAD.encode(nonce),
    );
    assert!(PairingIpcRequest::decode(replayed_as_begin.as_bytes())
        .expect("decode replay")
        .authenticate(&secret)
        .is_err());
}

#[tokio::test]
async fn authenticated_status_reports_an_unpaired_agent() {
    let directory = TempDir::new().expect("temporary runtime root");
    let socket_path = directory.path().join("pairing.sock");
    let database = Arc::new(
        DbActorHandle::open(&directory.path().join("agent.sqlite3"), "test")
            .await
            .expect("database"),
    );
    let store = Arc::new(MemoryStore::default());
    let secret = [0x42; 32];
    store
        .store(
            BRIDGE_CREDENTIAL_SERVICE,
            BRIDGE_CREDENTIAL_ACCOUNT,
            &secret,
        )
        .expect("Bridge secret");
    let socket = PairingSocket::bind(&socket_path).await.expect("socket");
    let (pairing_state_sender, _) = watch::channel(false);
    let authorization = CommunicationAuthorization::new();
    let (control_owner, control_commands) = CloudControlOwner::start(
        Arc::clone(&database),
        pairing_state_sender,
        authorization.clone(),
    );
    let server = PairingIpcServer::new(
        socket,
        Arc::clone(&database),
        store,
        control_commands,
        authorization,
    );
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let server_task = tokio::spawn(server.serve(shutdown_receiver));

    let nonce = [0x24; 32];
    let request_id = "01982222-7222-8222-8222-222222222222";
    let proof = create_proof(
        &secret,
        &nonce,
        1,
        "pca-setup-pairing-v1:01982222-7222-8222-8222-222222222222:status",
    );
    let request = serde_json::json!({
        "protocol_version": 1,
        "request_id": request_id,
        "operation": "status",
        "nonce": URL_SAFE_NO_PAD.encode(nonce),
        "proof": proof,
    });
    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect socket");
    write_frame(&mut stream, &request)
        .await
        .expect("send request");
    let response = timeout(Duration::from_secs(1), read_frame(&mut stream))
        .await
        .expect("response timeout")
        .expect("response frame");
    assert_eq!(response, serde_json::json!({ "paired": false }));

    shutdown_sender.send(true).expect("stop server");
    timeout(Duration::from_secs(1), server_task)
        .await
        .expect("server shutdown timeout")
        .expect("server task")
        .expect("server shutdown");
    control_owner
        .shutdown()
        .await
        .expect("control owner shutdown");
    let Ok(database) = Arc::try_unwrap(database) else {
        panic!("server released database");
    };
    database.shutdown().await.expect("database shutdown");
}
