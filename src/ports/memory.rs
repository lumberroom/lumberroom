//! The memory store. Everything a service needs from it, expressed without naming a database.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::domain::errors::Result;
use crate::domain::policy::{NamespaceCeiling, NamespaceGrant};
use crate::domain::types::{ConflictCandidate, Memory, SearchHit, Sensitivity};

/// One subject's versions, oldest first, with what the caller was not shown.
///
/// The counts exist because a timeline that silently drops a version is a lie in the shape of an
/// answer. A reader can tell "these three versions are all there ever were" from "these three are
/// the ones you may see" only if the store says which it handed over.
///
/// Counts rather than ids or namespaces. A gap has to be visible to be honest, and naming the
/// namespace a withheld version sits in would map the grant the caller was refused.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Timeline {
    /// Oldest first, the anchor's own version among them.
    pub versions: Vec<Memory>,
    /// Versions on this chain the grant does not admit. Zero when the caller may read all of it,
    /// and zero for an empty timeline: an id the caller cannot read reveals nothing, not even a
    /// number.
    pub withheld: i64,
    /// The walk stopped at the depth cap, so the chain may run past one or both ends of
    /// `versions`. A chain exactly as long as the cap trips this too, which is the cheap side of
    /// the trade: the alternative walks one hop further to tell the two apart.
    pub depth_capped: bool,
}

impl Timeline {
    /// The one-row answer for a fact with no chain, and the fallback a caller uses when the walk
    /// returns nothing it may read.
    pub fn single(row: Memory) -> Self {
        Self { versions: vec![row], withheld: 0, depth_capped: false }
    }

    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }
}

/// One row handed out, on its way to `recall_emission`.
#[derive(Debug, Clone)]
pub struct Emission {
    /// `crypto::Digester::digest` of the content as the client received it. The column keeps the
    /// name it was given when the value was a plain SHA-256; it is now an HMAC under a key derived
    /// from the KEK, so a dump holder cannot verify a guess against it.
    pub content_sha256: String,
    pub memory_id: uuid::Uuid,
}

/// A row on the supersession chain next to one about to be deleted: enough to apply the grant and
/// nothing a caller could read content out of. The caller may not be able to read the row at all,
/// which is the reason this is not a `Memory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    pub id: uuid::Uuid,
    pub namespace: String,
    pub sensitivity: Sensitivity,
}

/// Both directions around one row, read in one query.
///
/// `find_by_id` sees the row's own `supersedes` and `superseded_by` and cannot see who points at
/// it, and the rows pointing at it are the ones a delete changes.
#[derive(Debug, Clone, Default)]
pub struct ChainNeighbours {
    /// The row's own links, or `None` when there is no such row.
    pub row: Option<(Option<uuid::Uuid>, Option<uuid::Uuid>)>,
    /// Rows this one retired: `superseded_by` points here.
    pub predecessors: Vec<ChainLink>,
    /// Rows that name this one as the fact they replaced: `supersedes` points here.
    pub successors: Vec<ChainLink>,
}

/// What a delete may do to the chain around the doomed row. Decided by the service, which holds
/// the principal, and applied by the store in the delete's own transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePlan {
    /// Predecessors whose `superseded_by` is cleared, making them live again. Every id here has
    /// passed the caller's read and write grant at its own level.
    pub revive: Vec<uuid::Uuid>,
    /// Where every other predecessor is re-pointed: the doomed row's own successor. `None` when
    /// the doomed row is the head of its chain, in which case any predecessor not in `revive` is a
    /// foreign key the delete will trip over, and tripping is correct.
    pub splice_to: Option<uuid::Uuid>,
}

/// What a delete changed besides the row.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ChainEdits {
    pub revived: Vec<uuid::Uuid>,
    pub spliced: Vec<uuid::Uuid>,
    /// Successors whose `supersedes` now names the doomed row's predecessor, or nothing.
    pub relinked: Vec<uuid::Uuid>,
}

