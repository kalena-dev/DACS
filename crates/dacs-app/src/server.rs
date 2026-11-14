use crate::local::{self, Jobs};
use crate::network::{self, NetworkConfig, NetworkService};
use axum::{
    Form, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use dacs_core::Store;
use dacs_protocol::*;
use dacs_source::SourceService;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

/// Notices kept per session until the dashboard renders them; oldest are dropped first.
const MAX_PENDING_NOTICES: usize = 32;
/// Slightly above the 20 s netadmin request timeout, so an in-flight call can finish.
const MAINTENANCE_STOP_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub bootstrap_listen: Option<SocketAddr>,
    pub network_config: Option<PathBuf>,
    pub public_url: String,
    pub host_name: String,
    pub data_dir: PathBuf,
    pub local_setup: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub update_public_key: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) bearer: String,
    pub(crate) csrf: String,
    expires: i64,
    flash: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct App {
    pub(crate) store: Store,
    source: SourceService,
    pub(crate) config: Arc<Config>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    login_nonces: Arc<Mutex<HashMap<String, i64>>>,
    pub(crate) jobs: Jobs,
    network: NetworkService,
    shutdown: Arc<Notify>,
    pending_update: Arc<Mutex<Option<Value>>>,
}

pub(crate) struct WebError(pub(crate) ApiError);
pub(crate) type WebResult<T> = std::result::Result<T, WebError>;
impl From<ApiError> for WebError {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}
impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::Invalid => StatusCode::BAD_REQUEST,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Forbidden => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Conflict | ErrorCode::CapacityUnavailable => StatusCode::CONFLICT,
            ErrorCode::Io | ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0)).into_response()
    }
}

fn invalid(message: impl Into<String>) -> WebError {
    ApiError::new(ErrorCode::Invalid, message).into()
}
fn forbidden(message: &str) -> WebError {
    ApiError::new(ErrorCode::Forbidden, message).into()
}
fn unauthorized() -> WebError {
    ApiError::new(ErrorCode::Unauthorized, "Authentication required").into()
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> dacs_protocol::Result<T> + Send + 'static,
) -> WebResult<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            WebError(ApiError::new(
                ErrorCode::Internal,
                format!("Operation failed: {error}"),
            ))
        })?
        .map_err(Into::into)
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}

pub(crate) fn session(app: &App, headers: &HeaderMap) -> WebResult<(String, Session)> {
    let id = cookie(headers, "dacs_session").ok_or_else(unauthorized)?;
    let mut sessions = app
        .sessions
        .lock()
        .map_err(|_| forbidden("Session store unavailable"))?;
    sessions.retain(|_, value| value.expires > local::now());
    let value = sessions.get(id).cloned().ok_or_else(unauthorized)?;
    Ok((id.to_owned(), value))
}

fn origin_check(app: &App, headers: &HeaderMap) -> WebResult<()> {
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return Err(forbidden("Cross-site mutations are not allowed"));
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| forbidden("Origin header required for browser mutations"))?;
    if origin != app.config.public_url.trim_end_matches('/') {
        return Err(forbidden("Unexpected request origin"));
    }
    Ok(())
}

pub(crate) fn authenticate(app: &App, headers: &HeaderMap, mutation: bool) -> WebResult<Principal> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        let token = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(unauthorized)?;
        return app.store.authenticate(token).map_err(Into::into);
    }
    let (_, session) = session(app, headers)?;
    if mutation {
        origin_check(app, headers)?;
        let csrf = headers
            .get("x-dacs-csrf")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| forbidden("CSRF token required"))?;
        if !local::secure_eq(csrf, &session.csrf) {
            return Err(forbidden("Invalid CSRF token"));
        }
    }
    app.store.authenticate(&session.bearer).map_err(Into::into)
}

pub(crate) fn form_auth(
    app: &App,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> WebResult<(Principal, String)> {
    origin_check(app, headers)?;
    let (id, session) = session(app, headers)?;
    if !local::secure_eq(
        form.get("csrf").map(String::as_str).unwrap_or(""),
        &session.csrf,
    ) {
        return Err(forbidden("Invalid CSRF token"));
    }
    Ok((app.store.authenticate(&session.bearer)?, id))
}

pub(crate) fn local_admin(app: &App, principal: &Principal) -> WebResult<()> {
    if !app.config.local_setup || !app.config.listen.ip().is_loopback() {
        return Err(forbidden("Local setup is disabled on this listener"));
    }
    if !principal.is_admin {
        return Err(forbidden("Local owner authentication is required"));
    }
    Ok(())
}

fn set_cookie(app: &App, name: &str, value: &str, age: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
        if app.config.public_url.starts_with("https:") {
            "; Secure"
        } else {
            ""
        }
    ))
    .expect("generated cookie is valid")
}

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; script-src 'none'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn health() -> Json<Value> {
    Json(
        json!({"status":"ready","protocol_version":PROTOCOL_VERSION,"version":env!("CARGO_PKG_VERSION")}),
    )
}
async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        dacs_ui::CSS,
    )
}

