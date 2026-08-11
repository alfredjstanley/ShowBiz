use axum::{extract::State, Json};
use sqlx::error::ErrorKind;
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use std::collections::HashSet;

use crate::error::{AppError, AppResult};
use crate::gateway;
use crate::models::{BookingResponse, BookingStatus, CreateBookingRequest, SeatStatus};
use crate::state::AppState;

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateBookingRequest>,
) -> AppResult<Json<BookingResponse>> {
    if req.seat_ids.is_empty() {
        return Err(AppError::Validation(
            "At least one seat is required".to_string(),
        ));
    }
    // Run a SQL query and try to map each returned row into the Rust type `(i64,)`.
    //
    // `Option` means the query may return:
    //   Some((0,))  -> show exists and hasn't started
    //   Some((1,))  -> show exists and has started
    //   None        -> show doesn't exist

    let show: Option<(i64, String, i64)> = sqlx::query_as(
        // Ask SQLite whether the show's start time is <= the current time.
        //
        // `starts_at <= ...` produces a boolean-like value in SQLite:
        //   0 -> false -> show has NOT started
        //   1 -> true  -> show HAS started
        //
        // `strftime(..., 'now')` gets the current UTC time as a string.
        //
        // `AS already_started` gives the resulting column a name.
        //
        // price_multiplier_bp and currency: needed to price the seats below.
        "SELECT price_multiplier_bp, currency, \
            (starts_at <= strftime('%Y-%m-%dT%H:%M:%SZ', 'now')) AS already_started \
     FROM shows WHERE id = ?",
    )
    .bind(req.show_id)
    // Execute the query and return either:
    //   Some(row) -> a matching show was found
    //   None      -> no matching show was found
    .fetch_optional(&state.pool)
    .await?;

    let (price_multiplier_bp, currency, already_started) =
        show.ok_or_else(|| AppError::Validation(format!("unknown show {}", req.show_id)))?;
  

    if already_started != 0 {
        // Reject the booking because the show has already started.
        return Err(AppError::Validation(format!(
            // Include the show ID in the error message so the client
            // knows which show caused the validation failure.
            "show {} has already started",
            // Insert the requested show ID into the error message.
            req.show_id
        )));
    }

    // QueryBuilder, since sqlx can't bind a Vec directly.
    let mut qb: QueryBuilder<Sqlite> =
        QueryBuilder::new("SELECT id, price_minor FROM seats WHERE id IN (");

    let mut seperated = qb.separated(", ");
    for seat_id in &req.seat_ids {
        seperated.push_bind(seat_id);
    }
    seperated.push_unseparated(")");

    let priced_seats: Vec<(String, i64)> = qb.build_query_as().fetch_all(&state.pool).await?;

    if priced_seats.len() != req.seat_ids.len() {
        let found: HashSet<&str> = priced_seats.iter().map(|(id, _)| id.as_str()).collect();
        let unknown: Vec<&str> = req
            .seat_ids
            .iter()
            .map(String::as_str)
            .filter(|id| !found.contains(id))
            .collect();
        return Err(AppError::Validation(format!(
            "unknown seat id(s): {}",
            unknown.join(", ")
        )));
    }

    let seats_total: i64 = priced_seats.iter().map(|(_, price)| price).sum();
    let amount_minor = seats_total * price_multiplier_bp / 10_000;

    let booking_id = Uuid::new_v4().to_string();

    // One transaction: the booking row and every seat hold succeed together, or none of
    // them do. If any seat's INSERT hits the unique index, `tx` drops without `commit()`
    // which rolls back the booking row AND seats
    let mut tx = state.pool.begin().await?;

    sqlx::query(
        "INSERT INTO bookings (id, show_id, status, amount_minor, currency) \
        VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&booking_id)
    .bind(req.show_id)
    .bind(BookingStatus::Pending.as_str())
    .bind(amount_minor)
    .bind(&currency)
    .execute(&mut *tx)
    .await?;

    for seat_id in &req.seat_ids {
        let result = sqlx::query(
            "INSERT INTO booking_seats (booking_id, show_id, seat_id, status) VALUES (?, ?, ?, ?)",
        )
        .bind(&booking_id)
        .bind(req.show_id)
        .bind(seat_id)
        .bind(SeatStatus::Held.as_str())
        .execute(&mut *tx)
        .await;

        match result {
            Ok(_) => {}
            Err(sqlx::Error::Database(db_error))
                if db_error.kind() == ErrorKind::UniqueViolation =>
            {
                return Err(AppError::Conflict(format!(
                    "seat {seat_id} is already held or booked"
                )));
            }
            Err(other) => return Err(other.into()),
        }
    }

    tx.commit().await?;

    match gateway::create_payment(&state, &booking_id, amount_minor, &currency).await {
        Ok(payment) => {
            sqlx::query("UPDATE bookings SET transaction_id = ? WHERE id = ?")
                .bind(&payment.transaction_id)
                .bind(&booking_id)
                .execute(&state.pool)
                .await?;

            Ok(Json(BookingResponse {
                id: booking_id,
                show_id: req.show_id,
                seat_ids: req.seat_ids,
                amount_minor,
                currency,
                status: BookingStatus::Pending,
                transaction_id: Some(payment.transaction_id),
            }))
        }

        Err(err) => {
            // Gateway unreachable / rejected: release what we just held
            let mut tx = state.pool.begin().await?;

            sqlx::query("UPDATE bookings SET status = ? WHERE id = ?")
                .bind(BookingStatus::Failed.as_str())
                .bind(&booking_id)
                .execute(&mut *tx)
                .await?;

            sqlx::query("UPDATE booking_seats SET status = ? WHERE booking_id = ? AND status = ?")
                .bind(SeatStatus::Released.as_str())
                .bind(&booking_id)
                .bind(SeatStatus::Held.as_str())
                .execute(&mut *tx)
                .await?;

            tx.commit().await?;
            Err(err)
        }
    }
}
