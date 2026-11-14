//! Owner-only local orchestration. GET only reads private state; every effect is an
//! authenticated, CSRF-checked POST recorded by the shared local Jobs journal.
use crate::{
    local,
    network::NetworkContract,
    server::{self, App, WebResult},
};
use axum::{
    Form, Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use dacs_net_contract::{
    ADMIN_ACCESS_DISCLOSURE, ADMIN_ACCESS_DISCLOSURE_REVISION, CONTRACT_REVISION, DeviceId,
    EnrollmentGrant, HostConsent, HostName, LocalJoinResult, TAILNET_SWITCH_DISCLOSURE,
    TailnetAssessment, TailnetClientState, TailnetSwitchConsent,
};
use dacs_protocol::{ApiError, Enrollment, ErrorCode, Invitation, Result, WorkerRegistration};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path as FilePath, PathBuf},
    time::Duration,
};
use time::OffsetDateTime;

const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Serialize, Deserialize, Default)]
struct LocalState {
    review: String,
    endpoint: String,
    ca_file: Option<PathBuf>,
    invitation: Option<Invitation>,
    contract: Option<NetworkContract>,
    // Never serialize LocalState into a response or a job result.
    device_token: Option<String>,
    device_id: Option<String>,
    registration_started: bool,
    consent: Option<HostConsent>,
    consent_sent: bool,
    plan: Option<Value>,
    setup_result: Option<Value>,
    assessment: Option<TailnetAssessment>,
    assessment_id: Option<String>,
    join_result: Option<LocalJoinResult>,
    join_started: bool,
    network_state: Option<Value>,
    grant_ready: bool,
    blocker: Option<String>,
    #[serde(default)]
    registration: Option<WorkerRegistration>,
    #[serde(default)]
    worker_config_file: Option<PathBuf>,
    /// Development-only enrollment with a loopback coordinator that advertises no
    /// network infrastructure: the device registers without network membership.
    #[serde(default)]
    local_only: bool,
}

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route("/onboarding", get(page))
        .route("/ui/onboarding/{operation}", post(form_operation))
        .route("/api/onboarding/{operation}", post(api_operation))
}

fn failure(code: ErrorCode, message: &str) -> ApiError {
    ApiError::new(code, message)
}
fn invalid(message: &str) -> ApiError {
    failure(ErrorCode::Invalid, message)
}
fn conflict(message: &str) -> ApiError {
    failure(ErrorCode::Conflict, message)
}
fn storage_error(_: impl std::fmt::Display) -> ApiError {
    failure(
        ErrorCode::Io,
        "Private onboarding state could not be read or durably written",
    )
}
fn state_path(root: &FilePath) -> PathBuf {
    root.join("onboarding").join("owner.json")
}

fn load(root: &FilePath) -> Result<LocalState> {
    let path = state_path(root);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LocalState::default()),
        Err(e) => return Err(storage_error(e)),
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
            return Err(invalid("Onboarding state must be a regular private file"));
        }
        Ok(_) => {}
    }
    let file = fs::File::open(path).map_err(storage_error)?;
    serde_json::from_reader(file).map_err(storage_error)
}

fn save(root: &FilePath, state: &LocalState) -> Result<()> {
    let path = state_path(root);
    let directory = path.parent().expect("state directory");
    local::private_directory(directory)?;
    let temporary = directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let outcome = (|| {
        let mut file = options.open(&temporary).map_err(storage_error)?;
        serde_json::to_writer(&mut file, state).map_err(storage_error)?;
        file.sync_all().map_err(storage_error)?;
        drop(file);
        fs::rename(&temporary, &path).map_err(storage_error)?;
        #[cfg(unix)]
        fs::File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(storage_error)?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(temporary);
    }
    outcome
}

fn save_private_bytes(path: &FilePath, bytes: &[u8]) -> Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| invalid("Private file needs a directory"))?;
    local::private_directory(directory)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(invalid(
                "Worker configuration must be a regular private file",
            ));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(storage_error(error));
        }
        _ => {}
    }
    let temporary = directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let outcome = (|| {
        let mut file = options.open(&temporary).map_err(storage_error)?;
        file.write_all(bytes).map_err(storage_error)?;
        file.sync_all().map_err(storage_error)?;
        drop(file);
        fs::rename(&temporary, path).map_err(storage_error)?;
        #[cfg(unix)]
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(storage_error)?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(temporary);
    }
    outcome
}

fn owner_status(root: &FilePath, state: &LocalState) -> Value {
    let mut value = status(state);
    value["worker_config_file"] = json!(state.worker_config_file);
    if state.worker_config_file.is_some() {
        value["worker_status"] = match dacs_worker::local_control::status(&root.join("worker")) {
            Ok(status) => json!(status),
            Err(error) => json!({"unavailable":true,"message":error.to_string()}),
        };
    }
    value
}