async fn login(State(app): State<App>) -> WebResult<Response> {
    let csrf = local::secret();
    let mut nonces = app
        .login_nonces
        .lock()
        .map_err(|_| forbidden("Login service unavailable"))?;
    nonces.retain(|_, expiry| *expiry > local::now());
    if nonces.len() >= 4096 {
        return Err(forbidden("Too many pending login requests"));
    }
    nonces.insert(csrf.clone(), local::now() + 600);
    let mut response = Html(dacs_ui::render_login(None, &csrf)).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&app, "dacs_login", &csrf, 600),
    );
    Ok(response)
}

async fn login_submit(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> WebResult<Response> {
    origin_check(&app, &headers)?;
    let nonce = cookie(&headers, "dacs_login").ok_or_else(|| forbidden("Reload the login form"))?;
    let csrf = form.get("csrf").map(String::as_str).unwrap_or("");
    if !local::secure_eq(csrf, nonce) {
        return Err(forbidden("Invalid login CSRF token"));
    }
    let expiry = app
        .login_nonces
        .lock()
        .map_err(|_| forbidden("Login service unavailable"))?
        .remove(nonce)
        .ok_or_else(|| forbidden("Login form expired"))?;
    if expiry <= local::now() {
        return Err(forbidden("Login form expired"));
    }
    let token = form.get("token").ok_or_else(unauthorized)?;
    app.store.authenticate(token)?;
    let id = local::secret();
    let value = Session {
        bearer: token.clone(),
        csrf: local::secret(),
        expires: local::now() + 8 * 3600,
        flash: Vec::new(),
    };
    {
        let mut sessions = app
            .sessions
            .lock()
            .map_err(|_| forbidden("Session service unavailable"))?;
        sessions.retain(|_, value| value.expires > local::now());
        if sessions.len() >= 1024 {
            return Err(forbidden("Too many active browser sessions"));
        }
        sessions.insert(id.clone(), value);
    }
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(&app, "dacs_session", &id, 8 * 3600),
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(&app, "dacs_login", "", 0));
    Ok(response)
}

async fn dashboard(State(app): State<App>, headers: HeaderMap) -> WebResult<Response> {
    let (id, session) = match session(&app, &headers) {
        Ok(value) => value,
        Err(_) => return Ok(Redirect::to("/login").into_response()),
    };
    let principal = app.store.authenticate(&session.bearer)?;
    let store = app.store.clone();
    let snapshot = blocking(move || store.snapshot(&principal)).await?;
    let notices = app
        .sessions
        .lock()
        .map_err(|_| forbidden("Session service unavailable"))?
        .get_mut(&id)
        .map(|s| std::mem::take(&mut s.flash))
        .unwrap_or_default();
    let notice = (!notices.is_empty()).then(|| notices.join("\n\n"));
    Ok(Html(dacs_ui::render_dashboard(
        &snapshot,
        notice.as_deref(),
        &session.csrf,
    ))
    .into_response())
}

async fn network_page(State(app): State<App>, headers: HeaderMap) -> WebResult<Response> {
    let principal = authenticate(&app, &headers, false)?;
    let network = app.network.clone();
    let status = blocking(move || network.snapshot(&principal)).await?;
    Ok(Html(dacs_ui::render_network(&status)).into_response())
}

async fn compiler_pools_page(State(app): State<App>, headers: HeaderMap) -> WebResult<Response> {
    let principal = authenticate(&app, &headers, false)?;
    let pools = blocking(move || app.store.compiler_pools(&principal)).await?;
    Ok(Html(dacs_ui::render_compiler_pools(&pools)).into_response())
}

async fn logout(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> WebResult<Response> {
    let (_, id) = form_auth(&app, &headers, &form)?;
    app.sessions
        .lock()
        .map_err(|_| forbidden("Session service unavailable"))?
        .remove(&id);
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(&app, "dacs_session", "", 0));
    Ok(response)
}

async fn logout_api(State(app): State<App>, headers: HeaderMap) -> WebResult<Response> {
    authenticate(&app, &headers, true)?;
    if let Some(id) = cookie(&headers, "dacs_session") {
        app.sessions
            .lock()
            .map_err(|_| forbidden("Session service unavailable"))?
            .remove(id);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(&app, "dacs_session", "", 0));
    Ok(response)
}

