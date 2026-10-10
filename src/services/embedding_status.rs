//! The status JSON of an embedding-model migration (decision 0027).
//!
//! The sweep writes `published` into `embedding_control.server_status` at the end of each pass, and
//! `lumberroom-server embeddings status` reads it back, so the shape here is a contract between two
//! processes. It lives in `services` because the sweep calls it and services never import `http`.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::domain::embedding_migration::{Phase, Rollback};
use crate::domain::similarity::SimilarityThresholds;
use crate::services::embedding_migration::SweepStatus;

/// The phases in the order the summary lists them.
const PHASES: [Phase; 7] = [
    Phase::Steady,
    Phase::Filling,
    Phase::Held,
    Phase::Ready,
    Phase::Flipped,
    Phase::Retiring,
    Phase::Blocked,
];

/// The spec's summary JSON. Keys go in the spec's order, which a reader sees only when serde_json
/// builds with `preserve_order`; without it a `Value` sorts its keys and the content is the same.
///
/// `rows_pending` sums `other_pending`: the rows the inactive slot still lacks for its target.
pub fn summary(status: &SweepStatus, thresholds: &HashMap<String, Arc<SimilarityThresholds>>) -> Value {
    let mut units = Map::new();
    for phase in PHASES {
        let n = status.units.iter().filter(|u| u.phase == phase).count();
        units.insert(phase_name(phase), n.into());
    }

    // Sorted so two passes over the same state publish the same bytes.
    let mut by_model: Vec<(&String, &Arc<SimilarityThresholds>)> = thresholds.iter().collect();
    by_model.sort_by(|a, b| a.0.cmp(b.0));
    let mut models = Map::new();
    for (model, t) in by_model {
        let rows: Vec<Value> = t
            .values
            .iter()
            .map(|(key, r)| json!({ "key": key, "value": r.value, "source": r.source }))
            .collect();
        models.insert(model.clone(), Value::Array(rows));
    }

    let mut out = Map::new();
    out.insert("units".into(), Value::Object(units));
    out.insert("rows_pending".into(), status.units.iter().map(|u| u.counts.other_pending).sum::<i64>().into());
    out.insert("rows_failed".into(), status.units.iter().map(|u| u.counts.failed).sum::<i64>().into());
    out.insert("rate_rows_per_min".into(), json!(status.rate_rows_per_min));
    out.insert("rollback".into(), json!(status.rollback.unwrap_or(Rollback::Unavailable)));
    out.insert("disk".into(), json!(status.disk));
    out.insert("thresholds".into(), Value::Object(models));
    out.insert("last_pass_at".into(), json!(status.last_pass_at));
    out.insert("error".into(), json!(status.error));
    Value::Object(out)
}

/// What the sweep writes to `embedding_control.server_status` and the command reads back.
pub fn published(status: &SweepStatus, thresholds: &HashMap<String, Arc<SimilarityThresholds>>) -> Value {
    json!({ "summary": summary(status, thresholds), "units": status.units })
}

