use axum::{extract::State, Json};
use uuid::Uuid;

use crate::error::AppResult;
use crate::models::{BookingResponse, BookingStatus, CreateBookingRequest};
use crate::state::AppState;

pub async fn create(
    State(_state): State<AppState>,
    Json(req): Json<CreateBookingRequest>,
) -> AppResult<Json<BookingResponse>> {
    let booking_id = Uuid::new_v4().to_string();
    Ok(Json(BookingResponse {
        id: booking_id,
        show_id: req.show_id,
        status: BookingStatus::Pending,
    }))
}