async fn state(State(app): State<App>, headers: HeaderMap) -> WebResult<Json<Snapshot>> {
    let principal = authenticate(&app, &headers, false)?;
    Ok(Json(
        blocking(move || app.store.snapshot(&principal)).await?,
    ))
}

async fn execute(
    State(app): State<App>,
    headers: HeaderMap,
    Json(envelope): Json<CommandEnvelope>,
) -> WebResult<Json<CommandResponse>> {
    let principal = authenticate(&app, &headers, true)?;
    Ok(Json(
        blocking(move || {
            validate_workflow_artifact(&app, &principal, &envelope.command)?;
            app.store.execute(&principal, envelope)
        })
        .await?,
    ))
}

fn validate_workflow_artifact(
    app: &App,
    principal: &Principal,
    command: &Command,
) -> dacs_protocol::Result<()> {
    let Command::ReportEvent {
        attempt_id,
        generation,
        event:
            WorkerEvent::WorkflowCompleted {
                success,
                artifact_digest,
                evidence,
            },
        ..
    } = command
    else {
        return Ok(());
    };
    let (attempt, task) = app
        .store
        .attempt_context(principal, attempt_id, *generation)?;
    let bytes = app
        .source
        .get_artifact(principal, &task.spec.project_id, artifact_digest)?;
    let result: dacs_protocol::workflow::WorkflowResult =
        serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::new(
                ErrorCode::Invalid,
                "Workflow artifact is not a typed result",
            )
        })?;
    let checks_match = result.checks.len() == evidence.len()
        && result.checks.iter().zip(evidence).all(|(a, b)| {
            a.name == b.name
                && a.command == b.command
                && a.source_sha == b.source_sha
                && a.passed == b.passed
                && a.artifact_digest == b.artifact_digest
        });
    if result.task_id != task.id
        || result.attempt_id != attempt.id
        || result.generation != attempt.generation
        || result.source_commit != task.spec.source_commit
        || result.success != *success
        || !result.processes_reconciled
        || !checks_match
        || (*success && (result.exit_code != Some(0) || result.distribution_failure.is_some()))
    {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "Workflow result does not match the owned execution and completion evidence",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: i64,
}
async fn commands(
    State(app): State<App>,
    headers: HeaderMap,
    Query(cursor): Query<Cursor>,
) -> WebResult<Json<Vec<ControlMessage>>> {
    let principal = authenticate(&app, &headers, false)?;
    Ok(Json(
        blocking(move || app.store.commands(&principal, cursor.after)).await?,
    ))
}

async fn invitation_api(
    State(app): State<App>,
    Path(token): Path<String>,
) -> WebResult<Json<Invitation>> {
    Ok(Json(blocking(move || app.store.invitation(&token)).await?))
}

async fn invitation_page(
    State(app): State<App>,
    Path(token): Path<String>,
) -> WebResult<Html<String>> {
    let store = app.store.clone();
    let invitation = blocking(move || store.invitation(&token)).await?;
    Ok(Html(dacs_ui::render_invitation(
        &invitation,
        &app.config.host_name,
        None,
        "",
    )))
}

#[derive(Deserialize)]
struct EnrollRequest {
    invitation_token: String,
    registration: WorkerRegistration,
}
async fn enroll(
    State(app): State<App>,
    Json(request): Json<EnrollRequest>,
) -> WebResult<Json<Enrollment>> {
    Ok(Json(
        blocking(move || {
            app.store
                .enroll(&request.invitation_token, request.registration)
        })
        .await?,
    ))
}

async fn prepare(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Json<CandidateRecord>> {
    let principal = authenticate(&app, &headers, true)?;
    Ok(Json(
        blocking(move || app.source.prepare(&principal, &id)).await?,
    ))
}
async fn publish(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, true)?;
    Ok(Json(
        blocking(move || app.source.publish(&principal, &id)).await?,
    ))
}

async fn candidate_diff(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, false)?;
    Ok(Json(
        blocking(move || app.source.diff(&principal, &id)).await?,
    ))
}

