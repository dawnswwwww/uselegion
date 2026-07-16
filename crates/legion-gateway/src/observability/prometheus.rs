//! Prometheus text exposition format.

use super::{Metric, MetricValue};

/// Format a snapshot of metrics in Prometheus text format.
pub fn format_prometheus(metrics: &[Metric]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut seen_help: Vec<String> = Vec::new();

    for metric in metrics {
        if !seen_help.contains(&metric.name) {
            lines.push(format!(
                "# HELP {} {}",
                sanitize_name(&metric.name),
                metric.help
            ));
            let type_name = match metric.value {
                MetricValue::Counter(_) => "counter",
                MetricValue::Gauge(_) => "gauge",
            };
            lines.push(format!(
                "# TYPE {} {}",
                sanitize_name(&metric.name),
                type_name
            ));
            seen_help.push(metric.name.clone());
        }
        lines.push(format_metric(metric));
    }

    lines.join("\n") + "\n"
}

fn sanitize_name(name: &str) -> String {
    name.replace(|c: char| !c.is_alphanumeric() && c != '_', "_")
}

fn format_metric(metric: &Metric) -> String {
    let name = sanitize_name(&metric.name);
    let value = metric.value.as_f64();
    if metric.labels.is_empty() {
        format!("{} {}", name, value)
    } else {
        let labels: Vec<String> = metric
            .labels
            .iter()
            .map(|(k, v)| format!("{}=\"{}\"", sanitize_name(k), escape_label(v)))
            .collect();
        format!("{} {{{}}} {}", name, labels.join(","), value)
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counter_without_labels() {
        let metrics = vec![Metric {
            name: "requests_total".to_string(),
            help: "total requests".to_string(),
            value: MetricValue::Counter(42),
            labels: Vec::new(),
        }];
        let out = format_prometheus(&metrics);
        assert!(out.contains("# HELP requests_total total requests"));
        assert!(out.contains("# TYPE requests_total counter"));
        assert!(out.contains("requests_total 42"));
    }

    #[test]
    fn escapes_label_values() {
        let metrics = vec![Metric {
            name: "errors_total".to_string(),
            help: "errors".to_string(),
            value: MetricValue::Counter(1),
            labels: vec![("reason".to_string(), "bad \"value\"".to_string())],
        }];
        let out = format_prometheus(&metrics);
        assert!(out.contains("reason=\"bad \\\"value\\\"\""));
    }

    #[test]
    fn formats_gauge() {
        let metrics = vec![Metric {
            name: "active_connections".to_string(),
            help: "active connections".to_string(),
            value: MetricValue::Gauge(3.5),
            labels: Vec::new(),
        }];
        let out = format_prometheus(&metrics);
        assert!(out.contains("# TYPE active_connections gauge"));
        assert!(out.contains("active_connections 3.5"));
    }
}
