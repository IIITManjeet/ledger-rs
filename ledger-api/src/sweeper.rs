//! Background task that periodically deletes expired idempotency rows.
//!
//! Runs every `interval` (5 minutes in prod, tuned shorter in tests).
//! Listens on a `tokio::sync::watch` channel for the shutdown signal;
//! when it fires, the sweeper finishes any in-flight tick and exits
//! cleanly.

use ledger_db::LedgerDb;
use std::time::Duration;
use tokio::sync::watch;

/// Spawn the sweeper. Returns immediately; the actual loop runs in a
/// detached tokio task. The caller is expected to flip the `shutdown`
/// channel and `.await` the returned `JoinHandle` if it wants to wait.
pub fn spawn(
    db: LedgerDb,
    interval: Duration,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(sweeper_loop(db, interval, shutdown))
}

async fn sweeper_loop(db: LedgerDb, interval: Duration, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(interval);
    // Skip the first immediate tick; we don't want to sweep at startup
    // before the rest of the app is ready.
    ticker.tick().await;
    tracing::info!(?interval, "idempotency sweeper started");

    loop {
        tokio::select! {
            biased; // Check shutdown first each iteration.
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("idempotency sweeper shutting down");
                    return;
                }
            }
            _ = ticker.tick() => {
                match db.idempotency_sweep_expired().await {
                    Ok(n) if n > 0 => tracing::info!(rows = n, "swept expired idempotency rows"),
                    Ok(_) => tracing::debug!("sweeper tick: nothing to remove"),
                    Err(e) => tracing::warn!(error = %e, "sweeper tick failed; will retry next interval"),
                }
            }
        }
    }
}
