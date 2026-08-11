use axum::{extract::State, Json};
use sqlx::error::ErrorKind;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
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
    let exists: Option<(i64,)> = sqlx::query_as(
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
        // `WHERE id = ?` means:
        // "Only look for the show whose ID we'll provide with `.bind(...)`."
        "SELECT (starts_at <= strftime('%Y-%m-%dT%H:%M:%SZ', 'now')) AS already_started \
         FROM shows WHERE id = ?",
    )
    .bind(req.show_id)
    // Execute the query and return either:
    //   Some(row) -> a matching show was found
    //   None      -> no matching show was found
    .fetch_optional(&state.pool)
    .await?;

    // `exists` is currently:
    //
    //     Option<(i64,)>
    //
    // `ok_or_else(...)` converts that Option into a Result:
    //
    //     Some((0,)) -> Ok((0,))
    //     Some((1,)) -> Ok((1,))
    //     None       -> Err(AppError::Validation(...))
    //
    let (already_started,) =
        exists.ok_or_else(|| AppError::Validation(format!("unknown show {}", req.show_id)))?;

    // `(already_started,)` destructures the one-element tuple.
    //
    // For example:
    //
    //     (1,) -> already_started = 1
    //     (0,) -> already_started = 0
    //
    // After this line, `already_started` is simply an `i64`.

    // SQLite represents false as 0 and true as 1.
    //
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

    let booking_id = Uuid::new_v4().to_string();

    // One transaction: the booking row and every seat hold succeed together, or none of
    // them do. If any seat's INSERT hits the unique index, `tx` drops without `commit()`
    // which rolls back the booking row AND seats
    let mut tx = state.pool.begin().await?;

    sqlx::query("INSERT INTO bookings (id, show_id, status) VALUES (?, ?, ?)")
        .bind(&booking_id)
        .bind(req.show_id)
        .bind(BookingStatus::Pending.as_str())
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

    Ok(Json(BookingResponse {
        id: booking_id,
        show_id: req.show_id,
        seat_ids: req.seat_ids,
        status: BookingStatus::Pending,
    }))
}
