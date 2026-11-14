//! Operation API for Person 1's coordinator. Every route except `/v1/health`
//! requires `Authorization: Bearer <operation token>`; tokens are compared by
//! SHA-256 hash in constant time.

use std::sync::Arc;

use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dacs_net_contract::{
    ADMIN_ACCESS_DISCLOSURE, ADMIN_ACCESS_DISCLOSURE_REVISION, ApiError, CONTRACT_REVISION,
    CompilerEndpoint, EnrollmentGrant, EnrollmentRequest, MembershipQuery, NetworkMembership,
    OverlayNode, RevocationOutcome, RevocationRequest, TAILNET_SWITCH_DISCLOSURE,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;

use crate::headscale::{HeadscaleApi, PINNED_HEADSCALE_VERSION};
use crate::service::{Service, ServiceError};

pub struct AppState<H> {
    pub service: Service<H>,
    pub token_hashes: Vec<[u8; 32]>,
}

pub struct Failure(StatusCode, ApiError);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

fn fail(status: StatusCode, code: &str, message: impl Into<String>) -> Failure {
    Failure(
        status,
        ApiError {
            code: code.to_owned(),
            message: message.into(),
        },
    )
}

impl From<ServiceError> for Failure {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::BadRequest(m) => fail(StatusCode::BAD_REQUEST, "bad_request", m),
            ServiceError::Forbidden(m) => fail(StatusCode::FORBIDDEN, "forbidden", m),
            ServiceError::Conflict(m) => fail(StatusCode::CONFLICT, "conflict", m),
            ServiceError::Upstream(e) => {
                tracing::error!(error = %e, "headscale operation failed");
                fail(
                    StatusCode::BAD_GATEWAY,
                    "headscale_unavailable",
                    e.to_string(),
                )
            }
        }
    }
}

/// JSON body extractor whose rejections use the [`ApiError`] body.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = Failure;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(fail(rejection.status(), "bad_request", rejection.body_text())),
        }
    }
}

pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn authorize<H>(state: &AppState<H>, headers: &HeaderMap) -> Result<(), Failure> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            fail(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "operation token required",
            )
        })?;
    let h = hash_token(presented);
    let mut ok = subtle::Choice::from(0u8);
    for allowed in &state.token_hashes {
        ok |= allowed.ct_eq(&h);
    }
    if bool::from(ok) {
        Ok(())
    } else {
        Err(fail(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid operation token",
        ))
    }
}

pub fn router<H: HeadscaleApi + 'static>(state: Arc<AppState<H>>) -> Router {
    // The token layer runs before any extractor, so unauthenticated callers
    // learn nothing about request schemas.
    let protected = Router::new()
        .route("/v1/contract", get(contract::<H>))
        .route("/v1/enrollments", post(enroll::<H>))
        .route("/v1/memberships/query", post(membership::<H>))
        .route("/v1/revocations", post(revoke::<H>))
        .route("/v1/nodes", get(nodes::<H>))
        .route("/v1/compiler-endpoints", get(compiler_endpoints::<H>))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_token::<H>,
        ));
    Router::new()
        .route("/v1/health", get(health::<H>))
        .merge(protected)
        .with_state(state)
}

async fn require_token<H: HeadscaleApi>(
    State(st): State<Arc<AppState<H>>>,
    req: Request,
    next: Next,
) -> Result<Response, Failure> {
    authorize(&st, req.headers())?;
    Ok(next.run(req).await)
}

#[derive(Serialize)]
struct Health {
    ok: bool,
    contract_revision: &'static str,
    headscale_pinned: &'static str,
    headscale_error: Option<String>,
}

async fn health<H: HeadscaleApi>(State(st): State<Arc<AppState<H>>>) -> (StatusCode, Json<Health>) {
    let res = st.service.health().await;
    if let Err(e) = &res {
        tracing::warn!(error = %e, "health check: headscale unavailable");
    }
    let status = if res.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(Health {
            ok: res.is_ok(),
            contract_revision: CONTRACT_REVISION,
            headscale_pinned: PINNED_HEADSCALE_VERSION,
            headscale_error: res.err().map(|_| "headscale unavailable".to_owned()),
        }),
    )
}

/// Disclosures and endpoints Person 1's setup UI must show/use.
#[derive(Serialize)]
struct Contract {
    contract_revision: &'static str,
    admin_disclosure_revision: &'static str,
    admin_disclosure: &'static str,
    tailnet_switch_disclosure: &'static str,
    login_server: String,
    coordinator: dacs_net_contract::CoordinatorEndpoints,
    key_ttl_minutes: i64,
}

