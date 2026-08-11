//! Gateway persistence.
//!
//! The gateway keeps its own SQLite database, entirely separate from your booking
//! database. That is deliberate: it is a third-party service, and you should not be able
//! to reach into its tables to find out what it decided. Use its HTTP API.
//!
//! Everything about a payment's future — outcome, timing, delivery count — is written down
//! at accept time, and every delivery attempt is persisted as it happens. That is what
//! makes "keep retrying the webhook until it is acknowledged" survive a restart of
//! *either* process.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::outcome::{Outcome, Plan};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS payments (
    transaction_id      TEXT PRIMARY KEY,
    idempotency_key     TEXT UNIQUE,
    booking_reference   TEXT NOT NULL,
    amount_minor        INTEGER NOT NULL,
    currency            TEXT NOT NULL,
    callback_url        TEXT NOT NULL,
    scenario            TEXT NOT NULL,
    status              TEXT NOT NULL,
    failure_reason      TEXT,
    planned_outcome     TEXT NOT NULL,
    planned_failure_reason TEXT,
    deliver_webhook     INTEGER NOT NULL,
    deliver_after_ms    INTEGER NOT NULL DEFAULT 0,
    planned_deliveries  INTEGER NOT NULL,
    created_at          TEXT NOT NULL,
    decide_at           TEXT NOT NULL,
    decided_at          TEXT,
    refund_id           TEXT,
    refunded_at         TEXT
);

CREATE INDEX IF NOT EXISTS idx_payments_booking_reference
    ON payments (booking_reference);
CREATE INDEX IF NOT EXISTS idx_payments_pending
    ON payments (status, decide_at);

-- One row per *intended delivery*, not per event. A duplicate is two rows sharing one
-- `event_id`, both due at the same instant, delivered concurrently. That is what makes a
-- check-then-insert dedup in the receiver actually fail, which is the point.
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    delivery_id          TEXT PRIMARY KEY,
    event_id             TEXT NOT NULL,
    transaction_id       TEXT NOT NULL REFERENCES payments (transaction_id),
    attempts             INTEGER NOT NULL DEFAULT 0,
    acknowledged_count   INTEGER NOT NULL DEFAULT 0,
    next_attempt_at      TEXT NOT NULL,
    last_status_code     INTEGER,
    last_error           TEXT,
    created_at           TEXT NOT NULL,
    completed_at         TEXT
);

CREATE INDEX IF NOT EXISTS idx_deliveries_due
    ON webhook_deliveries (completed_at, next_attempt_at);
CREATE INDEX IF NOT EXISTS idx_deliveries_transaction
    ON webhook_deliveries (transaction_id);
"#;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// A payment as reported over the API.
#[derive(Debug, Clone)]
pub struct Payment {
    pub transaction_id: String,
    pub booking_reference: String,
    pub amount_minor: i64,
    pub currency: String,
    pub status: String,
    pub failure_reason: Option<String>,
    pub scenario: String,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub refund_id: Option<String>,
    pub refunded_at: Option<DateTime<Utc>>,
    pub webhook_attempts: i64,
    pub webhook_acknowledged: bool,
    pub webhook_suppressed: bool,
}

/// A webhook delivery that is due to be attempted.
#[derive(Debug, Clone)]
pub struct DueDelivery {
    /// Identifies this delivery row. Two rows can share one `event_id`.
    pub delivery_id: String,
    pub event_id: String,
    pub transaction_id: String,
    pub booking_reference: String,
    pub amount_minor: i64,
    pub currency: String,
    pub status: String,
    pub failure_reason: Option<String>,
    pub callback_url: String,
    pub decided_at: DateTime<Utc>,
    pub attempts: i64,
}

