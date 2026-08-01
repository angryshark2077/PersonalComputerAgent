#![allow(clippy::pedantic)]

use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use pca_agentd::cloud_control::{
    AgentControlSnapshot, AgentPairingService, CloudControlRuntime, CollectorControls,
    ControlClient, ControlError, ControlFuture, EnabledControl, PairingCallbackHandoff,
    PairingClient, PairingExchangeRequest, PairingSessionRequest, PairingSessionResponse,
    PairingStartHandoff, WeChatControl,
};
use pca_db_local::DbActorHandle;
use pca_domain::{CollectorState, CollectorStatus};
use pca_keychain::{
    load_device_credential, CredentialError, CredentialStore, DeviceCredential,
    DEVICE_CREDENTIAL_ACCOUNT, DEVICE_CREDENTIAL_SERVICE,
};
use serde::{Deserialize, Serialize};

type HarnessResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize)]
struct HarnessInput {
    device_id: String,
    workspace_id: String,
    device_access_token: Option<String>,
    refresh_token: Option<String>,
    access_expires_at_ms: Option<i64>,
    refresh_expires_at_ms: Option<i64>,
    callback_uri: Option<String>,
    configuration_revision: u64,
    #[serde(rename = "message_body_canary")]
    _message_body_canary: String,
}

#[derive(Serialize)]
struct HarnessStatus<'a> {
    phase: &'a str,
    agent_status: &'a str,
    paired: bool,
    applied_control_revision: Option<u64>,
    device_id: &'a str,
    workspace_id: &'a str,
}

struct FileCredentialStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileCredentialStore {
    fn new(runtime_root: &Path) -> Self {
        Self {
            path: runtime_root
                .join("test-keychain")
                .join("device-credential.json"),
            lock: Mutex::new(()),
        }
    }

    fn validate_identity(service: &str, account: &str) -> Result<(), CredentialError> {
        if service == DEVICE_CREDENTIAL_SERVICE && account == DEVICE_CREDENTIAL_ACCOUNT {
            Ok(())
        } else {
            Err(CredentialError::UnsupportedIdentity)
        }
    }
}

impl CredentialStore for FileCredentialStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        Self::validate_identity(service, account)?;
        let _guard = self.lock.lock().map_err(|_| CredentialError::Unavailable)?;
        match fs::read(&self.path) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(CredentialError::OperationFailed),
        }
    }

    fn store(&self, service: &str, account: &str, value: &[u8]) -> Result<(), CredentialError> {
        Self::validate_identity(service, account)?;
        let _guard = self.lock.lock().map_err(|_| CredentialError::Unavailable)?;
        let parent = self.path.parent().ok_or(CredentialError::OperationFailed)?;
        fs::create_dir_all(parent).map_err(|_| CredentialError::OperationFailed)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|_| CredentialError::OperationFailed)?;
        file.write_all(value)
            .map_err(|_| CredentialError::OperationFailed)
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), CredentialError> {
        Self::validate_identity(service, account)?;
        let _guard = self.lock.lock().map_err(|_| CredentialError::Unavailable)?;
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(CredentialError::OperationFailed),
        }
    }
}

struct FixedPairingClient {
    credential: DeviceCredential,
}

impl PairingClient for FixedPairingClient {
    fn create_pairing_session<'a>(
        &'a self,
        request: &'a PairingSessionRequest,
    ) -> ControlFuture<'a, PairingSessionResponse> {
        Box::pin(async move {
            if !request.callback_uri.starts_with("http://127.0.0.1:")
                || request.callback_state.is_empty()
                || request.code_challenge.is_empty()
                || request.device_public_key.is_empty()
            {
                return Err(ControlError::Contract);
            }
            Ok(PairingSessionResponse {
                session_id: "01983333-7333-8333-8333-333333333333".to_owned(),
                authorization_url: "https://dashboard.invalid/pair".to_owned(),
            })
        })
    }

    fn exchange_pairing_callback<'a>(
        &'a self,
        request: &'a PairingExchangeRequest,
    ) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async move {
            if request.session_id != "01983333-7333-8333-8333-333333333333"
                || request.authorization_code != "accepted-callback-code"
                || request.code_verifier.is_empty()
            {
                return Err(ControlError::Contract);
            }
            Ok(self.credential.clone())
        })
    }
}

