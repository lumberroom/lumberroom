//! The request and response shapes the server publishes, redeclared here.
//!
//! This crate does not depend on the server crate, which is the point: the client ships to a
//! machine that runs no database and no embedding runtime. The cost is a second copy of the
//! contract, and `tests/wire.rs` is what stops the two drifting. Every type below names the server
//! file and symbol it mirrors, so the pin test's fixtures have somewhere to be checked against.
//!
//! Two rules hold the shape. Responses type only the fields this client prints or branches on and
//! let serde ignore the rest, so a server that adds a field does not break an installed client.
//! Requests are exact: a missing key on a request fails at runtime against `Deserialize`, never at
//! compile time, so the pin test asserts the serialized key set.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---- responses ----

/// `GET /readyz`, from `readyz` in `src/http/mod.rs`.
#[derive(Debug, Deserialize)]
pub struct Ready {
    pub ok: bool,
    pub auth_mode: Option<String>,
}

/// `GET /admin/whoami`, from `whoami` in `src/http/mod.rs`.
#[derive(Debug, Deserialize)]
pub struct Whoami {
    pub client: String,
    pub mode: String,
}

/// `memory_search`'s structured content, from `services::search::SearchResult`.
#[derive(Debug, Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub hits: Vec<Hit>,
}

/// `services::search::Hit`.
#[derive(Debug, Deserialize)]
pub struct Hit {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub score: f64,
}

/// `memory_write`'s structured content, from `domain::types::WriteOutcome`.
///
/// `superseded` and `possible_conflicts` are skipped when empty on the server side, so both carry
/// `#[serde(default)]` here rather than being optional by accident.
#[derive(Debug, Deserialize)]
pub struct WriteOutcome {
    pub id: String,
    pub namespace: String,
    pub deduplicated: bool,
}

/// `domain::types::Memory`, as `GET /admin/memory/{id}` and the export and stale pages return it.
///
/// `content` is empty rather than absent for a row the caller may hold but not open, which is why
/// the Obsidian writer substitutes a placeholder instead of treating it as a bug.
#[derive(Debug, Deserialize)]
pub struct Memory {
    pub id: String,
    pub namespace: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_client: String,
    pub sensitivity: String,
    pub created_at: String,
}

