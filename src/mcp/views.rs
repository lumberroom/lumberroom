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

use crate::domain::types::{Invocation, Memory, Provenance, Sensitivity};
use crate::ports::registry::RegistryVersion;
use crate::ports::Timeline;
use crate::services::bootstrap::{Digest, Fact};
use crate::services::forget::{Doomed, ForgetOutcome};
use crate::services::registry::{RegistryGetResult, RegistryHistoryResult};
use crate::services::search::SearchResult;
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

/// How much of an answer a caller asked for. Brief is what a model reads unless it asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    #[default]
    Brief,
    Full,
}

/// An installed CLI parses `score` as required and cannot send `detail`, so a declared CLI call
/// reads the full shape. Remove once a CLI that sends `detail` itself has shipped in two tags.
pub fn detail_for(invocation: Invocation, asked: Option<Detail>) -> Detail {
    asked.unwrap_or(if invocation == Invocation::Cli { Detail::Full } else { Detail::Brief })
}

/// `rows` for `context_bootstrap`, by the rule `detail_for` follows.
pub fn rows_for(invocation: Invocation, asked: Option<bool>) -> bool {
    asked.unwrap_or(invocation == Invocation::Cli)
}

/// The text channel's copy of a JSON answer.
pub fn compact(json: &serde_json::Value) -> String {
    serde_json::to_string(json).unwrap_or_default()
}

/// Strips a brief row from the full shape's own JSON, so a field the brief keeps can never differ
/// from the full one: same value, same timestamp precision (D6).
fn brief_row(row: &mut serde_json::Value, dropped: &[&str], dropped_when_null: &[&str]) {
    let Some(o) = row.as_object_mut() else { return };
    for k in dropped {
        o.remove(*k);
    }
    for k in dropped_when_null {
        if o.get(*k).is_some_and(serde_json::Value::is_null) {
            o.remove(*k);
        }
    }
    // `open` is the default and tells a model nothing; a level above it is the warning that
    // matters before a fact gets quoted somewhere shared.
    if o.get("sensitivity").and_then(serde_json::Value::as_str) == Some("open") {
        o.remove("sensitivity");
    }
    if o.get("tags").and_then(serde_json::Value::as_array).is_some_and(Vec::is_empty) {
        o.remove("tags");
    }
}

fn brief_rows(answer: &mut serde_json::Value, list: &str, dropped: &[&str], when_null: &[&str]) {
    if let Some(rows) = answer.get_mut(list).and_then(serde_json::Value::as_array_mut) {
        for row in rows {
            brief_row(row, dropped, when_null);
        }
    }
}

/// `memory_search`'s structured answer.
pub fn search(result: &SearchResult, detail: Detail) -> serde_json::Value {
    let mut answer = serde_json::to_value(result).unwrap_or_default();
    if detail == Detail::Brief {
        // The array order already carries `score`, and agents quoted it back as if it meant
        // something; `namespace` and `also_searched` already say what `primary` says.
        brief_rows(&mut answer, "hits", &["score", "primary"], &[]);
    }
    answer
}

/// `memory_history`'s structured answer.
pub fn timeline_value(t: &TimelineView, detail: Detail) -> serde_json::Value {
    let mut answer = serde_json::to_value(t).unwrap_or_default();
    if detail == Detail::Brief {
        brief_rows(
            &mut answer,
            "versions",
            &["embedding_model", "access_count", "last_accessed_at"],
            &["supersedes", "superseded_by", "superseded_at", "last_confirmed_at"],
        );
    }
    answer
}

