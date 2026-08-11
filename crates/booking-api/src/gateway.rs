//! The module that speaks to `payment-gateway`.

use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Serialize)]
struct CreatePaymentRequest<'a> {
    booking_reference: &'a str,
    amount_minor: i64,
    currency: &'a str,
    callback_url: &'a str,
    // Reusing booking_reference as the idempotency key: one booking should only ever
    // start one payment, so if this handler is ever retried, the gateway hands back
    // the existing transaction instead of charging twice.
    idempotency_key: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct CreatePaymentResponse {
    pub transaction_id: String,
}

/// POST /v1/payments
/// Starts a payment. Returns once the gateway has accepted it;
/// the actual outcome (success/failure) arrives later via webhook.
pub async fn create_payment(
    state: &AppState,
    booking_reference: &str,
    amount_minor: i64,
    currency: &str,
) -> Result<CreatePaymentResponse, AppError> {
    let url = format!("{}/v1/payments", state.config.payment_gateway_url);

    let body = CreatePaymentRequest {
        booking_reference,
        amount_minor,
        currency,
        callback_url: &state.config.webhook_callback_url,
        idempotency_key: booking_reference,
    };

    let response = state
        .http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Upstream(format!("payment gateway unreachable: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::Upstream(format!(
            "payment gateway returned {status}: {text}"
        )));
    }

    response
        .json::<CreatePaymentResponse>()
        .await
        .map_err(|e| AppError::Upstream(format!("payment gateway response: {e}")))
}
