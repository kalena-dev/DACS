//! Bearer-only network bridge. Secrets are transported only to their device.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dacs_core::{Store, network::NetworkPhase};
use dacs_net_contract::{
    ADMIN_ACCESS_DISCLOSURE, ADMIN_ACCESS_DISCLOSURE_REVISION, CONTRACT_REVISION,
    CoordinatorEndpoints, EnrollmentGrant, HostConsent, NetworkMembership, RevocationOutcome,
    TAILNET_SWITCH_DISCLOSURE,
};
use dacs_protocol::{ApiError, ErrorCode, Principal, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};
use url::Url;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    pub netadmin_url: String,
    pub operation_token_file: PathBuf,
    pub ca_file: Option<PathBuf>,
}

/// Safe, allowlisted projection of the netadmin's existing contract response.
#[derive(Clone, Serialize, Deserialize)]
pub struct NetworkContract {
    pub contract_revision: String,
    pub admin_disclosure_revision: String,
    pub admin_disclosure: String,
    pub tailnet_switch_disclosure: String,
    pub login_server: String,
    pub coordinator: CoordinatorEndpoints,
    pub key_ttl_minutes: i64,
}

struct Transport {
    client: reqwest::Client,
    base: Url,
    token: HeaderValue,
}

#[derive(Clone)]
pub struct NetworkService {
    store: Store,
    transport: Option<Arc<Transport>>,
    // Serialize external operations without retaining a SQLite lock across await.
    operations: Arc<tokio::sync::Mutex<()>>,
    availability: Arc<RwLock<Option<bool>>>,
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}
fn unavailable() -> ApiError {
    ApiError::new(
        ErrorCode::Io,
        "network infrastructure unavailable; any pending operation remains unresolved",
    )
}
fn administrator() -> Principal {
    Principal {
        is_admin: true,
        device_id: None,
    }
}

fn endpoint(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| invalid("invalid network service URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
    {
        return Err(invalid(
            "network URLs require verified HTTPS; explicit loopback HTTP is development-only",
        ));
    }
    Ok(url)
}

fn local_file(path: &PathBuf, maximum: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).map_err(|_| invalid("cannot open network configuration file"))?;
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("cannot read network configuration file"))?;
    if bytes.len() as u64 > maximum {
        return Err(invalid("network configuration file exceeds size limit"));
    }
    Ok(bytes)
}