async fn contract<H: HeadscaleApi>(
    State(st): State<Arc<AppState<H>>>,
) -> Result<Json<Contract>, Failure> {
    let s = st.service.settings();
    Ok(Json(Contract {
        contract_revision: CONTRACT_REVISION,
        admin_disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION,
        admin_disclosure: ADMIN_ACCESS_DISCLOSURE,
        tailnet_switch_disclosure: TAILNET_SWITCH_DISCLOSURE,
        login_server: s.login_server.clone(),
        coordinator: s.coordinator.clone(),
        key_ttl_minutes: s.key_ttl.whole_minutes(),
    }))
}

async fn enroll<H: HeadscaleApi>(
    State(st): State<Arc<AppState<H>>>,
    ApiJson(req): ApiJson<EnrollmentRequest>,
) -> Result<Json<EnrollmentGrant>, Failure> {
    Ok(Json(
        st.service.enroll(&req, OffsetDateTime::now_utc()).await?,
    ))
}

async fn membership<H: HeadscaleApi>(
    State(st): State<Arc<AppState<H>>>,
    ApiJson(q): ApiJson<MembershipQuery>,
) -> Result<Json<NetworkMembership>, Failure> {
    Ok(Json(
        st.service.membership(&q, OffsetDateTime::now_utc()).await?,
    ))
}

async fn revoke<H: HeadscaleApi>(
    State(st): State<Arc<AppState<H>>>,
    ApiJson(req): ApiJson<RevocationRequest>,
) -> Result<Json<RevocationOutcome>, Failure> {
    Ok(Json(
        st.service.revoke(&req, OffsetDateTime::now_utc()).await?,
    ))
}

async fn nodes<H: HeadscaleApi>(
    State(st): State<Arc<AppState<H>>>,
) -> Result<Json<Vec<OverlayNode>>, Failure> {
    Ok(Json(st.service.nodes().await?))
}

async fn compiler_endpoints<H: HeadscaleApi>(
    State(st): State<Arc<AppState<H>>>,
) -> Result<Json<Vec<CompilerEndpoint>>, Failure> {
    Ok(Json(st.service.compiler_endpoints().await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Audit;
    use crate::headscale::{HeadscaleError, Node, PreAuthKey};
    use crate::service::{CompilerPorts, Settings};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct Empty;
    impl HeadscaleApi for Empty {
        async fn health(&self) -> Result<(), HeadscaleError> {
            Ok(())
        }
        async fn create_tagged_key(
            &self,
            _: &[String],
            _: OffsetDateTime,
        ) -> Result<PreAuthKey, HeadscaleError> {
            unreachable!("not used")
        }
        async fn expire_key(&self, _: u64) -> Result<(), HeadscaleError> {
            Ok(())
        }
        async fn list_keys(&self) -> Result<Vec<PreAuthKey>, HeadscaleError> {
            Ok(vec![])
        }
        async fn list_nodes(&self) -> Result<Vec<Node>, HeadscaleError> {
            Ok(vec![])
        }
        async fn delete_node(&self, _: u64) -> Result<(), HeadscaleError> {
            Ok(())
        }
        async fn rename_node(&self, _: u64, _: &str) -> Result<Node, HeadscaleError> {
            unreachable!("not used")
        }
    }

    fn app() -> Router {
        let settings = Settings {
            login_server: "https://hs.example.org".into(),
            coordinator: dacs_net_contract::CoordinatorEndpoints {
                bootstrap_url: "https://bootstrap.example.org".into(),
                overlay_url: "https://coord.dacs.internal:7443".into(),
            },
            key_ttl: time::Duration::minutes(15),
            compiler_ports: CompilerPorts {
                scheduler: 10600,
                server: 10501,
            },
        };
        router(Arc::new(AppState {
            service: Service::new(Empty, settings, Audit::disabled()),
            token_hashes: vec![hash_token("good-token")],
        }))
    }

    async fn status(req: Request<Body>) -> StatusCode {
        app().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn operations_require_valid_token() {
        let get = |auth: Option<&str>| {
            let mut b = Request::get("/v1/nodes");
            if let Some(a) = auth {
                b = b.header("authorization", a);
            }
            b.body(Body::empty()).unwrap()
        };
        assert_eq!(status(get(None)).await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            status(get(Some("Bearer wrong"))).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(get(Some("good-token"))).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(status(get(Some("Bearer good-token"))).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_is_public_and_enroll_is_not() {
        assert_eq!(
            status(Request::get("/v1/health").body(Body::empty()).unwrap()).await,
            StatusCode::OK
        );
        let req = Request::post("/v1/enrollments")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(status(req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_body_returns_api_error() {
        let req = Request::post("/v1/enrollments")
            .header("authorization", "Bearer good-token")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"device_id":"d","host_name":"Not Valid"}"#))
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let err: ApiError = serde_json::from_slice(&body).unwrap();
        assert_eq!(err.code, "bad_request");
    }
}
