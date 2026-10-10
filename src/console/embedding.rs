//! The embedding-model section of the console: where each unit stands in a model change, and which
//! rows the server could not embed. Read-only. The switch itself is `lumberroom-server embeddings`.

use serde_json::Value;

use crate::authserver::pages::escape;
use crate::domain::embedding_migration::UnitStatus;
use crate::services::embedding_migration::SweepStatus;

/// `summary` is `embedding_status::summary` for the same `status`; the aggregate line reads it so the
/// console and `embeddings status` cannot disagree. Every string goes through `escape`: model ids
/// and unit names come from configuration and from a client's tenant slug.
pub fn section(status: &SweepStatus, summary: &Value) -> String {
    let rows: String = status.units.iter().map(unit_row).collect();
    let table = if rows.is_empty() {
        "<p class=\"em-empty\">No units yet. The first pass seeds them.</p>".to_string()
    } else {
        format!(
            "<table class=\"em-units\"><thead><tr><th>Unit</th><th>Phase</th><th>Active</th>\
<th>Other</th><th>Pending</th><th>Failed</th><th>Blocked</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };
    let error = match summary["error"].as_str() {
        Some(e) => format!("<p class=\"em-error\">Last pass failed: {}</p>", escape(e)),
        None => String::new(),
    };
    format!(
        "<section class=\"em-sec\"><h3>Embedding model</h3><p class=\"em-line\">{line}</p>\
<p class=\"em-line\">{disk}</p>{error}{table}</section>",
        line = escape(&aggregate_line(summary)),
        disk = escape(&disk_line(&summary["disk"])),
    )
}

fn aggregate_line(summary: &Value) -> String {
    format!(
        "{} rows pending, {} failed, {} rows a minute. Rollback: {}. Last pass: {}.",
        summary["rows_pending"],
        summary["rows_failed"],
        summary["rate_rows_per_min"],
        summary["rollback"].as_str().unwrap_or("unavailable").replace('_', " "),
        summary["last_pass_at"].as_str().unwrap_or("none yet"),
    )
}

fn disk_line(disk: &Value) -> String {
    let (Some(free), Some(floor)) = (disk["free_bytes"].as_u64(), disk["floor_bytes"].as_u64())
    else {
        return "Disk floor: off.".to_string();
    };
    let state = if disk["paused"].as_bool().unwrap_or(false) {
        "filling is paused below the floor"
    } else {
        "above the floor"
    };
    format!("Disk: {} GiB free, floor {} GiB, {state}.", free >> 30, floor >> 30)
}

fn unit_row(u: &UnitStatus) -> String {
    let failed = if u.failed_ids.is_empty() {
        u.counts.failed.to_string()
    } else {
        let ids: Vec<String> = u.failed_ids.iter().map(|id| id.to_string()).collect();
        format!("{}: {}", u.counts.failed, ids.join(", "))
    };
    format!(
        "<tr class=\"em-unit\"><td>{unit}</td><td>{phase}</td><td>{active} (slot {slot})</td>\
<td>{other}</td><td>{pending}</td><td>{failed}</td><td>{blocked}</td></tr>",
        unit = escape(&u.unit),
        phase = escape(&name_of(&u.phase)),
        active = escape(&u.active),
        slot = escape(u.active_slot.as_str()),
        other = escape(u.other.as_deref().unwrap_or("none")),
        pending = u.counts.other_pending,
        failed = escape(&failed),
        blocked = escape(&blocked_text(u)),
    )
}

/// The serde name of a unit-variant enum, so the console and the JSON use one spelling.
fn name_of<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        _ => String::new(),
    }
}

fn blocked_text(u: &UnitStatus) -> String {
    let Some(b) = &u.blocked else { return String::new() };
    let v = serde_json::to_value(b).unwrap_or(Value::Null);
    let reason = v["reason"].as_str().unwrap_or("blocked").replace('_', " ");
    match &v["detail"] {
        Value::Null => reason,
        Value::String(d) => format!("{reason}: {d}"),
        other => format!("{reason}: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::embedding_migration::{Blocked, Counts, DiskStatus, Phase, UnitStatus};
    use crate::domain::embedding_slot::VectorSlot;
    use crate::services::embedding_status;
    use std::collections::HashMap;

    fn unit(name: &str, phase: Phase, counts: Counts) -> UnitStatus {
        UnitStatus {
            unit: name.into(),
            phase,
            active_slot: VectorSlot::A,
            active: "Xenova/bge-base-en-v1.5@q8".into(),
            other: Some("openai:google/embeddinggemma-2".into()),
            counts,
            failed_ids: vec![],
            blocked: None,
            flipped_at: None,
            retire_after: None,
        }
    }

    fn render(units: Vec<UnitStatus>) -> String {
        let status = SweepStatus {
            units,
            disk: Some(DiskStatus {
                free_bytes: 30_064_771_072,
                floor_bytes: 16_106_127_360,
                paused: true,
            }),
            error: Some("pass failed: <timeout>".into()),
            ..SweepStatus::default()
        };
        let summary = embedding_status::summary(&status, &HashMap::new());
        section(&status, &summary)
    }

    #[test]
    fn one_row_per_unit_with_its_failed_ids() {
        let id = uuid::Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let mut failing = unit("two", Phase::Blocked, Counts { failed: 1, ..Counts::default() });
        failing.failed_ids = vec![id];
        failing.blocked = Some(Blocked::Failed(1));
        let html = render(vec![
            unit("me", Phase::Filling, Counts { other_pending: 1840, ..Counts::default() }),
            failing,
        ]);
        assert_eq!(html.matches("<tr class=\"em-unit\"").count(), 2);
        assert!(html.contains("1840"));
        assert!(html.contains("11111111-2222-3333-4444-555555555555"));
        assert!(html.contains("filling"));
        assert!(html.contains("blocked"));
    }

    #[test]
    fn everything_that_came_from_outside_is_escaped() {
        let mut u = unit("<script>x</script>", Phase::Steady, Counts::default());
        u.active = "a\"b&c".into();
        let html = render(vec![u]);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a&quot;b&amp;c"));
        assert!(html.contains("pass failed: &lt;timeout&gt;"));
    }

    #[test]
    fn a_paused_disk_is_said_in_words() {
        let html = render(vec![unit("me", Phase::Filling, Counts::default())]);
        assert!(html.contains("paused"), "{html}");
    }

    #[test]
    fn no_units_says_so() {
        let html = render(vec![]);
        assert!(html.contains("No units"), "{html}");
    }
}
