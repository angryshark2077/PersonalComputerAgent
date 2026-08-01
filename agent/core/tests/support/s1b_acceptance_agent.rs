#![allow(clippy::pedantic)]

use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pca_agentd::cloud_control::{
    AgentControlSnapshot, CloudControlRuntime, ControlClient, ControlError, ControlFuture,
    LoadedDeviceCredentials,
};
use pca_db_local::DbActorHandle;
use pca_domain::{CollectorState, CollectorStatus};
use pca_keychain::{
    load_device_credential, store_device_credential, CredentialError, CredentialStore,
    DeviceCredential, DEVICE_CREDENTIAL_ACCOUNT, DEVICE_CREDENTIAL_SERVICE,
};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

type HarnessResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    api_origin: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallbackInput {
    callback_code: String,
}

#[derive(Serialize)]
struct HarnessStatus<'a> {
    phase: &'a str,
    agent_status: &'a str,
    paired: bool,
    applied_control_revision: Option<u64>,
    device_id: &'a str,
    workspace_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    paired_state_canary_checked: Option<bool>,
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

struct AcceptanceHttpClient {
    client: Client,
    base_url: Url,
}

impl AcceptanceHttpClient {
    fn new(origin: &str) -> Result<Self, ControlError> {
        let base_url = Url::parse(origin).map_err(|_| ControlError::Contract)?;
        if base_url.scheme() != "http"
            || base_url.host_str() != Some("127.0.0.1")
            || base_url.port().is_none()
            || base_url.path() != "/"
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(ControlError::Contract);
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| ControlError::Transient)?;
        Ok(Self { client, base_url })
    }

    fn endpoint(&self, path: &str) -> Result<Url, ControlError> {
        self.base_url.join(path).map_err(|_| ControlError::Contract)
    }

    async fn start_pairing(
        &self,
        device_public_key: &str,
        code_challenge: &str,
        callback_state: &str,
    ) -> Result<PairingSessionResponse, ControlError> {
        let callback_uri = self.endpoint("pca/pair/callback")?.to_string();
        let response = self
            .client
            .post(self.endpoint("v1/device-pairing/sessions")?)
            .json(&PairingSessionRequest {
                device_public_key,
                code_challenge,
                callback_uri: &callback_uri,
                callback_state,
            })
            .send()
            .await
            .map_err(|_| ControlError::Transient)?;
        parse_response(response).await
    }

    async fn exchange(
        &self,
        session_id: &str,
        callback_code: &str,
        verifier: &str,
    ) -> Result<DeviceCredential, ControlError> {
        let response = self
            .client
            .post(self.endpoint("v1/device-pairing/exchange")?)
            .json(&PairingExchangeRequest {
                session_id,
                authorization_code: callback_code,
                code_verifier: verifier,
            })
            .send()
            .await
            .map_err(|_| ControlError::Transient)?;
        let grant = parse_response::<CredentialGrant>(response).await?;
        credential_from_grant(grant, 1)
    }
}

impl ControlClient for AcceptanceHttpClient {
    fn refresh<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
    ) -> ControlFuture<'a, DeviceCredential> {
        Box::pin(async move {
            let response = self
                .client
                .post(self.endpoint("v1/devices/token/refresh")?)
                .bearer_auth(credentials.refresh_credential())
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            let generation = credentials.credential_generation().saturating_add(1);
            credential_from_grant(
                parse_response::<CredentialGrant>(response).await?,
                generation,
            )
        })
    }

    fn heartbeat_and_control<'a>(
        &'a self,
        credentials: &'a DeviceCredential,
        outbox_depth: u64,
    ) -> ControlFuture<'a, AgentControlSnapshot> {
        Box::pin(async move {
            let response = self
                .client
                .post(self.endpoint("v1/agent/control")?)
                .bearer_auth(credentials.access_credential())
                .json(&HeartbeatRequest {
                    heartbeat_id: Uuid::new_v4().to_string(),
                    agent_version: "s1b-acceptance",
                    presence: "online",
                    outbox_depth,
                })
                .send()
                .await
                .map_err(|_| ControlError::Transient)?;
            parse_response::<ControlResponse>(response)
                .await
                .map(|response| response.snapshot)
        })
    }
}