impl NetworkService {
    pub fn new(store: Store, config: Option<NetworkConfig>) -> Result<Self> {
        let transport = config
            .map(|config| -> Result<Arc<Transport>> {
                let mut base = endpoint(&config.netadmin_url)?;
                if !base.path().ends_with('/') {
                    base.set_path(&format!("{}/", base.path()));
                }
                let bytes = local_file(&config.operation_token_file, 4096)?;
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| invalid("invalid operation token encoding"))?
                    .trim();
                if text.len() < 32 || text.chars().any(char::is_whitespace) {
                    return Err(invalid(
                        "operation token must contain at least 32 non-whitespace bytes",
                    ));
                }
                let mut token = HeaderValue::from_str(&format!("Bearer {text}"))
                    .map_err(|_| invalid("invalid operation token encoding"))?;
                token.set_sensitive(true);
                let mut builder = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(Duration::from_secs(20))
                    .no_proxy();
                if let Some(path) = config.ca_file {
                    let bytes = local_file(&path, 1024 * 1024)?;
                    let cert = reqwest::Certificate::from_pem(&bytes)
                        .map_err(|_| invalid("invalid trusted network CA certificate"))?;
                    builder = builder.add_root_certificate(cert);
                }
                let client = builder
                    .build()
                    .map_err(|_| invalid("cannot initialize verified network transport"))?;
                Ok(Arc::new(Transport {
                    client,
                    base,
                    token,
                }))
            })
            .transpose()?;
        Ok(Self {
            store,
            transport,
            operations: Arc::new(tokio::sync::Mutex::new(())),
            availability: Arc::new(RwLock::new(None)),
        })
    }

    fn configured(&self) -> Result<&Transport> {
        self.transport.as_deref().ok_or_else(unavailable)
    }

    async fn call<T: DeserializeOwned>(&self, route: &str, body: Option<Value>) -> Result<T> {
        let transport = self.configured()?;
        let url = transport
            .base
            .join(route)
            .map_err(|_| invalid("invalid netadmin route"))?;
        let request = match body {
            Some(body) => transport.client.post(url).json(&body),
            None => transport.client.get(url),
        }
        .header(header::AUTHORIZATION, transport.token.clone());
        let outcome = async {
            let mut response = request.send().await.map_err(|_| unavailable())?;
            if !response.status().is_success() {
                // Upstream error bodies can contain credentials or infrastructure details.
                return Err(unavailable());
            }
            const MAX_BODY: usize = 256 * 1024;
            if response
                .content_length()
                .is_some_and(|n| n > MAX_BODY as u64)
            {
                return Err(unavailable());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
                if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
                    return Err(unavailable());
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| unavailable())
        }
        .await;
        if let Ok(mut available) = self.availability.write() {
            *available = Some(outcome.is_ok());
        }
        outcome
    }

    pub async fn contract(&self) -> Result<NetworkContract> {
        let contract: NetworkContract = self.call("v1/contract", None).await?;
        if contract.contract_revision != CONTRACT_REVISION
            || contract.admin_disclosure_revision != ADMIN_ACCESS_DISCLOSURE_REVISION
            || contract.admin_disclosure != ADMIN_ACCESS_DISCLOSURE
            || contract.tailnet_switch_disclosure != TAILNET_SWITCH_DISCLOSURE
            || contract.key_ttl_minutes <= 0
            || endpoint(&contract.login_server).is_err()
            || endpoint(&contract.coordinator.bootstrap_url).is_err()
            || endpoint(&contract.coordinator.overlay_url).is_err()
        {
            if let Ok(mut available) = self.availability.write() {
                *available = Some(false);
            }
            return Err(unavailable());
        }
        #[derive(Deserialize)]
        struct Health {
            ok: bool,
            contract_revision: String,
        }
        let health: Health = self.call("v1/health", None).await?;
        if !health.ok || health.contract_revision != CONTRACT_REVISION {
            if let Ok(mut available) = self.availability.write() {
                *available = Some(false);
            }
            return Err(unavailable());
        }
        Ok(contract)
    }

    pub fn snapshot(&self, principal: &Principal) -> Result<Value> {
        let mut value = self.store.network_snapshot(principal)?;
        let reachable = *self.availability.read().map_err(|_| {
            ApiError::new(ErrorCode::Internal, "network availability lock poisoned")
        })?;
        value["configured"] = json!(self.transport.is_some());
        value["reachable"] = json!(reachable);
        value["availability"] = json!(match (self.transport.is_some(), reachable) {
            (false, _) => "unavailable_unconfigured",
            (true, Some(true)) => "available",
            (true, Some(false)) => "unavailable",
            (true, None) => "not_checked",
        });
        Ok(value)
    }

    pub async fn maintain(&self) -> Result<()> {
        if self.transport.is_none() {
            // Nothing to maintain without a network transport; not an error.
            return Ok(());
        }
        // Issuance and membership refresh require a valid advertised contract;
        // revocation does not and must proceed regardless.
        let contract_error = self.contract().await.err();
        let mut first_error = None;
        for (worker, _) in self.store.network_records()? {
            // Hold the operations lock for one device at a time so an
            // administrator's request never waits behind the whole fleet, and
            // decide from the record as it is once the lock is held.
            let _operation = self.operations.lock().await;
            let result = match self.store.network_record(&administrator(), &worker.id) {
                Err(error) => Err(error),
                Ok(record) => {
                    // Worker revocation is permanent, so the listed flag cannot go
                    // stale; a revocation committed since the listing shows as phase.
                    if worker.revoked || record.phase == NetworkPhase::RevocationPending {
                        self.revoke_locked(&worker.id).await
                    } else if contract_error.is_some() {
                        Ok(())
                    } else if record.phase == NetworkPhase::AwaitingApproval {
                        self.issue_locked(&worker.id).await
                    } else if matches!(
                        record.phase,
                        NetworkPhase::GrantIssued | NetworkPhase::ReferencesRecovered
                    ) {
                        self.refresh_locked(&administrator(), &worker.id).await
                    } else {
                        Ok(())
                    }
                }
            };
            if let Err(error) = result {
                // With the contract check already failing, a transport failure means
                // netadmin is unreachable: do not spend a timeout on every device.
                let unreachable = contract_error.is_some() && error.code == ErrorCode::Io;
                first_error.get_or_insert(error);
                if unreachable {
                    break;
                }
            }
        }
        match contract_error.or(first_error) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn issue_locked(&self, device: &str) -> Result<()> {
        let Some((operation, request)) = self.store.network_begin_issue(device)? else {
            return Ok(());
        };
        let grant: EnrollmentGrant = self.call("v1/enrollments", Some(json!(request))).await?;
        if endpoint(&grant.login_server).is_err()
            || endpoint(&grant.coordinator.bootstrap_url).is_err()
            || endpoint(&grant.coordinator.overlay_url).is_err()
        {
            return Err(unavailable());
        }
        // Once intent is durable, every failure (including malformed success) is uncertain.
        // Only explicit audit-derived reconciliation can recover a missing response.
        self.store.network_finish_issue(device, &operation, grant)
    }

    async fn refresh_locked(&self, principal: &Principal, device: &str) -> Result<()> {
        let (operation, query) = self.store.network_begin_refresh(principal, device)?;
        let membership: NetworkMembership = self
            .call("v1/memberships/query", Some(json!(query)))
            .await?;
        self.store
            .network_membership(device, &operation, membership)
    }

    async fn revoke_locked(&self, device: &str) -> Result<()> {
        let record = self.store.network_revoke(&administrator(), device)?;
        if record.phase == NetworkPhase::Revoked {
            return Ok(());
        }
        let request = record.revocation_request()?;
        let outcome: RevocationOutcome = self.call("v1/revocations", Some(json!(request))).await?;
        let complete = outcome.verified_absent;
        self.store.network_finish_revoke(device, outcome)?;
        if !complete {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "network revocation remains pending; removal is not verified",
            ));
        }
        Ok(())
    }
}

