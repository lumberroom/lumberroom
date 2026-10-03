//! MCP answers that name a row's writer instead of identifying it (decision 0020).
//!
//! `memory_history`, `registry_get`, `registry_history` and `memory_forget` return service types
//! the admin routes and the console also serialise, and those keep the stored `source_client`. A
//! `Serialize` change on `Memory` or `Provenance` would have renamed the field on every surface at
//! once, so each tool maps its answer into a view here, with `source` where the id was.
//!
//! Each view spells out its fields rather than flattening the type it mirrors. A field added to
//! `Memory` later then stays off MCP until somebody adds it here, which is the direction to fail
//! in: the tests below compare key sets and name the field that went missing.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::types::{Memory, Provenance, Sensitivity};
use crate::ports::registry::RegistryVersion;
use crate::ports::Timeline;
use crate::services::forget::{Doomed, ForgetOutcome};
use crate::services::registry::{RegistryGetResult, RegistryHistoryResult};
use crate::services::{sources, Ctx};

/// A stored `source_client` through the names `sources::labels` returned.
fn named(names: &HashMap<String, String>, stored: &str) -> String {
    names.get(stored).cloned().unwrap_or_else(|| stored.to_string())
}

#[derive(Debug, Serialize)]
pub struct MemoryView {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source: String,
    pub embedding_model: Option<String>,
    pub sensitivity: Sensitivity,
    pub supersedes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_until: Option<DateTime<Utc>>,
    pub superseded_by: Option<String>,
    pub superseded_at: Option<DateTime<Utc>>,
    pub access_count: i32,
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub last_confirmed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

fn memory(m: Memory, names: &HashMap<String, String>) -> MemoryView {
    MemoryView {
        source: named(names, &m.source_client),
        id: m.id,
        namespace: m.namespace,
        content: m.content,
        tags: m.tags,
        embedding_model: m.embedding_model,
        sensitivity: m.sensitivity,
        supersedes: m.supersedes,
        occurred_at: m.occurred_at,
        occurred_until: m.occurred_until,
        superseded_by: m.superseded_by,
        superseded_at: m.superseded_at,
        access_count: m.access_count,
        last_accessed_at: m.last_accessed_at,
        last_confirmed_at: m.last_confirmed_at,
        created_at: m.created_at,
    }
}

#[derive(Debug, Serialize)]
pub struct TimelineView {
    pub versions: Vec<MemoryView>,
    pub withheld: i64,
    pub depth_capped: bool,
}

pub async fn timeline(ctx: &Ctx, t: Timeline) -> TimelineView {
    let writers: Vec<String> = t.versions.iter().map(|m| m.source_client.clone()).collect();
    timeline_view(t, &sources::labels(ctx, &writers).await)
}

fn timeline_view(t: Timeline, names: &HashMap<String, String>) -> TimelineView {
    TimelineView {
        versions: t.versions.into_iter().map(|m| memory(m, names)).collect(),
        withheld: t.withheld,
        depth_capped: t.depth_capped,
    }
}

/// `Provenance` without the writer, which travels beside it as `source`.
#[derive(Debug, Serialize)]
pub struct ProvenanceView {
    pub conv_id: Option<String>,
    pub confidence: f64,
    pub user_confirmed: bool,
    pub valid_from: String,
}

fn provenance(p: Provenance) -> ProvenanceView {
    ProvenanceView {
        conv_id: p.conv_id,
        confidence: p.confidence,
        user_confirmed: p.user_confirmed,
        valid_from: p.valid_from,
    }
}

#[derive(Debug, Serialize)]
pub struct RegistryGetView {
    pub found: bool,
    pub kind: String,
    pub key: String,
    pub namespace: Option<String>,
    pub value: serde_json::Value,
    pub provenance: Option<ProvenanceView>,
    /// The app that wrote the value, by name. Null exactly when `provenance` is.
    pub source: Option<String>,
    pub sensitivity: Option<Sensitivity>,
    pub version: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_from: Option<String>,
    pub searched: Vec<String>,
}

pub async fn registry_get(ctx: &Ctx, r: RegistryGetResult) -> RegistryGetView {
    let writers: Vec<String> = r.provenance.iter().map(|p| p.source_client.clone()).collect();
    registry_get_view(r, &sources::labels(ctx, &writers).await)
}

fn registry_get_view(r: RegistryGetResult, names: &HashMap<String, String>) -> RegistryGetView {
    RegistryGetView {
        source: r.provenance.as_ref().map(|p| named(names, &p.source_client)),
        provenance: r.provenance.map(provenance),
        found: r.found,
        kind: r.kind,
        key: r.key,
        namespace: r.namespace,
        value: r.value,
        sensitivity: r.sensitivity,
        version: r.version,
        resolved_from: r.resolved_from,
        searched: r.searched,
    }
}

#[derive(Debug, Serialize)]
pub struct RegistryVersionView {
    pub registry_id: String,
    pub namespace: String,
    pub kind: String,
    pub key: String,
    pub value: serde_json::Value,
    pub provenance: ProvenanceView,
    pub source: String,
    pub sensitivity: Sensitivity,
    pub version: i32,
    pub replaced_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_from: Option<String>,
}

fn registry_version(v: RegistryVersion, names: &HashMap<String, String>) -> RegistryVersionView {
    RegistryVersionView {
        source: named(names, &v.provenance.source_client),
        provenance: provenance(v.provenance),
        registry_id: v.registry_id,
        namespace: v.namespace,
        kind: v.kind,
        key: v.key,
        value: v.value,
        sensitivity: v.sensitivity,
        version: v.version,
        replaced_at: v.replaced_at,
        resolved_from: v.resolved_from,
    }
}

#[derive(Debug, Serialize)]
pub struct RegistryHistoryView {
    pub kind: String,
    pub key: String,
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_from: Option<String>,
    pub searched: Vec<String>,
    pub entries: Vec<RegistryVersionView>,
}

pub async fn registry_history(ctx: &Ctx, r: RegistryHistoryResult) -> RegistryHistoryView {
    let writers: Vec<String> =
        r.entries.iter().map(|e| e.provenance.source_client.clone()).collect();
    registry_history_view(r, &sources::labels(ctx, &writers).await)
}

fn registry_history_view(
    r: RegistryHistoryResult,
    names: &HashMap<String, String>,
) -> RegistryHistoryView {
    RegistryHistoryView {
        kind: r.kind,
        key: r.key,
        namespace: r.namespace,
        resolved_from: r.resolved_from,
        searched: r.searched,
        entries: r.entries.into_iter().map(|e| registry_version(e, names)).collect(),
    }
}

#[derive(Debug, Serialize)]
pub struct DoomedView {
    pub id: String,
    pub namespace: String,
    pub sensitivity: Sensitivity,
    pub source: String,
    pub created_at: String,
    pub preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
}

fn doomed(d: Doomed, names: &HashMap<String, String>) -> DoomedView {
    DoomedView {
        source: named(names, &d.source_client),
        id: d.id,
        namespace: d.namespace,
        sensitivity: d.sensitivity,
        created_at: d.created_at,
        preview: d.preview,
        similarity: d.similarity,
    }
}

#[derive(Debug, Serialize)]
pub struct ForgetView {
    pub dry_run: bool,
    pub count: usize,
    pub rows: Vec<DoomedView>,
    pub consequences: Vec<String>,
    pub revived: Vec<String>,
    pub spliced: Vec<String>,
    pub blocked: Vec<String>,
    pub text: String,
}

pub async fn forget(ctx: &Ctx, f: ForgetOutcome) -> ForgetView {
    let writers: Vec<String> = f.rows.iter().map(|r| r.source_client.clone()).collect();
    forget_view(f, &sources::labels(ctx, &writers).await)
}

fn forget_view(f: ForgetOutcome, names: &HashMap<String, String>) -> ForgetView {
    ForgetView {
        dry_run: f.dry_run,
        count: f.count,
        rows: f.rows.into_iter().map(|r| doomed(r, names)).collect(),
        consequences: f.consequences,
        revived: f.revived,
        spliced: f.spliced,
        blocked: f.blocked,
        text: f.text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The keys a value serialises to, with every `source_client` swapped for `source`: the one
    /// difference a view is allowed to have from the type it mirrors.
    fn keys_as_mcp_should_see_them(value: &impl Serialize) -> BTreeSet<String> {
        let json = serde_json::to_value(value).unwrap();
        json.as_object()
            .unwrap()
            .keys()
            .map(|k| if k == "source_client" { "source".to_string() } else { k.clone() })
            .collect()
    }

    fn keys(value: &impl Serialize) -> BTreeSet<String> {
        serde_json::to_value(value).unwrap().as_object().unwrap().keys().cloned().collect()
    }

    fn row() -> Memory {
        let now = Utc::now();
        Memory {
            id: "m1".into(),
            namespace: "global".into(),
            content: "a fact".into(),
            tags: vec!["t".into()],
            source_client: "id-codex".into(),
            embedding_model: Some("hash".into()),
            sensitivity: Sensitivity::Open,
            supersedes: None,
            occurred_at: Some(now),
            occurred_until: Some(now),
            superseded_by: None,
            superseded_at: None,
            access_count: 0,
            last_accessed_at: None,
            last_confirmed_at: None,
            created_at: now,
        }
    }

    fn names() -> HashMap<String, String> {
        HashMap::from([("id-codex".to_string(), "Codex".to_string())])
    }

    fn written_by_codex() -> Provenance {
        Provenance {
            source_client: "id-codex".into(),
            conv_id: Some("c".into()),
            confidence: 1.0,
            user_confirmed: false,
            valid_from: "2026-09-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_memory_view_carries_every_memory_field_with_source_in_place_of_source_client() {
        let view = memory(row(), &names());
        assert_eq!(keys(&view), keys_as_mcp_should_see_them(&row()));
        assert_eq!(view.source, "Codex");
        assert!(!serde_json::to_string(&view).unwrap().contains("id-codex"));
    }

    #[test]
    fn a_writer_no_client_matches_prints_as_stored() {
        let mut m = row();
        m.source_client = "cli-laptop".into();
        assert_eq!(memory(m, &names()).source, "cli-laptop");
    }

    #[test]
    fn a_provenance_view_drops_the_writer_and_keeps_the_rest() {
        let mut expected = keys(&written_by_codex());
        expected.remove("source_client");
        assert_eq!(keys(&provenance(written_by_codex())), expected);
    }

    #[test]
    fn a_registry_version_view_carries_every_field_with_source_beside_provenance() {
        let version = version_row();
        let mut expected = keys(&version);
        expected.insert("source".into());
        let view = registry_version(version, &names());
        assert_eq!(keys(&view), expected);
        assert_eq!(view.source, "Codex");
        assert!(!serde_json::to_string(&view).unwrap().contains("id-codex"));
    }

    fn doomed_row() -> Doomed {
        Doomed {
            id: "m1".into(),
            namespace: "global".into(),
            sensitivity: Sensitivity::Open,
            source_client: "id-codex".into(),
            created_at: "2026-09-01T00:00:00Z".into(),
            preview: "a fact".into(),
            similarity: Some(0.9),
        }
    }

    fn version_row() -> RegistryVersion {
        RegistryVersion {
            registry_id: "r1".into(),
            namespace: "global".into(),
            kind: "service".into(),
            key: "services.lumberroom.port".into(),
            value: serde_json::json!(8080),
            provenance: written_by_codex(),
            sensitivity: Sensitivity::Open,
            version: 1,
            replaced_at: Utc::now(),
            resolved_from: Some("lumberroom.port".into()),
        }
    }

    #[test]
    fn a_timeline_view_carries_every_timeline_field() {
        let t = Timeline { versions: vec![row()], withheld: 2, depth_capped: true };
        let expected = keys(&t);
        let view = timeline_view(t, &names());
        assert_eq!(keys(&view), expected);
        assert_eq!(view.versions[0].source, "Codex");
    }

    #[test]
    fn a_registry_get_view_carries_every_field_and_adds_source_beside_provenance() {
        let found = RegistryGetResult {
            found: true,
            kind: "service".into(),
            key: "services.lumberroom.port".into(),
            namespace: Some("global".into()),
            value: serde_json::json!(8080),
            provenance: Some(written_by_codex()),
            sensitivity: Some(Sensitivity::Open),
            version: Some(2),
            resolved_from: Some("lumberroom.port".into()),
            searched: vec!["global".into()],
        };
        let mut expected = keys(&found);
        expected.insert("source".into());
        let view = registry_get_view(found, &names());
        assert_eq!(keys(&view), expected);
        assert_eq!(view.source.as_deref(), Some("Codex"));
        assert!(!serde_json::to_string(&view).unwrap().contains("id-codex"));
    }

    #[test]
    fn a_registry_get_view_for_a_missing_key_has_no_source() {
        let missing = RegistryGetResult {
            found: false,
            kind: "service".into(),
            key: "services.nothing.port".into(),
            namespace: None,
            value: serde_json::Value::Null,
            provenance: None,
            sensitivity: None,
            version: None,
            resolved_from: None,
            searched: vec!["global".into()],
        };
        assert_eq!(registry_get_view(missing, &names()).source, None);
    }

    #[test]
    fn a_registry_history_view_carries_every_field() {
        let history = RegistryHistoryResult {
            kind: "service".into(),
            key: "services.lumberroom.port".into(),
            namespace: Some("global".into()),
            resolved_from: Some("lumberroom.port".into()),
            searched: vec!["global".into()],
            entries: vec![version_row()],
        };
        let expected = keys(&history);
        let view = registry_history_view(history, &names());
        assert_eq!(keys(&view), expected);
        assert_eq!(view.entries[0].source, "Codex");
        assert!(!serde_json::to_string(&view).unwrap().contains("id-codex"));
    }

    #[test]
    fn a_forget_view_carries_every_outcome_field() {
        let outcome = ForgetOutcome {
            dry_run: true,
            count: 1,
            rows: vec![doomed_row()],
            consequences: vec![],
            revived: vec![],
            spliced: vec![],
            blocked: vec![],
            text: "Would delete 1 row".into(),
        };
        let expected = keys(&outcome);
        let view = forget_view(outcome, &names());
        assert_eq!(keys(&view), expected);
        assert!(!serde_json::to_string(&view).unwrap().contains("id-codex"));
    }

    #[test]
    fn a_doomed_view_carries_every_field_with_source_in_place_of_source_client() {
        let row = doomed_row();
        let expected = keys_as_mcp_should_see_them(&row);
        let view = doomed(row, &names());
        assert_eq!(keys(&view), expected);
        assert_eq!(view.source, "Codex");
    }
}
