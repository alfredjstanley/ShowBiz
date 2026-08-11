//! The gateway's HTTP API. `README.md` covers the parts you need.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::outcome::{Plan, Scenario};
use crate::store::{Payment, RefundResult, Store};

#[derive(Clone)]
pub struct GatewayState {
    pub store: Store,
    pub config: Arc<Config>,
    /// Only used to resolve a payment's plan at accept time.
    pub rng: Arc<Mutex<StdRng>>,
}

impl GatewayState {
    pub fn new(store: Store, config: Config) -> Self {
        let rng = match config.rng_seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => StdRng::from_os_rng(),
        };

        Self {
            store,
            config: Arc::new(config),
            rng: Arc::new(Mutex::new(rng)),
        }
    }
}

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/payments", post(create_payment).get(list_payments))
        .route("/v1/payments/{transaction_id}", get(get_payment))
        .route("/v1/payments/{transaction_id}/refund", post(refund_payment))
        .with_state(state)
}

// ----------------------------------------------------------------------------------------
// POST /v1/payments
// ----------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePaymentRequest {
    pub booking_reference: String,
    pub amount_minor: i64,
    #[serde(default = "default_currency")]
    pub currency: String,
    pub callback_url: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

fn default_currency() -> String {
    "INR".to_owned()
}

#[derive(Debug, Serialize)]
pub struct CreatePaymentResponse {
    pub transaction_id: String,
    pub status: String,
    pub booking_reference: String,
    pub amount_minor: i64,
    pub currency: String,
    pub scenario: String,
}

