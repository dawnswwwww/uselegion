//! Lightweight observability layer: Prometheus exposition for the Gateway.
//!
//! The core metrics registry lives in `legion-host` (transport-neutral); this
//! module only keeps the HTTP handler and Prometheus text formatter.

mod prometheus;

pub use prometheus::format_prometheus;

use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Serve metrics in Prometheus text format from the gateway state.
pub async fn metrics_handler(
    axum::extract::Extension(state): axum::extract::Extension<Arc<crate::websocket::GatewayState>>,
) -> Response {
    let snapshot = state.metrics_registry.snapshot();
    let body = format_prometheus(&snapshot);
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

pub use legion_host::{Metric, MetricValue, MetricsRegistry};
