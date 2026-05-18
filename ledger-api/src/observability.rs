//! Prometheus metrics setup + the middleware that records per-request
//! latency.
//!
//! Three histograms / counters of note:
//!   - `http_request_duration_seconds{method,path,status}` — per-route latency.
//!   - `idempotency_outcome_total{kind}` — counts of fresh/in_flight/conflict/replay.
//!   - `ledger_serialization_retry_total` — count of 40001 retries.

use axum::{
    body::Body,
    extract::{MatchedPath, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::time::Instant;

/// Install the Prometheus recorder globally. Returns a handle that the
/// `/metrics` route uses to render the current snapshot.
///
/// Idempotent within one process: if the recorder is already installed
/// (e.g. multiple integration tests in the same process), we return a
/// stub handle and continue.
pub fn install_metrics_recorder() -> PrometheusHandle {
    // Define sensible histogram buckets for HTTP latency (seconds).
    // These cover the 1ms .. 10s range that matters for a payments API.
    let buckets: &[f64] = &[
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    PrometheusBuilder::new()
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Prefix(
                "http_request_duration_seconds".to_string(),
            ),
            buckets,
        )
        .expect("invalid bucket config")
        .install_recorder()
        .unwrap_or_else(|_| {
            // Already installed (probably by another test in this process).
            // Build a fresh handle that points at the existing recorder.
            // We can't actually fetch the live one, so return one that
            // just doesn't render — fine, since tests rarely scrape /metrics.
            PrometheusBuilder::new().build_recorder().handle()
        })
}

/// Axum middleware: time each request, record one histogram observation
/// labelled by method, matched-path template, and status. Using the
/// *matched path* (e.g. `/accounts/:id`) instead of the raw URI keeps
/// the label cardinality bounded — without this, every UUID would be a
/// distinct label set and Prometheus would melt.
pub async fn record_metrics(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_string());

    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method.to_string(),
        "path" => path,
        "status" => status,
    )
    .record(elapsed);

    response
}

/// State-extracted handler for `GET /metrics`. Wraps a `PrometheusHandle`.
#[derive(Clone)]
pub struct MetricsState {
    pub handle: PrometheusHandle,
}

pub async fn metrics_handler(State(state): State<MetricsState>) -> (StatusCode, String) {
    (StatusCode::OK, state.handle.render())
}

// Helper kept here so callers don't import body separately.
#[allow(dead_code)]
fn _phantom_body() -> Body {
    Body::empty()
}
