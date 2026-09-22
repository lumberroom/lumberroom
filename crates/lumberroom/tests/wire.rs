//! The pin. Every fixture under `tests/fixtures/` is a response the server produces, transcribed
//! from the code that builds it, with the file and symbol named above each case.
//!
//! Two directions, because they fail differently. A response that changes shape breaks a client
//! that already shipped, so those are checked by deserializing the fixture and asserting the
//! fields this client prints survive. A request that changes shape fails at the server's
//! `Deserialize` at runtime and never at compile time, so those are checked by serializing and
//! comparing the exact key set against the server's struct.
//!
//! What this does not do is watch the server. It pins the client against a transcription, so a
//! server-side rename lands here only when somebody updates the fixture. Closing that loop needs a
//! test in the server crate that serializes the real types into these same files and diffs; that
//! file is in `tests/`, which this work does not own, and it is in the return's not_done.

use lumberroom::wire::*;
use std::collections::BTreeSet;

fn fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn keys(value: &serde_json::Value) -> BTreeSet<String> {
    value.as_object().expect("an object").keys().cloned().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

// ---- responses ----

/// `readyz` in `src/http/mod.rs`.
#[test]
fn readyz_carries_ok_and_the_server_auth_mode() {
    let r: Ready = serde_json::from_value(fixture("readyz.json")).unwrap();
    assert!(r.ok);
    assert_eq!(r.auth_mode.as_deref(), Some("oauth"));
}

/// `whoami` in `src/http/mod.rs`. `mode` is the credential's mode, not the server's.
#[test]
fn whoami_carries_the_client_and_the_credential_mode() {
    let w: Whoami = serde_json::from_value(fixture("whoami.json")).unwrap();
    assert_eq!(w.client, "claude-code");
    assert_eq!(w.mode, "token");
}

/// `services::search::SearchResult` and `Hit`, as `memory_search` structured content.
#[test]
fn a_search_hit_carries_id_namespace_content_and_score() {
    let r: SearchResult = serde_json::from_value(fixture("memory_search.json")).unwrap();
    assert_eq!(r.hits.len(), 1);
    let hit = &r.hits[0];
    assert_eq!(hit.namespace, "user:me");
    assert!(hit.content.starts_with("The owner runs lumberroom"));
    assert!((hit.score - 0.81274).abs() < 1e-9);
    assert_eq!(hit.id, "9f1c2b4e-0000-4a1b-8c3d-1122334455aa");
}

/// `domain::types::WriteOutcome`. `superseded` and `possible_conflicts` are skipped when empty, so
/// the fixture omits them and this must still parse.
#[test]
fn a_write_outcome_parses_without_its_optional_fields() {
    let w: WriteOutcome = serde_json::from_value(fixture("memory_write.json")).unwrap();
    assert!(!w.deduplicated);
    assert_eq!(w.namespace, "user:me");
}

/// `domain::types::Memory`, the whole row, as `GET /admin/memory/{id}` returns it.
#[test]
fn a_memory_row_parses_and_ignores_the_fields_this_client_never_prints() {
    let m: Memory = serde_json::from_value(fixture("memory.json")).unwrap();
    assert_eq!(m.sensitivity, "open");
    assert_eq!(m.tags, vec!["infra".to_string()]);
    assert_eq!(m.source_client, "claude-code");
    assert_eq!(m.created_at, "2026-08-19T11:02:31.442Z");
}

#[test]
fn the_stale_review_is_a_list_of_memory_rows() {
    let r: StaleReview = serde_json::from_value(fixture("review_stale.json")).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].namespace, "global");
}

/// `conflict_side` in `src/http/mod.rs` is narrower than a `Memory`, which is why the client has a
/// separate type for it rather than reusing one.
#[test]
fn a_conflict_pair_has_two_sides_and_a_similarity() {
    let r: ConflictReview = serde_json::from_value(fixture("review_conflicts.json")).unwrap();
    let pair = &r.pairs[0];
    assert!((pair.similarity - 0.9412).abs() < 1e-9);
    assert_eq!(pair.older.content, "The owner prefers tabs.");
    assert_eq!(pair.newer.namespace, "user:me");
}

#[test]
fn the_registry_review_has_both_lists() {
    let r: RegistryReview = serde_json::from_value(fixture("review_registry.json")).unwrap();
    assert_eq!(r.due_for_review[0].key, "host:db.prod");
    assert_eq!(r.non_canonical[0].kind, "service");
}