fn configure_worker(root: &FilePath, state: &mut LocalState, request: &Value) -> Result<()> {
    require_approval(request, "worker_config_approved")?;
    let registration = state
        .registration
        .as_ref()
        .ok_or_else(|| conflict("A confirmed local device enrollment is required"))?;
    let token = state
        .device_token
        .as_ref()
        .ok_or_else(|| conflict("Device credential is unavailable"))?;
    let coordinator_url = if state.local_only {
        // The inspected origin may be a bootstrap-only listener; the owner may name
        // the loopback control origin instead. Never a non-loopback address here.
        match request
            .get("coordinator_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            Some(url) if is_loopback(url) => url.trim_end_matches('/').to_owned(),
            Some(_) => {
                return Err(invalid(
                    "A local-only worker must use a loopback coordinator control origin",
                ));
            }
            None => state.endpoint.clone(),
        }
    } else {
        state
            .contract
            .as_ref()
            .ok_or_else(|| conflict("Network contract is unavailable"))?
            .coordinator
            .overlay_url
            .clone()
    };
    let data_dir = fs::canonicalize(root)
        .map_err(storage_error)?
        .join("worker");
    let credential_file = data_dir.join("device.token");
    let config_file = data_dir.join("worker.json");
    let omp_executable = PathBuf::from(text(request, "omp_executable")?);
    if !omp_executable.is_absolute() || !omp_executable.is_file() {
        return Err(invalid(
            "Provide the absolute path to the installed, pinned OMP executable",
        ));
    }
    let config = dacs_worker::runtime::WorkerConfig {
        coordinator_url,
        credential_file: credential_file.clone(),
        worker_id: state
            .device_id
            .clone()
            .ok_or_else(|| conflict("Device identity is unavailable"))?,
        registration: registration.clone(),
        omp_executable,
        omp_version: registration.omp_version.clone(),
        omp_profile: request
            .get("omp_profile")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_owned),
        workspaces_dir: data_dir.join("workspaces"),
        data_dir,
        ca_certificate: state.ca_file.clone(),
        poll_interval_ms: 2000,
    };
    save_private_bytes(&credential_file, token.as_bytes())?;
    save_private_bytes(
        &config_file,
        &serde_json::to_vec_pretty(&config).map_err(storage_error)?,
    )?;
    state.worker_config_file = Some(config_file);
    Ok(())
}

fn status(state: &LocalState) -> Value {
    // Deliberate projection: neither invitation token, device token nor grant is rendered.
    let invitation = state.invitation.as_ref().map(|i| {
        json!({
            "host_name":i.host_name,"project_id":i.project_id,"expires_at":i.expires_at,
            "revoked":i.revoked,"requirements":i.requirements
        })
    });
    json!({"review":state.review,"endpoint":state.endpoint,"invitation":invitation,
        "contract":state.contract,"device_id":state.device_id,
        "credential_saved":state.device_token.is_some(),"registration_started":state.registration_started,
        "consent":state.consent,"consent_sent":state.consent_sent,"plan":state.plan,
        "setup_result":state.setup_result,"assessment":state.assessment,"assessment_id":state.assessment_id,
        "join_result":state.join_result,"join_started":state.join_started,
        "network_state":state.network_state,"grant_ready":state.grant_ready,"blocker":state.blocker,
        "local_only":state.local_only})
}

async fn page(State(app): State<App>, headers: HeaderMap) -> WebResult<Response> {
    let (_, session) = match server::session(&app, &headers) {
        Ok(value) => value,
        Err(_) => return Ok(Redirect::to("/login").into_response()),
    };
    let principal = app.store.authenticate(&session.bearer)?;
    server::local_admin(&app, &principal)?;
    let root = app.config.data_dir.clone();
    let state =
        tokio::task::spawn_blocking(move || load(&root).map(|state| owner_status(&root, &state)))
            .await
            .map_err(|_| failure(ErrorCode::Internal, "Onboarding state reader failed"))??;
    Ok(Html(dacs_ui::onboarding::render_onboarding(
        &state,
        None,
        &session.csrf,
    ))
    .into_response())
}

async fn api_operation(
    State(app): State<App>,
    headers: HeaderMap,
    Path(operation): Path<String>,
    Json(request): Json<Value>,
) -> WebResult<Json<local::Job>> {
    let principal = server::authenticate(&app, &headers, true)?;
    server::local_admin(&app, &principal)?;
    Ok(Json(start(&app, operation, request)?))
}

async fn form_operation(
    State(app): State<App>,
    headers: HeaderMap,
    Path(operation): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> WebResult<Response> {
    let (principal, _) = server::form_auth(&app, &headers, &form)?;
    server::local_admin(&app, &principal)?;
    let request = serde_json::to_value(form).map_err(|_| invalid("Invalid onboarding form"))?;
    let job = start(&app, operation, request)?;
    Ok(Redirect::to(&format!("/setup/jobs/{}", job.id)).into_response())
}

fn start(app: &App, operation: String, request: Value) -> Result<local::Job> {
    if !matches!(
        operation.as_str(),
        "inspect"
            | "plan"
            | "apply"
            | "register"
            | "consent"
            | "refresh"
            | "assess"
            | "join"
            | "configure_worker"
            | "worker_control"
    ) {
        return Err(invalid("Unknown onboarding operation"));
    }
    let root = app.config.data_dir.clone();
    let runtime = tokio::runtime::Handle::current();
    app.jobs.start(&format!("onboarding_{operation}"), move || {
        // Jobs serializes owner operations, including setup/update jobs; no SQLite lock
        // is held while the async HTTP client or network client is awaited.
        let mut state = load(&root)?;
        let result = runtime.block_on(operate(&root, &mut state, &operation, &request));
        match result {
            Ok(()) => {
                state.blocker = None;
                save(&root, &state)?;
                Ok(owner_status(&root, &state))
            }
            Err(error) => {
                state.blocker = Some(error.message.clone());
                save(&root, &state)?;
                Err(error)
            }
        }
    })
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid("Required onboarding form field is missing"))
}
fn approved(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .is_some_and(|v| v == true || v == "true" || v == "on")
}
fn require_approval(value: &Value, key: &str) -> Result<()> {
    if approved(value, key) {
        Ok(())
    } else {
        Err(failure(
            ErrorCode::Forbidden,
            "Explicit local owner consent is required",
        ))
    }
}
fn json_input(value: &Value, key: &str) -> Result<Value> {
    match value.get(key) {
        Some(Value::String(s)) => {
            serde_json::from_str(s).map_err(|_| invalid("Invalid JSON form field"))
        }
        Some(v) => Ok(v.clone()),
        None => Err(invalid("Required JSON form field is missing")),
    }
}
fn current_review(state: &LocalState, request: &Value) -> Result<()> {
    if state.review.is_empty() || text(request, "review")? != state.review {
        return Err(conflict(
            "Invitation review changed; reload onboarding and review again",
        ));
    }
    Ok(())
}