/// What supersession did to the periods it closed, counted over the pairs a caller may read.
///
/// The number 0014 says nobody knows. A supersession that leaves `occurred_until` NULL has written
/// a link and no interval, so an as-of read cannot tell the two versions apart at any instant, and
/// the store answers a question about the past with both facts. Counting them is the only way to
/// find out how much of the store is in that state.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct PairCounts {
    /// Retired rows whose successor the caller may also read.
    pub pairs: i64,
    /// Pairs where the retired row carries an end. These are the ones as-of can order.
    pub closed: i64,
    /// Pairs where the retired row has a start and no end, so it reads as holding at every instant
    /// after its start despite having been replaced. The same-day case lands here.
    pub dated_but_open: i64,
    /// Pairs where both rows carry a start, which is what a supersession needs to be datable at all.
    pub both_dated: i64,
}

impl PairCounts {
    /// The measure, as a fraction. `None` for an empty store rather than a misleading 1.0.
    pub fn closed_fraction(&self) -> Option<f64> {
        (self.pairs > 0).then(|| self.closed as f64 / self.pairs as f64)
    }
}

/// One edge, as a walk sees it: where it came from, where it goes, and why it exists.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GraphEdge {
    pub from_id: uuid::Uuid,
    pub to_id: uuid::Uuid,
    /// One of the closed set the table checks. Never a sentence: a model-written label beside
    /// encrypted content is the plaintext derivative 0005 refuses.
    pub relation: String,
}

/// The bounds a walk runs under. Named rather than passed as three bare integers, because three
/// integers at a call site is how the fan-out cap ends up in the degree slot.
#[derive(Debug, Clone, Copy)]
pub struct WalkBounds {
    /// Neighbours taken per node, per hop.
    pub fan_out: i64,
    /// A node with more readable edges than this is skipped. It connects to everything and
    /// discriminates nothing, and expanding it is unbounded work.
    ///
    /// Counted **inside the caller\'s subgraph**. A global degree would be a function of sealed and
    /// private writes, so a low-privilege client watching an entity\'s neighbours vanish would learn
    /// the volume and timing of writes it may not read, with no row ever crossing the boundary.
    pub degree_cap: i64,
    /// Retired rows are walkable only for a caller holding `may_read_history`. A supersession edge
    /// reaches exactly what `memory_history` refuses, and that door has been opened by a second
    /// spelling once already.
    pub include_retired: bool,
}

/// One row that left the live reads, with what took it out.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Retired {
    pub id: uuid::Uuid,
    pub namespace: String,
    pub content: String,
    pub sensitivity: crate::domain::types::Sensitivity,
    /// When the row left. A supersession's `superseded_at`, or the instant an expiry closed the
    /// period. The two never both apply, because an expiry writes no successor.
    pub retired_at: chrono::DateTime<chrono::Utc>,
    /// The period closed with nothing replacing the fact. The row is neither live nor retired, and
    /// `review::unexpire` brings it back.
    pub expired: bool,
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    pub occurred_until: Option<chrono::DateTime<chrono::Utc>>,
    /// The row was retired and its period never closed, so as-of reads still report it as holding.
    pub end_open: bool,
    /// Absent when the successor was deleted and the chain spliced past it.
    pub successor_id: Option<uuid::Uuid>,
    pub successor_namespace: Option<String>,
}

/// What a supersession did to the retired row's valid period.
///
/// The link is always written. The end is not always datable: a successor that starts on the same
/// day as the fact it replaces would close the period to `[T, T)`, an empty period meaning "never
/// true", so the store leaves the end open instead. That case used to reach the operator as a log
/// line and nobody else, which put the owner one dump apart from a timeline hole they could not
/// see. Both callers now hand it back, so the surface that asked for the supersession can say so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Superseded {
    /// True when the retired row kept an open end, so it reads as still holding at every instant.
    pub end_left_open: bool,
}

