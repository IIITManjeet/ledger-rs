#![forbid(unsafe_code)]

pub mod error;
pub mod observability;
pub mod routes;
pub mod state;
pub mod sweeper;

pub use error::AppError;
pub use state::AppState;

use anyhow::Context;
use axum::routing::{get, post};
use axum::Router;
use ledger_db::LedgerDb;
use std::time::Duration;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

/// Build the router with all routes wired up. Exposed for integration tests.
pub fn router(state: AppState) -> Router {
    let metrics_handle = observability::install_metrics_recorder();
    let metrics_state = observability::MetricsState {
        handle: metrics_handle,
    };

    // Main app routes — share AppState. Wrap them in the metrics
    // middleware so every request is timed and labelled with its
    // matched path template.
    let app_routes = Router::new()
        .route("/healthz", get(routes::health::healthz))
        .route("/readyz", get(routes::health::readyz))
        .route("/accounts", post(routes::accounts::create))
        .route("/accounts/:id", get(routes::accounts::get))
        .route(
            "/accounts/:id/postings",
            get(routes::accounts::list_postings),
        )
        .route("/transactions", post(routes::transactions::create))
        .route("/transactions/:id", get(routes::transactions::get))
        .with_state(state)
        .layer(axum::middleware::from_fn(observability::record_metrics));

    // /metrics has its own state (PrometheusHandle), so it lives on a
    // separate sub-router merged in.
    let metrics_router = Router::new()
        .route("/metrics", get(observability::metrics_handler))
        .with_state(metrics_state);

    app_routes
        .merge(metrics_router)
        // Per-request structured logs (INFO span per request).
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

/// Boot the service end-to-end:
///   - init tracing,
///   - connect to Postgres + apply migrations,
///   - install the Prometheus recorder,
///   - start the idempotency sweeper as a background task,
///   - bind a listener,
///   - serve with graceful shutdown until SIGTERM/SIGINT.
pub async fn run() -> anyhow::Result<()> {
    init_tracing();

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let bind = std::env::var("LEDGER_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let db = LedgerDb::connect(&database_url)
        .await
        .context("failed to connect to postgres")?;
    db.migrate().await.context("failed to apply migrations")?;

    // Shutdown signal: when this flips to true, both Axum and the sweeper exit.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Sweeper runs every 5 minutes by default.
    let sweeper_handle = sweeper::spawn(db.clone(), Duration::from_secs(300), shutdown_rx.clone());

    let app = router(AppState::new(db));
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;

    tracing::info!(%bind, "ledger-api listening");

    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal().await;
        // Tell the sweeper to stop too.
        let _ = shutdown_tx.send(true);
    });

    serve.await.context("server error")?;

    // Wait for the sweeper to finish its current cycle.
    let _ = sweeper_handle.await;
    tracing::info!("graceful shutdown complete");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("ledger_api=info,ledger_db=info,tower_http=info,sqlx=warn")
    });

    // LEDGER_LOG_FORMAT=json → newline-delimited JSON logs (one record per line).
    // Anything else (incl. unset) → pretty for local dev.
    let json = std::env::var("LEDGER_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry
            .with(fmt::layer().json().flatten_event(true).with_target(false))
            .init();
    } else {
        registry.with(fmt::layer().with_target(false)).init();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received; draining");
}
