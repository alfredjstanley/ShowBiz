//! Configuration, read from the environment with sane defaults.
//!
//! Hand-rolled rather than using a config crate, so you can read all of it in one screen.

use std::env;
use std::str::FromStr;
use std::time::Duration;

// Some of these go unused until you start calling the gateway. The allow keeps the starting
// build warning-free; drop it once you are using them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Config {
    /// Port this API listens on.
    pub port: u16,
    /// sqlx connection string. The parent directory is created automatically.
    pub database_url: String,
    /// Directory holding `*.sql` migrations, resolved relative to the working directory.
    pub migrations_dir: String,
    /// Base URL of the mock payment gateway, e.g. `http://127.0.0.1:9090`.
    pub payment_gateway_url: String,
    /// The URL you hand to the gateway so it knows where to deliver webhooks. It must
    /// point at *your* webhook route.
    pub webhook_callback_url: String,
    /// Shared secret the gateway signs webhook bodies with, in `X-Payment-Signature`.
    /// Only needed if you decide to verify it.
    pub webhook_secret: String,
    /// How long a booking may hold seats before you consider it abandoned. Here if you want
    /// it; nothing in the boilerplate uses it.
    pub booking_hold_ttl: Duration,
}

impl Config {
    /// Reads configuration from the environment.
    ///
    /// Malformed values are a hard error rather than a silent fallback to the default:
    /// a typo in `BOOKING_HOLD_TTL_SECONDS` should not look like it worked.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            port: parse_env("PORT", 8080)?,
            database_url: string_env("DATABASE_URL", "sqlite://data/booking.db?mode=rwc"),
            migrations_dir: string_env("MIGRATIONS_DIR", "./migrations"),
            payment_gateway_url: string_env("PAYMENT_GATEWAY_URL", "http://127.0.0.1:9090")
                .trim_end_matches('/')
                .to_owned(),
            webhook_callback_url: string_env(
                "WEBHOOK_CALLBACK_URL",
                "http://127.0.0.1:8080/webhooks/payments",
            ),
            webhook_secret: string_env("PAYMENT_WEBHOOK_SECRET", "dev-webhook-secret"),
            booking_hold_ttl: Duration::from_secs(parse_env("BOOKING_HOLD_TTL_SECONDS", 120)?),
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid value for environment variable {name}: {value:?}")]
pub struct ConfigError {
    pub name: &'static str,
    pub value: String,
}

fn string_env(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn parse_env<T: FromStr>(name: &'static str, default: T) -> Result<T, ConfigError> {
    match env::var(name) {
        Err(_) => Ok(default),
        Ok(raw) if raw.trim().is_empty() => Ok(default),
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|_| ConfigError { name, value: raw }),
    }
}
