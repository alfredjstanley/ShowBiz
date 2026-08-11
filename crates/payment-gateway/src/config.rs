//! Gateway configuration and chaos knobs.
//!
//! Everything here is read once at startup. The defaults are tuned to be *annoying but
//! survivable*: most payments succeed, a fifth get delivered twice, a few vanish.

use std::env;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub webhook_secret: String,

    /// Probability a payment is decided SUCCESS rather than FAILED.
    pub success_rate: f64,
    /// Probability a decided payment is delivered twice (same `event_id`).
    pub duplicate_rate: f64,
    /// Probability the outcome is reached but the webhook is *never* delivered. Only
    /// polling `GET /v1/payments/{id}` will reveal it.
    pub lost_webhook_rate: f64,
    /// Probability the webhook is delivered long after the outcome was decided.
    pub late_webhook_rate: f64,
    /// How long "long after" means.
    pub late_webhook_delay_ms: u64,

    /// Bounds on how long the gateway "thinks" before deciding an outcome.
    ///
    /// The minimum defaults to **zero**, on purpose: a webhook can then land before the call
    /// that created the payment has even returned. Anything keyed off the transaction id
    /// rather than the booking reference breaks there, which is the point.
    pub outcome_delay_ms_min: u64,
    pub outcome_delay_ms_max: u64,

    /// First retry waits this long; each subsequent attempt doubles it, capped below.
    pub webhook_backoff_base_ms: u64,
    pub webhook_backoff_max_ms: u64,

    /// Fixing this makes an entire session reproducible. Unset means OS randomness.
    pub rng_seed: Option<u64>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let config = Self {
            port: parse_env("GATEWAY_PORT", 9090)?,
            database_url: string_env("GATEWAY_DATABASE_URL", "sqlite://data/gateway.db?mode=rwc"),
            webhook_secret: string_env("PAYMENT_WEBHOOK_SECRET", "dev-webhook-secret"),

            success_rate: parse_env("SUCCESS_RATE", 0.7)?,
            duplicate_rate: parse_env("DUPLICATE_RATE", 0.2)?,
            lost_webhook_rate: parse_env("LOST_WEBHOOK_RATE", 0.05)?,
            late_webhook_rate: parse_env("LATE_WEBHOOK_RATE", 0.05)?,
            late_webhook_delay_ms: parse_env("LATE_WEBHOOK_DELAY_MS", 45_000)?,

            outcome_delay_ms_min: parse_env("OUTCOME_DELAY_MS_MIN", 0)?,
            outcome_delay_ms_max: parse_env("OUTCOME_DELAY_MS_MAX", 5_000)?,

            webhook_backoff_base_ms: parse_env("WEBHOOK_BACKOFF_BASE_MS", 1_000)?,
            webhook_backoff_max_ms: parse_env("WEBHOOK_BACKOFF_MAX_MS", 30_000)?,

            rng_seed: match env::var("RNG_SEED") {
                Ok(raw) if !raw.trim().is_empty() => {
                    Some(raw.trim().parse().map_err(|_| ConfigError {
                        name: "RNG_SEED",
                        value: raw,
                    })?)
                }
                _ => None,
            },
        };

        for (name, value) in [
            ("SUCCESS_RATE", config.success_rate),
            ("DUPLICATE_RATE", config.duplicate_rate),
            ("LOST_WEBHOOK_RATE", config.lost_webhook_rate),
            ("LATE_WEBHOOK_RATE", config.late_webhook_rate),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(ConfigError {
                    name,
                    value: format!("{value} (must be between 0.0 and 1.0)"),
                });
            }
        }

        if config.outcome_delay_ms_min > config.outcome_delay_ms_max {
            return Err(ConfigError {
                name: "OUTCOME_DELAY_MS_MIN",
                value: format!(
                    "{} exceeds OUTCOME_DELAY_MS_MAX ({})",
                    config.outcome_delay_ms_min, config.outcome_delay_ms_max
                ),
            });
        }

        Ok(config)
    }

    /// Config used by `self-test`: deterministic, and fast enough to assert on.
    pub fn for_self_test(database_url: String) -> Self {
        Self {
            port: 0,
            database_url,
            webhook_secret: "self-test-secret".to_owned(),
            success_rate: 1.0,
            duplicate_rate: 0.0,
            lost_webhook_rate: 0.0,
            late_webhook_rate: 0.0,
            late_webhook_delay_ms: 1_000,
            outcome_delay_ms_min: 0,
            outcome_delay_ms_max: 0,
            webhook_backoff_base_ms: 50,
            webhook_backoff_max_ms: 200,
            rng_seed: Some(42),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid value for environment variable {name}: {value}")]
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