struct Failure(ApiError);
impl From<ApiError> for Failure {
    fn from(value: ApiError) -> Self {
        Self(value)
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::Invalid => StatusCode::BAD_REQUEST,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict => StatusCode::CONFLICT,
            ErrorCode::Io | ErrorCode::CapacityUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0)).into_response()
    }
}

fn bearer(service: &NetworkService, headers: &HeaderMap) -> Result<Principal> {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return Err(ApiError::new(
            ErrorCode::Unauthorized,
            "one device or administrator bearer credential required",
        ));
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::new(ErrorCode::Unauthorized, "bearer credential required"))?;
    service.store.authenticate(token)
}

fn device_only(principal: &Principal) -> Result<()> {
    if principal.is_admin || principal.device_id.is_none() {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "device identity required",
        ));
    }
    Ok(())
}

async fn public_contract(
    State(service): State<NetworkService>,
) -> std::result::Result<Json<NetworkContract>, Failure> {
    Ok(Json(service.contract().await?))
}
async fn consent(
    State(service): State<NetworkService>,
    headers: HeaderMap,
    Json(consent): Json<HostConsent>,
) -> std::result::Result<Json<Value>, Failure> {
    let principal = bearer(&service, &headers)?;
    device_only(&principal)?;
    service.contract().await?;
    service.store.network_consent(&principal, consent)?;
    Ok(Json(service.snapshot(&principal)?))
}
async fn grant(
    State(service): State<NetworkService>,
    headers: HeaderMap,
) -> std::result::Result<Json<EnrollmentGrant>, Failure> {
    let principal = bearer(&service, &headers)?;
    service.configured()?;
    Ok(Json(service.store.network_grant(&principal)?))
}
async fn state(
    State(service): State<NetworkService>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, Failure> {
    let principal = bearer(&service, &headers)?;
    Ok(Json(service.snapshot(&principal)?))
}
async fn device_state(
    State(service): State<NetworkService>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, Failure> {
    let principal = bearer(&service, &headers)?;
    device_only(&principal)?;
    Ok(Json(service.snapshot(&principal)?))
}
async fn refresh(
    State(service): State<NetworkService>,
    Path(device): Path<String>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, Failure> {
    let principal = bearer(&service, &headers)?;
    let _operation = service.operations.lock().await;
    service.refresh_locked(&principal, &device).await?;
    Ok(Json(service.snapshot(&principal)?))
}
async fn device_refresh(
    State(service): State<NetworkService>,
    Path(device): Path<String>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, Failure> {
    let principal = bearer(&service, &headers)?;
    device_only(&principal)?;
    let _operation = service.operations.lock().await;
    service.refresh_locked(&principal, &device).await?;
    Ok(Json(service.snapshot(&principal)?))
}
async fn revoke(
    State(service): State<NetworkService>,
    Path(device): Path<String>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, Failure> {
    let principal = bearer(&service, &headers)?;
    // Revoke app authorization immediately, even if netadmin is down/unconfigured
    // or another external operation is in flight.
    service.store.network_revoke(&principal, &device)?;
    let _operation = service.operations.lock().await;
    service.revoke_locked(&device).await?;
    Ok(Json(service.snapshot(&principal)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconcileReferences {
    auth_key_id: Option<u64>,
    node_id: Option<u64>,
}
async fn reconcile(
    State(service): State<NetworkService>,
    Path(device): Path<String>,
    headers: HeaderMap,
    Json(references): Json<ReconcileReferences>,
) -> std::result::Result<Json<Value>, Failure> {
    let principal = bearer(&service, &headers)?;
    let _operation = service.operations.lock().await;
    service.store.network_reconcile(
        &principal,
        &device,
        references.auth_key_id,
        references.node_id,
    )?;
    let record = service.store.network_record(&principal, &device)?;
    if record.phase == NetworkPhase::RevocationPending {
        service.revoke_locked(&device).await?;
    } else {
        service.refresh_locked(&principal, &device).await?;
    }
    Ok(Json(service.snapshot(&principal)?))
}

async fn private_response(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn routes(service: NetworkService, bootstrap: bool) -> Router {
    let router = Router::new()
        .route("/api/network/contract", get(public_contract))
        .route("/api/network/consent", post(consent))
        .route("/api/network/grant", get(grant));
    let router = if bootstrap {
        router
            .route("/api/network/state", get(device_state))
            .route("/api/network/devices/{id}/refresh", post(device_refresh))
    } else {
        router
            .route("/api/network/state", get(state))
            .route("/api/network/devices/{id}/refresh", post(refresh))
            .route("/api/network/devices/{id}/revoke", post(revoke))
            .route("/api/network/devices/{id}/reconcile", post(reconcile))
    };
    router
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::map_response(private_response))
        .with_state(service)
}

pub fn router(service: NetworkService) -> Router {
    routes(service, false)
}
pub fn bootstrap_router(service: NetworkService) -> Router {
    routes(service, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dacs_protocol::{
        Command, CommandEnvelope, Invitation, PROTOCOL_VERSION, Project, Resources,
        WorkerRegistration,
    };

    const ADMIN: &str = "network-http-tests-administrator-token-32-bytes";
    fn command(store: &Store, command: Command) -> Value {
        store
            .execute(
                &administrator(),
                CommandEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    command_id: uuid::Uuid::new_v4().to_string(),
                    command,
                },
            )
            .unwrap()
            .result
    }
    fn enrollment(store: &Store) -> (String, String) {
        let project: Project = serde_json::from_value(command(
            store,
            Command::CreateProject {
                name: "network routes".into(),
                target_branch: "main".into(),
                max_tasks: 10,
                max_depth: 2,
            },
        ))
        .unwrap();
        let invitation: Invitation = serde_json::from_value(command(
            store,
            Command::CreateInvitation {
                project_id: project.id,
                host_name: "inviting-host".into(),
                expires_in_seconds: 3600,
            },
        ))
        .unwrap();
        let enrollment = store
            .enroll(
                &invitation.token,
                WorkerRegistration {
                    name: "local-pc".into(),
                    physical_host_id: uuid::Uuid::new_v4().to_string(),
                    os: "linux".into(),
                    architecture: "x86_64".into(),
                    capabilities: vec![],
                    capacity: Resources::default(),
                    omp_version: "test".into(),
                    policy_revision: 1,
                    owner_consented: true,
                    consent_host: invitation.host_name,
                },
            )
            .unwrap();
        (enrollment.worker.id, enrollment.device_token)
    }
    async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[test]
    fn endpoints_require_verified_tls_except_explicit_loopback_development() {
        for bad in [
            "http://example.test",
            "https://user:secret@example.test",
            "https://example.test/?token=secret",
            "file:///tmp/network",
        ] {
            assert!(endpoint(bad).is_err());
        }
        assert!(endpoint("https://infra.example.test:9443").is_ok());
        assert!(endpoint("http://127.0.0.1:9443").is_ok());
        assert!(endpoint("http://[::1]:9443").is_ok());
    }

    #[tokio::test]
    async fn unconfigured_contract_is_unavailable_and_cookies_cannot_authorize() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (device, token) = enrollment(&store);
        let service = NetworkService::new(store, None).unwrap();
        let (base, task) = serve(router(service)).await;
        let client = reqwest::Client::new();
        let contract = client
            .get(format!("{base}/api/network/contract"))
            .send()
            .await
            .unwrap();
        assert_eq!(contract.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(contract.headers()[header::CACHE_CONTROL], "no-store");
        let cookie = client
            .get(format!("{base}/api/network/state"))
            .header(header::COOKIE, format!("dacs_session={ADMIN}"))
            .send()
            .await
            .unwrap();
        assert_eq!(cookie.status(), StatusCode::UNAUTHORIZED);
        let state: Value = client
            .get(format!("{base}/api/network/state"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["configured"], false);
        assert_eq!(state["availability"], "unavailable_unconfigured");
        assert_eq!(state["devices"][0]["device_id"], device);
        assert!(!state.to_string().contains(&token));
        task.abort();
    }

    #[tokio::test]
    async fn bootstrap_rejects_admin_and_cross_device_refresh_and_has_no_revoke_route() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (first, token) = enrollment(&store);
        let (second, _) = enrollment(&store);
        let (base, task) = serve(bootstrap_router(NetworkService::new(store, None).unwrap())).await;
        let client = reqwest::Client::new();
        let admin_state = client
            .get(format!("{base}/api/network/state"))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(admin_state.status(), StatusCode::FORBIDDEN);
        let cross = client
            .post(format!("{base}/api/network/devices/{second}/refresh"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(cross.status(), StatusCode::FORBIDDEN);
        let admin_refresh = client
            .post(format!("{base}/api/network/devices/{first}/refresh"))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(admin_refresh.status(), StatusCode::FORBIDDEN);
        let revoke = client
            .post(format!("{base}/api/network/devices/{first}/revoke"))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(revoke.status(), StatusCode::NOT_FOUND);
        task.abort();
    }

    #[tokio::test]
    async fn revoke_commits_app_denial_before_unavailable_external_revocation() {
        use dacs_net_contract::{DeviceId, HostName};
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (device, token) = enrollment(&store);
        command(
            &store,
            Command::ApproveWorker {
                worker_id: device.clone(),
            },
        );
        let principal = store.authenticate(&token).unwrap();
        store
            .network_consent(
                &principal,
                HostConsent {
                    device_id: DeviceId(device.clone()),
                    accepted_host: HostName::try_from("worker-pc".to_owned()).unwrap(),
                    accepted_by: "local-owner".into(),
                    accepted_at: time::OffsetDateTime::now_utc(),
                    disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
                    tailnet_switch: None,
                },
            )
            .unwrap();
        store.network_begin_issue(&device).unwrap().unwrap();
        store
            .network_reconcile(&administrator(), &device, Some(42), None)
            .unwrap();
        let service = NetworkService::new(store.clone(), None).unwrap();
        let (base, task) = serve(router(service.clone())).await;
        let client = reqwest::Client::new();
        let denied = client
            .post(format!("{base}/api/network/devices/{device}/revoke"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert!(store.authenticate(&token).is_ok());
        let response = client
            .post(format!("{base}/api/network/devices/{device}/revoke"))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(store.authenticate(&token).is_err());
        let snapshot = service.snapshot(&administrator()).unwrap();
        assert_eq!(snapshot["devices"][0]["phase"], "revocation_pending");
        assert_eq!(snapshot["devices"][0]["auth_key_id"], 42);
        assert_eq!(snapshot["devices"][0]["application_revoked"], true);
        task.abort();
    }

    #[tokio::test]
    async fn disconnected_issue_is_durable_and_never_automatically_repeated() {
        use dacs_net_contract::{DeviceId, HostName};
        use tokio::io::AsyncReadExt;
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (device, token) = enrollment(&store);
        command(
            &store,
            Command::ApproveWorker {
                worker_id: device.clone(),
            },
        );
        let principal = store.authenticate(&token).unwrap();
        store
            .network_consent(
                &principal,
                HostConsent {
                    device_id: DeviceId(device.clone()),
                    accepted_host: HostName::try_from("disconnect-pc".to_owned()).unwrap(),
                    accepted_by: "local-owner".into(),
                    accepted_at: time::OffsetDateTime::now_utc(),
                    disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
                    tailnet_switch: None,
                },
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // Inject a transport loss after accepting the issue request. This cannot
        // establish whether a remote effect occurred and must never permit retry.
        let connection = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            let count = socket.read(&mut bytes).await.unwrap();
            assert!(count > 0);
        });
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("netadmin-token");
        std::fs::write(&token_file, "isolated-test-operation-token-32-bytes").unwrap();
        let service = NetworkService::new(
            store.clone(),
            Some(NetworkConfig {
                netadmin_url: format!("http://{address}"),
                operation_token_file: token_file,
                ca_file: None,
            }),
        )
        .unwrap();
        assert_eq!(
            service.issue_locked(&device).await.unwrap_err().code,
            ErrorCode::Io
        );
        connection.await.unwrap();
        assert_eq!(
            store
                .network_record(&administrator(), &device)
                .unwrap()
                .phase,
            NetworkPhase::IssueUncertain
        );
        // The listener is gone: a second external attempt would fail, not succeed.
        service.issue_locked(&device).await.unwrap();
        assert_eq!(
            service.snapshot(&principal).unwrap()["availability"],
            "unavailable"
        );
    }

    #[tokio::test]
    async fn grant_get_never_issues_and_pending_approval_is_conflict() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (device, token) = enrollment(&store);
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("netadmin-token");
        std::fs::write(&token_file, "isolated-test-operation-token-32-bytes").unwrap();
        // No listener is started at this address: grant GET must not call it.
        let service = NetworkService::new(
            store.clone(),
            Some(NetworkConfig {
                netadmin_url: "http://127.0.0.1:9".into(),
                operation_token_file: token_file,
                ca_file: None,
            }),
        )
        .unwrap();
        let (base, task) = serve(router(service)).await;
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{base}/api/network/grant"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let admin = client
            .get(format!("{base}/api/network/grant"))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(admin.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            store
                .network_record(&administrator(), &device)
                .unwrap()
                .phase,
            NetworkPhase::AwaitingApproval
        );
        assert!(store.network_records().unwrap().is_empty());
        task.abort();
    }

    #[tokio::test]
    async fn maintenance_is_idle_without_a_configured_transport() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        enrollment(&store);
        let service = NetworkService::new(store, None).unwrap();
        service.maintain().await.unwrap();
    }

    #[tokio::test]
    async fn maintenance_revokes_despite_an_invalid_contract_but_does_not_issue() {
        use dacs_net_contract::{DeviceId, HostName, RevocationRequest};
        let store = Store::open(":memory:", ADMIN).unwrap();
        let consented = |host: &str| {
            let (device, token) = enrollment(&store);
            command(
                &store,
                Command::ApproveWorker {
                    worker_id: device.clone(),
                },
            );
            let principal = store.authenticate(&token).unwrap();
            store
                .network_consent(
                    &principal,
                    HostConsent {
                        device_id: DeviceId(device.clone()),
                        accepted_host: HostName::try_from(host.to_owned()).unwrap(),
                        accepted_by: "local-owner".into(),
                        accepted_at: time::OffsetDateTime::now_utc(),
                        disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
                        tailnet_switch: None,
                    },
                )
                .unwrap();
            device
        };
        let revoked = consented("revoked-pc");
        store.network_begin_issue(&revoked).unwrap().unwrap();
        store
            .network_reconcile(&administrator(), &revoked, Some(42), None)
            .unwrap();
        assert_eq!(
            store
                .network_revoke(&administrator(), &revoked)
                .unwrap()
                .phase,
            NetworkPhase::RevocationPending
        );
        let waiting = consented("waiting-pc");

        // Netadmin advertises a contract revision this bridge rejects.
        let netadmin = Router::new()
            .route(
                "/v1/contract",
                get(|| async {
                    Json(NetworkContract {
                        contract_revision: format!("{CONTRACT_REVISION}-skewed"),
                        admin_disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
                        admin_disclosure: ADMIN_ACCESS_DISCLOSURE.into(),
                        tailnet_switch_disclosure: TAILNET_SWITCH_DISCLOSURE.into(),
                        login_server: "https://login.example.test".into(),
                        coordinator: CoordinatorEndpoints {
                            bootstrap_url: "https://bootstrap.example.test".into(),
                            overlay_url: "https://overlay.example.test".into(),
                        },
                        key_ttl_minutes: 10,
                    })
                }),
            )
            .route(
                "/v1/revocations",
                post(|Json(request): Json<RevocationRequest>| async move {
                    Json(RevocationOutcome {
                        device_id: request.device_id,
                        key_expired: true,
                        node_deleted: None,
                        verified_absent: true,
                        at: time::OffsetDateTime::now_utc(),
                    })
                }),
            );
        let (base, task) = serve(netadmin).await;
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("netadmin-token");
        std::fs::write(&token_file, "isolated-test-operation-token-32-bytes").unwrap();
        let service = NetworkService::new(
            store.clone(),
            Some(NetworkConfig {
                netadmin_url: base,
                operation_token_file: token_file,
                ca_file: None,
            }),
        )
        .unwrap();
        // The contract failure is still reported after the revocation is processed.
        assert_eq!(service.maintain().await.unwrap_err().code, ErrorCode::Io);
        let phase = |device: &str| {
            store
                .network_record(&administrator(), device)
                .unwrap()
                .phase
        };
        assert_eq!(phase(&revoked), NetworkPhase::Revoked);
        assert_eq!(phase(&waiting), NetworkPhase::AwaitingApproval);
        task.abort();
    }
}
