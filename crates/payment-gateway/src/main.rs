//! A deliberately unreliable mock payment provider.
//!
//! **This crate is part of the boilerplate. You should not need to change it.** Treat it
//! as a third-party service you are integrating with: talk to it over HTTP, read
//! `README.md`, and do not read its database.
//!
//! What it does, in one paragraph: you `POST /v1/payments`, it immediately returns a
//! transaction id with status `PENDING`, and some seconds later it decides `SUCCESS` or
//! `FAILED`. It then calls the `callback_url` you supplied, and keeps calling it —
//! forever, with exponential backoff — until it receives a 2xx. Roughly a fifth of
//! webhooks are delivered twice with the same `event_id`, and a few are never delivered at
//! all. All of it is configurable, and all of it can be forced deterministically with the
//! `X-Force-Scenario` header.

mod cli;
mod config;
mod delivery;
mod outcome;
mod selftest;
mod server;
mod signature;
mod store;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::delivery::Worker;
use crate::server::GatewayState;
use crate::store::Store;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("\nerror: {error}\n");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::SelfTest => {
            init_tracing("warn");
            selftest::run().await
        }
        Command::FireWebhook {
            url,
            status,
            transaction_id,
            event_id,
            booking_reference,
            amount_minor,
            duplicate,
        } => {
            init_tracing("info");
            fire_webhook(FireWebhookArgs {
                url,
                status,
                transaction_id,
                event_id,
                booking_reference,
                amount_minor,
                duplicate,
            })
            .await
        }
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing("payment_gateway=info");

    let config = Config::from_env()?;
    let store = Store::open(&config.database_url).await?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let worker = Arc::new(Worker::new(store.clone(), config.clone(), http));
    tokio::spawn(worker.run());

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
    let state = GatewayState::new(store, config.clone());
    let app = server::router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("payment gateway listening on http://{addr}");
    tracing::info!(
        success_rate = config.success_rate,
        duplicate_rate = config.duplicate_rate,
        lost_webhook_rate = config.lost_webhook_rate,
        late_webhook_rate = config.late_webhook_rate,
        seeded = config.rng_seed.is_some(),
        "chaos configuration"
    );
    tracing::info!(
        "outstanding webhooks resume automatically — this process can be restarted freely"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down; undelivered webhooks will resume on next start");
        })
        .await?;

    Ok(())
}

struct FireWebhookArgs {
    url: String,
    status: String,
    transaction_id: Option<String>,
    event_id: Option<String>,
    booking_reference: String,
    amount_minor: i64,
    duplicate: bool,
}

/// Sends one hand-crafted webhook, printing the response. No retries — this is a probe,
/// not the delivery loop.
async fn fire_webhook(args: FireWebhookArgs) -> Result<(), Box<dyn std::error::Error>> {
    let status = args.status.trim().to_ascii_uppercase();
    if status != "SUCCESS" && status != "FAILED" {
        return Err(format!("--status must be SUCCESS or FAILED, got {status:?}").into());
    }

    let config = Config::from_env()?;
    let transaction_id = args
        .transaction_id
        .unwrap_or_else(|| format!("txn_{}", uuid::Uuid::new_v4().simple()));
    let event_id = args
        .event_id
        .unwrap_or_else(|| format!("evt_{}", uuid::Uuid::new_v4().simple()));

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let copies = if args.duplicate { 2 } else { 1 };

    println!("--> POST {} x{copies}", args.url);
    println!("    event_id       {event_id}");
    println!("    transaction_id {transaction_id}");
    if args.duplicate {
        println!("    both sent concurrently, as the real gateway does");
    }

    // Fired concurrently, not in sequence — a receiver that dedups with a separate read then
    // write will double-apply, and that is exactly what this is for.
    let mut tasks = tokio::task::JoinSet::new();

    for copy in 1..=copies {
        let body = serde_json::json!({
            "event_id": event_id,
            "transaction_id": transaction_id,
            "booking_reference": args.booking_reference,
            "status": status,
            "amount_minor": args.amount_minor,
            "currency": "INR",
            "failure_reason": if status == "FAILED" { Some("card_declined") } else { None },
            "occurred_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            "attempt": 1,
        });

        let raw = serde_json::to_vec(&body)?;
        let sig = signature::sign(&config.webhook_secret, &raw);
        let http = http.clone();
        let url = args.url.clone();
        let event_id = event_id.clone();

        tasks.spawn(async move {
            let result = http
                .post(&url)
                .header("content-type", "application/json")
                .header("x-payment-event-id", &event_id)
                .header("x-payment-signature", sig)
                .header("idempotency-key", &event_id)
                .body(raw)
                .send()
                .await;

            match result {
                Ok(response) => {
                    let code = response.status();
                    let text = response.text().await.unwrap_or_default();
                    println!("<-- [{copy}] {code} {text}");

                    if !code.is_success() {
                        println!(
                            "    [{copy}] note: the real gateway treats this as a failed \
                             attempt and retries it."
                        );
                    }
                }
                Err(error) => {
                    println!("<-- [{copy}] delivery failed: {error}");
                    println!(
                        "    [{copy}] is your API running? the real gateway would keep \
                              retrying."
                    );
                }
            }
        });
    }

    while tasks.join_next().await.is_some() {}

    Ok(())
}

fn init_tracing(default: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