#[test]
fn an_export_page_is_memory_rows() {
    let p: ExportPage = serde_json::from_value(fixture("export.json")).unwrap();
    assert_eq!(p.rows[0].tags.len(), 2);
}

/// `tool_stats_body` in `src/http/mod.rs`, whose rows are `ports::tool_calls::ToolCallStats`.
#[test]
fn per_tool_stats_keep_their_snake_case_latency_keys() {
    let s: ToolStats = serde_json::from_value(fixture("statsz_by_tool.json")).unwrap();
    assert_eq!(s.window_hours, 168);
    assert_eq!(s.totals.unprompted_rate, Some(0.25));
    assert_eq!(s.by_tool[0].p50_ms, Some(41));
    assert_eq!(s.by_tool[0].p95_ms, None);
    // The rename that once turned every latency into "-ms" with nothing failing.
    let raw = fixture("statsz_by_tool.json");
    assert!(raw["by_tool"][0].get("p50_ms").is_some());
    assert!(raw["by_tool"][0].get("p50-ms").is_none());
}

/// `client_stats_body` in `src/http/mod.rs`, rows from `ports::tool_calls::ClientStats`.
#[test]
fn per_client_stats_carry_the_two_rates_the_client_prints() {
    let s: ClientStats = serde_json::from_value(fixture("statsz_by_client.json")).unwrap();
    let row = &s.by_client[0];
    assert_eq!(row.write_to_read_ratio, Some(0.33));
    assert_eq!(row.unprompted_write_rate, Some(0.5));
    assert_eq!(row.reads, 9);
}

/// `clients` in `src/authserver/routes.rs`.
#[test]
fn the_client_list_carries_consent_and_revocation() {
    let l: ClientList = serde_json::from_value(fixture("oauth_clients.json")).unwrap();
    let c = &l.clients[0];
    assert_eq!(c.client_name, "lumberroom");
    assert_eq!(c.registered_via, "dcr");
    assert!(c.consented_at.is_some());
    assert!(c.revoked_at.is_none());
}

#[test]
fn a_token_response_carries_the_refresh_token_and_the_ttl() {
    let t: TokenResponse = serde_json::from_value(fixture("oauth_token.json")).unwrap();
    assert_eq!(t.access_token, "at_abc");
    assert_eq!(t.refresh_token.as_deref(), Some("rt_def"));
    assert_eq!(t.expires_in, Some(3600));
}

/// A server that grows a field must not break a client already installed on a laptop.
#[test]
fn an_unknown_field_on_a_response_is_ignored() {
    let mut raw = fixture("whoami.json");
    raw["some_future_field"] = serde_json::json!({ "a": 1 });
    let w: Whoami = serde_json::from_value(raw).unwrap();
    assert_eq!(w.client, "claude-code");
}

// ---- requests ----

/// `RegistryWrite` in `src/http/mod.rs`: namespace, kind, key, value, and an optional sensitivity
/// this client does not send.
#[test]
fn a_registry_write_sends_exactly_the_four_required_keys() {
    let req = RegistryWriteRequest {
        namespace: "global",
        kind: "host",
        key: "host:db.prod",
        value: serde_json::json!({ "addr": "10.0.0.4" }),
    };
    let v = serde_json::to_value(req).unwrap();
    assert_eq!(keys(&v), set(&["namespace", "kind", "key", "value"]));
}

/// `SupersedeBody` in `src/http/mod.rs`.
#[test]
fn a_supersede_sends_new_id_and_nothing_else() {
    let v = serde_json::to_value(SupersedeRequest { new_id: "2222" }).unwrap();
    assert_eq!(keys(&v), set(&["new_id"]));
}

/// `mcp::SearchArgs`. Every optional is skipped when absent, so the server's own defaults apply.
#[test]
fn search_arguments_omit_what_was_not_asked_for() {
    let v = serde_json::to_value(SearchArgsRequest {
        query: "what do I prefer".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["query"]));

    let v = serde_json::to_value(SearchArgsRequest {
        query: "q".into(),
        namespaces: Some(vec!["user:me".into()]),
        limit: Some(5),
        project: Some("lumberroom".into()),
        as_of: None,
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["query", "namespaces", "limit", "project"]));
}

/// `mcp::WriteArgs`.
#[test]
fn write_arguments_always_carry_content_and_namespace() {
    let v = serde_json::to_value(WriteArgsRequest {
        content: "a fact".into(),
        namespace: "user:me".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["content", "namespace"]));

    let v = serde_json::to_value(WriteArgsRequest {
        content: "a fact".into(),
        namespace: "user:me".into(),
        tags: Some(vec!["infra".into()]),
        supersedes: Some("1111".into()),
        occurred_at: None,
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["content", "namespace", "tags", "supersedes"]));
}

