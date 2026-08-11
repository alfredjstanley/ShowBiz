//! Two race conditions handled here:
//!
//!   1. The webhook can arrive before `bookings.transaction_id` is even saved (our own
//!      create-booking handler may still be mid-flight). Handled by simply not finding
//!      a match and returning a non-2xx - the gateway's own retry-with-backoff resolves
//!      this without us writing any sleep/retry logic ourselves.
//!
//!   2. The same event can be delivered twice, concurrently. Handled by attempting to
//!      INSERT the event_id into `payment_events` *before* acting on it - its PRIMARY
//!      KEY rejects the second of two simultaneous duplicate inserts, atomically.

use axum::extract::State;
use axum::Json;
use serde_json::json;
use sqlx::error::ErrorKind;

use crate::error::{AppError, AppResult};
use crate::models::{BookingStatus, PaymentWebhookPayload, SeatStatus, WebhookPaymentStatus};
use crate::state::AppState;

pub async fn payments(
    State(state): State<AppState>,
    Json(payload): Json<PaymentWebhookPayload>,
) -> AppResult<Json<serde_json::Value>> {
    // Check does a booking with this transaction_id exist yet?
    let booking: Option<(String,)> =
        sqlx::query_as("SELECT id FROM bookings WHERE transaction_id = ?")
            .bind(&payload.transaction_id)
            .fetch_optional(&state.pool)
            .await?;

    let (booking_id,) = booking.ok_or_else(|| {
        AppError::NotFound(format!(
            "no booking for transaction {}",
            payload.transaction_id
        ))
    })?;

    // Claim this event. If the INSERT fails on the PRIMARY KEY, someone (maybe
    // this exact handler, running concurrently for the duplicate delivery) already
    // claimed it — do nothing further, but still return 200 otherwise the gateway
    // retries this "duplicate" forever.
    let claim = sqlx::query("INSERT INTO payment_events (event_id) VALUES (?)")
        .bind(&payload.event_id)
        .execute(&state.pool)
        .await;

    match claim {
        Ok(_) => {}
        Err(sqlx::Error::Database(db_err)) if db_err.kind() == ErrorKind::UniqueViolation => {
            return Ok(Json(json!({ "status": "already processed" })));
        }
        Err(other) => return Err(other.into()),
    }

    // First time seeing this event - resolve the booking + its seats,
    // both in one transaction so they move together.
    let mut tx = state.pool.begin().await?;

    let (booking_status, seat_status) = match payload.status {
        WebhookPaymentStatus::Success => (BookingStatus::Confirmed, SeatStatus::Confirmed),
        WebhookPaymentStatus::Failed => (BookingStatus::Failed, SeatStatus::Released),
    };

    sqlx::query("UPDATE bookings SET status = ? WHERE id = ? AND status = ?")
        .bind(booking_status.as_str())
        .bind(&booking_id)
        .bind(BookingStatus::Pending.as_str())
        .execute(&mut *tx)
        .await?;

    sqlx::query("UPDATE booking_seats SET status = ? WHERE booking_id = ? AND status = ?")
        .bind(seat_status.as_str())
        .bind(&booking_id)
        .bind(SeatStatus::Held.as_str())
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(Json(json!({ "status": "processed" })))
}