async fn candidate_page(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Html<String>> {
    let (_, session) = session(&app, &headers)?;
    let principal = app.store.authenticate(&session.bearer)?;
    let (candidate, diff) = blocking(move || {
        Ok((
            app.store.candidate(&principal, &id)?,
            app.source.diff(&principal, &id)?,
        ))
    })
    .await?;
    Ok(Html(dacs_ui::render_candidate(
        &candidate,
        &diff,
        &session.csrf,
    )))
}

#[derive(Deserialize)]
struct ImportQuery {
    #[serde(default)]
    seed: bool,
}
async fn import_source(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<ImportQuery>,
    body: Bytes,
) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, true)?;
    Ok(Json(
        blocking(move || app.source.import_bundle(&principal, &id, &body, query.seed)).await?,
    ))
}
async fn export_source(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Response> {
    let principal = authenticate(&app, &headers, false)?;
    let bytes = blocking(move || app.source.export_bundle(&principal, &id)).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=source.bundle",
            ),
        ],
        bytes,
    )
        .into_response())
}
async fn upload_artifact(
    State(app): State<App>,
    headers: HeaderMap,
    Path((project, digest)): Path<(String, String)>,
    body: Bytes,
) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, true)?;
    Ok(Json(
        blocking(move || {
            app.source
                .put_artifact(&principal, &project, &body, Some(&digest))
        })
        .await?,
    ))
}
async fn download_artifact(
    State(app): State<App>,
    headers: HeaderMap,
    Path((project, digest)): Path<(String, String)>,
) -> WebResult<Response> {
    let principal = authenticate(&app, &headers, false)?;
    let bytes = blocking(move || app.source.get_artifact(&principal, &project, &digest)).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment; filename=artifact"),
        ],
        bytes,
    )
        .into_response())
}

