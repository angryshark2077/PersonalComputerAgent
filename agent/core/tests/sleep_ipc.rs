use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_agentd::sleep_ipc::{SleepControlIpcServer, SleepControlSocket};
use pca_bridge_client::{
    auth::create_proof,
    framing::{read_frame, write_frame},
};
use pca_keychain::{
    CredentialError, CredentialStore, BRIDGE_CREDENTIAL_ACCOUNT, BRIDGE_CREDENTIAL_SERVICE,
};
use tempfile::TempDir;
use tokio::{
    net::UnixStream,
    sync::{mpsc, watch},
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
async fn authenticated_prepare_sleep_waits_for_the_agent_acknowledgement() {
    let directory = TempDir::new().expect("temporary runtime root");
    let socket_path = directory.path().join("sleep-control.sock");
    let secret = [0x42; 32];
    let store = Arc::new(MemoryStore::default());
    store
        .store(
            BRIDGE_CREDENTIAL_SERVICE,
            BRIDGE_CREDENTIAL_ACCOUNT,
            &secret,
        )
        .expect("Bridge secret");
    let socket = SleepControlSocket::bind(&socket_path).expect("socket");
    let (commands, mut command_receiver) = mpsc::channel(1);
    let server = SleepControlIpcServer::new(socket, store, commands);
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let server_task = tokio::spawn(server.serve(shutdown_receiver));

    let nonce = [0x24; 32];
    let request_id = "01982222-7222-8222-8222-222222222222";
    let proof = create_proof(
        &secret,
        &nonce,
        1,
        "pca-bridge-sleep-v1:01982222-7222-8222-8222-222222222222:prepare_sleep",
    );
    let request = serde_json::json!({
        "protocol_version": 1,
        "request_id": request_id,
        "operation": "prepare_sleep",
        "nonce": URL_SAFE_NO_PAD.encode(nonce),
        "proof": proof,
    });
    let mut stream = UnixStream::connect(&socket_path)
        .await
        .expect("connect socket");
    write_frame(&mut stream, &request)
        .await
        .expect("send request");
    let command = timeout(Duration::from_secs(1), command_receiver.recv())
        .await
        .expect("command timeout")
        .expect("command");
    command.response().send(Ok(())).expect("acknowledge sleep");
    let response = timeout(Duration::from_secs(1), read_frame(&mut stream))
        .await
        .expect("response timeout")
        .expect("response frame");
    assert_eq!(response, serde_json::json!({ "ok": true }));

    shutdown_sender.send(true).expect("stop server");
    timeout(Duration::from_secs(1), server_task)
        .await
        .expect("server shutdown timeout")
        .expect("server task")
        .expect("server shutdown");
}
