//! `legion costs`: aggregate the per-agent provider cost snapshots that the
//! gateway's `CostTracker` persists to `~/.legion/agents/<agentId>/costs.json`.

use crate::CliError;
use legion_provider::ops::CostSnapshot;
use std::collections::HashMap;
use std::path::Path;

/// One aggregated row of the `legion costs` report.
#[derive(Debug, Clone, PartialEq)]
pub struct CostRow {
    pub model: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub estimated_calls: u64,
}

/// Aggregate all `<agents_dir>/*/costs.json` snapshots per model, sorted by
/// descending cost. Missing directories yield an empty report; unreadable
/// snapshot files are skipped with a warning.
pub fn aggregate_costs(agents_dir: &Path) -> Result<Vec<CostRow>, CliError> {
    let mut rows: HashMap<String, CostRow> = HashMap::new();
    let entries = match std::fs::read_dir(agents_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(CliError::Io(err)),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path().join("costs.json");
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        let snapshot: CostSnapshot = match serde_json::from_str(&text) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                eprintln!(
                    "warning: skipping invalid cost file {}: {err}",
                    path.display()
                );
                continue;
            }
        };
        for (model, stats) in snapshot.models {
            let row = rows.entry(model.clone()).or_insert_with(|| CostRow {
                model,
                calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
                estimated_calls: 0,
            });
            row.calls += stats.calls;
            row.input_tokens += stats.input_tokens;
            row.output_tokens += stats.output_tokens;
            row.cost_usd += stats.cost_usd;
            row.estimated_calls += stats.estimated_calls;
        }
    }
    let mut rows: Vec<CostRow> = rows.into_values().collect();
    rows.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then_with(|| a.model.cmp(&b.model))
    });
    Ok(rows)
}

/// Render the aggregated rows as an aligned table.
pub fn render_costs(rows: &[CostRow]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<28} {:>6} {:>12} {:>12} {:>12} {:>10}\n",
        "MODEL", "CALLS", "INPUT TOKENS", "OUTPUT TOKENS", "COST (USD)", "ESTIMATED"
    ));
    let mut total_cost = 0.0;
    let mut total_calls = 0u64;
    for row in rows {
        total_cost += row.cost_usd;
        total_calls += row.calls;
        let estimated = if row.estimated_calls > 0 {
            format!("{} est", row.estimated_calls)
        } else {
            "-".to_string()
        };
        out.push_str(&format!(
            "{:<28} {:>6} {:>12} {:>12} {:>12.6} {:>10}\n",
            row.model, row.calls, row.input_tokens, row.output_tokens, row.cost_usd, estimated
        ));
    }
    out.push_str(&format!(
        "{:<28} {:>6} {:>12} {:>12} {:>12.6} {:>10}\n",
        "TOTAL", total_calls, "", "", total_cost, ""
    ));
    out
}

/// Run the `legion costs` command against the default agents directory.
pub fn run() -> Result<(), CliError> {
    let agents_dir = dirs::home_dir()
        .map(|h| h.join(".legion").join("agents"))
        .ok_or_else(|| CliError::Other("unable to determine home directory".to_string()))?;
    let rows = aggregate_costs(&agents_dir)?;
    if rows.is_empty() {
        println!(
            "no cost data yet — the gateway records usage to \
             ~/.legion/agents/<agentId>/costs.json; set `models.costs` rates \
             in legion.json for dollar amounts"
        );
        return Ok(());
    }
    print!("{}", render_costs(&rows));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_costs(agents_dir: &Path, agent: &str, json: &str) {
        let dir = agents_dir.join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("costs.json"), json).unwrap();
    }

    #[test]
    fn aggregates_across_agents_and_models() {
        let tmp = tempfile::tempdir().unwrap();
        write_costs(
            tmp.path(),
            "main",
            r#"{
                "models": {
                    "openai/gpt-4o": { "calls": 2, "inputTokens": 1000, "outputTokens": 500, "costUsd": 0.02, "estimatedCalls": 2 },
                    "anthropic/claude": { "calls": 1, "inputTokens": 100, "outputTokens": 50, "costUsd": 0.001, "estimatedCalls": 0 }
                },
                "totalCostUsd": 0.021
            }"#,
        );
        write_costs(
            tmp.path(),
            "work",
            r#"{
                "models": {
                    "openai/gpt-4o": { "calls": 1, "inputTokens": 2000, "outputTokens": 1000, "costUsd": 0.04, "estimatedCalls": 1 }
                },
                "totalCostUsd": 0.04
            }"#,
        );

        let rows = aggregate_costs(tmp.path()).unwrap();
        assert_eq!(rows.len(), 2);
        // Sorted by descending cost: merged gpt-4o first.
        let gpt = &rows[0];
        assert_eq!(gpt.model, "openai/gpt-4o");
        assert_eq!(gpt.calls, 3);
        assert_eq!(gpt.input_tokens, 3000);
        assert_eq!(gpt.output_tokens, 1500);
        assert!((gpt.cost_usd - 0.06).abs() < 1e-9);
        assert_eq!(gpt.estimated_calls, 3);
        assert_eq!(rows[1].model, "anthropic/claude");
    }

    #[test]
    fn missing_or_empty_dir_yields_empty_report() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            aggregate_costs(&tmp.path().join("nope"))
                .unwrap()
                .is_empty()
        );
        assert!(aggregate_costs(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn invalid_snapshot_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_costs(tmp.path(), "main", "not json");
        write_costs(
            tmp.path(),
            "work",
            r#"{ "models": { "m": { "calls": 1, "inputTokens": 1, "outputTokens": 1, "costUsd": 0.5, "estimatedCalls": 0 } }, "totalCostUsd": 0.5 }"#,
        );
        let rows = aggregate_costs(tmp.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "m");
    }

    #[test]
    fn render_marks_estimated_and_totals() {
        let rows = vec![CostRow {
            model: "openai/gpt-4o".to_string(),
            calls: 3,
            input_tokens: 3000,
            output_tokens: 1500,
            cost_usd: 0.06,
            estimated_calls: 2,
        }];
        let out = render_costs(&rows);
        assert!(out.contains("MODEL"));
        assert!(out.contains("openai/gpt-4o"));
        assert!(out.contains("2 est"));
        assert!(out.contains("0.060000"));
        assert!(out.contains("TOTAL"));
    }
}
