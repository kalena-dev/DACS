//! Read-only compiler pool ownership API. Worker credentials cannot inspect admin pool state.
use crate::server::{WebError, WebResult};
use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
use dacs_core::Store;
use dacs_protocol::{ApiError, ErrorCode, workflow::PoolOwnership};

pub fn router(store: Store) -> Router {
    Router::new()
        .route("/api/compiler-pools", get(pools))
        .route("/api/compiler-evidence/{attempt_id}", get(evidence))
        .with_state(store)
}
async fn pools(
    State(store): State<Store>,
    headers: HeaderMap,
) -> WebResult<Json<Vec<PoolOwnership>>> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| {
            WebError(ApiError::new(
                ErrorCode::Unauthorized,
                "administrator bearer token required",
            ))
        })?;
    let principal = store.authenticate(token)?;
    let result = tokio::task::spawn_blocking(move || store.compiler_pools(&principal))
        .await
        .map_err(|_| WebError(ApiError::new(ErrorCode::Internal, "pool query interrupted")))??;
    Ok(Json(result))
}

async fn evidence(
    State(store): State<Store>,
    headers: HeaderMap,
    axum::extract::Path(attempt_id): axum::extract::Path<String>,
) -> WebResult<Json<Vec<dacs_protocol::workflow::CompilerTelemetryRecord>>> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| {
            WebError(ApiError::new(
                ErrorCode::Unauthorized,
                "worker bearer token required",
            ))
        })?;
    let principal = store.authenticate(token)?;
    let result =
        tokio::task::spawn_blocking(move || store.compiler_telemetry(&principal, &attempt_id))
            .await
            .map_err(|_| {
                WebError(ApiError::new(
                    ErrorCode::Internal,
                    "compiler evidence query interrupted",
                ))
            })??;
    Ok(Json(result))
}