/// `context_bootstrap`'s structured answer. Brief carries the text, the counts and every row the
/// text did not print whole; `rows` true is the `Digest` without `tenant`.
pub fn digest(d: &Digest, rows: bool) -> serde_json::Value {
    if rows {
        return full_digest(d);
    }
    let mut answer = serde_json::json!({
        "counts": { "memories": d.counts.memories, "registry": d.counts.registry },
        "inventory": d.inventory,
        "sealed_inventory": d.sealed_inventory,
        "project": d.project,
        "text": d.text,
    });
    let o = answer.as_object_mut().expect("a json! object literal");
    let facts = || d.profile.iter().chain(&d.project_context).chain(&d.recent);
    // An id the sections do not hold has nothing to carry, so it drops out here. `render_report`
    // takes its ids from these same sections, so that means a digest built by hand.
    let left_out: Vec<serde_json::Value> = d
        .left_out
        .iter()
        .filter_map(|id| facts().find(|f| &f.id == id))
        .map(left_out_fact)
        .collect();
    if !left_out.is_empty() {
        o.insert("left_out".into(), left_out.into());
    }
    let left_out_registry: Vec<serde_json::Value> = d
        .left_out_registry
        .iter()
        .filter_map(|(ns, kind, key)| {
            d.registry.iter().find(|r| &r.namespace == ns && &r.kind == kind && &r.key == key)
        })
        .map(|r| serde_json::to_value(r).unwrap_or_default())
        .collect();
    if !left_out_registry.is_empty() {
        o.insert("left_out_registry".into(), left_out_registry.into());
    }
    if facts().next().is_some() {
        o.insert("more".into(), DIGEST_MORE.into());
    }
    answer
}

/// A memory the text left out, with the stored text exactly: the model reads it nowhere else.
fn left_out_fact(f: &Fact) -> serde_json::Value {
    let mut row = serde_json::json!({ "id": f.id, "namespace": f.namespace, "content": f.content });
    let o = row.as_object_mut().expect("a json! object literal");
    if f.sensitivity > Sensitivity::Open {
        o.insert("sensitivity".into(), serde_json::to_value(f.sensitivity).unwrap_or_default());
    }
    if !f.tags.is_empty() {
        o.insert("tags".into(), f.tags.clone().into());
    }
    row
}

/// `rows: true`: today's `Digest` serialization minus `tenant`, which names the operator's tenant
/// to a model (D7), plus what the text left out by id.
fn full_digest(d: &Digest) -> serde_json::Value {
    let mut answer = serde_json::to_value(d).unwrap_or_default();
    if let Some(o) = answer.as_object_mut() {
        o.remove("tenant");
        o.insert("left_out".into(), d.left_out.clone().into());
        let registry: Vec<serde_json::Value> = d
            .left_out_registry
            .iter()
            .map(|(ns, kind, key)| serde_json::json!({ "namespace": ns, "kind": kind, "key": key }))
            .collect();
        o.insert("left_out_registry".into(), registry.into());
    }
    answer
}

