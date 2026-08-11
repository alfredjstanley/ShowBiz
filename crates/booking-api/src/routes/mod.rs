//! Route table.
//!
//! Add your routes here. Only the webhook path is fixed, because the gateway calls it;
//! everything else is yours to design. See `README.md`.

pub mod booking;
pub mod health;
pub mod webhooks;

use axum::routing::{get, post};
use axum::Router;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health::health))
        .route("/bookings", post(booking::create))
        .route("/webhooks/payments", post(webhooks::payments))
        // Your routes go here. For example:
        //
        //   .route("/shows", get(shows::list))
        //   .route("/shows/{id}/seats", get(shows::seats))
        //   .route("/bookings", post(bookings::create))
        //   .route("/bookings/{id}", get(bookings::get))
        //   .route("/webhooks/payments", post(webhooks::payments))
        //
        // Note axum 0.8 path syntax: `{id}`, not the `:id` used in 0.7 and earlier.
        .with_state(state)
}
