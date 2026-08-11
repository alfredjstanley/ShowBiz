//! Command-line interface.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "payment-gateway",
    about = "A deliberately unreliable mock payment provider.",
    long_about = "A deliberately unreliable mock payment provider.\n\n\
        Accepts a payment, returns a transaction id immediately, then decides SUCCESS or \
        FAILURE asynchronously and calls your webhook — retrying forever until it gets a \
        2xx. Some webhooks arrive twice, and some never arrive at all.\n\n\
        See README.md for the API and the environment knobs."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the gateway (the default if no subcommand is given).
    Serve,

    /// Send a single webhook by hand, without creating a payment first.
    ///
    /// Useful for building your handler before the booking flow exists, and for poking at
    /// idempotency: run it twice with the same --event-id.
    FireWebhook {
        /// Your webhook endpoint.
        #[arg(long, default_value = "http://127.0.0.1:8080/webhooks/payments")]
        url: String,

        /// SUCCESS or FAILED.
        #[arg(long, default_value = "SUCCESS")]
        status: String,

        /// Defaults to a fresh random id.
        #[arg(long)]
        transaction_id: Option<String>,

        /// Defaults to a fresh random id. Pass the same value twice to test idempotency.
        #[arg(long)]
        event_id: Option<String>,

        #[arg(long, default_value = "bkg_manual_test")]
        booking_reference: String,

        #[arg(long, default_value_t = 20_000)]
        amount_minor: i64,

        /// Send the identical request twice in a row.
        #[arg(long)]
        duplicate: bool,
    },

    /// Verify the gateway's own retry, duplicate and suppression behaviour.
    ///
    /// Runs entirely in-process against a temporary database and a throwaway receiver.
    /// Nothing to do with your service — this checks that the gateway itself works.
    SelfTest,
}