#[derive(Debug, Clone)]
pub enum DeleteOutcome {
    /// No row by this id in this tenant. A delete that lost a race lands here too.
    Missing,
    Deleted(ChainEdits),
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub tenant_id: String,
    /// Ranked first. Each namespace carries the ceiling this caller holds for it, so the
    /// sensitivity filter is part of the query rather than a pass over the results: a row a client
    /// may not see must never enter that client's process memory.
    pub primary: Vec<NamespaceCeiling>,
    /// Scanned at a penalty; empty in strict mode.
    pub secondary: Vec<NamespaceCeiling>,
    pub embedding: Vec<f32>,
    /// The raw text, for the lexical arm.
    pub text: String,
    pub limit: i64,
    pub weights: Weights,
    /// Live rows only by default. History stays queryable, which is what makes the decision log a
    /// side effect rather than a feature to build.
    ///
    /// Retired rows come back with their content when this is set, so the caller checks
    /// `Principal::may_read_history` first, the same duty `as_of` carries below. The two fields
    /// open the same door and for a while only one of them was guarded.
    pub include_superseded: bool,
    /// What held at this instant, on the valid-time axis: the `occurred_at` and `occurred_until`
    /// pair, never `created_at`. The other question, what the store believed at an instant, is
    /// transaction time and needs its own name; the spec reserves `believed_at` for it.
    ///
    /// Set, it overrides `include_superseded`. A row retired last week is exactly the row that
    /// answers a question about last month, so an as-of read applies no supersession filter and
    /// `include_superseded` decides nothing the period predicate has not already decided.
    ///
    /// Unset, the search is the one this server has always run, down to the statement text.
    ///
    /// Retired rows reach a caller through this field, so the caller checks
    /// `Principal::may_read_history` before setting it. Nothing below this line can: a repository
    /// holds no principal.
    pub as_of: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub vector: f64,
    pub lexical: f64,
    pub secondary_penalty: f64,
    /// A small recency-and-use boost, capped low enough that it cannot outrank semantic relevance.
    pub usage: f64,
}

#[derive(Debug, Clone)]
pub struct NewMemory {
    pub tenant_id: String,
    /// Chosen by the caller rather than by the database.
    ///
    /// Encryption binds the row id in as associated data, so the id has to exist before the content
    /// is sealed. `None` lets the database generate one, which is the plaintext path.
    pub id: Option<uuid::Uuid>,
    pub namespace: String,
    /// Plaintext. Ignored, and never stored, when `sealed` is present.
    pub content: String,
    /// Computed over the plaintext and stored in the clear even for an encrypted row, because
    /// search has to work. That leaks the gist of a private fact to anyone holding the database,
    /// which is a defended and documented trade, not an oversight
    /// (docs/research/encryption-and-sensitivity.md).
    pub embedding: Vec<f32>,
    pub tags: Vec<String>,
    pub supersedes: Option<uuid::Uuid>,
    pub source_client: String,
    pub embedding_model: String,
    /// Resolved before it reaches here: the namespace default, raised if the caller asked for more.
    pub sensitivity: Sensitivity,
    /// Present when the service encrypted the content. The repository stores these bytes and holds
    /// no key: the service layer owns the KEK, and the store never sees a private row's plaintext.
    ///
    /// `ports` naming a type from `crypto` is deliberate. `SealedContent` is a pure data carrier
    /// with no I/O, and defining a second identical struct here to preserve a layering diagram
    /// would mean a conversion on every write and two places to get the field set wrong.
    pub sealed: Option<crate::crypto::envelope::SealedContent>,
    /// Valid time, when the caller knew it. The write path never guesses one: a date derived from
    /// content rather than stated by the owner is a guess stored as a fact, which is the pattern
    /// the credential tripwire exists to refuse.
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A row reproduced exactly, for restore alone.
///
/// `NewMemory` cannot carry this. `INSERT INTO memory` binds a fixed column list with no
/// `created_at`, `occurred_until`, `access_count`, `last_accessed_at`, `last_confirmed_at` or
/// `superseded_by`, so a restore through the ordinary write path restamps a two-year-old store as
/// learned today.
#[derive(Debug, Clone)]
pub struct RestoreRow {
    pub tenant_id: String,
    /// Always present. A restore that let the database mint an id would break every supersession
    /// link in the archive it is reproducing.
    pub id: uuid::Uuid,
    pub namespace: String,
    /// Plaintext. Ignored, and never stored, when `sealed` is present, which is the contract
    /// `NewMemory` already holds to.
    pub content: String,
    pub embedding: Vec<f32>,
    pub tags: Vec<String>,
    pub source_client: String,
    pub embedding_model: String,
    pub sensitivity: Sensitivity,
    /// Present when the service sealed the content under THIS install's key. Never bytes copied
    /// out of an archive: `envelope::seal` authenticates the row id, so copied ciphertext fails its
    /// tag check even when the id is preserved and the key happens to match.
    pub sealed: Option<crate::crypto::envelope::SealedContent>,
    pub supersedes: Option<uuid::Uuid>,
    pub superseded_by: Option<uuid::Uuid>,
    pub superseded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    pub occurred_until: Option<chrono::DateTime<chrono::Utc>>,
    pub access_count: i32,
    pub last_accessed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_confirmed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Live rows in one namespace near a candidate embedding. Feeds both duplicate collapse and the
/// conflict candidates a write hands back.
#[derive(Debug, Clone)]
pub struct NeighbourQuery {
    pub tenant_id: String,
    pub namespace: String,
    pub embedding: Vec<f32>,
    /// Cosine similarity floor. Below it the row is not worth mentioning.
    pub min_similarity: f64,
    pub limit: i64,
    /// Ceiling for the caller, so a neighbour they may not read is never quoted back at them.
    pub max_sensitivity: Sensitivity,
}

/// One page of stored facts, newest first, for a reading surface.
///
/// Beside `list_for_export` rather than replacing it, because the two contracts agree on nothing.
/// The export is oldest first, offset paged and bounded by `EXPORT_MAX_SENSITIVITY`, so a vault
/// synced to a third party never carries private content. This one is newest first, keyset paged
/// and filtered on both policy axes inside the query, which is what a console page needs and what
/// the export must never gain.
#[derive(Debug, Clone)]
pub struct RecentQuery {
    pub tenant_id: String,
    /// Every namespace the caller may read, with its ceiling. Both axes run in the query, the same
    /// contract every other read path here holds to.
    pub readable: Vec<NamespaceCeiling>,
    /// One namespace, or all of `readable` when absent.
    pub namespace: Option<String>,
    /// Keyset cursor: rows strictly older than this `(created_at, id)`. Keyset rather than an
    /// offset because a write landing mid-read shifts every later offset by one and the reader
    /// sees a row twice or not at all.
    pub before: Option<(chrono::DateTime<chrono::Utc>, uuid::Uuid)>,
    pub limit: i64,
    /// Retired rows come back alongside the live ones, so a correction reads as a revision in
    /// place rather than as a row that vanished.
    pub include_superseded: bool,
}

/// What one namespace holds, filtered on both axes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NamespaceSummary {
    pub namespace: String,
    pub live: i64,
    pub retired: i64,
    /// Live rows above `open`. The size of a namespace and its exposure are different questions.
    pub above_open: i64,
    pub last_write: Option<chrono::DateTime<chrono::Utc>>,
}

/// Rows keyed to one namespace in one table.
///
/// Per table because the remediation is per table: eight of them carry a `namespace` column and an
/// operator who moves only `memory` leaves the registry, the aliases and the ingest queue behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceRows {
    pub namespace: String,
    /// The table name as `migrations/` spells it.
    pub table: String,
    pub rows: i64,
}

