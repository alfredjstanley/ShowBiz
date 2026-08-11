//! Deciding what will happen to a payment.
//!
//! The whole plan — outcome, timing, whether the webhook is delivered at all, how many
//! times — is resolved **once**, when the payment is accepted, and then persisted. Two
//! consequences worth knowing:
//!
//!   * Restarting the gateway never re-rolls the dice. A payment that was going to fail
//!     still fails; a webhook that was going to be delivered twice still is.
//!   * With `RNG_SEED` set, an entire session replays identically.

use std::fmt;
use std::str::FromStr;

use rand::Rng;

use crate::config::Config;

/// The outcome the gateway will report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "SUCCESS",
            Outcome::Failed => "FAILED",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Value of the `X-Force-Scenario` request header.
///
/// Use these when you want a specific failure mode instead of waiting for one to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Roll the dice using the configured rates. The default.
    Random,
    /// Succeeds, webhook delivered once, promptly.
    Success,
    /// Fails, webhook delivered once, promptly. Seats must be released.
    Failure,
    /// Succeeds, but the webhook is **never delivered**. The money is taken and your
    /// service is never told. Only polling `GET /v1/payments/{id}` finds this.
    Lost,
    /// Succeeds, and the same `event_id` is delivered twice — **concurrently**.
    Duplicate,
    /// Succeeds, but the webhook arrives long after the outcome was decided.
    LateSuccess,
    /// Succeeds and is decided immediately, with no thinking time at all. Makes the webhook
    /// race the response to the call that created the payment.
    Instant,
}

impl Scenario {
    /// The canonical spelling, matching what `X-Force-Scenario` accepts. Derived from
    /// `Debug` this would come out as `latesuccess`, so it is spelled out.
    pub fn as_str(self) -> &'static str {
        match self {
            Scenario::Random => "random",
            Scenario::Success => "success",
            Scenario::Failure => "failure",
            Scenario::Lost => "lost",
            Scenario::Duplicate => "duplicate",
            Scenario::LateSuccess => "late_success",
            Scenario::Instant => "instant",
        }
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scenario {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "random" => Ok(Scenario::Random),
            "success" => Ok(Scenario::Success),
            "failure" | "failed" | "fail" => Ok(Scenario::Failure),
            "lost" => Ok(Scenario::Lost),
            "duplicate" => Ok(Scenario::Duplicate),
            "late_success" | "late" => Ok(Scenario::LateSuccess),
            "instant" => Ok(Scenario::Instant),
            other => Err(format!(
                "unknown scenario {other:?}; expected one of: random, success, failure, lost, \
                 duplicate, late_success, instant"
            )),
        }
    }
}

/// Everything the gateway has decided about a payment, up front.
#[derive(Debug, Clone)]
pub struct Plan {
    pub outcome: Outcome,
    pub failure_reason: Option<&'static str>,
    /// Milliseconds to wait before the outcome becomes known.
    pub decide_after_ms: u64,
    /// Whether a webhook is ever sent.
    pub deliver_webhook: bool,
    /// Extra delay between deciding the outcome and the first delivery attempt.
    pub deliver_after_ms: u64,
    /// How many successful acknowledgements the gateway insists on. `2` means your
    /// endpoint sees the same `event_id` twice.
    pub deliveries: i64,
}

const FAILURE_REASONS: [&str; 4] = [
    "insufficient_funds",
    "card_declined",
    "issuer_unavailable",
    "risk_check_failed",
];

impl Plan {
    pub fn resolve<R: Rng>(scenario: Scenario, config: &Config, rng: &mut R) -> Self {
        let decide_after_ms = if config.outcome_delay_ms_max == config.outcome_delay_ms_min {
            config.outcome_delay_ms_min
        } else {
            rng.random_range(config.outcome_delay_ms_min..=config.outcome_delay_ms_max)
        };

        // The happy path, which most forced scenarios are a small variation on.
        let succeeds = Self {
            outcome: Outcome::Success,
            failure_reason: None,
            decide_after_ms,
            deliver_webhook: true,
            deliver_after_ms: 0,
            deliveries: 1,
        };

        match scenario {
            Scenario::Success => succeeds,

            Scenario::Failure => Self {
                outcome: Outcome::Failed,
                failure_reason: Some(FAILURE_REASONS[0]),
                ..succeeds
            },

            Scenario::Lost => Self {
                deliver_webhook: false,
                ..succeeds
            },

            Scenario::Duplicate => Self {
                deliveries: 2,
                ..succeeds
            },

            Scenario::LateSuccess => Self {
                deliver_after_ms: config.late_webhook_delay_ms,
                ..succeeds
            },

            Scenario::Instant => Self {
                decide_after_ms: 0,
                ..succeeds
            },

            Scenario::Random => {
                let outcome = if rng.random_bool(config.success_rate) {
                    Outcome::Success
                } else {
                    Outcome::Failed
                };

                let failure_reason = match outcome {
                    Outcome::Failed => {
                        Some(FAILURE_REASONS[rng.random_range(0..FAILURE_REASONS.len())])
                    }
                    Outcome::Success => None,
                };

                let lost = rng.random_bool(config.lost_webhook_rate);
                let late = !lost && rng.random_bool(config.late_webhook_rate);
                let duplicate = !lost && rng.random_bool(config.duplicate_rate);

                Self {
                    outcome,
                    failure_reason,
                    decide_after_ms,
                    deliver_webhook: !lost,
                    deliver_after_ms: if late {
                        config.late_webhook_delay_ms
                    } else {
                        0
                    },
                    deliveries: if duplicate { 2 } else { 1 },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn plan(scenario: Scenario) -> Plan {
        let config = Config::for_self_test("sqlite://:memory:".to_owned());
        let mut rng = StdRng::seed_from_u64(7);
        Plan::resolve(scenario, &config, &mut rng)
    }

    #[test]
    fn forced_scenarios_are_deterministic() {
        assert_eq!(plan(Scenario::Success).outcome, Outcome::Success);
        assert_eq!(plan(Scenario::Failure).outcome, Outcome::Failed);
        assert!(plan(Scenario::Failure).failure_reason.is_some());
        assert!(!plan(Scenario::Lost).deliver_webhook);
        assert_eq!(plan(Scenario::Duplicate).deliveries, 2);
        assert!(plan(Scenario::LateSuccess).deliver_after_ms > 0);
        assert_eq!(plan(Scenario::Instant).decide_after_ms, 0);
    }

    #[test]
    fn lost_and_duplicate_are_mutually_exclusive() {
        // A webhook that is never delivered cannot also be delivered twice.
        let mut config = Config::for_self_test("sqlite://:memory:".to_owned());
        config.lost_webhook_rate = 1.0;
        config.duplicate_rate = 1.0;
        let mut rng = StdRng::seed_from_u64(1);

        let plan = Plan::resolve(Scenario::Random, &config, &mut rng);
        assert!(!plan.deliver_webhook);
        assert_eq!(plan.deliveries, 1);
    }

    #[test]
    fn scenario_parsing_accepts_aliases_and_rejects_junk() {
        assert_eq!("FAILURE".parse::<Scenario>().unwrap(), Scenario::Failure);
        assert_eq!("fail".parse::<Scenario>().unwrap(), Scenario::Failure);
        assert!("nonsense".parse::<Scenario>().is_err());
    }
}