/// `POST /admin/currency`.
#[derive(Debug, Deserialize)]
pub struct CurrencyReport {
    pub coverage: PairCounts,
    #[serde(default)]
    pub closed_fraction: Option<f64>,
    #[serde(default)]
    pub accuracy: Option<f64>,
    #[serde(default)]
    pub returned_both: usize,
    #[serde(default)]
    pub cases: Vec<CurrencyCaseOutcome>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PairCounts {
    #[serde(default)]
    pub pairs: i64,
    #[serde(default)]
    pub closed: i64,
    #[serde(default)]
    pub dated_but_open: i64,
    #[serde(default)]
    pub both_dated: i64,
}

#[derive(Debug, Deserialize)]
pub struct CurrencyCaseOutcome {
    pub question: String,
    pub as_of: String,
    pub found: bool,
    pub also_returned_the_other: bool,
    #[serde(default)]
    pub rank: Option<usize>,
}

/// `GET /admin/review/dates`.
#[derive(Debug, Deserialize)]
pub struct DateReview {
    #[serde(default)]
    pub rows: Vec<DateCandidate>,
}

#[derive(Debug, Deserialize)]
pub struct DateCandidate {
    pub id: String,
    pub namespace: String,
    pub content: String,
    #[serde(default)]
    pub proposed: Option<String>,
    #[serde(default)]
    pub ambiguous: Vec<String>,
}

/// `GET /admin/review/stale`.
#[derive(Debug, Deserialize)]
pub struct StaleReview {
    #[serde(default)]
    pub rows: Vec<Memory>,
}

/// `GET /admin/review/conflicts`.
#[derive(Debug, Deserialize)]
pub struct ConflictReview {
    #[serde(default)]
    pub pairs: Vec<ConflictPair>,
}

#[derive(Debug, Deserialize)]
pub struct ConflictPair {
    pub similarity: f64,
    pub older: ConflictSide,
    pub newer: ConflictSide,
}

/// `conflict_side` in `src/http/mod.rs`, which is deliberately narrower than a `Memory`.
#[derive(Debug, Deserialize)]
pub struct ConflictSide {
    pub id: String,
    pub namespace: String,
    pub content: String,
}

/// `GET /admin/review/registry`, whose two lists are `domain::types::RegistryEntry`.
#[derive(Debug, Deserialize)]
pub struct RegistryReview {
    #[serde(default)]
    pub due_for_review: Vec<RegistryEntryRef>,
    #[serde(default)]
    pub non_canonical: Vec<RegistryEntryRef>,
}

#[derive(Debug, Deserialize)]
pub struct RegistryEntryRef {
    pub namespace: String,
    pub kind: String,
    pub key: String,
}

/// `GET /admin/export`, one page.
#[derive(Debug, Deserialize)]
pub struct ExportPage {
    #[serde(default)]
    pub rows: Vec<Memory>,
}

/// `GET /statsz`, the per-tool shape, from `tool_stats_body`.
#[derive(Debug, Deserialize)]
pub struct ToolStats {
    pub window_hours: i64,
    pub totals: StatsTotals,
    #[serde(default)]
    pub by_tool: Vec<ToolStatsRow>,
}

#[derive(Debug, Deserialize)]
pub struct StatsTotals {
    pub calls: i64,
    pub failures: i64,
    pub unprompted: i64,
    pub unprompted_rate: Option<f64>,
}

/// `ports::tool_calls::ToolCallStats`.
#[derive(Debug, Deserialize)]
pub struct ToolStatsRow {
    pub tool: String,
    pub client: String,
    pub calls: i64,
    pub unprompted: i64,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
}

/// `GET /statsz?by=client`, from `client_stats_body`.
#[derive(Debug, Deserialize)]
pub struct ClientStats {
    pub window_hours: i64,
    #[serde(default)]
    pub by_client: Vec<ClientStatsRow>,
}

/// `ports::tool_calls::ClientStats`.
#[derive(Debug, Deserialize)]
pub struct ClientStatsRow {
    pub client: String,
    pub calls: i64,
    pub reads: i64,
    pub writes: i64,
    pub write_to_read_ratio: Option<f64>,
    pub unprompted_write_rate: Option<f64>,
}

/// `GET /oauth/clients`, from `clients` in `src/authserver/routes.rs`.
#[derive(Debug, Deserialize)]
pub struct ClientList {
    #[serde(default)]
    pub clients: Vec<ClientRecord>,
}

#[derive(Debug, Deserialize)]
pub struct ClientRecord {
    pub client_id: String,
    pub client_name: String,
    pub registered_via: String,
    pub consented_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// `POST /oauth/register`, RFC 7591.
#[derive(Debug, Deserialize)]
pub struct RegistrationResponse {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// `POST /oauth/token`, RFC 6749 §5.1.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_in: Option<i64>,
}

/// `GET /admin/memory/{id}/history`, a route this crate assumes rather than one it has confirmed
/// against a server implementation; see `wire_in` in the phase-7 handoff for the exact shape to
/// reconcile. Wire order is not trusted: `format::order_chain` walks `superseded_by` itself, since
/// S4 lets a later approval state an earlier fact and the two orders can disagree.
#[derive(Debug, Deserialize)]
pub struct HistoryChain {
    #[serde(default)]
    pub entries: Vec<HistoryEntry>,
}

/// One row of a chain, carrying the valid-time and supersession columns off
/// `domain::types::Memory`. `occurred_at`/`occurred_until` follow phase 7's half-open convention:
/// a `None` start means the fact reads as always having held, a `None` end means it still does.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub occurred_at: Option<String>,
    #[serde(default)]
    pub occurred_until: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub superseded_at: Option<String>,
    pub created_at: String,
}

/// One name for an entity, from `ports::alias::Alias`. `since`/`until` carry the same half-open
/// convention valid time does on a memory.
#[derive(Debug, Clone, Deserialize)]
pub struct AliasRecord {
    pub namespace: String,
    pub alias: String,
    pub canonical: String,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    pub origin: String,
    pub created_at: String,
}

/// `GET /admin/alias`, assumed shape.
#[derive(Debug, Deserialize)]
pub struct AliasList {
    #[serde(default)]
    pub aliases: Vec<AliasRecord>,
}

/// `DELETE /admin/alias/{name}`, assumed shape.
#[derive(Debug, Deserialize)]
pub struct AliasForgetResponse {
    pub forgotten: bool,
}

/// `services::review_queue::Source`, the `source` list on `GET /admin/review/queue` and the `key`
/// prefix on `POST /admin/review/decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Conflict,
    Stale,
    Proposal,
}