#[derive(Debug, Clone)]
pub struct DigestQuery {
    pub tenant_id: String,
    pub user_namespace: String,
    pub project_namespace: Option<String>,
    /// Everything the caller may read, with its ceiling. Every subquery intersects with this;
    /// Phase 1 shipped a bug where the profile and project subqueries skipped the namespace filter,
    /// and the leak path in a memory system is the convenience surface, not the obvious one.
    pub readable: Vec<NamespaceCeiling>,
    pub profile_limit: i64,
    pub project_limit: i64,
    pub recent_limit: i64,
    pub registry_limit: i64,
    pub recent_days: i32,
}

#[derive(Debug, Clone, Default)]
pub struct DigestData {
    pub profile: Vec<Memory>,
    pub project_context: Vec<Memory>,
    pub recent: Vec<Memory>,
    pub registry: Vec<RegistrySummary>,
    pub memories_count: i64,
    pub registry_count: i64,
    pub by_namespace: HashMap<String, i64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegistrySummary {
    pub namespace: String,
    pub kind: String,
    pub key: String,
    pub value: serde_json::Value,
}

/// Two live rows in one namespace close enough that one probably should have retired the other.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConflictPair {
    pub older: ConflictCandidate,
    pub newer: ConflictCandidate,
    pub similarity: f64,
}

/// The three numbers that say whether the store is decaying: a store read often and written rarely
/// is decaying, and one never read at all is dead weight (system PRD §8).
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct Staleness {
    pub live_rows: i64,
    pub never_retrieved: i64,
    pub never_retrieved_pct: f64,
    pub median_age_days_retrieved: Option<f64>,
    pub superseded_rows: i64,
    pub oldest_never_retrieved_days: Option<f64>,
}

