//! Narrow authenticated Bridge-to-Agent control used only during system sleep preparation.

use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_bridge_client::{
    auth::verify_proof,
    framing::{read_frame_bytes, write_frame},
};
use pca_keychain::{load_bridge_shared_secret, CredentialStore};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, watch},
    time::{timeout, Duration},
};
use uuid::Uuid;

const SLEEP_CONTROL_PROTOCOL_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// A private local endpoint used only by the signed Bridge while `AppKit` delays sleep.
pub struct SleepControlSocket {
    listener: UnixListener,
    path: PathBuf,
}

#[derive(Debug)]
pub enum SleepControlSocketError {
    UnsafePath,
    Io(io::Error),
}

#[derive(Debug)]
pub enum SleepControlIpcServerError {
    Socket(SleepControlSocketError),
}

/// A request that the Agent main lifecycle owns and acknowledges.
pub struct SleepControlCommand {
    response: oneshot::Sender<Result<(), ()>>,
}

impl SleepControlCommand {
    #[must_use]
    pub fn response(self) -> oneshot::Sender<Result<(), ()>> {
        self.response
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepControlRequest {
    protocol_version: u32,
    request_id: Uuid,
    operation: String,
    nonce: String,
    proof: String,
}

impl SleepControlRequest {
    fn decode(bytes: &[u8]) -> Result<Self, ()> {
        let request: Self = serde_json::from_slice(bytes).map_err(|_| ())?;
        if request.protocol_version != SLEEP_CONTROL_PROTOCOL_VERSION
            || request.operation != "prepare_sleep"
            || request.proof.is_empty()
        {
            return Err(());
        }
        let nonce = URL_SAFE_NO_PAD.decode(&request.nonce).map_err(|_| ())?;
        if nonce.len() != 32 {
            return Err(());
        }
        Ok(request)
    }

    fn authenticate(&self, secret: &[u8; 32]) -> Result<(), ()> {
        let nonce = URL_SAFE_NO_PAD.decode(&self.nonce).map_err(|_| ())?;
        let nonce: [u8; 32] = nonce.try_into().map_err(|_| ())?;
        let context = format!("pca-bridge-sleep-v1:{}:{}", self.request_id, self.operation);
        verify_proof(secret, &nonce, self.protocol_version, &context, &self.proof).map_err(|_| ())
    }
}

impl SleepControlSocket {
    /// Binds one 0600 Unix-domain socket after refusing symlinks and regular-file targets.
    ///
    /// # Errors
    ///
    /// Returns an error when the target is unsafe or the socket cannot be created securely.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, SleepControlSocketError> {
        let path = path.as_ref();
        if !path.is_absolute() || path == Path::new("/") || path.parent().is_none() {
            return Err(SleepControlSocketError::UnsafePath);
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                fs::remove_file(path).map_err(SleepControlSocketError::Io)?;
            }
            Ok(_) => return Err(SleepControlSocketError::UnsafePath),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(SleepControlSocketError::Io(error)),
        }
        let listener = UnixListener::bind(path).map_err(SleepControlSocketError::Io)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(SleepControlSocketError::Io)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    fn shutdown(self) -> Result<(), SleepControlSocketError> {
        let Self { listener, path } = self;
        drop(listener);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                fs::remove_file(path).map_err(SleepControlSocketError::Io)
            }
            Ok(_) => Err(SleepControlSocketError::UnsafePath),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SleepControlSocketError::Io(error)),
        }
    }
}

/// Serves exactly one authenticated operation: prepare durable Agent state before sleep.
pub struct SleepControlIpcServer {
    socket: SleepControlSocket,
    store: Arc<dyn CredentialStore>,
    commands: mpsc::Sender<SleepControlCommand>,
}

impl SleepControlIpcServer {
    #[must_use]
    pub fn new(
        socket: SleepControlSocket,
        store: Arc<dyn CredentialStore>,
        commands: mpsc::Sender<SleepControlCommand>,
    ) -> Self {
        Self {
            socket,
            store,
            commands,
        }
    }

    /// # Errors
    ///
    /// Returns an error when the owned socket cannot be removed during shutdown.
    pub async fn serve(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), SleepControlIpcServerError> {
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                accepted = self.socket.listener.accept() => {
                    let (stream, _) = accepted
                        .map_err(|error| SleepControlIpcServerError::Socket(SleepControlSocketError::Io(error)))?;
                    let _ = self.handle_connection(stream).await;
                }
            }
        }
        self.socket
            .shutdown()
            .map_err(SleepControlIpcServerError::Socket)
    }

    async fn handle_connection(&self, mut stream: UnixStream) -> Result<(), ()> {
        let bytes = timeout(Duration::from_secs(2), read_frame_bytes(&mut stream))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let request = SleepControlRequest::decode(&bytes)?;
        let secret = load_bridge_shared_secret(self.store.as_ref())
            .map_err(|_| ())?
            .ok_or(())?;
        request.authenticate(&secret)?;
        let (response, receiver) = oneshot::channel();
        timeout(
            REQUEST_TIMEOUT,
            self.commands.send(SleepControlCommand { response }),
        )
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
        let response = timeout(REQUEST_TIMEOUT, receiver)
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let payload: Value = if response.is_ok() {
            json!({ "ok": true })
        } else {
            json!({ "ok": false })
        };
        timeout(Duration::from_secs(2), write_frame(&mut stream, &payload))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use pca_bridge_client::auth::create_proof;

    use super::SleepControlRequest;

    #[test]
    fn rejects_non_sleep_control_operations() {
        let request = br#"{"protocol_version":1,"request_id":"a0f0e3a3-2cb5-4fe8-8c4a-12827326ca76","operation":"wake","nonce":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","proof":"proof"}"#;
        assert!(SleepControlRequest::decode(request).is_err());
    }

    #[test]
    fn proof_is_bound_to_prepare_sleep_request_id_and_operation() {
        let secret = [0x42; 32];
        let nonce = [0x24; 32];
        let request_id = "01982222-7222-8222-8222-222222222222";
        let proof = create_proof(
            &secret,
            &nonce,
            1,
            "pca-bridge-sleep-v1:01982222-7222-8222-8222-222222222222:prepare_sleep",
        );
        let request = format!(
            r#"{{"protocol_version":1,"request_id":"{request_id}","operation":"prepare_sleep","nonce":"{}","proof":"{proof}"}}"#,
            URL_SAFE_NO_PAD.encode(nonce),
        );
        assert!(SleepControlRequest::decode(request.as_bytes())
            .expect("decode request")
            .authenticate(&secret)
            .is_ok());
    }
}