impl Store {
    pub async fn open(database_url: &str) -> Result<Self, sqlx::Error> {
        let options: SqliteConnectOptions = database_url.parse()?;

        if let Some(parent) = options.get_filename().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(sqlx::Error::Io)?;
            }
        }

        let options = options
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        // Plain DDL rather than a migration directory: the gateway's schema is not
        // something anyone is meant to evolve.
        for statement in SCHEMA.split(';').filter(|s| !s.trim().is_empty()) {
            sqlx::query(statement).execute(&pool).await?;
        }

        Ok(Self { pool })
    }

    /// Looks up an existing payment by its idempotency key.
    pub async fn find_by_idempotency_key(&self, key: &str) -> Result<Option<Payment>, sqlx::Error> {
        let id: Option<String> =
            sqlx::query_scalar("SELECT transaction_id FROM payments WHERE idempotency_key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;

        match id {
            Some(id) => self.get_payment(&id).await,
            None => Ok(None),
        }
    }

    /// Records a newly accepted payment, along with the plan for how it will play out.
    ///
    /// Returns `false` if the `idempotency_key` was already taken, in which case nothing was
    /// written.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_payment(
        &self,
        transaction_id: &str,
        idempotency_key: Option<&str>,
        booking_reference: &str,
        amount_minor: i64,
        currency: &str,
        callback_url: &str,
        scenario: &str,
        plan: &Plan,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();
        let decide_at = now + chrono::Duration::milliseconds(plan.decide_after_ms as i64);

        let result = sqlx::query(
            "INSERT INTO payments (
                 transaction_id, idempotency_key, booking_reference, amount_minor, currency,
                 callback_url, scenario, status, planned_outcome, planned_failure_reason,
                 deliver_webhook, deliver_after_ms, planned_deliveries, created_at, decide_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 'PENDING', ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (idempotency_key) DO NOTHING",
        )
        .bind(transaction_id)
        .bind(idempotency_key)
        .bind(booking_reference)
        .bind(amount_minor)
        .bind(currency)
        .bind(callback_url)
        .bind(scenario)
        .bind(plan.outcome.as_str())
        .bind(plan.failure_reason)
        .bind(plan.deliver_webhook)
        .bind(plan.deliver_after_ms as i64)
        .bind(plan.deliveries)
        .bind(iso(now))
        .bind(iso(decide_at))
        .execute(&self.pool)
        .await?;

        // `false` means a concurrent request already claimed this idempotency key. The
        // caller re-reads and replays rather than surfacing a constraint violation as a 500 —
        // charging twice, or 500ing a legitimate retry, would both be worse than this.
        Ok(result.rows_affected() > 0)
    }

    pub async fn get_payment(&self, transaction_id: &str) -> Result<Option<Payment>, sqlx::Error> {
        let row = sqlx::query(&payment_query("p.transaction_id = ?"))
            .bind(transaction_id)
            .fetch_optional(&self.pool)
            .await?;

        row.as_ref().map(payment_from_row).transpose()
    }

    pub async fn list_by_booking_reference(
        &self,
        booking_reference: &str,
    ) -> Result<Vec<Payment>, sqlx::Error> {
        let rows = sqlx::query(&payment_query(
            "p.booking_reference = ? ORDER BY p.created_at DESC",
        ))
        .bind(booking_reference)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(payment_from_row).collect()
    }

    /// Payments whose outcome is now known but has not been applied yet.
    pub async fn take_decidable(&self) -> Result<Vec<DecidablePayment>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT transaction_id, planned_outcome, planned_failure_reason, deliver_webhook,
                    deliver_after_ms, planned_deliveries
             FROM payments
             WHERE status = 'PENDING' AND decide_at <= ?
             ORDER BY decide_at
             LIMIT 50",
        )
        .bind(iso(Utc::now()))
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                Ok(DecidablePayment {
                    transaction_id: row.try_get("transaction_id")?,
                    outcome: if row.try_get::<String, _>("planned_outcome")? == "SUCCESS" {
                        Outcome::Success
                    } else {
                        Outcome::Failed
                    },
                    failure_reason: row.try_get("planned_failure_reason")?,
                    deliver_webhook: row.try_get("deliver_webhook")?,
                    deliver_after_ms: row.try_get("deliver_after_ms")?,
                    deliveries: row.try_get("planned_deliveries")?,
                })
            })
            .collect()
    }

    /// Applies a decided outcome and, if the plan calls for it, queues the webhook.
    ///
    /// The status update is conditional on the row still being `PENDING`, so two
    /// overlapping ticks cannot both queue a delivery for the same payment.
    pub async fn apply_decision(
        &self,
        payment: &DecidablePayment,
        event_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let now = Utc::now();
        // Non-zero only for the `late_success` scenario, which delivers long after the
        // outcome is known.
        let first_attempt_at = now + chrono::Duration::milliseconds(payment.deliver_after_ms);
        let mut tx = self.pool.begin().await?;

        let updated = sqlx::query(
            "UPDATE payments SET status = ?, failure_reason = ?, decided_at = ?
             WHERE transaction_id = ? AND status = 'PENDING'",
        )
        .bind(payment.outcome.as_str())
        .bind(payment.failure_reason.as_deref())
        .bind(iso(now))
        .bind(&payment.transaction_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if updated == 0 {
            tx.rollback().await?;
            return Ok(false);
        }

        if payment.deliver_webhook {
            // One row per intended delivery, every one due right now. A planned duplicate
            // therefore goes out *simultaneously* rather than politely spaced out.
            for _ in 0..payment.deliveries.max(1) {
                sqlx::query(
                    "INSERT INTO webhook_deliveries (
                         delivery_id, event_id, transaction_id, next_attempt_at, created_at
                     ) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(format!("dlv_{}", uuid::Uuid::new_v4().simple()))
                .bind(event_id)
                .bind(&payment.transaction_id)
                .bind(iso(first_attempt_at))
                .bind(iso(now))
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(true)
    }

    /// Deliveries whose next attempt is due.
    pub async fn take_due_deliveries(&self) -> Result<Vec<DueDelivery>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT d.delivery_id, d.event_id, d.transaction_id, d.attempts,
                    p.booking_reference, p.amount_minor, p.currency, p.status,
                    p.failure_reason, p.callback_url, p.decided_at
             FROM webhook_deliveries d
             JOIN payments p ON p.transaction_id = d.transaction_id
             WHERE d.completed_at IS NULL AND d.next_attempt_at <= ?
             ORDER BY d.next_attempt_at
             LIMIT 20",
        )
        .bind(iso(Utc::now()))
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                Ok(DueDelivery {
                    delivery_id: row.try_get("delivery_id")?,
                    event_id: row.try_get("event_id")?,
                    transaction_id: row.try_get("transaction_id")?,
                    booking_reference: row.try_get("booking_reference")?,
                    amount_minor: row.try_get("amount_minor")?,
                    currency: row.try_get("currency")?,
                    status: row.try_get("status")?,
                    failure_reason: row.try_get("failure_reason")?,
                    callback_url: row.try_get("callback_url")?,
                    decided_at: row.try_get("decided_at")?,
                    attempts: row.try_get("attempts")?,
                })
            })
            .collect()
    }

    /// Records a rejected attempt for one delivery row and schedules its next try.
    pub async fn record_attempt_failed(
        &self,
        delivery_id: &str,
        status_code: Option<u16>,
        error: &str,
        retry_after: Duration,
    ) -> Result<(), sqlx::Error> {
        let next = Utc::now() + chrono::Duration::from_std(retry_after).unwrap_or_default();

        sqlx::query(
            "UPDATE webhook_deliveries
             SET attempts = attempts + 1, last_status_code = ?, last_error = ?,
                 next_attempt_at = ?
             WHERE delivery_id = ?",
        )
        .bind(status_code.map(i64::from))
        .bind(error)
        .bind(iso(next))
        .bind(delivery_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Records a 2xx and completes this delivery row.
    ///
    /// Rows are independent: a duplicate is a second row with the same `event_id`, and it
    /// completes on its own acknowledgement.
    pub async fn record_attempt_acknowledged(
        &self,
        delivery_id: &str,
        status_code: u16,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            "UPDATE webhook_deliveries
             SET attempts = attempts + 1,
                 acknowledged_count = acknowledged_count + 1,
                 last_status_code = ?,
                 last_error = NULL,
                 completed_at = ?
             WHERE delivery_id = ?",
        )
        .bind(i64::from(status_code))
        .bind(iso(now))
        .bind(delivery_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Refunds a successful payment. Idempotent: calling it twice returns the same id.
    pub async fn refund(&self, transaction_id: &str) -> Result<RefundResult, sqlx::Error> {
        let Some(payment) = self.get_payment(transaction_id).await? else {
            return Ok(RefundResult::NotFound);
        };

        if let Some(existing) = payment.refund_id {
            return Ok(RefundResult::AlreadyRefunded(existing));
        }

        if payment.status != "SUCCESS" {
            return Ok(RefundResult::NotRefundable(payment.status));
        }

        let refund_id = format!("rfnd_{}", uuid::Uuid::new_v4().simple());

        // `status` deliberately stays SUCCESS. A refund is a separate fact about a payment
        // that did succeed, not a retroactive change to its outcome — and a reconciliation
        // poll that suddenly saw REFUNDED where it expected SUCCESS would be needlessly
        // confusing to handle.
        sqlx::query(
            "UPDATE payments SET refund_id = ?, refunded_at = ?
             WHERE transaction_id = ? AND refund_id IS NULL",
        )
        .bind(&refund_id)
        .bind(iso(Utc::now()))
        .bind(transaction_id)
        .execute(&self.pool)
        .await?;

        Ok(RefundResult::Refunded(refund_id))
    }
}

#[derive(Debug, Clone)]
pub struct DecidablePayment {
    pub transaction_id: String,
    pub outcome: Outcome,
    /// Chosen when the payment was accepted, so it does not change across restarts.
    pub failure_reason: Option<String>,
    pub deliver_webhook: bool,
    /// Extra delay between deciding and the first delivery attempt.
    pub deliver_after_ms: i64,
    pub deliveries: i64,
}

#[derive(Debug)]
pub enum RefundResult {
    Refunded(String),
    AlreadyRefunded(String),
    NotRefundable(String),
    NotFound,
}

const PAYMENT_COLUMNS: &str = "
    p.transaction_id, p.booking_reference, p.amount_minor, p.currency, p.status,
    p.failure_reason, p.scenario, p.created_at, p.decided_at, p.refund_id, p.refunded_at,
    p.deliver_webhook,
    (SELECT COALESCE(SUM(d.attempts), 0)
       FROM webhook_deliveries d
      WHERE d.transaction_id = p.transaction_id)           AS webhook_attempts,
    (SELECT COALESCE(SUM(d.acknowledged_count), 0)
       FROM webhook_deliveries d
      WHERE d.transaction_id = p.transaction_id)           AS webhook_acknowledged_count
";

/// Correlated subqueries rather than a join, deliberately.
///
/// A payment can now own *several* delivery rows (a planned duplicate is two rows sharing an
/// `event_id`), so a `LEFT JOIN` would emit one output row per delivery and `fetch_optional`
/// would silently return whichever came first. Aggregating in subqueries keeps it one row per
/// payment, and still reports payments whose webhook was suppressed entirely and therefore
/// have no delivery rows at all.
fn payment_query(filter: &str) -> String {
    format!(
        "SELECT {PAYMENT_COLUMNS}
         FROM payments p
         WHERE {filter}"
    )
}

fn payment_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Payment, sqlx::Error> {
    Ok(Payment {
        transaction_id: row.try_get("transaction_id")?,
        booking_reference: row.try_get("booking_reference")?,
        amount_minor: row.try_get("amount_minor")?,
        currency: row.try_get("currency")?,
        status: row.try_get("status")?,
        failure_reason: row.try_get("failure_reason")?,
        scenario: row.try_get("scenario")?,
        created_at: row.try_get("created_at")?,
        decided_at: row.try_get("decided_at")?,
        refund_id: row.try_get("refund_id")?,
        refunded_at: row.try_get("refunded_at")?,
        webhook_attempts: row.try_get("webhook_attempts")?,
        webhook_acknowledged: row.try_get::<i64, _>("webhook_acknowledged_count")? > 0,
        webhook_suppressed: !row.try_get::<bool, _>("deliver_webhook")?,
    })
}

fn iso(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}
