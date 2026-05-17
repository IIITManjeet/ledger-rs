#![forbid(unsafe_code)]

pub mod error;
pub mod routes;
pub mod state;

pub use error::AppError;
pub use state::AppState;

use anyhow::Context;
use axum::routing::{get, post};
use axum::Router;
use ledger_db::LedgerDb;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

/// Build the router with all routes wired up. Exposed for integration tests.
pub fn router(state: AppState) -> Router {
    Router::new()
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
        // Per-request logging: one INFO span per request with method, uri,
        // status, and latency. Without this, the only tracing output is the
        // boot line — nothing per request.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

/// Boot the service: init tracing, connect to Postgres, apply migrations,
/// build the router, bind a listener, serve until SIGTERM/SIGINT.
pub async fn run() -> anyhow::Result<()> {
    init_tracing();

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let bind = std::env::var("LEDGER_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let db = LedgerDb::connect(&database_url)
        .await
        .context("failed to connect to postgres")?;
    db.migrate().await.context("failed to apply migrations")?;

    let app = router(AppState::new(db));
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;

    tracing::info!(%bind, "ledger-api listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    tracing::info!("graceful shutdown complete");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("ledger_api=info,ledger_db=info,tower_http=info,sqlx=warn")
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .init();
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
