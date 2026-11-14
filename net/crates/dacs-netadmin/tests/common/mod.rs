//! Shared fixtures for the netadmin hardening tests.
//!
//! [`MockHeadscale`] is an in-process HTTP stand-in for the Headscale v0.29.x
//! REST API (`/api/v1`, grpc-gateway shapes as recorded in
//! `net/evidence/local-headscale-integration.md`). It runs on an ephemeral
//! loopback port, checks the bearer API key, records every request and can
//! inject faults, so the real [`HeadscaleClient`] and HTTP router are tested
//! end to end without a Headscale binary.
#![allow(dead_code)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use dacs_net_contract::{ApiError, CoordinatorEndpoints, Secret};
use dacs_netadmin::audit::Audit;
use dacs_netadmin::headscale::HeadscaleClient;
use dacs_netadmin::http::{AppState, hash_token, router};
use dacs_netadmin::service::{CompilerPorts, Service, Settings};
use rand::RngCore;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

pub const TOKEN: &str = "dacs-op-test-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
pub const LOGIN_SERVER: &str = "https://hs.dacs.test";
pub const BOOTSTRAP_URL: &str = "https://bootstrap.dacs.test";
pub const OVERLAY_URL: &str = "https://coordinator.dacs.internal:7443";

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Health,
    CreateKey,
    ListKeys,
    ExpireKey,
    ListNodes,
    DeleteNode,
    RenameNode,
}

impl Op {
    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Op::CreateKey | Op::ExpireKey | Op::DeleteNode | Op::RenameNode
        )
    }
}

fn classify(method: &Method, path: &str) -> Option<Op> {
    let rest = path.strip_prefix("/api/v1/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["health"]) => Some(Op::Health),
        ("POST", ["preauthkey"]) => Some(Op::CreateKey),
        ("GET", ["preauthkey"]) => Some(Op::ListKeys),
        ("POST", ["preauthkey", "expire"]) => Some(Op::ExpireKey),
        ("GET", ["node"]) => Some(Op::ListNodes),
        ("DELETE", ["node", _]) => Some(Op::DeleteNode),
        ("POST", ["node", _, "rename", _]) => Some(Op::RenameNode),
        _ => None,
    }
}

/// Injected Headscale misbehaviour, consumed by the first matching request.
#[derive(Clone, Debug)]
pub enum Fault {
    /// Reply with this status/body before applying anything.
    Status(u16, String),
    /// Apply the operation, then break the response mid-body. The caller sees a
    /// transport error although the change happened (lost reply / timeout).
    ApplyThenDrop,
    /// Break the response without applying anything.
    DropBeforeApply,
    /// Reply `200 {}` without applying (Headscale does this for a missing key id).
    NoopOk,
    /// 500 whose body echoes the request's `Authorization` header, as a debug
    /// proxy or misconfigured front end might.
    EchoAuth,
    /// Apply the operation, then reply 200 with this body instead.
    ApplyThenBody(Value),
    /// Handle normally (lets a later fault for the same op hit the N-th call).
    Pass,
    /// Run this on the server state first (e.g. a device joining at that
    /// moment), then handle normally.
    Before(fn(&mut HsState)),
}

