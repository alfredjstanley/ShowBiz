//! `payment-gateway self-test` — checks the gateway itself, not your service.
//!
//! Stands up a real gateway and a throwaway receiver in one process, then asserts the
//! three behaviours the exercise depends on:
//!
//!   1. A webhook rejected with 500 is retried until it is accepted.
//!   2. The `duplicate` scenario delivers the same `event_id` twice.
//!   3. The `lost` scenario never delivers, yet the payment still reaches a terminal
//!      status that polling can discover.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::Router;
use serde_json::Value;

use crate::config::Config;
use crate::delivery::Worker;
use crate::server::{self, GatewayState};
use crate::store::Store;

/// Records what the receiver saw, so the assertions have something to check.
#[derive(Default)]
struct Received {
    events: Mutex<Vec<Value>>,
    /// Number of 500s still to return before the receiver starts accepting.
    reject_budget: AtomicUsize,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = std::env::temp_dir().join(format!(
        "gateway-selftest-{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let _ = std::fs::remove_file(&db_path);

    let config = Config::for_self_test(format!("sqlite://{}?mode=rwc", db_path.display()));

    let store = Store::open(&config.database_url).await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    // The receiver: rejects the first three attempts, then accepts everything.
    let received = Arc::new(Received::default());
    received.reject_budget.store(3, Ordering::SeqCst);

    let receiver_addr = spawn_receiver(received.clone()).await?;
    let callback_url = format!("http://{receiver_addr}/webhooks/payments");
    println!("receiver listening on {receiver_addr}");

    // The gateway under test.
    let gateway_state = GatewayState::new(store.clone(), config.clone());
    let gateway_addr = spawn_gateway(gateway_state).await?;
    println!("gateway listening on {gateway_addr}");

    let worker = Arc::new(Worker::new(store.clone(), config.clone(), http.clone()));
    tokio::spawn(worker.run());

    let base = format!("http://{gateway_addr}");
    let mut failures = Vec::new();

    // -- 1. retry until acknowledged -----------------------------------------------------
    let txn = create_payment(&http, &base, "success", "bkg_retry", &callback_url).await?;
    println!("\n[1/3] retry-until-acknowledged  {txn}");

    match wait_for(Duration::from_secs(15), || {
        let events = received.events.lock().unwrap();
        events.iter().any(|e| e["transaction_id"] == txn.as_str())
    })
    .await
    {
        false => failures.push("webhook was never acknowledged after retries".to_owned()),
        true => {
            let payment = get_payment(&http, &base, &txn).await?;
            let attempts = payment["webhook_attempts"].as_i64().unwrap_or(0);

            // Three rejections plus the accepted one.
            if attempts < 4 {
                failures.push(format!("expected at least 4 attempts, saw {attempts}"));
            } else {
                println!("      ok — acknowledged after {attempts} attempts");
            }

            if payment["webhook_acknowledged"] != Value::Bool(true) {
                failures.push("payment does not report the webhook as acknowledged".to_owned());
            }
            if payment["status"] != "SUCCESS" {
                failures.push(format!("expected SUCCESS, got {}", payment["status"]));
            }
            if let Some(sig) = signature_of(&received, &txn) {
                let expected_prefix = "sha256=";
                if !sig.starts_with(expected_prefix) || sig.len() != expected_prefix.len() + 64 {
                    failures.push(format!("malformed X-Payment-Signature: {sig}"));
                } else {
                    println!("      ok — signed with {}", &sig[..16]);
                }
            } else {
                failures.push("webhook arrived without a signature".to_owned());
            }
        }
    }

    // -- 2. duplicate delivery -----------------------------------------------------------
    let txn = create_payment(&http, &base, "duplicate", "bkg_dupe", &callback_url).await?;
    println!("\n[2/3] duplicate delivery        {txn}");

    let delivered_twice = wait_for(Duration::from_secs(15), || {
        let events = received.events.lock().unwrap();
        let mine: Vec<_> = events
            .iter()
            .filter(|e| e["transaction_id"] == txn.as_str())
            .collect();

        mine.len() >= 2 && mine[0]["event_id"] == mine[1]["event_id"]
    })
    .await;

    if delivered_twice {
        let events = received.events.lock().unwrap();
        let mine: Vec<_> = events
            .iter()
            .filter(|e| e["transaction_id"] == txn.as_str())
            .collect();

        let ms = |e: &Value, k: &str| e[k].as_u64().unwrap_or(0);
        let latest_entry = ms(mine[0], "__entered_ms").max(ms(mine[1], "__entered_ms"));
        let earliest_exit = ms(mine[0], "__exited_ms").min(ms(mine[1], "__exited_ms"));

        println!(
            "      ok — event {} delivered {} times",
            mine[0]["event_id"].as_str().unwrap_or("?"),
            mine.len()
        );

        // The whole point of the delivery model: the two must be in flight together, not
        // politely spaced out. Sequential delivery is what lets a naive dedup pass.
        if latest_entry < earliest_exit {
            println!(
                "      ok — the two deliveries overlapped in time by {}ms",
                earliest_exit - latest_entry
            );
        } else {
            failures.push(format!(
                "duplicate deliveries did not overlap — {}ms apart, so a naive \
                 read-then-write dedup would still pass",
                latest_entry.saturating_sub(earliest_exit)
            ));
        }
    } else {
        failures.push("duplicate scenario did not redeliver the same event_id".to_owned());
    }

    // -- 3. lost webhook -----------------------------------------------------------------
    let txn = create_payment(&http, &base, "lost", "bkg_lost", &callback_url).await?;
    println!("\n[3/3] suppressed webhook        {txn}");

    // Give it well past the point at which a delivered webhook would have arrived.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let payment = get_payment(&http, &base, &txn).await?;
    let arrived = received
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|e| e["transaction_id"] == txn.as_str());

    if arrived {
        failures.push("lost scenario delivered a webhook it should have suppressed".to_owned());
    } else if payment["status"] != "SUCCESS" {
        failures.push(format!(
            "lost payment should still reach a terminal status, got {}",
            payment["status"]
        ));
    } else {
        println!("      ok — never delivered, but polling reports SUCCESS");
    }

    let _ = std::fs::remove_file(&db_path);

    println!();
    if failures.is_empty() {
        println!("self-test passed");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("FAIL: {failure}");
        }
        Err(format!("{} check(s) failed", failures.len()).into())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn signature_of(received: &Received, transaction_id: &str) -> Option<String> {
    received
        .events
        .lock()
        .unwrap()
        .iter()
        .find(|e| e["transaction_id"] == transaction_id)
        .and_then(|e| e["__signature"].as_str().map(str::to_owned))
}

async fn create_payment(
    http: &reqwest::Client,
    base: &str,
    scenario: &str,
    booking_reference: &str,
    callback_url: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let response = http
        .post(format!("{base}/v1/payments"))
        .header("x-force-scenario", scenario)
        .json(&serde_json::json!({
            "booking_reference": booking_reference,
            "amount_minor": 20_000,
            "currency": "INR",
            "callback_url": callback_url,
        }))
        .send()
        .await?
        .error_for_status()?;

    let body: Value = response.json().await?;
    Ok(body["transaction_id"]
        .as_str()
        .ok_or("response had no transaction_id")?
        .to_owned())
}

async fn get_payment(
    http: &reqwest::Client,
    base: &str,
    transaction_id: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(http
        .get(format!("{base}/v1/payments/{transaction_id}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Polls `predicate` until it is true or the deadline passes.
async fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;

    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    predicate()
}

async fn spawn_gateway(
    state: GatewayState,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        let _ = axum::serve(listener, server::router(state)).await;
    });

    Ok(addr)
}

async fn spawn_receiver(
    received: Arc<Received>,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
    async fn handle(
        State(received): State<Arc<Received>>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> StatusCode {
        // Burn through the rejection budget first, so we can watch the gateway retry.
        // fetch_update, not load-then-store: deliveries now arrive concurrently, and a
        // read-modify-write on the budget would itself be racy.
        let claimed =
            received
                .reject_budget
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if let Ok(remaining) = claimed {
            println!("      receiver: rejecting with 500 ({remaining} rejections left)");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }

        let entered_ms = now_ms();
        // Hold the request open briefly. Two deliveries that genuinely overlap will have
        // intersecting [entered, exited] windows; strictly sequential ones cannot.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let exited_ms = now_ms();

        if let Ok(mut event) = serde_json::from_str::<Value>(&body) {
            // Stash the signature and timing alongside the payload so assertions can reach it.
            if let Some(object) = event.as_object_mut() {
                let signature = headers
                    .get("x-payment-signature")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                object.insert("__signature".to_owned(), Value::String(signature));
                object.insert("__entered_ms".to_owned(), Value::from(entered_ms));
                object.insert("__exited_ms".to_owned(), Value::from(exited_ms));
            }
            received.events.lock().unwrap().push(event);
        }

        StatusCode::OK
    }

    let app = Router::new()
        .route("/webhooks/payments", post(handle))
        .with_state(received);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Ok(addr)
}
