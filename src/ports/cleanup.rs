//! The cleanup queue's storage contract.
//!
//! Two reads and three writes. The reads are the candidate queries a pass runs; the writes are
//! queue, decide, and the watermark. Applying a proposal is not here on purpose: it goes through
//! `services::review::supersede` and `services::forget::run`, which already hold the grant check,
//! the ceiling check and the history rules. A second write path into `memory` would put those
//! checks in one branch and none in the other, which is the mistake `ingest_proposal`'s own comment
//! warns about.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::cleanup::{CleanupKind, Disposition};
use crate::domain::errors::Result;
use crate::domain::policy::NamespaceGrant;
use crate::domain::types::Sensitivity;

/// One row a pass is looking at, with only the fields a pass needs.
///
/// Not `Memory`: a candidate carries no embedding and no wrapped key, and a struct that could hold
/// them is a struct someone will hand to a provider by accident.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub namespace: String,
    pub sensitivity: Sensitivity,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub access_count: i32,
    /// When the fact held, not when the store learned it. `None` for most rows. Supersession
    /// ordering reads this and never `created_at`: a July fact approved in August is older than an
    /// August fact approved in July, and the transaction clock says the opposite.
    pub occurred_at: Option<DateTime<Utc>>,
}

/// Two live rows and how close they are.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidatePair {
    pub older: Candidate,
    pub newer: Candidate,
    pub similarity: f64,
}

/// What a pass asks for.
#[derive(Debug, Clone)]
pub struct CandidateQuery {
    /// A namespace glob, matched the way grants are. `None` means no narrowing beyond `grant`.
    pub namespace: Option<String>,
    /// The highest sensitivity to return, on top of whatever `grant` admits.
    pub max_sensitivity: Sensitivity,
    /// Whose rows these are, applied inside the query. A row is a candidate only when some entry
    /// matches its namespace at or above its stored sensitivity, the same rule `policy::admits`
    /// states. The scheduled pass passes `NamespaceGrant::everything()`, which is the honest
    /// spelling of "the whole tenant"; an HTTP caller passes its own read grant, and there is no
    /// value of this field that reads as "skip the check".
    pub grant: Vec<NamespaceGrant>,
    /// Anchors the pass, and anchors only one side of it.
    ///
    /// A run reads rows created at or after this and compares each against **every** live row,
    /// windowed or not. Filtering both sides would make the pass blind to its most common case:
    /// restating a fact the store learned a month ago produces one new row and one old one, and a
    /// window holding only the new one has nothing to compare it to. The window bounds the work,
    /// not the corpus.
    pub since: Option<DateTime<Utc>>,
    pub limit: i64,
}

/// A cluster on the way into the queue.
#[derive(Debug, Clone)]
pub struct NewProposal {
    pub kind: CleanupKind,
    pub namespace: String,
    /// `None` for a kind that names no survivor.
    pub keep_id: Option<String>,
    pub rationale: String,
    pub produced_by: String,
    pub similarity: Option<f64>,
    /// The client that posted it over HTTP, or `None` for the in-process pass. `produced_by` is a
    /// string the poster chose; this one is what the server knows.
    pub posted_by: Option<String>,
    pub members: Vec<NewMember>,
}

/// How many values a subject holds at once, as the owner declared it.
///
/// Not inferable. A later fact ends an earlier one only when the thing holds one value at a time,
/// and the sentence never says whether it does: "the limit is 40k now" replaces its predecessor and
/// "applying for b and c" replaces nothing, and the two are the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arity {
    /// One value at a time. A later dated fact carrying this tag may end an earlier one.
    Single,
    /// A list. Declaring it stops the pass re-offering a subject the owner has already settled.
    Many,
}

impl Arity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Many => "many",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "single" => Some(Self::Single),
            "many" => Some(Self::Many),
            _ => None,
        }
    }
}