#[derive(Clone, Debug)]
pub struct Key {
    pub id: u64,
    pub secret: String,
    pub reusable: bool,
    pub ephemeral: bool,
    pub used: bool,
    pub expiration: Option<OffsetDateTime>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct HsNode {
    pub id: u64,
    pub name: String,
    pub given_name: String,
    pub ips: Vec<String>,
    pub tags: Vec<String>,
    pub key_id: Option<u64>,
    pub online: bool,
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub op: Option<Op>,
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: String,
}

pub struct HsState {
    pub api_key: String,
    pub keys: Vec<Key>,
    pub nodes: Vec<HsNode>,
    pub next_id: u64,
    pub faults: Vec<(Op, Fault)>,
    pub requests: Vec<Recorded>,
    /// Create keys as reusable/ephemeral regardless of the request
    /// (simulates a server that ignores the single-use request).
    pub force_reusable: bool,
    pub force_ephemeral: bool,
    /// Alter each created key before it is stored and returned (a server that
    /// ignores requested tags/expiry, or answers with odd values).
    pub tamper_key: Option<fn(&mut Key)>,
}

fn ts(t: Option<OffsetDateTime>) -> Value {
    match t {
        Some(t) => Value::String(t.format(&Rfc3339).unwrap()),
        None => Value::Null,
    }
}

impl HsState {
    /// What a Tailscale client does with `--auth-key` (see [`MockHeadscale::register`]).
    pub fn register(&mut self, secret: &str, os_name: &str, ip: &str) -> Result<u64, String> {
        let s = self;
        let now = OffsetDateTime::now_utc();
        let k = s
            .keys
            .iter_mut()
            .find(|k| k.secret == secret)
            .ok_or("invalid authkey")?;
        if k.used && !k.reusable {
            return Err("authkey already used".into());
        }
        if k.expiration.is_some_and(|e| e <= now) {
            return Err("authkey expired".into());
        }
        k.used = true;
        let (key_id, tags) = (k.id, k.tags.clone());
        s.next_id += 1;
        let id = s.next_id;
        s.nodes.push(HsNode {
            id,
            name: os_name.into(),
            given_name: os_name.into(),
            ips: vec![ip.into()],
            tags,
            key_id: Some(key_id),
            online: true,
        });
        Ok(id)
    }

    fn key_json(&self, k: &Key, full_secret: bool) -> Value {
        // Real list responses mask the key (`hskey-auth-<prefix>-***`); only the
        // create response carries the full secret.
        let key = if full_secret {
            k.secret.clone()
        } else {
            format!("{}-***", &k.secret[..k.secret.len().min(23)])
        };
        json!({
            "user": null,
            "id": k.id.to_string(),
            "key": key,
            "reusable": k.reusable,
            "ephemeral": k.ephemeral,
            "used": k.used,
            "expiration": ts(k.expiration),
            "createdAt": "2026-11-14T10:00:00Z",
            "aclTags": k.tags,
        })
    }

    fn node_json(&self, n: &HsNode) -> Value {
        let pre_auth_key = n
            .key_id
            .and_then(|id| self.keys.iter().find(|k| k.id == id))
            .map(|k| {
                let mut v = self.key_json(k, false);
                v["key"] = Value::String(String::new());
                v
            });
        json!({
            "id": n.id.to_string(),
            "machineKey": format!("mkey:{}", n.id),
            "ipAddresses": n.ips,
            "name": n.name,
            "givenName": n.given_name,
            "lastSeen": "2026-11-14T10:05:00Z",
            "expiry": null,
            "preAuthKey": pre_auth_key,
            "online": n.online,
            "tags": n.tags,
        })
    }