/// `mcp::RegistryArgs`.
#[test]
fn registry_get_arguments_send_kind_and_key_at_minimum() {
    let v = serde_json::to_value(RegistryArgsRequest {
        kind: "host".into(),
        key: "host:db.prod".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["kind", "key"]));
}

/// `mcp::BootstrapArgs`. Project is the only argument, and an absent one is absent rather than null.
#[test]
fn bootstrap_arguments_are_a_project_or_an_empty_object() {
    let v = serde_json::to_value(BootstrapArgsRequest::default()).unwrap();
    assert!(keys(&v).is_empty());
    let v = serde_json::to_value(BootstrapArgsRequest { project: Some("/repo".into()) }).unwrap();
    assert_eq!(keys(&v), set(&["project"]));
}

/// `ExportBody` in `src/http/archive.rs`. Plaintext is a field the caller sets, so a request that
/// carries no passphrase and no consent has to reach the server as a refusal rather than as a
/// file.
#[test]
fn an_archive_export_names_plaintext_when_it_sends_no_passphrase() {
    let v = serde_json::to_value(ArchiveExportRequest { passphrase: None, allow_plaintext: true })
        .unwrap();
    assert_eq!(keys(&v), set(&["allow_plaintext"]));
    assert_eq!(v["allow_plaintext"], true);

    let v = serde_json::to_value(ArchiveExportRequest {
        passphrase: Some("correct horse battery staple".into()),
        allow_plaintext: false,
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["passphrase", "allow_plaintext"]));
}

/// `ImportBody` in `src/http/archive.rs`. A missing key here fails at the server's `Deserialize`
/// at runtime rather than at this client's compile time, which is what this pin exists to catch.
/// `restore` is the one worth naming: serde drops a key it does not recognise in silence, so a
/// wrong name here merges an archive the owner asked to restore and says nothing about it.
#[test]
fn an_archive_import_sends_the_bytes_the_restore_flag_and_the_dry_run_flag() {
    let v = serde_json::to_value(ArchiveImportRequest {
        archive_base64: "aGVsbG8=".into(),
        passphrase: None,
        allow_plaintext: true,
        restore: false,
        dry_run: false,
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["archive_base64", "allow_plaintext", "restore", "dry_run"]));
    assert_eq!(v["restore"], false);
}

/// The passphrase travels only when one was read from stdin. `--allow-plaintext` sends the
/// request with the key absent rather than sending `null`, matching every other optional here.
#[test]
fn an_archive_import_carries_the_passphrase_only_when_one_was_given() {
    let v = serde_json::to_value(ArchiveImportRequest {
        archive_base64: "aGVsbG8=".into(),
        passphrase: Some("correct horse battery staple".into()),
        allow_plaintext: false,
        restore: true,
        dry_run: true,
    })
    .unwrap();
    assert_eq!(
        keys(&v),
        set(&["archive_base64", "passphrase", "allow_plaintext", "restore", "dry_run"])
    );
    assert_eq!(v["restore"], true);
    assert_eq!(v["dry_run"], true);
}

/// `services::archive::ApplyReport`, transcribed from the plan rather than from server code this
/// task does not own; see `wire_in` for the fixture this client's build still needs.
#[test]
fn an_apply_report_pairs_a_refusal_with_its_reason() {
    let raw = serde_json::json!({
        "applied": 40,
        "skipped_already_applied": 2,
        "collapsed": 3,
        "refused": [["archive-id-9", "sensitivity_ceiling"]],
    });
    let r: ApplyReport = serde_json::from_value(raw).unwrap();
    assert_eq!(r.applied, 40);
    assert_eq!(r.skipped_already_applied, 2);
    assert_eq!(r.collapsed, 3);
    assert_eq!(r.refused, vec![("archive-id-9".to_string(), "sensitivity_ceiling".to_string())]);
}

/// A response missing every optional counter still parses: a dry run against an empty store is a
/// legitimate report, not a malformed one.
#[test]
fn an_apply_report_parses_with_only_the_required_field() {
    let raw = serde_json::json!({ "applied": 0 });
    let r: ApplyReport = serde_json::from_value(raw).unwrap();
    assert_eq!(r.skipped_already_applied, 0);
    assert!(r.refused.is_empty());
}

// ---- phase 8, the review queue ----