fn field<'a>(form: &'a HashMap<String, String>, name: &str) -> WebResult<&'a str> {
    form.get(name)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("Missing {name}")))
}
fn number<T: std::str::FromStr>(form: &HashMap<String, String>, name: &str) -> WebResult<T> {
    field(form, name)?
        .parse()
        .map_err(|_| invalid(format!("Invalid {name}")))
}
fn json_field<T: serde::de::DeserializeOwned>(
    form: &HashMap<String, String>,
    name: &str,
) -> WebResult<T> {
    serde_json::from_str(field(form, name)?)
        .map_err(|error| invalid(format!("Invalid {name}: {error}")))
}
fn json_default<T: serde::de::DeserializeOwned + Default>(
    form: &HashMap<String, String>,
    name: &str,
) -> WebResult<T> {
    match form.get(name).filter(|s| !s.trim().is_empty()) {
        Some(value) => serde_json::from_str(value).map_err(|error| invalid(error.to_string())),
        None => Ok(T::default()),
    }
}
fn lines(form: &HashMap<String, String>, name: &str) -> Vec<String> {
    form.get(name)
        .map(|v| {
            v.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
fn checked(form: &HashMap<String, String>, name: &str) -> bool {
    form.get(name).is_some_and(|v| v == "true" || v == "on")
}

async fn ui_action(
    State(app): State<App>,
    headers: HeaderMap,
    Path(action): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> WebResult<Response> {
    let (principal, session_id) = form_auth(&app, &headers, &form)?;
    let result = ui_action_result(&app, principal, &action, &form).await;
    let notice = match result {
        Ok(value) => {
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "Operation completed".into())
        }
        Err(error) => format!("Operation rejected: {}", error.0.message),
    };
    if let Some(session) = app
        .sessions
        .lock()
        .map_err(|_| forbidden("Session service unavailable"))?
        .get_mut(&session_id)
    {
        // Appended, not replaced: an action from another tab must not discard a
        // notice (such as an invitation URL) that has not been rendered yet.
        if session.flash.len() >= MAX_PENDING_NOTICES {
            session.flash.remove(0);
        }
        session.flash.push(notice);
    }
    Ok(Redirect::to("/").into_response())
}

async fn ui_action_result(
    app: &App,
    principal: Principal,
    action: &str,
    form: &HashMap<String, String>,
) -> WebResult<Value> {
    if matches!(action, "stage" | "publish") {
        let source = app.source.clone();
        let id = field(form, "candidate_id")?.to_owned();
        return if action == "stage" {
            Ok(
                serde_json::to_value(blocking(move || source.prepare(&principal, &id)).await?)
                    .map_err(|e| invalid(e.to_string()))?,
            )
        } else {
            blocking(move || source.publish(&principal, &id)).await
        };
    }
    let command = match action {
        "command" => json_field(form, "command")?,
        "create-project" => Command::CreateProject {
            name: field(form, "name")?.into(),
            target_branch: field(form, "target_branch")?.into(),
            max_tasks: number(form, "max_tasks")?,
            max_depth: number(form, "max_depth")?,
        },
        "create-task" => Command::CreateTask {
            spec: json_field(form, "spec")?,
        },
        "invitation" => Command::CreateInvitation {
            project_id: field(form, "project_id")?.into(),
            host_name: field(form, "host_name")?.into(),
            expires_in_seconds: number(form, "expires_in_seconds")?,
        },
        "requirements" => Command::SetRequirements {
            project_id: field(form, "project_id")?.into(),
            requirements: json_field(form, "requirements")?,
        },
        "message" => Command::SendMessage {
            project_id: field(form, "project_id")?.into(),
            recipient_task_id: field(form, "recipient_task_id")?.into(),
            message_id: field(form, "message_id")?.into(),
            kind: field(form, "kind")?.into(),
            body: json_field(form, "body")?,
        },
        "review" => Command::ReviewCandidate {
            candidate_id: field(form, "candidate_id")?.into(),
            decision: ReviewDecision {
                candidate_sha: field(form, "candidate_sha")?.into(),
                approve: field(form, "approve")? == "true",
                reason: field(form, "reason")?.into(),
                checks_run: json_default(form, "checks_run")?,
                skipped_checks: lines(form, "skipped_checks"),
            },
        },
        _ => return Err(invalid("Unknown UI action")),
    };
    let command_id = field(form, "command_id")?.to_owned();
    let state = app.clone();
    let response = blocking(move || {
        // Same gate as the API route: a browser session must not bypass artifact checks.
        validate_workflow_artifact(&state, &principal, &command)?;
        state.store.execute(
            &principal,
            CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                command_id,
                command,
            },
        )
    })
    .await?;
    if action == "invitation" {
        let mut value = response.result;
        if let Some(token) = value
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            value["invitation_url"] = json!(format!(
                "{}/invite/{token}",
                app.config.public_url.trim_end_matches('/')
            ));
        }
        Ok(value)
    } else {
        Ok(response.result)
    }
}

async fn setup_page(State(app): State<App>, headers: HeaderMap) -> WebResult<Response> {
    let (_, session) = match session(&app, &headers) {
        Ok(value) => value,
        Err(_) => return Ok(Redirect::to("/login").into_response()),
    };
    let principal = app.store.authenticate(&session.bearer)?;
    local_admin(&app, &principal)?;
    Ok(Html(dacs_ui::render_setup(
        &dacs_setup::catalog(),
        None,
        None,
        &session.csrf,
    ))
    .into_response())
}

async fn setup_catalog(State(app): State<App>, headers: HeaderMap) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, false)?;
    local_admin(&app, &principal)?;
    Ok(Json(dacs_setup::catalog()))
}
async fn setup_detect(State(app): State<App>, headers: HeaderMap) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, false)?;
    local_admin(&app, &principal)?;
    Ok(Json(blocking(dacs_setup::detect).await?))
}
async fn setup_operation(
    State(app): State<App>,
    headers: HeaderMap,
    Path(operation): Path<String>,
    Json(request): Json<Value>,
) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, true)?;
    local_admin(&app, &principal)?;
    Ok(Json(run_setup(&app, &operation, request).await?))
}
async fn update_operation(
    State(app): State<App>,
    headers: HeaderMap,
    Path(operation): Path<String>,
    Json(request): Json<Value>,
) -> WebResult<Json<Value>> {
    let principal = authenticate(&app, &headers, true)?;
    local_admin(&app, &principal)?;
    Ok(Json(
        run_setup(&app, &format!("update/{operation}"), request).await?,
    ))
}
async fn job_api(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Json<crate::local::Job>> {
    let principal = authenticate(&app, &headers, false)?;
    local_admin(&app, &principal)?;
    Ok(Json(app.jobs.get(&id)?))
}
async fn job_page(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> WebResult<Html<String>> {
    let (_, session) = session(&app, &headers)?;
    let principal = app.store.authenticate(&session.bearer)?;
    local_admin(&app, &principal)?;
    let job = serde_json::to_value(app.jobs.get(&id)?).map_err(|e| invalid(e.to_string()))?;
    Ok(Html(dacs_ui::render_setup(
        &dacs_setup::catalog(),
        Some(&job),
        Some("Refresh this page to read the durable operation outcome."),
        &session.csrf,
    )))
}

fn object_field(request: &Value, key: &str) -> WebResult<Value> {
    request
        .get(key)
        .cloned()
        .ok_or_else(|| invalid(format!("Missing {key}")))
}

async fn run_setup(app: &App, operation: &str, request: Value) -> WebResult<Value> {
    match operation {
        "detect" => blocking(dacs_setup::detect).await,
        "plan" => blocking(move || dacs_setup::plan(request)).await,
        "apply" => {
            let plan = object_field(&request, "plan")?;
            let consent = object_field(&request, "consent")?;
            let state = app.config.data_dir.join("setup");
            Ok(json!(app.jobs.start("dependency_apply", move || {
                dacs_setup::apply(plan, consent, &state)
            })?))
        }
        "scan" => {
            let repo = request
                .get("repo")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Missing repo"))?
                .to_owned();
            let use_ai = request
                .get("use_ai")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(json!(app.jobs.start("project_scan", move || {
                dacs_setup::scan_project(std::path::Path::new(&repo), use_ai)
            })?))
        }
        "update/verify" | "update/stage" => {
            let key = app
                .config
                .update_public_key
                .clone()
                .ok_or_else(|| forbidden("No trusted release key configured locally"))?;
            let manifest = object_field(&request, "manifest")?;
            if operation == "update/verify" {
                blocking(move || {
                    dacs_setup::verify_update(manifest, &key, env!("CARGO_PKG_VERSION"))
                })
                .await
            } else {
                let state = app.config.data_dir.join("updates");
                Ok(json!(app.jobs.start("update_stage", move || {
                    dacs_setup::stage_update(manifest, &key, env!("CARGO_PKG_VERSION"), &state)
                })?))
            }
        }
        "update/apply" | "update/rollback" => {
            let target = request
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Missing update target"))?
                .to_owned();
            let state = app.config.data_dir.join("updates");
            let shutdown = app.shutdown.clone();
            let pending = app.pending_update.clone();
            let rollback = operation == "update/rollback";
            Ok(json!(app.jobs.start(
                if rollback {
                    "update_rollback"
                } else {
                    "update_apply"
                },
                move || {
                    let value = if rollback {
                        dacs_setup::rollback_update(&state, std::path::Path::new(&target))?
                    } else {
                        dacs_setup::apply_staged_update(&state, std::path::Path::new(&target))?
                    };
                    if value.get("exit_required").and_then(Value::as_bool) == Some(true) {
                        *pending.lock().map_err(|_| {
                            ApiError::new(ErrorCode::Internal, "Update handoff unavailable")
                        })? = Some(value.clone());
                        shutdown.notify_one();
                    }
                    Ok(value)
                }
            )?))
        }
        _ => Err(invalid("Unknown setup operation")),
    }
}

async fn setup_form(
    State(app): State<App>,
    headers: HeaderMap,
    Path(operation): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> WebResult<Response> {
    setup_form_inner(app, headers, operation, form).await
}
async fn update_form(
    State(app): State<App>,
    headers: HeaderMap,
    Path(operation): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> WebResult<Response> {
    setup_form_inner(app, headers, format!("update/{operation}"), form).await
}
async fn setup_form_inner(
    app: App,
    headers: HeaderMap,
    operation: String,
    form: HashMap<String, String>,
) -> WebResult<Response> {
    let (principal, session_id) = form_auth(&app, &headers, &form)?;
    local_admin(&app, &principal)?;
    let request = match operation.as_str() {
        "detect" => Value::Null,
        "scan" => json!({"repo":field(&form,"repo")?,"use_ai":checked(&form,"use_ai")}),
        "plan" => {
            let packages: Vec<Value> = form
                .iter()
                .filter_map(|(key, value)| {
                    let id = key.strip_prefix("package.")?;
                    if value != "true" && value != "on" {
                        return None;
                    }
                    let version = form.get(&format!("version.{id}")).filter(|s| !s.is_empty());
                    Some(json!({"id":id,"version":version}))
                })
                .collect();
            let invitation: Value = json_default(&form, "invitation")?;
            json!({"operation":field(&form,"operation")?,"preset":form.get("preset").filter(|s|!s.is_empty()),"packages":packages,"declined":lines(&form,"declined"),"invitation":invitation})
        }
        "apply" => {
            json!({"plan":json_field::<Value>(&form,"plan")?,"consent":{"plan_sha256":field(&form,"plan_sha256")?,"owner_approved":checked(&form,"owner_approved"),"administrator_access_approved":checked(&form,"administrator_access_approved"),"resume_interrupted":checked(&form,"resume_interrupted")}})
        }
        "update/verify" | "update/stage" => {
            json!({"manifest":json_field::<Value>(&form,"manifest")?})
        }
        "update/apply" | "update/rollback" => json!({"target":field(&form,"target")?}),
        _ => return Err(invalid("Unknown setup form")),
    };
    let csrf = app
        .sessions
        .lock()
        .map_err(|_| forbidden("Session service unavailable"))?
        .get(&session_id)
        .map(|s| s.csrf.clone())
        .ok_or_else(unauthorized)?;
    match run_setup(&app, &operation, request).await {
        Ok(value) if value.get("id").and_then(Value::as_str).is_some() => Ok(Redirect::to(
            &format!("/setup/jobs/{}", value["id"].as_str().expect("checked ID")),
        )
        .into_response()),
        Ok(value) => Ok(Html(dacs_ui::render_setup(
            &dacs_setup::catalog(),
            Some(&value),
            None,
            &csrf,
        ))
        .into_response()),
        Err(error) => Ok((
            StatusCode::BAD_REQUEST,
            Html(dacs_ui::render_setup(
                &dacs_setup::catalog(),
                None,
                Some(&error.0.message),
                &csrf,
            )),
        )
            .into_response()),
    }
}

async fn serve_listener(
    listener: std::net::TcpListener,
    router: Router,
    tls: Option<axum_server::tls_rustls::RustlsConfig>,
    handle: axum_server::Handle,
) -> std::io::Result<()> {
    match tls {
        Some(tls) => {
            axum_server::from_tcp_rustls(listener, tls)
                .handle(handle)
                .serve(router.into_make_service())
                .await
        }
        None => {
            axum_server::from_tcp(listener)
                .handle(handle)
                .serve(router.into_make_service())
                .await
        }
    }
}

/// Resolves on Ctrl-C and, on Unix, on SIGTERM, so a service-manager stop is graceful.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = terminate.recv() => {},
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn run(mut config: Config) -> anyhow::Result<Option<Value>> {
    let tls = config.tls_cert.is_some() && config.tls_key.is_some();
    anyhow::ensure!(
        config.tls_cert.is_some() == config.tls_key.is_some(),
        "Both TLS certificate and key are required"
    );
    anyhow::ensure!(
        config.listen.ip().is_loopback() || tls,
        "Non-loopback listeners require TLS"
    );
    if let Some(bootstrap) = config.bootstrap_listen {
        anyhow::ensure!(tls, "Bootstrap listener requires TLS");
        anyhow::ensure!(
            bootstrap != config.listen,
            "Bootstrap and control listener addresses must differ"
        );
    }
    anyhow::ensure!(
        !config.local_setup || config.listen.ip().is_loopback(),
        "Local setup cannot be enabled on a non-loopback listener"
    );
    let url = url::Url::parse(&config.public_url)?;
    anyhow::ensure!(
        url.scheme() == if tls { "https" } else { "http" }
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "public-url must be an origin matching the listener TLS mode"
    );
    config.public_url = url.origin().ascii_serialization();
    // Bind every socket before opening any state, spawning any server or announcing
    // readiness. A second instance on the same data directory therefore fails here,
    // before it can rewrite the live instance's job journal. An error drops
    // already-bound sockets, so partial startup cannot survive.
    let control_listener = std::net::TcpListener::bind(config.listen)?;
    control_listener.set_nonblocking(true)?;
    let bootstrap_listener = config
        .bootstrap_listen
        .map(std::net::TcpListener::bind)
        .transpose()?;
    if let Some(listener) = &bootstrap_listener {
        listener.set_nonblocking(true)?;
    }
    local::private_directory(&config.data_dir)?;
    config.data_dir = std::fs::canonicalize(&config.data_dir)?;
    let token = local::admin_token(&config.data_dir)?;
    let store = Store::open(config.data_dir.join("coordinator.db"), &token)?;
    let source = SourceService::new(config.data_dir.join("source"), store.clone())?;
    let network_config: Option<NetworkConfig> = config
        .network_config
        .as_deref()
        .map(|path| -> anyhow::Result<NetworkConfig> {
            Ok(serde_json::from_value(local::read_json(path)?)?)
        })
        .transpose()?;
    let network = NetworkService::new(store.clone(), network_config)?;
    let jobs = Jobs::open(config.data_dir.join("jobs"))?;
    let shutdown = Arc::new(Notify::new());
    let pending_update = Arc::new(Mutex::new(None));
    let app = App {
        store: store.clone(),
        source,
        config: Arc::new(config.clone()),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        login_nonces: Arc::new(Mutex::new(HashMap::new())),
        jobs: jobs.clone(),
        network: network.clone(),
        shutdown: shutdown.clone(),
        pending_update: pending_update.clone(),
    };
    let router = Router::new()
        .route("/health", get(health))
        .route("/assets/dacs.css", get(css))
        .route("/", get(dashboard))
        .route("/network", get(network_page))
        .route("/builds", get(compiler_pools_page))
        .route("/login", get(login))
        .route("/session", post(login_submit).delete(logout_api))
        .route("/ui/logout", post(logout))
        .route("/ui/{action}", post(ui_action))
        .route("/invite/{token}", get(invitation_page))
        .route("/api/state", get(state))
        .route("/api/command", post(execute))
        .route("/api/commands", get(commands))
        .route("/api/invitations/{token}", get(invitation_api))
        .route("/api/enroll", post(enroll))
        .route("/api/candidates/{id}/prepare", post(prepare))
        .route("/api/candidates/{id}/publish", post(publish))
        .route("/api/candidates/{id}/diff", get(candidate_diff))
        .route("/candidates/{id}", get(candidate_page))
        .route(
            "/api/projects/{id}/source",
            get(export_source)
                .post(import_source)
                .layer(DefaultBodyLimit::max(128 * 1024 * 1024)),
        )
        .route(
            "/api/projects/{id}/artifacts/{digest}",
            get(download_artifact)
                .post(upload_artifact)
                .layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/setup", get(setup_page))
        .route("/setup/jobs/{id}", get(job_page))
        .route("/ui/setup/{operation}", post(setup_form))
        .route("/ui/setup/update/{operation}", post(update_form))
        .route("/api/setup/catalog", get(setup_catalog))
        .route("/api/setup/detect", get(setup_detect))
        .route("/api/setup/jobs/{id}", get(job_api))
        .route("/api/setup/{operation}", post(setup_operation))
        .route("/api/setup/update/{operation}", post(update_operation))
        .merge(crate::onboarding::router())
        .with_state(app.clone())
        .merge(network::router(network.clone()))
        .merge(crate::builds::router(app.store.clone()))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers));
    // Deliberately separate route tree: adding a control route never publishes it
    // on the public bootstrap listener. Enrollment uses device bearer tokens,
    // not browser sessions or administrator login.
    let bootstrap_router = Router::new()
        .route("/health", get(health))
        .route("/assets/dacs.css", get(css))
        .route("/invite/{token}", get(invitation_page))
        .route("/api/invitations/{token}", get(invitation_api))
        .route("/api/enroll", post(enroll))
        .with_state(app)
        .merge(network::bootstrap_router(network.clone()))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers));
    let rustls = if tls {
        Some(
            axum_server::tls_rustls::RustlsConfig::from_pem_file(
                config.tls_cert.as_ref().expect("checked certificate"),
                config.tls_key.as_ref().expect("checked key"),
            )
            .await?,
        )
    } else {
        None
    };
    // Store and network maintenance run independently, so slow netadmin calls never
    // delay lease expiry or cancellation handling. Both stop cooperatively: the
    // signal is observed only between iterations, so an enrollment call in flight
    // always records its outcome.
    let (stop_maintenance, stopping) = tokio::sync::watch::channel(false);
    let store_maintenance = tokio::spawn({
        let mut stopping = stopping.clone();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stopping.changed() => break,
                }
                let store = store.clone();
                if let Err(error) = blocking(move || store.maintain()).await {
                    tracing::error!("Coordinator maintenance failed: {}", error.0.message);
                }
            }
        }
    });
    let network_maintenance = tokio::spawn({
        let mut stopping = stopping;
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stopping.changed() => break,
                }
                if let Err(error) = network.maintain().await {
                    tracing::error!("Network maintenance failed: {}", error.message);
                }
            }
        }
    });
    let handles = [axum_server::Handle::new(), axum_server::Handle::new()];
    let stop_handles = handles.clone();
    let stop = tokio::spawn(async move {
        tokio::select! { _ = shutdown.notified() => {}, _ = shutdown_signal() => {} };
        for handle in &stop_handles {
            handle.graceful_shutdown(Some(Duration::from_secs(30)));
        }
    });
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(serve_listener(
        control_listener,
        router,
        rustls.clone(),
        handles[0].clone(),
    ));
    if let Some(listener) = bootstrap_listener {
        servers.spawn(serve_listener(
            listener,
            bootstrap_router,
            rustls,
            handles[1].clone(),
        ));
    }
    tracing::info!(
        "DACS listening at {}; bootstrap: {:?}; admin token file: {}",
        config.public_url,
        config.bootstrap_listen,
        config.data_dir.join("admin.token").display()
    );
    let mut result = Ok(());
    while let Some(completed) = servers.join_next().await {
        // A failed or unexpectedly terminated listener also stops its sibling.
        for handle in &handles {
            handle.graceful_shutdown(Some(Duration::from_secs(30)));
        }
        if result.is_ok() {
            result = completed
                .map_err(anyhow::Error::from)
                .and_then(|value| value.map_err(anyhow::Error::from));
        }
    }
    stop.abort();
    let _ = stop.await;
    let _ = stop_maintenance.send(true);
    let deadline = tokio::time::Instant::now() + MAINTENANCE_STOP_TIMEOUT;
    for (name, mut task) in [
        ("Store", store_maintenance),
        ("Network", network_maintenance),
    ] {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            tracing::warn!("{name} maintenance did not stop in time; aborting it");
            task.abort();
            let _ = task.await;
        }
    }
    jobs.drain().await;
    result?;
    let update = pending_update
        .lock()
        .map_err(|_| anyhow::anyhow!("Update handoff unavailable"))?
        .take();
    Ok(update)
}