    fn apply(&mut self, op: Op, path: &str, body: &str) -> (StatusCode, Value) {
        let now = OffsetDateTime::now_utc();
        match op {
            Op::Health => (StatusCode::OK, json!({"databaseConnectivity": true})),
            Op::CreateKey => {
                let b: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                self.next_id += 1;
                let expiration = b["expiration"]
                    .as_str()
                    .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok());
                let key = Key {
                    id: self.next_id,
                    secret: format!("hskey-auth-{}", random_hex(24)),
                    reusable: b["reusable"].as_bool().unwrap_or(false) || self.force_reusable,
                    ephemeral: b["ephemeral"].as_bool().unwrap_or(false) || self.force_ephemeral,
                    used: false,
                    expiration,
                    tags: b["aclTags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                let mut key = key;
                if let Some(tamper) = self.tamper_key {
                    tamper(&mut key);
                }
                let v = json!({"preAuthKey": self.key_json(&key, true)});
                self.keys.push(key);
                (StatusCode::OK, v)
            }
            Op::ListKeys => {
                let keys: Vec<Value> = self.keys.iter().map(|k| self.key_json(k, false)).collect();
                (StatusCode::OK, json!({"preAuthKeys": keys}))
            }
            Op::ExpireKey => {
                let b: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let id: Option<u64> = b["id"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| b["id"].as_u64());
                if let Some(k) = id.and_then(|id| self.keys.iter_mut().find(|k| k.id == id)) {
                    k.expiration = Some(now);
                }
                // Headscale v0.29.7 answers 200 {} even for a missing id.
                (StatusCode::OK, json!({}))
            }
            Op::ListNodes => {
                let nodes: Vec<Value> = self.nodes.iter().map(|n| self.node_json(n)).collect();
                (StatusCode::OK, json!({"nodes": nodes}))
            }
            Op::DeleteNode => {
                let id: u64 = path.rsplit('/').next().unwrap().parse().unwrap_or(0);
                let before = self.nodes.len();
                self.nodes.retain(|n| n.id != id);
                if self.nodes.len() == before {
                    (
                        StatusCode::NOT_FOUND,
                        json!({"code": 5, "message": "node not found"}),
                    )
                } else {
                    (StatusCode::OK, json!({}))
                }
            }
            Op::RenameNode => {
                let parts: Vec<&str> = path.split('/').collect();
                // ["", "api", "v1", "node", id, "rename", name]
                let id: u64 = parts[4].parse().unwrap_or(0);
                let name = parts[6].to_owned();
                match self.nodes.iter_mut().find(|n| n.id == id) {
                    Some(n) => {
                        n.given_name = name;
                        let n = n.clone();
                        (StatusCode::OK, json!({"node": self.node_json(&n)}))
                    }
                    None => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"code": 2, "message": format!("node no longer exists in NodeStore: {id}")}),
                    ),
                }
            }
        }
    }
}

type Shared = Arc<Mutex<HsState>>;

/// A body that fails after the status line and headers are sent: hyper aborts
/// the connection, so the client sees a transport error mid-response.
struct FailingBody;

impl axum::body::HttpBody for FailingBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        std::task::Poll::Ready(Some(Err(std::io::Error::other("connection lost"))))
    }
}

fn broken() -> Response {
    let mut r = Response::new(Body::new(FailingBody));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    r
}

async fn handle(
    State(st): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_owned();
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = String::from_utf8_lossy(&body).into_owned();
    let op = classify(&method, &path);
    let mut s = st.lock().unwrap();
    s.requests.push(Recorded {
        op,
        method: method.to_string(),
        path: path.clone(),
        authorization: authorization.clone(),
        body: body.clone(),
    });
    if authorization.as_deref() != Some(format!("Bearer {}", s.api_key).as_str()) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    let Some(op) = op else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };
    let fault = s
        .faults
        .iter()
        .position(|(o, _)| *o == op)
        .map(|i| s.faults.remove(i).1);
    match &fault {
        Some(Fault::Status(code, body)) => {
            return (StatusCode::from_u16(*code).unwrap(), body.clone()).into_response();
        }
        Some(Fault::DropBeforeApply) => return broken(),
        Some(Fault::NoopOk) => return axum::Json(json!({})).into_response(),
        Some(Fault::Before(hook)) => hook(&mut s),
        Some(Fault::EchoAuth) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "upstream error; request headers: authorization: {}",
                    authorization.unwrap_or_default()
                ),
            )
                .into_response();
        }
        _ => {}
    }
    let (status, value) = s.apply(op, &path, &body);
    match fault {
        Some(Fault::ApplyThenDrop) => broken(),
        Some(Fault::ApplyThenBody(v)) => axum::Json(v).into_response(),
        _ => (status, axum::Json(value)).into_response(),
    }
}

pub struct MockHeadscale {
    pub addr: SocketAddr,
    pub state: Shared,
    pub api_key: String,
}

impl MockHeadscale {
    pub async fn start() -> Self {
        let api_key = format!("hskey-api-{}", random_hex(24));
        let state = Arc::new(Mutex::new(HsState {
            api_key: api_key.clone(),
            keys: Vec::new(),
            nodes: Vec::new(),
            next_id: 0,
            faults: Vec::new(),
            requests: Vec::new(),
            force_reusable: false,
            force_ephemeral: false,
            tamper_key: None,
        }));
        let app = Router::new().fallback(handle).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr,
            state,
            api_key,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, HsState> {
        self.state.lock().unwrap()
    }

    pub fn fault(&self, op: Op, fault: Fault) {
        self.lock().faults.push((op, fault));
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.lock().requests.clone()
    }

    pub fn clear_requests(&self) {
        self.lock().requests.clear();
    }

    /// Mutating requests (create/expire key, delete/rename node) seen so far.
    pub fn mutations(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.op.is_some_and(Op::is_mutation))
            .collect()
    }