/// `services::review_queue::Queue`, `GET /admin/review/queue`. `sources.proposal` names the
/// origins this server fills; the fixture carries one so a client is never left assuming an
/// engine's own answer of `[]`.
#[test]
fn a_review_queue_parses_and_keeps_the_proposal_origins() {
    let q: ReviewQueue = serde_json::from_value(fixture("review_queue.json")).unwrap();
    assert_eq!(q.items.len(), 3);
    assert_eq!(q.sources.proposal, vec!["jev".to_string()]);
    assert_eq!(q.refused.get("conflict").map(String::as_str), Some("namespace_too_large"));
    assert_eq!(q.dismissed, 12);
    assert!(!q.has_more);
}

/// A server that adds a field to the envelope or to an item must not break an installed client.
#[test]
fn a_review_queue_parses_when_the_server_adds_a_field_this_client_does_not_read() {
    let mut raw = fixture("review_queue.json");
    raw["some_future_field"] = serde_json::json!("added later");
    raw["items"][0]["some_future_item_field"] = serde_json::json!(true);
    let q: ReviewQueue = serde_json::from_value(raw).unwrap();
    assert_eq!(q.items.len(), 3);
}

/// The fixture's second item is stale: no `similarity`, an `age_days`, one row.
#[test]
fn a_review_item_without_similarity_is_a_stale_item() {
    let q: ReviewQueue = serde_json::from_value(fixture("review_queue.json")).unwrap();
    let stale = &q.items[1];
    assert_eq!(stale.source, Source::Stale);
    assert!(stale.similarity.is_none());
    assert_eq!(stale.age_days, Some(400));
    assert_eq!(stale.rows.len(), 1);
}

/// `services::review_queue::ProposalItem`: fields keep the source's own order, and `verdicts`
/// carries what the source, not the engine, says this proposal takes.
#[test]
fn a_proposal_item_keeps_its_fields_in_order_and_its_verdicts() {
    let q: ReviewQueue = serde_json::from_value(fixture("review_queue.json")).unwrap();
    let item = &q.items[2];
    assert_eq!(item.source, Source::Proposal);
    let proposal = item.proposal.as_ref().expect("a proposal item carries a proposal");
    assert_eq!(proposal.origin, "jev");
    assert_eq!(proposal.fields[0].label, "why");
    assert_eq!(proposal.fields[1].label, "confidence");
    assert_eq!(proposal.verdicts, vec![Verdict::Apply, Verdict::Dismiss]);
}

/// `services::review_queue::Decision`, the request body of `POST /admin/review/decide`. Every
/// key the server reads and no other.
#[test]
fn a_decision_request_serialises_exactly_the_keys_the_server_reads() {
    let v = serde_json::to_value(DecisionRequest {
        key: "conflict:1111:2222",
        verdict: Verdict::Supersede,
        keep: Some("2222"),
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["key", "verdict", "keep"]));

    let v = serde_json::to_value(DecisionRequest {
        key: "conflict:1111:2222",
        verdict: Verdict::Merge,
        keep: None,
        id: None,
        content: Some("the merged text"),
        tags: Some(vec!["infra".into()]),
        occurred_at: Some("2026-09-01T00:00:00Z".into()),
        reason: None,
    })
    .unwrap();
    assert_eq!(keys(&v), set(&["key", "verdict", "content", "tags", "occurred_at"]));
}

/// `services::review_queue::Decided`. Every optional field is skipped by the server when empty,
/// so this must parse from just `key` and `verdict`.
#[test]
fn a_decided_body_parses_without_its_optional_fields() {
    let raw = serde_json::json!({ "key": "stale:3333", "verdict": "confirm" });
    let d: Decided = serde_json::from_value(raw).unwrap();
    assert!(d.written.is_none());
    assert!(d.superseded.is_empty());
    assert!(!d.already_dismissed);

    let d: Decided = serde_json::from_value(fixture("review_decide.json")).unwrap();
    assert_eq!(d.verdict, Verdict::Merge);
    assert_eq!(d.written.as_deref(), Some("2b7cabcd-0000-4a1b-8c3d-112233445566"));
    assert_eq!(d.superseded.len(), 2);
}

/// `services::review_queue::DismissedListing`, one entry of `GET /admin/review/dismissed`.
#[test]
fn a_dismissed_pair_carries_both_rows() {
    let p: DismissedPair = serde_json::from_value(fixture("review_dismissed.json")).unwrap();
    assert_eq!(p.dismissed_by, "claude-code");
    assert_eq!(p.rows.len(), 2);
    assert_eq!(p.rows[0].namespace, "user:me");
}