fn endpoint(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value).map_err(|_| invalid("Invalid coordinator URL"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid(
            "Use a coordinator HTTPS origin without credentials, path or query (loopback HTTP is development-only)",
        ));
    }
    Ok(url)
}

fn is_loopback(endpoint_url: &str) -> bool {
    endpoint(endpoint_url).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    })
}

fn client(state: &LocalState) -> Result<reqwest::Client> {
    endpoint(&state.endpoint)?;
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));
    if let Some(path) = &state.ca_file {
        let pem = fs::read(path)
            .map_err(|_| invalid("Cannot read the selected trusted CA certificate"))?;
        let certificate = reqwest::Certificate::from_pem(&pem)
            .map_err(|_| invalid("Invalid trusted CA PEM certificate"))?;
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|_| invalid("Could not initialize verified HTTPS client"))
}

async fn remote<T: DeserializeOwned>(
    client: &reqwest::Client,
    state: &LocalState,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
    authenticated: bool,
) -> Result<T> {
    let mut request = client.request(method, format!("{}{}", state.endpoint, path));
    if authenticated {
        request = request.bearer_auth(
            state
                .device_token
                .as_deref()
                .ok_or_else(|| conflict("Register this device first"))?,
        );
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    // Never expose reqwest errors, remote error bodies or URLs: they can contain
    // invitation credentials or server-controlled secret echoes.
    let mut response = request.send().await.map_err(|_| failure(ErrorCode::Io, "Coordinator request failed; check connectivity and TLS trust. An interrupted registration or join must be reconciled, not blindly repeated"))?;
    if response.status() == reqwest::StatusCode::CONFLICT {
        return Err(conflict(
            "Coordinator approval, grant issuance or membership reconciliation is pending; refresh after the coordinator owner resolves it",
        ));
    }
    if !response.status().is_success() {
        let code = match response.status().as_u16() {
            400 => ErrorCode::Invalid,
            401 => ErrorCode::Unauthorized,
            403 => ErrorCode::Forbidden,
            404 => ErrorCode::NotFound,
            _ => ErrorCode::Io,
        };
        return Err(failure(
            code,
            &format!(
                "Coordinator returned HTTP {}; check invitation eligibility, device authorization and network service configuration",
                response.status().as_u16()
            ),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure(ErrorCode::Io, "Coordinator response was interrupted"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
            return Err(failure(
                ErrorCode::Io,
                "Coordinator response exceeds the allowed size; any mutation outcome is uncertain",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| failure(ErrorCode::Io, "Coordinator response does not match the supported contract; any mutation outcome is uncertain"))
}

fn definite_rejection(error: &ApiError) -> bool {
    matches!(
        error.code,
        ErrorCode::Invalid
            | ErrorCode::Unauthorized
            | ErrorCode::Forbidden
            | ErrorCode::NotFound
            | ErrorCode::Conflict
    )
}

fn validate_contract(contract: &NetworkContract) -> Result<()> {
    if contract.contract_revision != CONTRACT_REVISION
        || contract.admin_disclosure_revision != ADMIN_ACCESS_DISCLOSURE_REVISION
        || contract.admin_disclosure != ADMIN_ACCESS_DISCLOSURE
        || contract.tailnet_switch_disclosure != TAILNET_SWITCH_DISCLOSURE
    {
        return Err(conflict(
            "Coordinator network disclosure/contract is not this client's current reviewed revision",
        ));
    }
    if url::Url::parse(&contract.login_server)
        .ok()
        .is_none_or(|u| {
            u.scheme() != "https"
                || u.host_str().is_none()
                || !u.username().is_empty()
                || u.password().is_some()
        })
    {
        return Err(invalid(
            "Network login server must be verified HTTPS without credentials",
        ));
    }
    endpoint(&contract.coordinator.bootstrap_url)?;
    endpoint(&contract.coordinator.overlay_url)?;
    Ok(())
}

async fn current_contract(client: &reqwest::Client, state: &LocalState) -> Result<NetworkContract> {
    let contract: NetworkContract = remote(
        client,
        state,
        reqwest::Method::GET,
        "/api/network/contract",
        None,
        false,
    )
    .await?;
    validate_contract(&contract)?;
    let reviewed = state
        .contract
        .as_ref()
        .ok_or_else(|| conflict("Inspect the current network disclosure first"))?;
    if serde_json::to_value(&contract).map_err(storage_error)?
        != serde_json::to_value(reviewed).map_err(storage_error)?
    {
        return Err(conflict(
            "Network contract changed after review; inspect the invitation again before consent or join",
        ));
    }
    Ok(contract)
}

async fn fetch_invitation(
    client: &reqwest::Client,
    state: &LocalState,
    token: &str,
) -> Result<Invitation> {
    let mut url = endpoint(&state.endpoint)?;
    url.path_segments_mut()
        .map_err(|_| invalid("Invalid coordinator origin"))?
        .extend(["api", "invitations", token]);
    let invitation: Invitation =
        remote(client, state, reqwest::Method::GET, url.path(), None, false).await?;
    if invitation.token != token || invitation.revoked || invitation.expires_at <= local::now() {
        return Err(conflict(
            "Invitation is expired, revoked or does not match the requested invitation",
        ));
    }
    Ok(invitation)
}

fn validate_registration(registration: &WorkerRegistration, invitation: &Invitation) -> Result<()> {
    if registration.name.trim().is_empty()
        || registration.omp_version.trim().is_empty()
        || registration.physical_host_id.trim().is_empty()
    {
        return Err(invalid(
            "Fill in the device name, stable physical-host identity and actual installed OMP version before registering",
        ));
    }
    if !registration.owner_consented || registration.consent_host != invitation.host_name {
        return Err(failure(
            ErrorCode::Forbidden,
            "Worker registration must explicitly accept the exact inviting coordinator name",
        ));
    }
    Ok(())
}

fn network_consent(state: &LocalState, request: &Value, id: String) -> Result<HostConsent> {
    require_approval(request, "network_approved")?;
    let contract = state
        .contract
        .as_ref()
        .ok_or_else(|| conflict("Review the network contract first"))?;
    validate_contract(contract)?;
    let host = HostName::try_from(text(request, "network_host")?.to_owned())
        .map_err(|_| invalid("Network host must be a valid single DNS label"))?;
    let accepted_by = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .map_err(|_| invalid("Local OS owner identity is unavailable"))?;
    Ok(HostConsent {
        device_id: DeviceId(id),
        accepted_host: host,
        accepted_by,
        accepted_at: OffsetDateTime::now_utc(),
        disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
        tailnet_switch: None,
    })
}

fn bind_grant(state: &LocalState, grant: &EnrollmentGrant) -> Result<()> {
    let consent = state
        .consent
        .as_ref()
        .ok_or_else(|| conflict("Record network consent first"))?;
    let contract = state
        .contract
        .as_ref()
        .ok_or_else(|| conflict("Review network contract first"))?;
    if grant.contract_revision != CONTRACT_REVISION
        || grant.device_id != consent.device_id
        || grant.host_name != consent.accepted_host
        || grant.login_server != contract.login_server
        || grant.coordinator != contract.coordinator
        || grant.expires_at <= OffsetDateTime::now_utc()
    {
        return Err(conflict(
            "Device grant is expired or does not match the exact reviewed device, host and network endpoints",
        ));
    }
    Ok(())
}

fn switch_consent(
    assessment: &TailnetAssessment,
    request: &Value,
) -> Result<Option<TailnetSwitchConsent>> {
    if !assessment.requires_switch_consent {
        return Ok(None);
    }
    require_approval(request, "switch_approved")?;
    let previous = assessment
        .previous_control_url
        .as_ref()
        .ok_or_else(|| conflict("Current tailnet could not be identified"))?;
    if text(request, "previous_control_url")? != previous {
        return Err(conflict(
            "The existing Tailscale network changed; assess and explicitly approve the new switch",
        ));
    }
    let tailnet = match &assessment.client {
        TailnetClientState::LoggedIn { tailnet, .. } => tailnet.clone(),
        _ => None,
    };
    Ok(Some(TailnetSwitchConsent {
        previous_control_url: previous.clone(),
        previous_tailnet: tailnet,
        accepted_at: OffsetDateTime::now_utc(),
    }))
}

async fn refresh(client: &reqwest::Client, state: &mut LocalState) -> Result<()> {
    let id = state
        .device_id
        .as_deref()
        .ok_or_else(|| conflict("Register this device first"))?;
    let mut url = endpoint(&state.endpoint)?;
    url.path_segments_mut()
        .map_err(|_| invalid("Invalid coordinator origin"))?
        .extend(["api", "network", "devices", id, "refresh"]);
    let _: Value = remote(client, state, reqwest::Method::POST, url.path(), None, true).await?;
    state.network_state = Some(
        remote(
            client,
            state,
            reqwest::Method::GET,
            "/api/network/state",
            None,
            true,
        )
        .await?,
    );
    Ok(())
}

async fn operate(
    root: &FilePath,
    state: &mut LocalState,
    operation: &str,
    request: &Value,
) -> Result<()> {
    if operation == "inspect" {
        let target = endpoint(text(request, "endpoint")?)?
            .as_str()
            .trim_end_matches('/')
            .to_owned();
        let token = text(request, "invitation_token")?;
        if state.registration_started
            && (target != state.endpoint
                || state.invitation.as_ref().is_none_or(|i| i.token != token))
        {
            return Err(conflict(
                "This owner state already has a registration intent. Reconcile/revoke it at that coordinator before changing invitations; its credential will not be overwritten",
            ));
        }
        let mut inspected = LocalState {
            endpoint: target,
            ca_file: request
                .get("ca_file")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            ..LocalState::default()
        };
        let http = client(&inspected)?;
        let invitation = fetch_invitation(&http, &inspected, token).await?;
        inspected.invitation = Some(invitation);
        let contract_result = remote::<NetworkContract>(
            &http,
            &inspected,
            reqwest::Method::GET,
            "/api/network/contract",
            None,
            false,
        )
        .await;
        // Keep the real invitation visible even when infrastructure is not configured.
        let contract = match contract_result {
            Ok(c) => {
                validate_contract(&c)?;
                Some(c)
            }
            Err(error) => {
                inspected.blocker = Some(error.message.clone());
                None
            }
        };
        // A failed fetch never replaces a previously reviewed contract.
        let fetched = contract.is_some();
        if state.registration_started {
            state.invitation = inspected.invitation;
            state.ca_file = inspected.ca_file;
            if contract.is_some() {
                state.contract = contract;
            }
            state.assessment = None;
            state.assessment_id = None;
            state.plan = None;
        } else {
            inspected.contract = contract;
            *state = inspected;
        }
        state.review = uuid::Uuid::new_v4().to_string();
        if state.contract.is_none() && is_loopback(&state.endpoint) {
            if !state.registration_started {
                state.local_only = true;
            }
            if state.local_only {
                return Ok(());
            }
        }
        if state.contract.is_none() {
            return Err(conflict(
                "Invitation inspected, but network infrastructure/disclosure is unavailable. Configure the coordinator bridge and inspect again before registration",
            ));
        }
        if !fetched {
            return Err(conflict(
                "Invitation inspected, but the network disclosure could not be fetched; the previously reviewed contract is kept. Inspect again once the coordinator bridge is reachable",
            ));
        }
        return Ok(());
    }
    current_review(state, request)?;
    if operation == "configure_worker" {
        return configure_worker(root, state, request);
    }
    if operation == "worker_control" {
        use dacs_worker::local_control::{self, ControlAction};
        if state.worker_config_file.is_none() {
            return Err(conflict("Prepare the enrolled worker configuration first"));
        }
        let action = match text(request, "action")? {
            "drain" => ControlAction::Drain,
            "emergency_cancel" => {
                require_approval(request, "emergency_approved")?;
                ControlAction::EmergencyCancel
            }
            "pause_desktop" => ControlAction::PauseDesktop,
            "resume_desktop" => ControlAction::ResumeDesktop,
            _ => return Err(invalid("Unknown local worker control")),
        };
        return local_control::request(&root.join("worker"), action)
            .map_err(|error| failure(ErrorCode::Io, &error.to_string()));
    }
    if operation == "plan" {
        let invitation = state
            .invitation
            .as_ref()
            .ok_or_else(|| conflict("Inspect an invitation first"))?;
        let manifest = &invitation.requirements;
        let _version = text(manifest, "requirements_version")?;
        let packages = manifest.get("required_packages").filter(|p| p.is_array()).ok_or_else(|| invalid("Reviewed requirements need requirements_version and required_packages (catalogue IDs with optional versions); ask the coordinator owner to correct the manifest"))?;
        let extra = request
            .get("packages")
            .map(|_| json_input(request, "packages"))
            .transpose()?
            .unwrap_or_else(|| json!([]));
        let mut selected = packages.as_array().expect("validated array").clone();
        selected.extend(
            extra
                .as_array()
                .ok_or_else(|| invalid("Additional packages must be a JSON array"))?
                .iter()
                .cloned(),
        );
        let declined: Vec<String> = match request.get("declined") {
            Some(Value::String(s)) => s
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|_| invalid("Declined packages must be a list of IDs"))?,
            None => Vec::new(),
        };
        // No invitation secret enters the plan or the installer journal; enrollment
        // belongs to this flow, not the setup engine's external-enrollment adapter.
        state.plan = Some(dacs_setup::plan(
            json!({"operation":"install","packages":selected,"declined":declined}),
        )?);
        state.setup_result = None;
        return Ok(());
    }
    if operation == "apply" {
        let plan = state
            .plan
            .as_ref()
            .ok_or_else(|| conflict("Generate and review a dependency plan first"))?;
        if text(request, "plan_sha256")? != plan["sha256"].as_str().unwrap_or("") {
            return Err(conflict("Dependency plan changed; review its new digest"));
        }
        require_approval(request, "owner_approved")?;
        require_approval(request, "administrator_access_approved")?;
        state.setup_result = Some(dacs_setup::apply(
            plan.clone(),
            json!({"plan_sha256":text(request,"plan_sha256")?,"owner_approved":true,"administrator_access_approved":true,"resume_interrupted":approved(request,"resume_interrupted")}),
            &root.join("setup"),
        )?);
        return Ok(());
    }
    let http = client(state)?;
    if state.local_only && matches!(operation, "consent" | "assess" | "join") {
        return Err(conflict(
            "This PC is enrolled local-only with a loopback development coordinator; network consent, assessment and join do not apply",
        ));
    }
    if state.local_only && operation == "refresh" {
        // Approval is observable as access to the device's coordinator state.
        let approved = remote::<Value>(&http, state, reqwest::Method::GET, "/api/state", None, true)
            .await
            .is_ok();
        state.network_state = Some(json!({"local_only":true,"approved":approved}));
        return Ok(());
    }
    if operation == "register" {
        if state.registration_started {
            return Err(conflict(
                "Registration was already attempted. Use the saved device if present; otherwise ask the coordinator owner to reconcile the uncertain registration before trying another enrollment",
            ));
        }
        require_approval(request, "owner_approved")?;
        let reviewed = state
            .invitation
            .as_ref()
            .ok_or_else(|| conflict("Inspect an invitation first"))?;
        let current = fetch_invitation(&http, state, &reviewed.token).await?;
        if serde_json::to_value(&current).map_err(storage_error)?
            != serde_json::to_value(reviewed).map_err(storage_error)?
        {
            return Err(conflict(
                "Invitation or requirements changed; inspect and review again",
            ));
        }
        if !state.local_only {
            current_contract(&http, state).await?;
        }
        let registration: WorkerRegistration =
            serde_json::from_value(json_input(request, "registration")?)
                .map_err(|_| invalid("Invalid typed worker registration"))?;
        validate_registration(&registration, reviewed)?;
        // Validate all network consent fields before an irreversible enrollment.
        let consent = if state.local_only {
            None
        } else {
            Some(network_consent(state, request, String::new())?)
        };
        let payload = json!({"invitation_token":reviewed.token,"registration":registration});
        state.registration_started = true;
        if let Err(error) = save(root, state) {
            // Nothing was sent: do not let the error path persist an intent.
            state.registration_started = false;
            return Err(error);
        }
        let enrollment: Enrollment = match remote(
            &http,
            state,
            reqwest::Method::POST,
            "/api/enroll",
            Some(&payload),
            false,
        )
        .await
        {
            Ok(enrollment) => enrollment,
            Err(error) => {
                // Only an explicit rejection permits correction and another attempt.
                // Timeout, 5xx, malformed/truncated success or crash remain uncertain.
                if definite_rejection(&error) {
                    state.registration_started = false;
                }
                return Err(error);
            }
        };
        if enrollment.device_token.is_empty() || enrollment.worker.id.is_empty() {
            return Err(conflict(
                "Enrollment response omitted the device credential; coordinator reconciliation is required",
            ));
        }
        // Persist the credential before any subsequent external request. No Enrollment
        // value is ever sent to a browser or to the job journal.
        state.device_token = Some(enrollment.device_token);
        state.device_id = Some(enrollment.worker.id.clone());
        state.registration = Some(enrollment.worker.registration);
        let Some(mut consent) = consent else {
            save(root, state)?;
            return Ok(());
        };
        consent.device_id = DeviceId(enrollment.worker.id);
        state.consent = Some(consent);
        save(root, state)?;
        send_consent(&http, state).await?;
        return Ok(());
    }
    if operation == "consent" {
        current_contract(&http, state).await?;
        require_approval(request, "network_approved")?;
        let consent = state.consent.as_ref().ok_or_else(|| conflict("No saved registration consent is available; reconcile the registration with the coordinator owner"))?;
        if text(request, "network_host")? != consent.accepted_host.as_str() {
            return Err(conflict(
                "The registered network host label cannot be changed; coordinate revocation and re-enrollment",
            ));
        }
        if state.join_started {
            return Err(conflict(
                "Network consent was already used for joining; refresh actual membership instead",
            ));
        }
        // Replay the exact saved consent, including accepted_at. A failed response
        // may still have committed it and issued a grant at the coordinator.
        state.consent_sent = false;
        save(root, state)?;
        return send_consent(&http, state).await;
    }
    if operation == "refresh" {
        state.grant_ready = false;
        // Read safe authorization state even if membership refresh is not ready yet.
        state.network_state = Some(
            remote(
                &http,
                state,
                reqwest::Method::GET,
                "/api/network/state",
                None,
                true,
            )
            .await?,
        );
        if state.join_result.is_some() || state.join_started {
            return refresh(&http, state).await;
        }
        let grant: EnrollmentGrant = remote(
            &http,
            state,
            reqwest::Method::GET,
            "/api/network/grant",
            None,
            true,
        )
        .await?;
        bind_grant(state, &grant)?;
        state.grant_ready = true;
        return Ok(());
    }
    if operation == "assess" {
        let contract = current_contract(&http, state).await?;
        state.assessment = Some(dacs_netclient::assess(&contract.login_server).await);
        state.assessment_id = Some(uuid::Uuid::new_v4().to_string());
        return Ok(());
    }
    if operation == "join" {
        require_approval(request, "join_approved")?;
        if state.join_started {
            return Err(conflict(
                "A join was already attempted. Refresh real membership and reconcile the local Tailscale profile before any new join; a consumed key is never retried automatically",
            ));
        }
        if state.assessment_id.as_deref() != Some(text(request, "assessment_id")?) {
            return Err(conflict(
                "Tailnet assessment changed; reload and review again",
            ));
        }
        let contract = current_contract(&http, state).await?;
        let grant: EnrollmentGrant = remote(
            &http,
            state,
            reqwest::Method::GET,
            "/api/network/grant",
            None,
            true,
        )
        .await?;
        bind_grant(state, &grant)?;
        let assessed = dacs_netclient::assess(&contract.login_server).await;
        let reviewed = state
            .assessment
            .as_ref()
            .ok_or_else(|| conflict("Assess this PC's current Tailscale network first"))?;
        if reviewed.previous_control_url != assessed.previous_control_url
            || reviewed.requires_switch_consent != assessed.requires_switch_consent
            || previous_tailnet(reviewed) != previous_tailnet(&assessed)
        {
            state.assessment = Some(assessed);
            state.assessment_id = Some(uuid::Uuid::new_v4().to_string());
            return Err(conflict(
                "Local Tailscale network changed; review the new assessment and give separate switch consent",
            ));
        }
        let switch = switch_consent(&assessed, request)?;
        // The coordinator's original HostConsent is immutable after grant issuance.
        // Switch consent is a separate local owner decision bound to this fresh
        // assessment: it is checked on a copy and persisted only together with the
        // join intent, so an attempt that never starts leaves the saved consent as sent.
        let original = state
            .consent
            .clone()
            .ok_or_else(|| conflict("Record host network consent first"))?;
        let mut consent = original.clone();
        consent.tailnet_switch = switch;
        dacs_netclient::check_join(&grant, &consent, &assessed, OffsetDateTime::now_utc()).map_err(|_| conflict("Join prerequisites or consent no longer match; inspect the assessment, Tailscale service and grant expiry"))?;
        state.join_started = true;
        state.consent = Some(consent.clone());
        if let Err(error) = save(root, state) {
            // Nothing was started: do not let the error path persist an intent.
            state.join_started = false;
            state.consent = Some(original);
            return Err(error);
        }
        let result = match dacs_netclient::join(
            &grant,
            &consent,
            &dacs_netclient::JoinOptions {
                accept_dns: approved(request, "accept_dns"),
                ..Default::default()
            },
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                if let Some(not_started) = join_not_started(&error) {
                    // Rejected before `tailscale login`: the key is unused.
                    state.join_started = false;
                    state.consent = Some(original);
                    save(root, state)?;
                    return Err(not_started);
                }
                return Err(conflict(
                    "Network join did not confirm success. Refresh membership and inspect the local Tailscale profile; do not blindly repeat a possibly consumed grant",
                ));
            }
        };
        state.join_result = Some(result);
        state.grant_ready = false;
        save(root, state)?;
        refresh(&http, state).await?;
        return Ok(());
    }
    Err(invalid("Unknown onboarding operation"))
}

/// Join errors raised before `tailscale login` is started, so the enrollment key
/// is certainly unused. Every other error may follow a consumed key.
fn join_not_started(error: &dacs_netclient::NetError) -> Option<ApiError> {
    use dacs_netclient::NetError;
    match error {
        NetError::Prerequisite(_)
        | NetError::Consent(_)
        | NetError::SwitchConsentRequired { .. }
        | NetError::Grant(_)
        | NetError::AlreadyJoined => Some(conflict(&format!(
            "Network join was not started and the grant is unused: {error}. Resolve this, assess again and retry the join"
        ))),
        NetError::Cli(_)
        | NetError::NotDacs { .. }
        | NetError::JoinTimeout { .. }
        | NetError::Io(_) => None,
    }
}

fn previous_tailnet(assessment: &TailnetAssessment) -> Option<&str> {
    match &assessment.client {
        TailnetClientState::LoggedIn { tailnet, .. } => tailnet.as_deref(),
        _ => None,
    }
}

async fn send_consent(client: &reqwest::Client, state: &mut LocalState) -> Result<()> {
    let payload = serde_json::to_value(
        state
            .consent
            .as_ref()
            .ok_or_else(|| conflict("Host network consent is missing"))?,
    )
    .map_err(storage_error)?;
    let _: Value = remote(
        client,
        state,
        reqwest::Method::POST,
        "/api/network/consent",
        Some(&payload),
        true,
    )
    .await?;
    state.consent_sent = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_enrollment_outcomes_never_enable_an_automatic_retry() {
        assert!(!definite_rejection(&failure(
            ErrorCode::Io,
            "interrupted or malformed response"
        )));
        assert!(!definite_rejection(&failure(
            ErrorCode::Internal,
            "unconfirmed operation"
        )));
        assert!(definite_rejection(&failure(
            ErrorCode::Forbidden,
            "explicit rejection"
        )));
        assert!(definite_rejection(&failure(
            ErrorCode::Invalid,
            "invalid registration"
        )));
    }

    #[test]
    fn credential_projection_does_not_include_private_tokens() {
        let mut state = LocalState {
            device_token: Some("private-device-token".into()),
            ..Default::default()
        };
        state.invitation = Some(Invitation {
            token: "private-invite-token".into(),
            project_id: "p".into(),
            host_name: "coordinator".into(),
            expires_at: i64::MAX,
            requirements: json!({}),
            revoked: false,
        });
        let rendered = serde_json::to_string(&status(&state)).unwrap();
        assert!(!rendered.contains("private-device-token"));
        assert!(!rendered.contains("private-invite-token"));
        assert!(rendered.contains("credential_saved"));
    }

    #[test]
    fn local_only_worker_config_targets_a_loopback_coordinator_only() {
        let root = tempfile::tempdir().unwrap();
        let mut state = LocalState {
            endpoint: "http://127.0.0.1:7447".into(),
            local_only: true,
            device_token: Some("device-token".into()),
            device_id: Some("worker-1".into()),
            registration: Some(WorkerRegistration {
                name: "w".into(),
                physical_host_id: "h".into(),
                os: "macos".into(),
                architecture: "arm64".into(),
                capabilities: vec![],
                capacity: Default::default(),
                omp_version: "18.16.0".into(),
                policy_revision: 1,
                owner_consented: true,
                consent_host: "c".into(),
            }),
            ..Default::default()
        };
        // Any absolute path to an existing file passes the executable check; the test's own
        // binary exists on every platform, unlike `/bin/sh`.
        let executable = std::env::current_exe().unwrap();
        let executable = executable.to_str().unwrap().to_owned();
        let request = |url: Option<&str>| {
            let mut value = json!({"omp_executable":executable,"worker_config_approved":"on"});
            if let Some(url) = url {
                value["coordinator_url"] = json!(url);
            }
            value
        };
        let url = |state: &LocalState| {
            let config: Value =
                serde_json::from_slice(&fs::read(state.worker_config_file.as_ref().unwrap()).unwrap())
                    .unwrap();
            config["coordinator_url"].as_str().unwrap().to_owned()
        };
        configure_worker(root.path(), &mut state, &request(None)).unwrap();
        assert_eq!(url(&state), "http://127.0.0.1:7447");
        fs::remove_file(state.worker_config_file.take().unwrap()).unwrap();
        fs::remove_file(root.path().join("worker").join("device.token")).ok();
        configure_worker(root.path(), &mut state, &request(Some("https://localhost:7443/"))).unwrap();
        assert_eq!(url(&state), "https://localhost:7443");
        let err = configure_worker(root.path(), &mut state, &request(Some("https://coordinator.example.test")))
            .unwrap_err();
        assert!(err.message.contains("loopback"), "{}", err.message);
        // Without local-only mode a worker still needs the reviewed network contract.
        state.local_only = false;
        assert!(configure_worker(root.path(), &mut state, &request(None)).is_err());
        assert!(is_loopback("http://[::1]:7447") && !is_loopback("https://192.0.2.1"));
    }

    #[test]
    fn arbitrary_remote_http_and_embedded_credentials_are_rejected() {
        assert!(endpoint("http://192.0.2.1:7444").is_err());
        assert!(endpoint("https://owner:secret@example.test").is_err());
        assert!(endpoint("https://example.test?token=secret").is_err());
        assert!(endpoint("http://127.0.0.1:7444").is_ok());
    }

    #[test]
    fn switch_approval_is_separate_and_binds_exact_previous_control_server() {
        let assessment = dacs_netclient::assessment(
            TailnetClientState::LoggedIn {
                control_url: "https://old.example.test".into(),
                backend_state: "Running".into(),
                tailnet: Some("old-network".into()),
                self_name: None,
                overlay_ips: vec![],
            },
            "https://new.example.test",
        );
        assert!(
            switch_consent(
                &assessment,
                &json!({"join_approved":true,"previous_control_url":"https://old.example.test"})
            )
            .is_err()
        );
        assert!(
            switch_consent(
                &assessment,
                &json!({"switch_approved":true,"previous_control_url":"https://other.example.test"})
            )
            .is_err()
        );
        let consent = switch_consent(
            &assessment,
            &json!({"switch_approved":true,"previous_control_url":"https://old.example.test"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(consent.previous_tailnet.as_deref(), Some("old-network"));
    }

    #[test]
    fn stale_review_cannot_authorize_another_invitation() {
        let state = LocalState {
            review: "current".into(),
            ..Default::default()
        };
        assert!(current_review(&state, &json!({"review":"old"})).is_err());
        assert!(current_review(&state, &json!({"review":"current"})).is_ok());
    }

    #[test]
    fn only_join_errors_before_tailscale_login_release_the_join_latch() {
        use dacs_netclient::NetError;
        for unused in [
            NetError::Prerequisite("Tailscale service is not running".into()),
            NetError::Consent("consent belongs to another device".into()),
            NetError::SwitchConsentRequired {
                previous: "https://old.example.test".into(),
            },
            NetError::Grant("enrollment key expired; request a new grant".into()),
            NetError::AlreadyJoined,
        ] {
            let error = join_not_started(&unused).expect("pre-effect join error");
            assert_eq!(error.code, ErrorCode::Conflict);
            assert!(error.message.contains(&unused.to_string()));
        }
        for uncertain in [
            NetError::JoinTimeout {
                login_server: "https://login.example.test".into(),
                secs: 30,
                state: String::new(),
            },
            NetError::Io(std::io::Error::other("interrupted")),
            NetError::NotDacs {
                active: "https://old.example.test".into(),
                dacs: "https://login.example.test".into(),
            },
        ] {
            assert!(join_not_started(&uncertain).is_none());
        }
    }

    #[tokio::test]
    async fn reinspection_keeps_the_reviewed_contract_when_the_fetch_fails() {
        const TOKEN: &str = "private-invite-token";
        fn invitation() -> Invitation {
            Invitation {
                token: TOKEN.into(),
                project_id: "p".into(),
                host_name: "coordinator".into(),
                expires_at: i64::MAX,
                requirements: json!({}),
                revoked: false,
            }
        }
        // The coordinator still serves the invitation; its network bridge is down.
        let coordinator = Router::new()
            .route(
                "/api/invitations/{token}",
                get(|| async { Json(invitation()) }),
            )
            .route(
                "/api/network/contract",
                get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, coordinator).await.unwrap();
        });
        let reviewed = NetworkContract {
            contract_revision: CONTRACT_REVISION.into(),
            admin_disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
            admin_disclosure: ADMIN_ACCESS_DISCLOSURE.into(),
            tailnet_switch_disclosure: TAILNET_SWITCH_DISCLOSURE.into(),
            login_server: "https://login.example.test".into(),
            coordinator: dacs_net_contract::CoordinatorEndpoints {
                bootstrap_url: "https://bootstrap.example.test".into(),
                overlay_url: "https://overlay.example.test".into(),
            },
            key_ttl_minutes: 10,
        };
        let expected = serde_json::to_value(&reviewed).unwrap();
        let mut state = LocalState {
            review: "previous".into(),
            endpoint: base.clone(),
            invitation: Some(invitation()),
            contract: Some(reviewed),
            registration_started: true,
            ..Default::default()
        };
        let error = operate(
            FilePath::new("unused"),
            &mut state,
            "inspect",
            &json!({"endpoint":base,"invitation_token":TOKEN}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert_eq!(
            serde_json::to_value(&state.contract).unwrap(),
            expected,
            "an unreachable bridge must not erase the reviewed contract"
        );
        assert_ne!(state.review, "previous");
        server.abort();
    }
}