fn phase_name(phase: Phase) -> String {
    match serde_json::to_value(phase) {
        Ok(Value::String(s)) => s,
        _ => unreachable!("Phase serialises as a string"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::embedding_migration::{
        Blocked, Counts, DiskStatus, Phase, Rollback, UnitStatus,
    };
    use crate::domain::embedding_slot::VectorSlot;
    use crate::domain::similarity::{Resolved, Source};

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

    fn status() -> SweepStatus {
        let filling = Counts { eligible: 12000, other_pending: 1840, without_vector: 3, ..Counts::default() };
        let mut failing = unit("two", Phase::Blocked, Counts { failed: 2, other_pending: 5, ..Counts::default() });
        failing.failed_ids = vec![uuid::Uuid::nil()];
        failing.blocked = Some(Blocked::Failed(2));
        SweepStatus {
            units: vec![unit("me", Phase::Filling, filling), failing],
            disk: Some(DiskStatus { free_bytes: 30_064_771_072, floor_bytes: 16_106_127_360, paused: false }),
            rollback: None,
            rate_rows_per_min: 118.0,
            last_pass_at: Some("2026-10-12T09:30:00Z".parse().unwrap()),
            error: None,
        }
    }

    fn thresholds() -> HashMap<String, Arc<SimilarityThresholds>> {
        let mut values = BTreeMap::new();
        values.insert("conflict".to_string(), Resolved { value: 0.91, source: Source::Study });
        values.insert("route_max_top".to_string(), Resolved { value: 0.65, source: Source::Carried });
        let t = SimilarityThresholds {
            model: "openai:google/embeddinggemma-2".into(),
            family: Some("embeddinggemma-2".into()),
            values,
        };
        HashMap::from([(t.model.clone(), Arc::new(t))])
    }

    #[test]
    fn the_summary_matches_the_spec_shape() {
        let v = summary(&status(), &thresholds());
        let keys: std::collections::BTreeSet<&str> =
            v.as_object().unwrap().keys().map(String::as_str).collect();
        let want = [
            "units", "rows_pending", "rows_failed", "rate_rows_per_min", "rollback", "disk",
            "thresholds", "last_pass_at", "error",
        ];
        assert_eq!(keys, want.into_iter().collect());

        assert_eq!(
            v["units"],
            serde_json::json!({ "steady": 0, "filling": 1, "held": 0, "ready": 0,
                                "flipped": 0, "retiring": 0, "blocked": 1 })
        );
        assert_eq!(v["rows_pending"], 1845);
        assert_eq!(v["rows_failed"], 2);
        assert_eq!(v["rate_rows_per_min"], 118.0);
        assert_eq!(v["rollback"], "unavailable");
        assert_eq!(
            v["disk"],
            serde_json::json!({ "free_bytes": 30_064_771_072u64, "floor_bytes": 16_106_127_360u64, "paused": false })
        );
        assert_eq!(
            v["thresholds"]["openai:google/embeddinggemma-2"],
            serde_json::json!([
                { "key": "conflict", "value": 0.91, "source": "study" },
                { "key": "route_max_top", "value": 0.65, "source": "carried" },
            ])
        );
        assert_eq!(v["last_pass_at"], "2026-10-12T09:30:00Z");
        assert!(v["error"].is_null());
    }

    #[test]
    fn rollback_names_each_state() {
        for (rollback, want) in [
            (None, "unavailable"),
            (Some(Rollback::Unavailable), "unavailable"),
            (Some(Rollback::NeedsFill), "needs_fill"),
            (Some(Rollback::Instant), "instant"),
        ] {
            let mut s = status();
            s.rollback = rollback;
            assert_eq!(summary(&s, &thresholds())["rollback"], want);
        }
    }

    #[test]
    fn a_pass_with_no_disk_check_reports_null() {
        let mut s = status();
        s.disk = None;
        assert!(summary(&s, &thresholds())["disk"].is_null());
    }

    #[test]
    fn published_round_trips_unit_statuses() {
        let s = status();
        let text = serde_json::to_string(&published(&s, &thresholds())).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(back["summary"], summary(&s, &thresholds()));
        let units = back["units"].as_array().unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0]["unit"], "me");
        assert_eq!(units[0]["phase"], "filling");
        assert_eq!(units[0]["active_slot"], "a");
        // The counts are flattened into the unit, as the spec's per-unit shape has them.
        assert_eq!(units[0]["other_pending"], 1840);
        assert_eq!(units[0]["eligible"], 12000);
        assert_eq!(units[1]["failed"], 2);
        assert_eq!(units[1]["failed_ids"][0], uuid::Uuid::nil().to_string());
        assert_eq!(units[1]["blocked"]["reason"], "failed");
        assert_eq!(back["units"], serde_json::to_value(&s.units).unwrap());
    }
}
