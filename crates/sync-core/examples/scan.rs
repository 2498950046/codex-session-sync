use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;
use sync_core::{default_codex_home, scan_codex_home};

fn main() -> anyhow::Result<()> {
    let home = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_codex_home);
    let report = scan_codex_home(home)?;
    let mut warning_counts = BTreeMap::<String, usize>::new();
    for warning in &report.warnings {
        *warning_counts
            .entry(format!("{:?}", warning.kind))
            .or_default() += 1;
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "codexHome": report.codex_home,
            "databaseCount": report.database_paths.len(),
            "activeCount": report.active_count,
            "archivedCount": report.archived_count,
            "threadCount": report.total_count(),
            "totalRolloutBytes": report.total_rollout_bytes,
            "warningCounts": warning_counts,
        }))?
    );
    Ok(())
}
