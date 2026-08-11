//! Liveness endpoint — and a worked example of the shape every handler here takes.
//!
//! Note the pieces, because you will repeat them: `State<AppState>` to reach the pool,
//! sqlx's *runtime* query API, `?` on a fallible call because `AppError` implements
//! `IntoResponse`, and a `Json` return value.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::error::AppResult;
use crate::state::AppState;

#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
    db: &'static str,
    /// Number of shows currently seeded, as a quick confirmation that reference data is
    /// present and the schedule has not gone stale.
    shows: i64,
}

pub async fn health(State(state): State<AppState>) -> AppResult<Json<HealthResponse>> {
    let shows: i64 = sqlx::query_scalar("SELECT count(*) FROM shows")
        .fetch_one(&state.pool)
        .await?;

    Ok(Json(HealthResponse {
        status: "ok",
        db: "ok",
        shows,
    }))
}