async fn create_payment(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<CreatePaymentRequest>,
) -> Result<Response, GatewayError> {
    if request.booking_reference.trim().is_empty() {
        return Err(GatewayError::invalid("booking_reference must not be empty"));
    }
    if request.amount_minor <= 0 {
        return Err(GatewayError::invalid(
            "amount_minor must be greater than zero",
        ));
    }
    if request.currency.len() != 3 {
        return Err(GatewayError::invalid("currency must be a 3-letter code"));
    }
    if reqwest::Url::parse(&request.callback_url).is_err() {
        return Err(GatewayError::invalid(
            "callback_url must be an absolute URL, e.g. http://127.0.0.1:8080/webhooks/payments",
        ));
    }

    // Replaying an idempotency key must never create a second charge.
    if let Some(key) = request.idempotency_key.as_deref() {
        if let Some(existing) = state.store.find_by_idempotency_key(key).await? {
            return Ok(replay_response(existing));
        }
    }

    let scenario = match headers.get("x-force-scenario") {
        None => Scenario::Random,
        Some(value) => value
            .to_str()
            .map_err(|_| GatewayError::invalid("X-Force-Scenario must be valid ASCII"))?
            .parse()
            .map_err(GatewayError::invalid)?,
    };

    let plan = {
        let mut rng = state.rng.lock().await;
        Plan::resolve(scenario, &state.config, &mut *rng)
    };

    let transaction_id = format!("txn_{}", uuid::Uuid::new_v4().simple());
    let scenario_label = scenario.as_str().to_owned();

    let inserted = state
        .store
        .insert_payment(
            &transaction_id,
            request.idempotency_key.as_deref(),
            &request.booking_reference,
            request.amount_minor,
            &request.currency,
            &request.callback_url,
            &scenario_label,
            &plan,
        )
        .await?;

    if !inserted {
        // A concurrent request with the same idempotency key beat us to it. Re-read and
        // replay: the alternative is a unique-constraint 500 on a perfectly reasonable retry.
        let key = request.idempotency_key.as_deref().unwrap_or_default();
        let existing = state
            .store
            .find_by_idempotency_key(key)
            .await?
            .ok_or_else(|| GatewayError::Database(sqlx::Error::RowNotFound))?;

        return Ok(replay_response(existing));
    }

    tracing::info!(
        %transaction_id,
        booking_reference = %request.booking_reference,
        amount_minor = request.amount_minor,
        scenario = %scenario_label,
        decide_in_ms = plan.decide_after_ms,
        "payment accepted"
    );

    let body = CreatePaymentResponse {
        transaction_id,
        status: "PENDING".to_owned(),
        booking_reference: request.booking_reference,
        amount_minor: request.amount_minor,
        currency: request.currency,
        scenario: scenario_label,
    };

    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// The response for a create call that turned out to be a replay of an existing payment.
///
/// Reports the payment's **real** status. Hardcoding `PENDING` here would mean a caller
/// replaying a key for an already-resolved payment gets told it is still in flight.
fn replay_response(existing: Payment) -> Response {
    tracing::info!(
        transaction_id = %existing.transaction_id,
        status = %existing.status,
        "idempotent replay — returning the existing transaction"
    );

    let body = CreatePaymentResponse {
        transaction_id: existing.transaction_id,
        status: existing.status,
        booking_reference: existing.booking_reference,
        amount_minor: existing.amount_minor,
        currency: existing.currency,
        scenario: existing.scenario,
    };

    (
        StatusCode::ACCEPTED,
        [("x-idempotent-replay", "true")],
        Json(body),
    )
        .into_response()
}

// ----------------------------------------------------------------------------------------
// GET /v1/payments/{id} and GET /v1/payments?booking_reference=…
// ----------------------------------------------------------------------------------------

async fn get_payment(
    State(state): State<GatewayState>,
    Path(transaction_id): Path<String>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let payment = state
        .store
        .get_payment(&transaction_id)
        .await?
        .ok_or_else(|| GatewayError::not_found(&transaction_id))?;

    Ok(Json(payment_json(&payment)))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    booking_reference: Option<String>,
}

async fn list_payments(
    State(state): State<GatewayState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let Some(reference) = query.booking_reference else {
        return Err(GatewayError::invalid(
            "booking_reference query parameter is required",
        ));
    };

    let payments = state.store.list_by_booking_reference(&reference).await?;
    let items: Vec<_> = payments.iter().map(payment_json).collect();

    Ok(Json(
        json!({ "booking_reference": reference, "payments": items }),
    ))
}

fn payment_json(payment: &Payment) -> serde_json::Value {
    json!({
        "transaction_id": payment.transaction_id,
        "booking_reference": payment.booking_reference,
        "amount_minor": payment.amount_minor,
        "currency": payment.currency,
        "status": payment.status,
        "failure_reason": payment.failure_reason,
        "scenario": payment.scenario,
        "created_at": iso(payment.created_at),
        "decided_at": payment.decided_at.map(iso),
        "refund_id": payment.refund_id,
        "refunded_at": payment.refunded_at.map(iso),
        "webhook_attempts": payment.webhook_attempts,
        "webhook_acknowledged": payment.webhook_acknowledged,
        // True when this payment's webhook will never be delivered (the `lost` scenario).
        // A real gateway would not tell you this; it is here so the exercise is debuggable.
        "webhook_suppressed": payment.webhook_suppressed,
    })
}

fn iso(at: chrono::DateTime<chrono::Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ----------------------------------------------------------------------------------------
// POST /v1/payments/{id}/refund
// ----------------------------------------------------------------------------------------

async fn refund_payment(
    State(state): State<GatewayState>,
    Path(transaction_id): Path<String>,
) -> Result<Response, GatewayError> {
    match state.store.refund(&transaction_id).await? {
        RefundResult::Refunded(refund_id) => {
            tracing::info!(%transaction_id, %refund_id, "payment refunded");
            Ok(Json(json!({
                "refund_id": refund_id,
                "transaction_id": transaction_id,
                "status": "REFUNDED",
            }))
            .into_response())
        }

        RefundResult::AlreadyRefunded(refund_id) => Ok((
            StatusCode::OK,
            [("x-idempotent-replay", "true")],
            Json(json!({
                "refund_id": refund_id,
                "transaction_id": transaction_id,
                "status": "REFUNDED",
            })),
        )
            .into_response()),

        RefundResult::NotRefundable(status) => Err(GatewayError::conflict(format!(
            "cannot refund a payment with status {status}"
        ))),

        RefundResult::NotFound => Err(GatewayError::not_found(&transaction_id)),
    }
}

async fn health(
    State(state): State<GatewayState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(json!({
        "status": "ok",
        "success_rate": state.config.success_rate,
        "duplicate_rate": state.config.duplicate_rate,
        "lost_webhook_rate": state.config.lost_webhook_rate,
        "late_webhook_rate": state.config.late_webhook_rate,
        "seeded": state.config.rng_seed.is_some(),
    })))
}

// ----------------------------------------------------------------------------------------
// Errors
// ----------------------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("no payment with transaction_id {0}")]
    NotFound(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl GatewayError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    fn not_found(transaction_id: &str) -> Self {
        Self::NotFound(transaction_id.to_owned())
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            GatewayError::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
            GatewayError::Conflict(_) => (StatusCode::CONFLICT, "CONFLICT"),
            GatewayError::NotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            GatewayError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "GATEWAY_ERROR"),
        };

        if status.is_server_error() {
            tracing::error!(error = %self, "gateway request failed");
        }

        let body = Json(json!({ "error": { "code": code, "message": self.to_string() } }));
        (status, body).into_response()
    }
}