#[derive(Serialize)]
struct PairingSessionRequest<'a> {
    device_public_key: &'a str,
    code_challenge: &'a str,
    callback_uri: &'a str,
    callback_state: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingSessionResponse {
    session_id: String,
    authorization_url: String,
}

#[derive(Serialize)]
struct PairingExchangeRequest<'a> {
    session_id: &'a str,
    authorization_code: &'a str,
    code_verifier: &'a str,
}

#[derive(Serialize)]
struct HeartbeatRequest {
    heartbeat_id: String,
    agent_version: &'static str,
    presence: &'static str,
    outbox_depth: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialGrant {
    workspace_id: String,
    device_id: String,
    device_access_token: String,
    refresh_token: String,
    access_expires_at: String,
    refresh_expires_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlResponse {
    snapshot: AgentControlSnapshot,
    #[allow(dead_code)]
    server_time: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorBody {
    error_code: String,
    #[allow(dead_code)]
    message: String,
    #[allow(dead_code)]
    retryable: bool,
}

#[tokio::main]
async fn main() -> HarnessResult<()> {
    let arguments = parse_arguments()?;
    let input = read_input_line::<StartInput>()?;
    let runtime_root = PathBuf::from(required_argument(&arguments, "--runtime-root")?);
    let status_file = PathBuf::from(required_argument(&arguments, "--status-file")?);
    let phase = required_argument(&arguments, "--phase")?;
    fs::create_dir_all(&runtime_root)?;
    let database =
        Arc::new(DbActorHandle::open(&runtime_root.join("agent.sqlite"), "s1b-acceptance").await?);
    let store = Arc::new(FileCredentialStore::new(&runtime_root));
    let client = Arc::new(
        AcceptanceHttpClient::new(&input.api_origin)
            .map_err(|error| control_failure("invalid local Cloud origin", error))?,
    );

    match phase {
        "pair-control" => {
            pair_and_apply(&database, &store, client, &runtime_root, &status_file).await?
        }
        "revoke" => revoke(&database, &store, client, &status_file).await?,
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
    client: Arc<AcceptanceHttpClient>,
    runtime_root: &Path,
    status_file: &Path,
) -> HarnessResult<()> {
    let verifier = random_base64url();
    let callback_state = loop {
        let candidate = random_base64url();
        if candidate != verifier {
            break candidate;
        }
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let session = client
        .start_pairing(&random_base64url(), &challenge, &callback_state)
        .await
        .map_err(|error| control_failure("pairing start failed", error))?;
    Url::parse(&session.authorization_url)
        .map_err(|_| "pairing start returned an invalid authorization URL")?;
    let callback = read_input_line::<CallbackInput>()?;
    let credential = client
        .exchange(&session.session_id, &callback.callback_code, &verifier)
        .await
        .map_err(|error| control_failure("pairing exchange failed", error))?;
    let device_id = credential.device_id().to_owned();
    let workspace_id = credential.workspace_id().to_owned();
    let access_token = credential.access_credential().to_owned();
    let refresh_token = credential.refresh_credential().to_owned();
    store_device_credential(store.as_ref(), &credential)?;
    let runtime = CloudControlRuntime::start(
        Arc::clone(database),
        LoadedDeviceCredentials::new(credential, Arc::clone(store) as Arc<dyn CredentialStore>),
        client,
    )
    .await?;
    let revision = await_revision(&runtime).await?;
    runtime.shutdown().await?;
    let pairing_state = database
        .load_pairing_state()
        .await?
        .ok_or("pairing state missing")?;
    if pairing_state.applied_control_revision != revision {
        return Err("control revision was not durable".into());
    }
    scan_paired_sqlite(
        runtime_root,
        [
            callback.callback_code.as_str(),
            verifier.as_str(),
            access_token.as_str(),
            refresh_token.as_str(),
        ],
    )?;
    write_status(
        status_file,
        &HarnessStatus {
            phase: "pair-control",
            agent_status: "degraded",
            paired: true,
            applied_control_revision: Some(revision),
            device_id: &device_id,
            workspace_id: &workspace_id,
            paired_state_canary_checked: Some(true),
        },
    )
}

async fn revoke(
    database: &Arc<DbActorHandle>,
    store: &Arc<FileCredentialStore>,
    client: Arc<AcceptanceHttpClient>,
    status_file: &Path,
) -> HarnessResult<()> {
    let credential = load_device_credential(store.as_ref())?.ok_or("missing paired credential")?;
    let device_id = credential.device_id().to_owned();
    let workspace_id = credential.workspace_id().to_owned();
    let revision = database
        .load_pairing_state()
        .await?
        .ok_or("pairing state missing")?
        .applied_control_revision;
    for collector_key in ["network", "communication.wechat"] {
        database
            .upsert_collector_state(&CollectorState {
                collector_key: collector_key.to_owned(),
                collector_version: "s1b-acceptance".to_owned(),
                status: CollectorStatus::Running,
                desired_config_revision: revision,
                applied_config_revision: revision,
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
        client,
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
            device_id: &device_id,
            workspace_id: &workspace_id,
            paired_state_canary_checked: None,
        },
    )
}

async fn await_revision(
    runtime: &pca_agentd::cloud_control::CloudControlHandle,
) -> HarnessResult<u64> {
    for _ in 0..200 {
        if let Some(revision) = runtime
            .applied_revision()
            .await
            .filter(|revision| *revision > 0)
        {
            return Ok(revision);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("timed out waiting for control revision".into())
}

async fn await_unpaired(
    runtime: &pca_agentd::cloud_control::CloudControlHandle,
) -> HarnessResult<()> {
    for _ in 0..200 {
        if runtime.is_unpaired().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("timed out waiting for revocation".into())
}

fn credential_from_grant(
    grant: CredentialGrant,
    generation: u64,
) -> Result<DeviceCredential, ControlError> {
    let access_expires_at_ms = parse_time_ms(&grant.access_expires_at)?;
    let refresh_expires_at_ms = parse_time_ms(&grant.refresh_expires_at)?;
    DeviceCredential::new(
        grant.device_id,
        grant.workspace_id,
        &grant.device_access_token,
        &grant.refresh_token,
    )
    .map(|credential| {
        credential.with_metadata(generation, access_expires_at_ms, refresh_expires_at_ms)
    })
    .map_err(|_| ControlError::Contract)
}

async fn parse_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, ControlError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| ControlError::Transient)?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|_| ControlError::Contract);
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Err(ControlError::Transient);
    }
    if matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::GONE
    ) {
        return match serde_json::from_slice::<ErrorResponse>(&bytes) {
            Ok(error) if error.error.error_code == "DEVICE_REVOKED" => Err(ControlError::Revoked),
            _ => Err(ControlError::InvalidCredential),
        };
    }
    Err(ControlError::Contract)
}

fn parse_time_ms(value: &str) -> Result<i64, ControlError> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| ControlError::Contract)?;
    i64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000).map_err(|_| ControlError::Contract)
}

fn control_failure(context: &str, error: ControlError) -> io::Error {
    io::Error::other(format!("{context}: {error:?}"))
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

fn scan_paired_sqlite<'a>(
    runtime_root: &Path,
    sensitive_values: impl IntoIterator<Item = &'a str>,
) -> HarnessResult<()> {
    let sensitive_values = sensitive_values
        .into_iter()
        .map(str::as_bytes)
        .collect::<Vec<_>>();
    for name in ["agent.sqlite", "agent.sqlite-wal", "agent.sqlite-shm"] {
        let path = runtime_root.join(name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if sensitive_values
            .iter()
            .any(|sensitive| contains_bytes(&bytes, sensitive))
        {
            return Err(format!(
                "paired-state SQLite artifact {name} contained sensitive material"
            )
            .into());
        }
    }
    Ok(())
}

fn contains_bytes(value: &[u8], candidate: &[u8]) -> bool {
    !candidate.is_empty()
        && value
            .windows(candidate.len())
            .any(|window| window == candidate)
}

fn random_base64url() -> String {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn read_input_line<T: for<'de> Deserialize<'de>>() -> HarnessResult<T> {
    let mut input = String::new();
    let bytes_read = io::stdin().lock().read_line(&mut input)?;
    if bytes_read == 0 {
        return Err("missing acceptance input".into());
    }
    Ok(serde_json::from_str(&input)?)
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