struct SnapshotClient {
    snapshot: AgentControlSnapshot,
}

impl ControlClient for SnapshotClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::InvalidCredential) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        Box::pin(async move { Ok(self.snapshot.clone()) })
    }
}

struct RevokedClient;

impl ControlClient for RevokedClient {
    fn refresh<'a>(&'a self, _: &'a DeviceCredential) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async { Err(ControlError::Revoked) })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        _: &'a DeviceCredential,
        _: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        Box::pin(async { Err(ControlError::Revoked) })
    }
}

#[tokio::main]
async fn main() -> HarnessResult<()> {
    let arguments = parse_arguments()?;
    let input = read_input()?;
    let runtime_root = PathBuf::from(required_argument(&arguments, "--runtime-root")?);
    let status_file = PathBuf::from(required_argument(&arguments, "--status-file")?);
    let phase = required_argument(&arguments, "--phase")?;
    fs::create_dir_all(&runtime_root)?;
    let database =
        Arc::new(DbActorHandle::open(&runtime_root.join("agent.sqlite"), "s1b-acceptance").await?);
    let store = Arc::new(FileCredentialStore::new(&runtime_root));

    match phase {
        "pair-control" => pair_and_apply(&database, &store, &input, &status_file).await?,
        "revoke" => revoke(&database, &store, &input, &status_file).await?,
        _ => return Err("unsupported acceptance phase".into()),
    }

    database.checkpoint().await?;
    match Arc::try_unwrap(database) {
        Ok(database) => database.shutdown().await?,
        Err(_) => return Err("acceptance database still shared".into()),
    }
    println!("S1B acceptance Agent phase completed.");
    Ok(())
}

async fn pair_and_apply(
    database: &Arc<DbActorHandle>,
    store: &Arc<FileCredentialStore>,
    input: &HarnessInput,
    status_file: &Path,
) -> HarnessResult<()> {
    let credential = DeviceCredential::new(
        input.device_id.clone(),
        input.workspace_id.clone(),
        input
            .device_access_token
            .as_deref()
            .ok_or("missing access credential")?,
        input
            .refresh_token
            .as_deref()
            .ok_or("missing refresh credential")?,
    )?
    .with_metadata(
        1,
        input.access_expires_at_ms.ok_or("missing access expiry")?,
        input
            .refresh_expires_at_ms
            .ok_or("missing refresh expiry")?,
    );
    let pairing = AgentPairingService::new(
        Arc::clone(database),
        Arc::clone(store) as Arc<dyn CredentialStore>,
        Arc::new(FixedPairingClient {
            credential: credential.clone(),
        }),
    );
    let session = pairing
        .begin(PairingStartHandoff {
            callback_uri: input.callback_uri.clone().ok_or("missing callback URI")?,
        })
        .await
        .map_err(|_| "pairing begin failed")?;
    pairing
        .complete(PairingCallbackHandoff {
            session_id: session.session_id,
            authorization_code: "accepted-callback-code".to_owned(),
        })
        .await?;
    let stored = load_device_credential(store.as_ref())?.ok_or("credential was not stored")?;
    if stored != credential {
        return Err("stored credential did not round trip".into());
    }

    let runtime = CloudControlRuntime::start_from_keychain(
        Arc::clone(database),
        Arc::clone(store) as Arc<dyn CredentialStore>,
        Arc::new(SnapshotClient {
            snapshot: snapshot(input, false),
        }),
    )
    .await?
    .ok_or("paired runtime did not start")?;
    await_revision(&runtime, input.configuration_revision).await?;
    runtime.shutdown().await?;
    let pairing_state = database
        .load_pairing_state()
        .await?
        .ok_or("pairing state missing")?;
    if pairing_state.applied_control_revision != input.configuration_revision {
        return Err("control revision was not durable".into());
    }
    write_status(
        status_file,
        &HarnessStatus {
            phase: "pair-control",
            agent_status: "degraded",
            paired: true,
            applied_control_revision: Some(input.configuration_revision),
            device_id: &input.device_id,
            workspace_id: &input.workspace_id,
        },
    )
}