pub const DIGEST_MORE: &str =
    "rows: true adds each printed fact's id and exact text; memory_search finds more";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::RegistrySummary;
    use crate::services::bootstrap::{Counts, Fact};
    use crate::services::search::Hit;
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

    fn hit(id: &str) -> Hit {
        Hit {
            id: id.into(),
            namespace: "global".into(),
            content: format!("fact {id}"),
            tags: vec!["t".into()],
            source: "Codex".into(),
            source_client: "id-codex".into(),
            sensitivity: Sensitivity::Open,
            created_at: "2026-09-28T17:35:39.860834+00:00".into(),
            score: 0.031,
            similarity: 0.7608,
            primary: true,
            superseded_by: None,
            occurred_at: None,
            occurred_until: None,
        }
    }

    /// Three hits: one private, one with `occurred_at`, one untagged.
    fn search_fixture() -> SearchResult {
        let mut private = hit("h1");
        private.sensitivity = Sensitivity::Private;
        let mut dated = hit("h2");
        dated.occurred_at = Some("2026-06-01T09:30:00.123456+00:00".into());
        dated.primary = false;
        let mut untagged = hit("h3");
        untagged.tags = vec![];
        SearchResult {
            namespaces: vec!["global".into()],
            also_searched: vec!["project:other".into()],
            hits: vec![private, dated, untagged],
        }
    }

    #[test]
    fn a_brief_search_keeps_ids_in_order_and_drops_only_the_listed_fields() {
        let result = search_fixture();
        let full = search(&result, Detail::Full);
        let brief = search(&result, Detail::Brief);
        let ids = |v: &serde_json::Value| -> Vec<String> {
            v["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(ids(&full), ids(&brief));
        assert_eq!(full, serde_json::to_value(&result).unwrap());
        for (f, b) in full["hits"].as_array().unwrap().iter().zip(brief["hits"].as_array().unwrap()) {
            assert!(b.get("score").is_none() && b.get("primary").is_none());
            for kept in ["similarity", "content", "source", "created_at", "id", "namespace"] {
                assert_eq!(b[kept], f[kept], "{kept}");
            }
            assert_eq!(b.get("occurred_at"), f.get("occurred_at"), "full precision kept");
            assert_eq!(b.get("sensitivity").is_some(), f["sensitivity"] != "open");
        }
    }

    #[test]
    fn a_brief_search_drops_empty_tags_and_keeps_the_top_level() {
        let result = search_fixture();
        let brief = search(&result, Detail::Brief);
        assert_eq!(brief["hits"][0]["tags"], serde_json::json!(["t"]));
        assert!(brief["hits"][2].get("tags").is_none());
        assert_eq!(brief["namespaces"], serde_json::json!(["global"]));
        assert_eq!(brief["also_searched"], serde_json::json!(["project:other"]));
    }

    #[test]
    fn a_brief_history_keeps_the_chain_and_drops_the_counters() {
        let t = timeline_view(Timeline { versions: vec![row()], withheld: 0, depth_capped: false }, &names());
        let brief = timeline_value(&t, Detail::Brief);
        let v = &brief["versions"][0];
        for gone in ["embedding_model", "access_count", "last_accessed_at"] {
            assert!(v.get(gone).is_none(), "{gone} survived: {v}");
        }
        let full = timeline_value(&t, Detail::Full);
        assert_eq!(full, serde_json::to_value(&t).unwrap());
        assert_eq!(v["created_at"], full["versions"][0]["created_at"]);
        assert_eq!(v["occurred_at"], full["versions"][0]["occurred_at"]);
    }

    #[test]
    fn a_brief_history_names_a_link_only_when_it_is_set() {
        let mut newer = row();
        newer.id = "m2".into();
        newer.supersedes = Some("m1".into());
        newer.sensitivity = Sensitivity::Private;
        newer.tags = vec![];
        let t = timeline_view(
            Timeline { versions: vec![row(), newer], withheld: 1, depth_capped: true },
            &names(),
        );
        let brief = timeline_value(&t, Detail::Brief);
        let (old, new) = (&brief["versions"][0], &brief["versions"][1]);
        for unset in ["supersedes", "superseded_by", "superseded_at", "last_confirmed_at", "sensitivity"] {
            assert!(old.get(unset).is_none(), "{unset} survived: {old}");
        }
        assert_eq!(new["supersedes"], "m1");
        assert_eq!(new["sensitivity"], "private");
        assert!(new.get("tags").is_none());
        assert_eq!(old["source"], "Codex");
        assert_eq!(brief["withheld"], 1);
        assert_eq!(brief["depth_capped"], true);
    }

    fn fact(id: &str, ns: &str) -> Fact {
        Fact {
            id: id.into(),
            namespace: ns.into(),
            content: format!("fact {id}\nsecond line"),
            tags: vec![],
            source: "Codex".into(),
            sensitivity: Sensitivity::Open,
            created_at: "2026-09-28T17:35:39.860834+00:00".into(),
        }
    }

    /// Profile p1, p2; recent r1; registry one port; tenant "t-1".
    fn digest_fixture() -> Digest {
        let mut p2 = fact("p2", "user:me");
        p2.sensitivity = Sensitivity::Private;
        p2.tags = vec!["digest".into()];
        Digest {
            generated_at: "2026-10-12T10:00:00.000001+00:00".into(),
            tenant: "t-1".into(),
            project: Some("project:lumberroom".into()),
            namespaces: vec!["user:me".into(), "global".into()],
            inventory: HashMap::from([("user:me".to_string(), 3)]),
            sealed_inventory: HashMap::from([("user:me".to_string(), 1)]),
            profile: vec![fact("p1", "user:me"), p2],
            project_context: vec![],
            recent: vec![fact("r1", "global")],
            registry: vec![RegistrySummary {
                namespace: "global".into(),
                kind: "service".into(),
                key: "services.lumberroom.port".into(),
                value: serde_json::json!(8080),
            }],
            counts: Counts {
                memories: 3,
                registry: 1,
                by_namespace: HashMap::from([("user:me".to_string(), 3)]),
            },
            cached: false,
            text: "## Memory digest\n- fact p1 second line\n".into(),
            left_out: vec![],
            left_out_registry: vec![],
        }
    }

    #[test]
    fn a_brief_digest_carries_every_row_the_text_left_out_whole() {
        let mut d = digest_fixture();
        d.left_out = vec!["p2".into(), "r1".into()];
        let brief = digest(&d, false);
        let left: Vec<&str> =
            brief["left_out"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(left, ["p2", "r1"]);
        assert_eq!(brief["left_out"][0]["content"], d.profile[1].content.as_str());
        assert!(brief.get("profile").is_none() && brief.get("tenant").is_none());
        assert!(brief["counts"].get("by_namespace").is_none());
        assert_eq!(brief["inventory"], serde_json::to_value(&d.inventory).unwrap());
        assert_eq!(brief["sealed_inventory"], serde_json::to_value(&d.sealed_inventory).unwrap());
        assert_eq!(brief["text"], d.text.as_str());
        assert_eq!(brief["more"], DIGEST_MORE);
    }

    #[test]
    fn a_left_out_row_names_sensitivity_and_tags_only_when_they_say_something() {
        let mut d = digest_fixture();
        d.left_out = vec!["p2".into(), "r1".into()];
        let brief = digest(&d, false);
        let (p2, r1) = (&brief["left_out"][0], &brief["left_out"][1]);
        assert_eq!(p2["sensitivity"], "private");
        assert_eq!(p2["tags"], serde_json::json!(["digest"]));
        assert_eq!(p2["namespace"], "user:me");
        assert!(r1.get("sensitivity").is_none() && r1.get("tags").is_none());
        assert!(r1.get("source").is_none() && r1.get("created_at").is_none());
    }

    #[test]
    fn a_brief_digest_carries_each_registry_entry_the_text_left_out() {
        let mut d = digest_fixture();
        d.left_out_registry =
            vec![("global".into(), "service".into(), "services.lumberroom.port".into())];
        let brief = digest(&d, false);
        assert_eq!(brief["left_out_registry"], serde_json::to_value(&d.registry).unwrap());
        assert!(digest(&digest_fixture(), false).get("left_out_registry").is_none());
    }

    #[test]
    fn a_digest_that_printed_everything_carries_no_left_out_key() {
        let d = digest_fixture();
        assert!(digest(&d, false).get("left_out").is_none());
    }

    #[test]
    fn a_digest_with_no_facts_carries_no_more_hint() {
        let mut d = digest_fixture();
        d.profile.clear();
        d.recent.clear();
        let brief = digest(&d, false);
        assert!(brief.get("more").is_none());
        assert_eq!(brief["project"], "project:lumberroom");
        assert_eq!(brief["counts"], serde_json::json!({"memories": 3, "registry": 1}));
    }

    #[test]
    fn the_full_digest_drops_only_the_tenant() {
        let d = digest_fixture();
        let full = digest(&d, true);
        let mut expected = serde_json::to_value(&d).unwrap();
        expected.as_object_mut().unwrap().remove("tenant");
        for (k, v) in expected.as_object().unwrap() {
            assert_eq!(full.get(k), Some(v), "{k}");
        }
        assert!(full.get("tenant").is_none());
    }

    #[test]
    fn the_full_digest_lists_what_the_text_left_out_by_id() {
        let mut d = digest_fixture();
        d.left_out = vec!["r1".into()];
        d.left_out_registry =
            vec![("global".into(), "service".into(), "services.lumberroom.port".into())];
        let full = digest(&d, true);
        assert_eq!(full["left_out"], serde_json::json!(["r1"]));
        assert_eq!(
            full["left_out_registry"],
            serde_json::json!([{"namespace": "global", "kind": "service", "key": "services.lumberroom.port"}])
        );
    }

    #[test]
    fn a_cli_call_reads_the_full_shape_unless_it_asks_otherwise() {
        assert_eq!(detail_for(Invocation::Cli, None), Detail::Full);
        assert_eq!(detail_for(Invocation::Hook, None), Detail::Brief);
        assert_eq!(detail_for(Invocation::User, None), Detail::Brief);
        assert_eq!(detail_for(Invocation::Cli, Some(Detail::Brief)), Detail::Brief);
        assert!(rows_for(Invocation::Cli, None) && !rows_for(Invocation::Model, None));
    }
}