#[async_trait]
pub trait MemoryRepository: Send + Sync {
    async fn search(&self, q: SearchQuery) -> Result<Vec<SearchHit>>;
    async fn insert(&self, m: NewMemory) -> Result<Memory>;

    /// Exact-content match, used for duplicate collapse. Only reaches plaintext rows: an encrypted
    /// row has no content column to compare, so private duplicates are caught by similarity alone.
    async fn find_exact(
        &self,
        tenant: &str,
        namespace: &str,
        content: &str,
    ) -> Result<Option<Memory>>;

    async fn find_by_id(&self, tenant: &str, id: uuid::Uuid) -> Result<Option<Memory>>;

    /// Many rows by id, in one query. Ids that do not exist are absent rather than an error.
    ///
    /// A walk reaches thousands of nodes at the documented bounds, and one `find_by_id` each would
    /// be thousands of serial round trips inside one request. No grant filter here: the caller
    /// decided reachability, and every id reaching this point came out of a query that already
    /// applied both axes to both endpoints.
    async fn find_many(&self, tenant: &str, ids: &[uuid::Uuid]) -> Result<Vec<Memory>>;

    /// One round trip. The bootstrap latency budget depends on it staying that way.
    async fn digest(&self, q: DigestQuery) -> Result<DigestData>;

    /// Namespace to row count, **pre-policy**: no grant and no ceiling are applied to either half.
    ///
    /// The one read here that takes no policy argument, and it stays that way because a glob grant
    /// resolves against concrete names. Nothing can ask for "the namespaces I may read" until
    /// something has listed the namespaces that exist, so discovery runs first and the grant runs
    /// over its result. Giving this method a ceiling would make it two queries or move glob
    /// matching into SQL, and policy in the adapter is worse than a documented contract here.
    ///
    /// **The contract: neither the counts nor the names may reach a response as they stand.**
    ///
    /// The counts include rows above the caller's ceiling. The digest's `by_namespace` arm is the
    /// filtered count and is what an inventory is built from; a digest that took its inventory from
    /// here told a client granted `*` at open that `personal:finance` holds one row, refusing the
    /// content and publishing the name and the number.
    ///
    /// The names are safe to feed `filter_readable` and nothing else. Surviving that call means the
    /// namespace axis admits the name, which is not the same as the caller being able to read
    /// anything there: a ceiling of open over a namespace holding private rows publishes a name the
    /// second axis refused. Publish a name only once a both-axes count has put a row behind it.
    async fn namespace_counts(&self, tenant: &str) -> Result<HashMap<String, i64>>;

    /// Every row under a `user:` namespace, per table, superseded rows included.
    ///
    /// Deliberately not `namespace_counts`. That one answers "which namespaces exist for glob
    /// resolution" and answers it for live memory rows plus registry names, which is the wrong
    /// question for an upgrade guard: a namespace holding only retired rows never appears there at
    /// all, and a registry-only namespace appears with a count of zero. Both are stranded, and both
    /// boot clean if the guard reads that map.
    ///
    /// Same contract as `namespace_counts` on what may be done with the result: pre-policy, for the
    /// operator's log and never for a client response.
    async fn user_namespace_rows(&self, tenant: &str) -> Result<Vec<NamespaceRows>>;

    /// One page of facts, newest first, filtered on both axes inside the query.
    ///
    /// Takes the ceilings for the reason `namespace_counts` does not: this one returns content, so
    /// the grant has to be part of the plan rather than a pass over the rows.
    async fn recent(&self, q: RecentQuery) -> Result<Vec<Memory>>;