    pub fn key(&self, id: u64) -> Option<Key> {
        self.lock().keys.iter().find(|k| k.id == id).cloned()
    }

    pub fn node(&self, id: u64) -> Option<HsNode> {
        self.lock().nodes.iter().find(|n| n.id == id).cloned()
    }

    /// What a Tailscale client does with `--auth-key`: Headscale refuses a used
    /// single-use key or an expired key; otherwise it registers a node that
    /// inherits the key's tags.
    pub fn register(&self, secret: &str, os_name: &str, ip: &str) -> Result<u64, String> {
        self.lock().register(secret, os_name, ip)
    }

    /// Add a node that netadmin did not enroll (infrastructure, personal).
    pub fn add_foreign_node(&self, name: &str, tags: &[&str], key_tags: Option<&[&str]>) -> u64 {
        let mut s = self.lock();
        let key_id = key_tags.map(|kt| {
            s.next_id += 1;
            let id = s.next_id;
            s.keys.push(Key {
                id,
                secret: format!("hskey-auth-{}", random_hex(24)),
                reusable: false,
                ephemeral: false,
                used: true,
                expiration: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
                tags: kt.iter().map(|t| (*t).to_owned()).collect(),
            });
            id
        });
        s.next_id += 1;
        let id = s.next_id;
        s.nodes.push(HsNode {
            id,
            name: name.into(),
            given_name: name.into(),
            ips: vec![format!("100.64.9.{id}")],
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            key_id,
            online: true,
        });
        id
    }

    /// Add an unused key netadmin did not issue.
    pub fn add_foreign_key(&self, tags: &[&str]) -> u64 {
        let mut s = self.lock();
        s.next_id += 1;
        let id = s.next_id;
        s.keys.push(Key {
            id,
            secret: format!("hskey-auth-{}", random_hex(24)),
            reusable: false,
            ephemeral: false,
            used: false,
            expiration: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        });
        id
    }

    /// Every secret this Headscale knows: its API key and all pre-auth keys.
    pub fn secrets(&self) -> Vec<String> {
        let s = self.lock();
        let mut out = vec![s.api_key.clone()];
        out.extend(
            s.keys
                .iter()
                .filter(|k| !k.secret.is_empty())
                .map(|k| k.secret.clone()),
        );
        out
    }
}

pub fn settings() -> Settings {
    settings_with_ttl(15)
}

pub fn settings_with_ttl(minutes: i64) -> Settings {
    Settings {
        login_server: LOGIN_SERVER.into(),
        coordinator: CoordinatorEndpoints {
            bootstrap_url: BOOTSTRAP_URL.into(),
            overlay_url: OVERLAY_URL.into(),
        },
        key_ttl: time::Duration::minutes(minutes),
        compiler_ports: CompilerPorts {
            scheduler: 10600,
            server: 10501,
        },
    }
}