/// `services::review_queue::Verdict`, the key line's answers and `DecisionRequest::verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Supersede,
    Merge,
    KeepBoth,
    Delete,
    Confirm,
    Apply,
    Dismiss,
}

/// `services::review_queue::QueueRow`. The loop prints a row in full, so every field it carries is
/// read somewhere: `opened` decides whether content is safe to trust, `access_count` and the two
/// timestamps are the "read Nx" line, `sensitivity` marks a private row in the header.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewRow {
    pub id: String,
    pub namespace: String,
    pub sensitivity: String,
    pub content: String,
    pub opened: bool,
    pub created_at: String,
    #[serde(default)]
    pub occurred_at: Option<String>,
    pub access_count: i32,
    #[serde(default)]
    pub last_accessed_at: Option<String>,
    #[serde(default)]
    pub last_confirmed_at: Option<String>,
}

/// `services::review_queue::ProposalField`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewProposalField {
    pub label: String,
    pub value: String,
}

/// `services::review_queue::ProposalItem`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewProposal {
    pub id: String,
    pub origin: String,
    pub kind: String,
    #[serde(default)]
    pub proposed_content: Option<String>,
    #[serde(default)]
    pub fields: Vec<ReviewProposalField>,
    pub created_at: String,
    pub verdicts: Vec<Verdict>,
}

/// `services::review_queue::QueueItem`. `key` is the address a decision names; the loop draws its
/// key line from `verdicts` and nothing else, per the CLI's own rule.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewItem {
    pub key: String,
    pub source: Source,
    pub namespace: String,
    #[serde(default)]
    pub rows: Vec<ReviewRow>,
    #[serde(default)]
    pub similarity: Option<f64>,
    #[serde(default)]
    pub age_days: Option<i64>,
    #[serde(default)]
    pub proposal: Option<ReviewProposal>,
    #[serde(default)]
    pub verdicts: Vec<Verdict>,
}

/// `services::review_queue::Sources`. `proposal` lists the origins a server fills; an engine
/// answers `[]`, which is what makes `--source proposal` a `source_not_filled` refusal rather than
/// an empty page.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReviewSources {
    pub conflict: bool,
    pub stale: bool,
    #[serde(default)]
    pub proposal: Vec<String>,
}

/// `GET /admin/review/queue`'s envelope, mirroring `services::review_queue::Queue`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewQueue {
    #[serde(default)]
    pub items: Vec<ReviewItem>,
    pub sources: ReviewSources,
    #[serde(default)]
    pub refused: std::collections::BTreeMap<String, String>,
    pub dismissed: i64,
    pub stale_days: i32,
    pub min_similarity: f64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

/// `POST /admin/review/decide`'s response, mirroring `services::review_queue::Decided`. The loop's
/// one-line answer branches on every field here.
#[derive(Debug, Clone, Deserialize)]
pub struct Decided {
    pub key: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub written: Option<String>,
    #[serde(default)]
    pub superseded: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
    #[serde(default)]
    pub end_left_open: bool,
    #[serde(default)]
    pub unfinished: Vec<String>,
    #[serde(default)]
    pub already_dismissed: bool,
    #[serde(default)]
    pub proposal_state: Option<String>,
}

/// `GET /admin/review/dismissed`'s per-pair listing, mirroring
/// `services::review_queue::DismissedListing`.
#[derive(Debug, Clone, Deserialize)]
pub struct DismissedPair {
    pub lo_id: String,
    pub hi_id: String,
    pub dismissed_by: String,
    pub dismissed_token: String,
    pub dismissed_at: String,
    #[serde(default)]
    pub rows: Vec<ReviewRow>,
}

/// `services::review_queue::Decision`, the request body of `POST /admin/review/decide`. Every key
/// the server reads and no other; a paraphrased field there is a runtime failure, never a compile
/// one, which is the whole reason `tests/wire.rs` pins the serialized key set.
#[derive(Debug, Serialize)]
pub struct DecisionRequest<'a> {
    pub key: &'a str,
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
}

