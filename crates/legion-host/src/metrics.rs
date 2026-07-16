use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A recorded metric value.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    Counter(u64),
    Gauge(f64),
}

impl MetricValue {
    pub fn as_f64(&self) -> f64 {
        match self {
            MetricValue::Counter(v) => *v as f64,
            MetricValue::Gauge(v) => *v,
        }
    }
}

/// A metric with optional labels.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: String,
    pub help: String,
    pub value: MetricValue,
    pub labels: Vec<(String, String)>,
}

/// In-memory metrics registry.
#[derive(Default, Clone)]
pub struct MetricsRegistry {
    inner: Arc<Mutex<MetricsState>>,
}

#[derive(Default)]
struct MetricsState {
    counters: HashMap<String, HashMap<Vec<(String, String)>, u64>>,
    gauges: HashMap<String, f64>,
    metadata: HashMap<String, String>,
}

fn normalize_labels(labels: &[(String, String)]) -> Vec<(String, String)> {
    let mut sorted = labels.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    sorted
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment a counter (without labels) by one.
    pub fn increment_counter(&self, name: impl Into<String>, help: impl Into<String>) {
        self.add_counter_with_labels(name, help, &[], 1);
    }

    /// Add `value` to a counter (without labels).
    pub fn add_counter(&self, name: impl Into<String>, help: impl Into<String>, value: u64) {
        self.add_counter_with_labels(name, help, &[], value);
    }

    /// Increment a labeled counter series by one. Each distinct label set forms
    /// its own series under the same metric name.
    pub fn increment_counter_with_labels(
        &self,
        name: impl Into<String>,
        help: impl Into<String>,
        labels: &[(String, String)],
    ) {
        self.add_counter_with_labels(name, help, labels, 1);
    }

    /// Add `value` to a labeled counter series.
    pub fn add_counter_with_labels(
        &self,
        name: impl Into<String>,
        help: impl Into<String>,
        labels: &[(String, String)],
        value: u64,
    ) {
        let name = name.into();
        let key = normalize_labels(labels);
        let mut state = self.inner.lock().unwrap();
        let series = state.counters.entry(name.clone()).or_default();
        let entry = series.entry(key).or_insert(0);
        *entry += value;
        state.metadata.insert(name, help.into());
    }

    /// Set a gauge value.
    pub fn set_gauge(&self, name: impl Into<String>, help: impl Into<String>, value: f64) {
        let name = name.into();
        let mut state = self.inner.lock().unwrap();
        state.gauges.insert(name.clone(), value);
        state.metadata.insert(name, help.into());
    }

    /// Snapshot all metrics as a vector suitable for Prometheus exposition.
    pub fn snapshot(&self) -> Vec<Metric> {
        let state = self.inner.lock().unwrap();
        let mut metrics = Vec::new();
        for (name, series) in &state.counters {
            for (labels, value) in series {
                metrics.push(Metric {
                    name: name.clone(),
                    help: state.metadata.get(name).cloned().unwrap_or_default(),
                    value: MetricValue::Counter(*value),
                    labels: labels.clone(),
                });
            }
        }
        for (name, value) in &state.gauges {
            metrics.push(Metric {
                name: name.clone(),
                help: state.metadata.get(name).cloned().unwrap_or_default(),
                value: MetricValue::Gauge(*value),
                labels: Vec::new(),
            });
        }
        metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increments() {
        let reg = MetricsRegistry::new();
        reg.increment_counter("requests_total", "total requests");
        reg.increment_counter("requests_total", "total requests");
        let snapshot = reg.snapshot();
        let req = snapshot
            .iter()
            .find(|m| m.name == "requests_total")
            .unwrap();
        assert_eq!(req.value, MetricValue::Counter(2));
        assert!(req.labels.is_empty());
    }

    #[test]
    fn gauge_sets_and_overwrites() {
        let reg = MetricsRegistry::new();
        reg.set_gauge("active_connections", "active connections", 5.0);
        reg.set_gauge("active_connections", "active connections", 3.0);
        let snapshot = reg.snapshot();
        let conn = snapshot
            .iter()
            .find(|m| m.name == "active_connections")
            .unwrap();
        assert_eq!(conn.value, MetricValue::Gauge(3.0));
    }

    #[test]
    fn snapshot_contains_help() {
        let reg = MetricsRegistry::new();
        reg.increment_counter("errors_total", "total errors");
        let snapshot = reg.snapshot();
        let err = snapshot.iter().find(|m| m.name == "errors_total").unwrap();
        assert_eq!(err.help, "total errors");
    }

    #[test]
    fn counter_with_labels_splits_series() {
        let reg = MetricsRegistry::new();
        reg.increment_counter_with_labels(
            "mcp_calls_total",
            "total mcp calls",
            &[
                ("server".to_string(), "fs".to_string()),
                ("tool".to_string(), "read".to_string()),
            ],
        );
        reg.increment_counter_with_labels(
            "mcp_calls_total",
            "total mcp calls",
            &[
                ("server".to_string(), "fs".to_string()),
                ("tool".to_string(), "read".to_string()),
            ],
        );
        reg.increment_counter_with_labels(
            "mcp_calls_total",
            "total mcp calls",
            &[
                ("server".to_string(), "fs".to_string()),
                ("tool".to_string(), "write".to_string()),
            ],
        );
        let snapshot = reg.snapshot();
        let read = snapshot
            .iter()
            .find(|m| {
                m.name == "mcp_calls_total"
                    && m.labels.iter().any(|(k, v)| k == "tool" && v == "read")
            })
            .unwrap();
        assert_eq!(read.value, MetricValue::Counter(2));
        let write = snapshot
            .iter()
            .find(|m| {
                m.name == "mcp_calls_total"
                    && m.labels.iter().any(|(k, v)| k == "tool" && v == "write")
            })
            .unwrap();
        assert_eq!(write.value, MetricValue::Counter(1));
    }
}
