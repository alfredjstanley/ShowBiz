//! Shared application state, handed to every handler via `State<AppState>`.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;

/// Cheap to clone: the pool is internally reference-counted, `Config` sits behind an `Arc`,
/// and `reqwest::Client` is itself an `Arc` around a connection pool.
///
/// That last point matters. Build **one** `reqwest::Client` and clone it; constructing a
/// fresh client per request throws away connection pooling and will eventually exhaust
/// ephemeral ports under load. It is already wired up for you here.
#[allow(dead_code)] // `config` and `http` go unread until you call the gateway.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");

        Self {
            pool,
            config: Arc::new(config),
            http,
        }
    }
}
