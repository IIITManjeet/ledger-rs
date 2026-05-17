use crate::error::AppError;
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde_json::json;

/// Liveness: 200 if the process is reachable. No DB call — used by the
/// orchestrator to decide whether to restart the container.
pub async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Readiness: 200 if we can serve traffic — DB pool is up and responding.
/// Used by the load balancer to decide whether to route requests here.
pub async fn readyz(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(state.db.pool())
        .await
        .map_err(|e| AppError::not_ready(format!("db ping failed: {e}")))?;
    Ok(Json(json!({ "status": "ready" })))
}