    /// Rows retired inside a window, newest retirement first.
    ///
    /// Ordered on when the row was retired, not on when it was written, because those are different
    /// questions and `recent` already answers the second. A wrong supersession is otherwise found by
    /// missing a fact, which is the one way this store loses something quietly.
    async fn retired_since(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
        since: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<Vec<Retired>>;

    /// Per-namespace counts and the last write, for a reader deciding where to look.
    ///
    /// Both axes, so a namespace whose rows all sit above the caller's ceiling is absent rather
    /// than present at zero. An entry at zero is the same disclosure with a smaller number on it.
    async fn namespace_summary(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
    ) -> Result<Vec<NamespaceSummary>>;

    /// Random sample of stored content the caller may read, for the recall monitor.
    ///
    /// Takes the ceilings because the sample reaches a response: the report quotes the opening
    /// characters of every probe it measured, and `/admin/recall` asks for authentication and
    /// nothing else. Sampling the whole tenant read open rows out of namespaces the caller's grant
    /// never reached, to any client that could hold a token.
    async fn sample_content(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
        n: i64,
    ) -> Result<Vec<String>>;

    /// Nearest ids by embedding. `exact` forces a sequential scan for ground truth.
    async fn nearest_ids(
        &self,
        tenant: &str,
        embedding: &[f32],
        k: i64,
        exact: bool,
    ) -> Result<Vec<String>>;

    /// Live rows near this embedding, for dedupe bands and conflict candidates.
    async fn neighbours(&self, q: NeighbourQuery) -> Result<Vec<ConflictCandidate>>;

    /// Retire `old` in favour of `new`, writing the link on both rows in one transaction.
    ///
    /// Must reject a target that is already superseded, and must reject a cycle: a two-row cycle
    /// makes both rows invisible, which is data loss dressed as a correction.
    async fn supersede(&self, tenant: &str, old: uuid::Uuid, new: uuid::Uuid)
        -> Result<Superseded>;

    /// Rebuild edges from structure the store already holds, and report how many exist after.
    ///
    /// Three seeders, none of which calls a model. A supersession link is an edge. Two rows whose
    /// names the alias table says denote one subject are an edge. Two rows sharing a tag the owner
    /// curated are an edge. 0014 assumed entity extraction and warned the graph has to earn that
    /// cost; these cost nothing, so they go first and the extractor waits for evidence that they
    /// were not enough.
    ///
    /// Idempotent. Running it twice writes nothing the second time, which is what makes it safe on
    /// a schedule.
    async fn rebuild_edges(&self, tenant: &str) -> Result<i64>;

    /// One hop out from `from`, inside the caller\'s subgraph.
    ///
    /// Every edge returned has **both** endpoints readable under `grants`. A node the caller may not
    /// read is not a node it may walk through: passing through a forbidden node while withholding
    /// its content would leak the existence of a fact and the shape around it, and edge count and
    /// path length are facts no content filter hides.
    ///
    /// This severs, which reverses what `subject_history` does on purpose. That statement filters
    /// after the recursion so a chain stays whole across a row it must withhold, and it can report
    /// the gap as a bare count because a chain has one subject and a readable anchor. A graph has
    /// neither past hop one: its shape is the answer, so describing the gap is the answer leaking.
    /// No count and no flag comes back from here for that reason.
    async fn graph_neighbours(
        &self,
        tenant: &str,
        grants: &[NamespaceGrant],
        from: &[uuid::Uuid],
        bounds: WalkBounds,
    ) -> Result<Vec<GraphEdge>>;

    /// Count what supersession did to the periods it closed, on both grant axes.
    async fn pair_counts(&self, tenant: &str, grants: &[NamespaceGrant]) -> Result<PairCounts>;

    /// Live rows carrying no start date, newest first, for the date review.
    ///
    /// Both grant axes, like every other read. The review reads content to look for a day the fact
    /// states about itself, so a row the caller may not read must not reach that scan.
    async fn undated(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
        limit: i64,
    ) -> Result<Vec<Memory>>;

    /// Fill a start date that was never recorded. Fills only where `occurred_at` is NULL, and
    /// answers false when the row already carries one.
    ///
    /// Fill, never change. A supersession ends a period and never moves its start, and until this
    /// landed nothing in the store could write `occurred_at` after the insert at all. That was not
    /// an oversight so much as an unexamined consequence: the near-now fence refused a same-day
    /// date, so a fact recorded the day it happened lost its date permanently with no way back.
    /// Filling a NULL adds a fact the row never held; overwriting one would rewrite history, so the
    /// statement refuses it rather than trusting the caller.
    async fn fill_occurred_at(
        &self,
        tenant: &str,
        id: uuid::Uuid,
        when: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool>;

    /// Close a live row's validity with no successor, and report the instant written.
    ///
    /// The third thing that can happen to a row, beside a confirmation and a supersession, and the
    /// first that retires one with nothing to retire it into. `None` says the statement moved
    /// nothing: the row is already retired, already closed, or gone.
    ///
    /// Must refuse a row a successor already retired. That row's end belongs to the supersession
    /// that wrote it.
    async fn expire(
        &self,
        tenant: &str,
        id: uuid::Uuid,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>>;

    /// Reopen a row this instant closed.
    ///
    /// Guarded on the instant, so a close somebody else wrote is not this caller's to undo. A
    /// `false` return means the statement moved nothing and the row stands as it was.
    async fn unexpire(
        &self,
        tenant: &str,
        id: uuid::Uuid,
        until: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool>;

    /// Walk the chain to the row that is live now, so a rejection can name the current head.
    async fn supersession_head(&self, tenant: &str, id: uuid::Uuid) -> Result<Option<Memory>>;

    /// One subject's whole supersession chain, oldest first, each row carrying its valid period.
    ///
    /// This is what reads back as "the port was 8080, and since 20 August it is 8787". A search
    /// answers what holds now and an as-of search answers what held at one instant; neither can
    /// show the sequence, because the live filter hides every row but the last and an as-of read
    /// returns one slice.
    ///
    /// `id` may sit anywhere in the chain. The walk runs both ways from it, so a caller holding the
    /// first version and a caller holding the current one get the same timeline.
    ///
    /// Retired rows are the entire content of the answer, so the caller checks
    /// `Principal::may_read_history` before asking.
    ///
    /// **The whole grant arrives, globs and ceilings, rather than one namespace and one ceiling.**
    /// A supersession may retire a row in one namespace in favour of a row in another, so the
    /// namespaces on a chain are unknown until the walk has run and cannot be resolved to concrete
    /// ceilings in advance. Scoping the walk to a single namespace stopped it at the first
    /// boundary and reported a short history as a complete one. The walk now crosses, and every
    /// row is admitted by the same rule `policy::admits` states: some granted pattern matches the
    /// row's namespace and its ceiling reaches the row's level. Both axes still run inside the
    /// query, so a version this caller may not read never enters this process.
    ///
    /// An id the caller may not read gives an empty timeline with no gap reported, which is what
    /// `search` answers about the same row. Learning that a fact exists is the disclosure this
    /// refuses.
    async fn subject_history(
        &self,
        tenant: &str,
        grants: &[NamespaceGrant],
        id: uuid::Uuid,
    ) -> Result<Timeline>;

    /// Record that these rows were actually returned in a result. Fire and forget and batched by
    /// contract: a search must not turn into a write storm, and it must not pay for this.
    fn touch_accessed(&self, tenant: &str, ids: Vec<uuid::Uuid>);

    /// Record what the store handed a client, so content it emitted cannot come back to it as a
    /// new fact.
    ///
    /// Same contract as `touch_accessed`: batched, fire and forget, a failure is a log line. A read
    /// that waited on this or failed because of it would trade the failure this layer exists to
    /// catch for a worse one.
    ///
    /// The hash arrives already computed, and that is the point. It has to be
    /// `crypto::Digester::digest` of the same content, because a second normaliser gives the echo
    /// check a hash that can never meet a proposal's, which is how the earlier version of this
    /// layer was built unreachable. The service also decides which rows are recorded at all:
    /// encrypted rows are not, and a repository never sees their plaintext to hash.
    fn record_emissions(
        &self,
        tenant: &str,
        tool: &'static str,
        session_id: Option<String>,
        rows: Vec<Emission>,
    );

    /// Repetition is confirmation. Set when a write restates a fact rather than contradicting it.
    async fn confirm(&self, tenant: &str, id: uuid::Uuid) -> Result<()>;

    /// The chain around one row, for the service to plan a delete against. Ids and levels only.
    async fn chain_neighbours(&self, tenant: &str, id: uuid::Uuid) -> Result<ChainNeighbours>;

    /// Hard delete, with the chain edits in `plan` applied in the same transaction. For a private
    /// row the wrapped DEK goes with it, so ciphertext in any older backup is already unreadable.
    ///
    /// A predecessor that is neither in `revive` nor covered by `splice_to` is a foreign key the
    /// row cannot be deleted under. The store reports that as a `Conflict` and changes nothing,
    /// which is what a neighbour that appeared between `chain_neighbours` and this call should
    /// produce. Every other foreign key into the row was made `ON DELETE SET NULL` or `CASCADE` by
    /// the migrations, and the store maps any that still refuses onto the same `Conflict`.
    async fn delete(
        &self,
        tenant: &str,
        id: uuid::Uuid,
        plan: &DeletePlan,
    ) -> Result<DeleteOutcome>;

    /// Live rows older than a threshold that were never retrieved. The review queue, not a reaper.
    /// Live, never-accessed rows older than `older_than_days`, oldest first.
    ///
    /// `reader` is a term of the query rather than a pass over the rows, so `limit` counts only what
    /// the caller may see. Filtering afterwards returns a short page and calls it full, and it puts
    /// rows the caller may not read into that caller's process on the way.
    async fn stale(
        &self,
        tenant: &str,
        older_than_days: i32,
        limit: i64,
        reader: &[NamespaceGrant],
    ) -> Result<Vec<Memory>>;

    async fn staleness(&self, tenant: &str) -> Result<Staleness>;

    /// Near-duplicate live pairs, for `lumberroom review`. Computed on demand rather than recorded at
    /// write time: a stored queue drifts out of step with the store it describes, and this runs by
    /// hand rather than on the hot path.
    async fn conflicts(
        &self,
        tenant: &str,
        min_similarity: f64,
        limit: i64,
    ) -> Result<Vec<ConflictPair>>;

    /// The ciphertext columns for rows the caller already holds, so the service can decrypt them.
    ///
    /// Batched, because the shape that needs it is a search result rather than a single row.
    /// Deliberately separate from the read paths: one that returned plaintext and ciphertext
    /// together would make it possible to hand a caller ciphertext in a text field by mistake.
    ///
    /// The third element is the id of the key that wrapped the row's DEK. A row wrapped by a key
    /// this deployment no longer holds must fail to open rather than appear empty.
    async fn sealed_batch(
        &self,
        tenant: &str,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, crate::crypto::envelope::SealedContent, Option<String>)>>;

    async fn sealed_one(
        &self,
        tenant: &str,
        id: uuid::Uuid,
    ) -> Result<Option<(crate::crypto::envelope::SealedContent, Option<String>)>>;

    /// Everything the export may see, oldest first. Bounded by sensitivity, because private content
    /// in a vault synced to a third party defeats the encryption it was given.
    async fn list_for_export(
        &self,
        tenant: &str,
        max_sensitivity: Sensitivity,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Memory>>;

    /// Every row this tenant holds, live and retired, keyset paged by id.
    ///
    /// Carries no grant and no ceiling, and that is the design rather than an omission.
    /// `services::archive::build` refuses a caller who does not pass `reads_whole_store` before it
    /// reaches this method, so filtering here would turn a refusal into a partial archive that
    /// looks like a whole one. `list_for_export` is the other shape and stays: it is bounded by
    /// `EXPORT_MAX_SENSITIVITY`, which defaults to open, and an archive built through it would
    /// carry no private rows while claiming to be a copy of the store.
    async fn list_whole_store(
        &self,
        tenant: &str,
        limit: i64,
        after: Option<uuid::Uuid>,
    ) -> Result<Vec<Memory>>;

    /// Insert a row as recorded, timestamps and counters included.
    ///
    /// The only insert path besides `insert`, and the last one this codebase should gain.
    /// `services::archive::apply` in
    /// restore mode is its sole caller, it refuses a target holding any memory row, and the checks
    /// it keeps from the write path are enumerated in the archive spec. Adding a caller means
    /// re-reading that table.
    async fn restore_row(&self, row: RestoreRow) -> Result<()>;

    /// Point one restored row at its neighbours on the supersession chain, once every row has
    /// landed.
    ///
    /// `supersedes` and `superseded_by` are both foreign keys into this table and neither is
    /// deferrable, so a restore that binds them on the insert can only succeed when every target
    /// already exists. `superseded_by` names a newer row, and a restore walking the archive in id
    /// order reaches the newer row second, so that direction fails on any store holding a single
    /// correction. Restore therefore inserts with both links empty and writes them here on a second
    /// pass. Two `None`s do nothing, so a caller can hand over every row without sorting them first.
    async fn relink_restored(
        &self,
        tenant: &str,
        id: uuid::Uuid,
        supersedes: Option<uuid::Uuid>,
        superseded_by: Option<uuid::Uuid>,
    ) -> Result<()>;
}