/// netadmin's HTTP router wired to the real [`HeadscaleClient`] and a mock
/// Headscale, with an audit file in a private temp dir.
pub struct Harness {
    pub hs: MockHeadscale,
    pub app: Router,
    pub audit_path: PathBuf,
    pub dir: tempfile::TempDir,
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(settings()).await
    }

    pub async fn start_with(settings: Settings) -> Self {
        let hs = MockHeadscale::start().await;
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("audit.jsonl");
        let client = HeadscaleClient::new(&hs.url(), Secret::new(hs.api_key.clone())).unwrap();
        let app = router(Arc::new(AppState {
            service: Service::new(client, settings, Audit::open(&audit_path).unwrap()),
            token_hashes: vec![hash_token(TOKEN)],
        }));
        Self {
            hs,
            app,
            audit_path,
            dir,
        }
    }

    pub fn audit(&self) -> String {
        std::fs::read_to_string(&self.audit_path).unwrap_or_default()
    }

    pub fn audit_records(&self) -> Vec<Value> {
        self.audit()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub async fn call(
        &self,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        call(&self.app, method, path, auth, body).await
    }

    /// Authenticated call with the configured operation token.
    pub async fn op(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, String) {
        let auth = format!("Bearer {TOKEN}");
        self.call(method, path, Some(&auth), body).await
    }

    pub async fn enroll(&self, req: &Value) -> (StatusCode, String) {
        self.op("POST", "/v1/enrollments", Some(req.clone())).await
    }

    pub async fn revoke(&self, req: &Value) -> (StatusCode, String) {
        self.op("POST", "/v1/revocations", Some(req.clone())).await
    }

    pub async fn membership(&self, q: &Value) -> (StatusCode, String) {
        self.op("POST", "/v1/memberships/query", Some(q.clone()))
            .await
    }
}

pub async fn call(
    app: &Router,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(a) = auth {
        b = b.header(header::AUTHORIZATION, a);
    }
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

pub fn api_error(body: &str) -> ApiError {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not an ApiError ({e}): {body}"))
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
}

/// A valid coordinator `EnrollmentRequest` (wire JSON) for `host`.
pub fn enrollment_request(host: &str, roles: &[&str]) -> Value {
    let device = format!("dev-{host}");
    let now = now_rfc3339();
    json!({
        "device_id": device,
        "host_name": host,
        "host_kind": {"kind": "physical"},
        "roles": roles,
        "consent": {
            "device_id": device,
            "accepted_host": host,
            "accepted_by": "owner-account",
            "accepted_at": now,
            "disclosure_revision": dacs_net_contract::ADMIN_ACCESS_DISCLOSURE_REVISION,
            "tailnet_switch": null
        },
        "approval": {
            "device_id": device,
            "approved_by": "project-owner",
            "approved_at": now,
            "authorization_ref": format!("authz-{}", random_hex(4))
        }
    })
}

pub fn revocation(device: &str, key: Option<u64>, node: Option<u64>) -> Value {
    json!({
        "device_id": device,
        "auth_key_id": key,
        "node_id": node,
        "requested_by": "project-owner",
        "reason": "test revocation"
    })
}

pub fn membership_query(device: &str, host: &str, key: Option<u64>, node: Option<u64>) -> Value {
    json!({
        "device_id": device,
        "host_name": host,
        "auth_key_id": key,
        "node_id": node
    })
}

/// Panic if any secret occurs in `haystack`.
pub fn assert_no_secrets(haystack: &str, secrets: &[String], what: &str) {
    for s in secrets {
        assert!(!s.is_empty());
        assert!(
            !haystack.contains(s.as_str()),
            "{what} contains secret material {}…",
            &s[..s.len().min(14)]
        );
    }
}

/// Captures `tracing` output (all levels) for the current thread.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// Install as the thread's default subscriber until the guard drops.
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(self.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }
}

pub struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(self.0.clone())
    }
}

/// Write `contents` to `path` with mode 600 (secret files).
pub fn write_private(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Write the secret files and a config for `listen`; returns the config path.
pub fn write_config(
    dir: &Path,
    listen: &str,
    headscale_url: &str,
    api_key: &str,
    extra: Value,
) -> PathBuf {
    let key_path = dir.join("headscale-api-key");
    let tokens_path = dir.join("operation-tokens.sha256");
    write_private(&key_path, &format!("{api_key}\n"));
    write_private(
        &tokens_path,
        &format!("# coordinator\n{}\n", hex::encode(hash_token(TOKEN))),
    );
    let mut cfg = json!({
        "listen": listen,
        "headscale_url": headscale_url,
        "headscale_api_key_path": key_path,
        "operation_token_hashes_path": tokens_path,
        "login_server": LOGIN_SERVER,
        "coordinator": {"bootstrap_url": BOOTSTRAP_URL, "overlay_url": OVERLAY_URL},
        "audit_log_path": dir.join("audit.jsonl"),
    });
    if let (Some(obj), Some(extra)) = (cfg.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();
    path
}