/// `POST /admin/archive/import`'s response, mirroring `services::archive::ApplyReport`.
///
/// `id_map` is left out: a single CLI request has nothing to resume between runs, so nothing this
/// client does reads it. `refused` keeps the reason beside the id, because that pairing is the one
/// thing an owner reading a report acts on.
#[derive(Debug, Deserialize)]
pub struct ApplyReport {
    pub applied: usize,
    #[serde(default)]
    pub skipped_already_applied: usize,
    #[serde(default)]
    pub collapsed: usize,
    #[serde(default)]
    pub refused: Vec<(String, String)>,
}

// ---- requests ----

/// `RegistryWrite` in `src/http/mod.rs`. `sensitivity` is optional there and this client does not
/// send it, so it is skipped rather than sent as null: the field is `Option<String>` behind
/// `#[serde(default)]` and a null would parse, but sending a key nobody asked for invites a
/// deny-unknown-fields handler later.
#[derive(Debug, Serialize)]
pub struct RegistryWriteRequest<'a> {
    pub namespace: &'a str,
    pub kind: &'a str,
    pub key: &'a str,
    pub value: Value,
}

/// `SupersedeBody` in `src/http/mod.rs`.
#[derive(Debug, Serialize)]
pub struct SupersedeRequest<'a> {
    pub new_id: &'a str,
}

/// `mcp::SearchArgs`. Absent fields keep the server's defaults, so every optional is skipped.
///
/// `as_of` is not confirmed against a server field yet: phase 7 section 9 reads as deferring it to
/// phase 2, and this crate was told Track A is landing it anyway. Coded to the name `as_of` per
/// instruction; see `wire_in` in the phase-7 handoff.
#[derive(Debug, Default, Serialize)]
pub struct SearchArgsRequest {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespaces: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// An RFC 3339 UTC instant, sent regardless of which of the two accepted forms the owner
    /// typed, so the server never has to guess which midnight a bare date meant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
}

/// `mcp::WriteArgs`.
#[derive(Debug, Default, Serialize)]
pub struct WriteArgsRequest {
    pub content: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// An RFC 3339 UTC instant. `commands::write` parses both accepted forms locally and always
    /// sends this shape, so the server's own `parse_occurred_at` sees only the form it produces
    /// from a date-only input itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
}

/// `POST /admin/alias`, assumed shape mirroring `AliasWrite` in `src/http/mod.rs` for the
/// registry-key alias. Origin is not a field here: the server fixes it to `manual` the way
/// `admin_registry_alias` fixes its own origin, and a client able to choose it could promote a
/// derived guess to a decision.
#[derive(Debug, Serialize)]
pub struct AliasSetRequest<'a> {
    pub namespace: &'a str,
    pub alias: &'a str,
    pub canonical: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// `mcp::RegistryArgs`.
#[derive(Debug, Default, Serialize)]
pub struct RegistryArgsRequest {
    pub kind: String,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `mcp::BootstrapArgs`.
#[derive(Debug, Default, Serialize)]
pub struct BootstrapArgsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `ExportBody` in `src/http/archive.rs`. The passphrase travels in the body on this route too,
/// including on the GET the server also accepts: a query string reaches the access log of every
/// proxy in front of the server and a header reaches most of them, and this one value opens every
/// private fact in the store.
///
/// No `Debug`, here or on `ArchiveImportRequest`, matching the server structs these mirror. Both
/// hold the passphrase, and a derived formatter is how one reaches a log line somebody adds later.
#[derive(Serialize)]
pub struct ArchiveExportRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// The caller saying out loud that it wants a file anyone holding it can read. Absent it, the
    /// server refuses rather than writing plaintext.
    pub allow_plaintext: bool,
}

/// `ImportBody` in `src/http/archive.rs`. The bytes travel base64-encoded inside JSON rather than
/// as a raw body, because the passphrase has to reach the same handler that opens them and a
/// second transport for one string invites a disagreement with the route.
///
/// `restore` and `allow_plaintext` ride on every request rather than resting on the server's
/// default. Serde drops a key it does not recognise without a word, so a client that gets either
/// name wrong merges when the owner asked for an exact reproduction, and that failure is silent.
///
/// `prior_id_map` is left out: one CLI run has nothing to resume from a previous one.
#[derive(Serialize)]
pub struct ArchiveImportRequest {
    pub archive_base64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    pub allow_plaintext: bool,
    pub restore: bool,
    pub dry_run: bool,
}