/// One declaration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Cardinality {
    pub tag: String,
    pub arity: Arity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewMember {
    pub memory_id: String,
    pub disposition: Disposition,
    /// The content as the pass read it. Apply compares this against the row and refuses when they
    /// differ, so a proposal written an hour ago cannot retire a row the owner has edited since.
    pub seen_content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub id: String,
    pub kind: CleanupKind,
    pub namespace: String,
    pub keep_id: Option<String>,
    pub rationale: String,
    pub produced_by: String,
    /// The client that posted this, or `None` when the in-process pass wrote it. A reader deciding
    /// whether to trust `produced_by` has this beside it.
    pub posted_by: Option<String>,
    pub similarity: Option<f64>,
    pub state: String,
    pub reason: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Member {
    pub memory_id: String,
    pub disposition: Disposition,
    pub seen_content: String,
    /// The row's text now. `None` when the row is gone.
    pub current_content: Option<String>,
    /// Set when something else retired this row since the pass read it.
    pub superseded_by: Option<String>,
    /// Where the row lives and at what level, as stored. Both `None` when the row is gone. The
    /// query already refused a proposal holding a member the caller cannot read; these let the
    /// service check that claim a second time at the boundary.
    pub namespace: Option<String>,
    pub sensitivity: Option<Sensitivity>,
    /// The row's own facts, read when the proposal is, so an owner can tell apart the members of
    /// a cluster whose text is identical. All `None` when the row is gone, like `current_content`.
    /// `occurred_at` is also `None` on a live row that never said when it held; `current_content`
    /// tells the two apart.
    pub created_at: Option<DateTime<Utc>>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub access_count: Option<i32>,
    /// The client that wrote the row, as stored.
    pub source_client: Option<String>,
}

/// What `queue` did with a cluster it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOutcome {
    /// A new row.
    Queued,
    /// The same cluster is already in the queue, in any state. Nothing was written.
    ///
    /// Including `rejected`, which is the point: an hourly pass finds the same cluster every hour,
    /// and one the owner has already refused must not come back.
    AlreadyKnown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watermark {
    pub last_run_at: DateTime<Utc>,
    pub through: DateTime<Utc>,
}

#[async_trait]
pub trait CleanupRepository: Send + Sync {
    /// Declare how many values a tagged subject holds at once. Replaces any earlier declaration for
    /// the same tag, because a declaration is a statement about the world rather than an append-only
    /// log, and two contradictory ones would leave the pass to pick.
    async fn declare_arity(
        &self,
        tenant: &str,
        tag: &str,
        arity: Arity,
        note: Option<&str>,
    ) -> Result<()>;

    /// Every declaration, for the review page and for the pass that reads them.
    async fn arities(&self, tenant: &str) -> Result<Vec<Cardinality>>;

    /// Forget a declaration. An owner who declared wrong needs the undo before they will trust the
    /// declaration at all.
    async fn forget_arity(&self, tenant: &str, tag: &str) -> Result<bool>;
    /// Live rows whose normalised content is byte-identical, grouped.
    ///
    /// Exact duplicates are the one finding that needs no model and no threshold, so they get their
    /// own query rather than falling out of a cosine band with a high cutoff.
    async fn exact_duplicates(
        &self,
        tenant: &str,
        q: &CandidateQuery,
    ) -> Result<Vec<Vec<Candidate>>>;

    /// Live pairs within a cosine band, nearest first.
    ///
    /// The band has two ends. Above `min` is what a pass looks at; a caller that wants the
    /// near-certain band passes a high `min` and treats the result as a duplicate, and one that
    /// wants candidates for a model passes a lower one.
    ///
    /// The implementation compares exactly and does not touch the HNSW index. Every query here
    /// filters by namespace, by sensitivity and to live rows, and filtered HNSW is the failure this
    /// repository has already paid for once: a query asking for ten rows returned zero against
    /// 40,000, having pulled its candidates and filtered all of them away, with no error. Migration
    /// 003 mitigates that for search; a pass whose whole job is to notice what the store contains
    /// must not be able to answer "nothing" because an index truncated. Anchored work is new rows
    /// times live rows, which is cheap at any size a personal store reaches.
    async fn similar_pairs(
        &self,
        tenant: &str,
        q: &CandidateQuery,
        min_similarity: f64,
    ) -> Result<Vec<CandidatePair>>;

    /// Live rows carrying `tag` that have a start date, oldest first by valid time.
    ///
    /// The candidate source for supersession, and the blast radius a declaration has to show before
    /// it hides anything. Both questions are the same query: which dated facts share this subject,
    /// and in what order did they hold.
    ///
    /// Dated only. A row with no `occurred_at` cannot be ordered against another without guessing,
    /// and 0008 refuses the guess.
    async fn tagged_dated(
        &self,
        tenant: &str,
        q: &CandidateQuery,
        tag: &str,
        limit: i64,
    ) -> Result<Vec<Candidate>>;

    /// Live rows nothing has read, older than `days`.
    async fn unread(&self, tenant: &str, q: &CandidateQuery, days: i64) -> Result<Vec<Candidate>>;

    /// The newest `created_at` among live rows in scope, findings or no findings.
    ///
    /// A run that turns up nothing still read the store, and its watermark still has to move. Set
    /// from the candidates instead, a quiet run advances nothing and the next one re-reads exactly
    /// the same rows, forever. The window then never narrows and the pass costs the same on a
    /// store that has not changed in a week.
    async fn newest_in_scope(
        &self,
        tenant: &str,
        q: &CandidateQuery,
    ) -> Result<Option<DateTime<Utc>>>;

    /// Queue a cluster, or report that it is already known.
    async fn queue(&self, tenant: &str, p: NewProposal) -> Result<(QueueOutcome, String)>;

    /// The queue as one caller may see it.
    ///
    /// A proposal is visible when its own namespace is readable at `open` and every member row
    /// is readable at the level it is stored at. The filter runs inside the query, so a row the
    /// caller may not read never reaches the caller's process and the limit counts only what the
    /// caller gets; a pass over the result after the limit would hand back pages that silently
    /// shrink.
    async fn list(
        &self,
        tenant: &str,
        state: Option<&str>,
        limit: i64,
        grant: &[NamespaceGrant],
    ) -> Result<Vec<Proposal>>;

    /// One proposal, under the same rule as `list`. `None` for an id outside the grant as much as
    /// for one that does not exist, so the answer is not an existence check.
    async fn get(
        &self,
        tenant: &str,
        id: &str,
        grant: &[NamespaceGrant],
    ) -> Result<Option<Proposal>>;

    /// Move a proposal to `applied`, `rejected` or `obsolete`. False when it was not `proposed`.
    async fn decide(
        &self,
        tenant: &str,
        id: &str,
        state: &str,
        reason: Option<&str>,
    ) -> Result<bool>;

    /// Return a rejected proposal to the queue. False when it was not `rejected`.
    ///
    /// A cluster rejected because the code that produced it was wrong blocks its own replacement:
    /// `queue` counts every state as known, so the fixed pass finds the pair and says nothing. The
    /// alternative was deleting the row by hand in psql.
    async fn unreject(&self, tenant: &str, id: &str) -> Result<bool>;

    /// Close every proposed row whose cluster the store has already answered.
    ///
    /// A contradiction names no survivor, so the owner resolves it with `lumberroom supersede` and the
    /// proposal has nothing left to say. The same holds for any proposal whose members have been
    /// retired, deleted or edited since the pass read them: applying it is refused anyway, so
    /// leaving it in the queue only costs the owner a read. Returns the ids it closed.
    async fn close_answered(&self, tenant: &str) -> Result<Vec<String>>;

    /// Valid-time start for each id, in the order asked for. `None` where the row has none or is
    /// gone.
    ///
    /// Supersession validates on valid time and the pass chooses its survivor on other grounds, so
    /// something has to reconcile the two before a proposal reaches the queue.
    async fn valid_times(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<(String, Option<DateTime<Utc>>)>>;

    async fn watermark(
        &self,
        tenant: &str,
        scope: &str,
        cadence: &str,
    ) -> Result<Option<Watermark>>;

    /// Advance to `through`, which is the newest row the run actually read.
    ///
    /// Never `now()`. A row written while a run is in flight has to be picked up by the next run,
    /// and a watermark set to the clock rather than to the data skips it silently.
    async fn advance(
        &self,
        tenant: &str,
        scope: &str,
        cadence: &str,
        through: DateTime<Utc>,
    ) -> Result<()>;
}
