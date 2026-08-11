//! The background worker: decides pending payments, then delivers webhooks.
//!
//! One tick every 250ms, two phases. All the state lives in SQLite, so "resume after a
//! restart" needs no special code path — whatever was due before the process died is still
//! due when it comes back, with its attempt counter intact.
//!
//! ## Deliveries are concurrent
//!
//! Every delivery due on a tick is dispatched **at once**, not one after another. A planned
//! duplicate is two delivery rows sharing one `event_id` and one `next_attempt_at`, so both
//! requests hit the receiver simultaneously. Real providers behave this way, and a receiver
//! that dedups with a separate read-then-write will double-apply under it.
//!
//! ## Retry policy
//!
//! A delivery is acknowledged only by a **2xx**. Anything else — 4xx, 5xx, connection
//! refused, timeout — is a failed attempt, and the gateway will try again, **forever**,
//! with exponential backoff: base, 2x, 4x, 8x, … capped at `WEBHOOK_BACKOFF_MAX_MS`, with
//! up to 20% jitter.
//!
//! There is no attempt limit and no dead-letter queue. Kill your API mid-flight and the
//! attempts pile up; bring it back and the webhook lands. That is the point.

use std::sync::Arc;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::signature;
use crate::store::{DueDelivery, Store};

const TICK: Duration = Duration::from_millis(250);

pub struct Worker {
    store: Store,
    config: Config,
    http: reqwest::Client,
    rng: Mutex<StdRng>,
}

impl Worker {
    pub fn new(store: Store, config: Config, http: reqwest::Client) -> Self {
        let rng = match config.rng_seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => StdRng::from_os_rng(),
        };

        Self {
            store,
            config,
            http,
            rng: Mutex::new(rng),
        }
    }

    /// Runs until the process ends.
    pub async fn run(self: std::sync::Arc<Self>) {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            if let Err(error) = self.decide_due_payments().await {
                tracing::error!(%error, "failed to decide payments");
            }
            if let Err(error) = self.deliver_due_webhooks().await {
                tracing::error!(%error, "failed to deliver webhooks");
            }
        }
    }

    /// Phase one: apply outcomes whose "thinking time" has elapsed.
    async fn decide_due_payments(&self) -> Result<(), sqlx::Error> {
        for payment in self.store.take_decidable().await? {
            let event_id = format!("evt_{}", uuid::Uuid::new_v4().simple());
            let applied = self.store.apply_decision(&payment, &event_id).await?;

            if applied {
                tracing::info!(
                    transaction_id = %payment.transaction_id,
                    outcome = %payment.outcome,
                    webhook = payment.deliver_webhook,
                    deliveries = payment.deliveries,
                    "payment decided"
                );

                if !payment.deliver_webhook {
                    tracing::warn!(
                        transaction_id = %payment.transaction_id,
                        "webhook suppressed for this payment — only GET /v1/payments/{{id}} \
                         will reveal the outcome"
                    );
                }
            }
        }

        Ok(())
    }

    /// Phase two: dispatch every due delivery concurrently.
    ///
    /// `take_due_deliveries` caps the batch, which bounds how many run at once. A failure on
    /// one delivery is logged and skipped rather than propagated — otherwise a single database
    /// error would silently starve every other delivery on this tick.
    async fn deliver_due_webhooks(self: &Arc<Self>) -> Result<(), sqlx::Error> {
        let due = self.store.take_due_deliveries().await?;
        if due.is_empty() {
            return Ok(());
        }

        let mut tasks = JoinSet::new();
        for delivery in due {
            let worker = Arc::clone(self);
            tasks.spawn(async move {
                if let Err(error) = worker.attempt(&delivery).await {
                    tracing::error!(
                        delivery_id = %delivery.delivery_id,
                        %error,
                        "could not record webhook delivery outcome"
                    );
                }
            });
        }

        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    async fn attempt(&self, delivery: &DueDelivery) -> Result<(), sqlx::Error> {
        let attempt = delivery.attempts + 1;
        let body = self.build_body(delivery, attempt);
        let raw = serde_json::to_vec(&body).expect("event payload is serialisable");
        let sig = signature::sign(&self.config.webhook_secret, &raw);

        let response = self
            .http
            .post(&delivery.callback_url)
            .header("content-type", "application/json")
            .header("x-payment-event-id", &delivery.event_id)
            .header("x-payment-signature", sig)
            .header("idempotency-key", &delivery.event_id)
            .body(raw)
            .send()
            .await;

        match response {
            Ok(response) if response.status().is_success() => {
                let code = response.status().as_u16();
                self.store
                    .record_attempt_acknowledged(&delivery.delivery_id, code)
                    .await?;

                tracing::info!(
                    event_id = %delivery.event_id,
                    transaction_id = %delivery.transaction_id,
                    attempt,
                    status = code,
                    "webhook acknowledged"
                );
            }

            Ok(response) => {
                let code = response.status().as_u16();
                let retry_in = self.backoff(attempt).await;

                self.store
                    .record_attempt_failed(
                        &delivery.delivery_id,
                        Some(code),
                        &format!("HTTP {code}"),
                        retry_in,
                    )
                    .await?;

                tracing::warn!(
                    event_id = %delivery.event_id,
                    attempt,
                    status = code,
                    retry_in_ms = retry_in.as_millis(),
                    "webhook rejected, will retry"
                );
            }

            Err(error) => {
                let retry_in = self.backoff(attempt).await;
                let reason = concise_error(&error);

                self.store
                    .record_attempt_failed(&delivery.delivery_id, None, &reason, retry_in)
                    .await?;

                tracing::warn!(
                    event_id = %delivery.event_id,
                    attempt,
                    error = %reason,
                    retry_in_ms = retry_in.as_millis(),
                    "webhook delivery failed, will retry"
                );
            }
        }

        Ok(())
    }

    fn build_body(&self, delivery: &DueDelivery, attempt: i64) -> serde_json::Value {
        json!({
            "event_id": delivery.event_id,
            "transaction_id": delivery.transaction_id,
            "booking_reference": delivery.booking_reference,
            "status": delivery.status,
            "amount_minor": delivery.amount_minor,
            "currency": delivery.currency,
            "failure_reason": delivery.failure_reason,
            "occurred_at": delivery.decided_at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            "attempt": attempt,
        })
    }

    async fn backoff(&self, attempt: i64) -> Duration {
        let mut rng = self.rng.lock().await;
        backoff_for(&self.config, attempt, &mut *rng)
    }
}

