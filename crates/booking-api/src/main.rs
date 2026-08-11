//! Theatre booking API — the service you are building.
//!
//! Wired up already: config, the SQLite pool, the migration runner, a JSON error type,
//! shared state with an HTTP client, `GET /health`, and graceful shutdown.
//!
//! Not wired up: anything to do with bookings or payments. See `README.md`.

mod config;
mod db;
mod error;
mod routes;
mod state;
mod models;

use std::net::{Ipv4Addr, SocketAddr};

use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() {
    // Print failures with Display, not Debug. Returning `Result` from `main` would render a
    // multi-line message as one line of escaped `\n`, which is exactly when you most need it
    // readable.
    if let Err(error) = run().await {
        eprintln!("\nerror: {error}\n");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let config = Config::from_env()?;
    tracing::info!(?config, "starting booking-api");

    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool, &config.migrations_dir).await?;
    tracing::info!("migrations applied");

    // Creates and seeds the database without leaving a server running. Handy after you
    // add a migration and just want to check that it applies.
    if migrate_only() {
        let shows: i64 = sqlx::query_scalar("SELECT count(*) FROM shows")
            .fetch_one(&pool)
            .await?;
        let seats: i64 = sqlx::query_scalar("SELECT count(*) FROM seats")
            .fetch_one(&pool)
            .await?;

        tracing::info!("seeded {seats} seats and {shows} shows; exiting");
        return Ok(());
    }

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
    let state = AppState::new(pool, config);
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// True when invoked as `--migrate-only`, or with `MIGRATE_ONLY` set to anything non-empty.
fn migrate_only() -> bool {
    std::env::args().any(|arg| arg == "--migrate-only")
        || std::env::var("MIGRATE_ONLY").is_ok_and(|v| !v.trim().is_empty())
}

fn init_tracing() {
    // `RUST_LOG=debug` for more, `RUST_LOG=booking_api=trace,sqlx=warn` to focus.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("booking_api=debug,tower_http=debug,sqlx=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .init();
}

/// Resolves on Ctrl-C or SIGTERM, letting in-flight requests finish.
///
/// This matters more than it looks: you will be restarting this process constantly to
/// watch the gateway retry a webhook, and a clean shutdown means you never lose a
/// half-committed transaction to a hard kill.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for Ctrl-C");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