async fn revoke(
    database: &Arc<DbActorHandle>,
    store: &Arc<FileCredentialStore>,
    input: &HarnessInput,
    status_file: &Path,
) -> HarnessResult<()> {
    for collector_key in ["network", "communication.wechat"] {
        database
            .upsert_collector_state(&CollectorState {
                collector_key: collector_key.to_owned(),
                collector_version: "s1b-acceptance".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: input.configuration_revision,
                applied_config_revision: input.configuration_revision,
                last_event_at_ms: None,
                last_health_at_ms: None,
                last_error_code: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .await?;
    }
    let runtime = CloudControlRuntime::start_from_keychain(
        Arc::clone(database),
        Arc::clone(store) as Arc<dyn CredentialStore>,
        Arc::new(RevokedClient),
    )
    .await?
    .ok_or("revocation runtime did not start")?;
    await_unpaired(&runtime).await?;
    runtime.shutdown().await?;
    if load_device_credential(store.as_ref())?.is_some() {
        return Err("revocation left a Keychain credential".into());
    }
    if database.load_pairing_state().await?.is_some() {
        return Err("revocation left pairing state".into());
    }
    let statuses = database
        .load_collector_states()
        .await?
        .into_iter()
        .map(|state| (state.collector_key, state.status))
        .collect::<BTreeMap<_, _>>();
    for collector_key in ["network", "communication.wechat"] {
        if statuses.get(collector_key) != Some(&CollectorStatus::Disabled) {
            return Err("revocation left a sensitive Collector enabled".into());
        }
    }
    write_status(
        status_file,
        &HarnessStatus {
            phase: "revoke",
            agent_status: "unpaired",
            paired: false,
            applied_control_revision: None,
            device_id: &input.device_id,
            workspace_id: &input.workspace_id,
        },
    )
}

fn snapshot(input: &HarnessInput, revoked: bool) -> AgentControlSnapshot {
    AgentControlSnapshot {
        device_id: input.device_id.clone(),
        workspace_id: input.workspace_id.clone(),
        revoked,
        configuration_revision: input.configuration_revision,
        collectors: CollectorControls {
            network: EnabledControl { enabled: true },
            communication_wechat: WeChatControl {
                enabled: true,
                direction: "outgoing".to_owned(),
                message_type: "text".to_owned(),
                sync_mode: "full".to_owned(),
            },
        },
    }
}

async fn await_revision(
    runtime: &pca_agentd::cloud_control::CloudControlHandle,
    revision: u64,
) -> HarnessResult<()> {
    for _ in 0..100 {
        if runtime.applied_revision().await == Some(revision) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("timed out waiting for control revision".into())
}

async fn await_unpaired(
    runtime: &pca_agentd::cloud_control::CloudControlHandle,
) -> HarnessResult<()> {
    for _ in 0..100 {
        if runtime.is_unpaired().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("timed out waiting for revocation".into())
}

fn write_status(path: &Path, status: &HarnessStatus<'_>) -> HarnessResult<()> {
    let bytes = serde_json::to_vec(status)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    Ok(())
}

fn read_input() -> HarnessResult<HarnessInput> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    Ok(serde_json::from_slice(&input)?)
}

fn parse_arguments() -> HarnessResult<BTreeMap<String, String>> {
    let mut values = env::args().skip(1);
    let mut arguments = BTreeMap::new();
    while let Some(key) = values.next() {
        let value = values.next().ok_or("missing argument value")?;
        arguments.insert(key, value);
    }
    Ok(arguments)
}

fn required_argument<'a>(
    arguments: &'a BTreeMap<String, String>,
    key: &str,
) -> HarnessResult<&'a str> {
    arguments
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing argument: {key}").into())
}