/// Exponential backoff with jitter, capped.
///
/// Free function rather than a method so it can be tested without standing up a database
/// pool and an HTTP client.
fn backoff_for<R: Rng>(config: &Config, attempt: i64, rng: &mut R) -> Duration {
    let exponent = u32::try_from((attempt.max(1) - 1).min(16)).unwrap_or(16);
    let base = config
        .webhook_backoff_base_ms
        .saturating_mul(2u64.saturating_pow(exponent))
        .min(config.webhook_backoff_max_ms);

    // Jitter stops a fleet of retries from synchronising. Here it mostly just makes the log
    // timings look realistic.
    let jitter = rng.random_range(0..=(base / 5).max(1));

    Duration::from_millis(base + jitter)
}

/// `reqwest`'s Display is a chain of sources; the first line is the useful part.
fn concise_error(error: &reqwest::Error) -> String {
    if error.is_connect() {
        "connection refused".to_owned()
    } else if error.is_timeout() {
        "timed out".to_owned()
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        let config = Config {
            webhook_backoff_base_ms: 1_000,
            webhook_backoff_max_ms: 30_000,
            ..Config::for_self_test("sqlite://:memory:".to_owned())
        };
        let mut rng = StdRng::seed_from_u64(1);

        let mut ms = |attempt| backoff_for(&config, attempt, &mut rng).as_millis() as u64;

        // 1s, 2s, 4s, 8s … then flat at the 30s cap. Jitter adds up to 20% on top.
        assert!((1_000..1_200).contains(&ms(1)));
        assert!((2_000..2_400).contains(&ms(2)));
        assert!((4_000..4_800).contains(&ms(3)));
        assert!((8_000..9_600).contains(&ms(4)));
        assert!((30_000..36_000).contains(&ms(20)), "should be capped");
    }

    #[test]
    fn backoff_never_overflows_on_a_long_running_retry() {
        let config = Config::for_self_test("sqlite://:memory:".to_owned());
        let mut rng = StdRng::seed_from_u64(1);

        // A webhook against a permanently-down endpoint really does reach these numbers.
        for attempt in [100_i64, 10_000, i64::MAX] {
            let waited = backoff_for(&config, attempt, &mut rng);
            assert!(waited.as_millis() as u64 <= config.webhook_backoff_max_ms * 2);
        }
    }
}
