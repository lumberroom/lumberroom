//! What a page needs, assembled from the ports and the services.
//!
//! Nothing here renders. Every value a page prints is shaped here first, so the renderer holds no
//! policy decision and the decision has one place to be wrong.
//!
//! Two rules run through the file.
//!
//! **A private row is decrypted through `services::decrypt` and never a second time here.** That
//! helper opens each row once and reuses the plaintext when the same row arrives twice, which is
//! the bug that once dropped a fact from both sections it appeared in. A second decryption path in
//! this module would be a second place for that to go wrong.
//!
//! **A sealed item is counted and never opened.** The server holds no key for one by construction,
//! so the count is the whole honest answer and `Entry` carries no content for it to print.

use chrono::{DateTime, Datelike, Timelike, Utc};
use std::collections::HashMap;

use crate::adapters::auth::{can_read, filter_readable};
use crate::domain::errors::Result;
use crate::domain::namespaces;
use crate::domain::policy::NamespaceCeiling;
use crate::domain::types::{Memory, RegistryEntry, Sensitivity};
use crate::ports::ingest::{IngestRepository, Proposal, ProposalFilter};
use crate::ports::memory::Retired;
use crate::ports::RecentQuery;
use crate::ports::Timeline;
use crate::services::{search, Ctx};

/// How many entries one page holds by default, and the ceiling on what a query string may ask for.
/// The floor is 1: a page of zero entries is a reader staring at a heading.
pub const DEFAULT_PAGE: i64 = 40;
pub const MAX_PAGE: i64 = 200;

/// Score bands on the search page. Display thresholds, not policy, and they never reach a page as
/// numbers: the reader sees `Close`, `Related` and `nothing matched well` in those words.
const CLOSE_BAND: f64 = 0.60;
const RELATED_BAND: f64 = 0.35;

/// How many proposals the queue page asks for. `list_proposals` clamps to 1000 regardless, and a
/// run big enough to fill this in one sitting is a run the owner wants to know overflowed.
const QUEUE_LIMIT: i64 = 500;

/// A page size a query string asked for, held inside what a page can render.
pub fn page_size(asked: Option<i64>) -> i64 {
    asked.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

/// One fact as the notebook prints it: a dateline, the prose, and the margin.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub namespace: String,
    /// Empty when `withheld` is set. A sealed row never carries content this far.
    pub content: String,
    pub tags: Vec<String>,
    pub source_client: String,
    pub sensitivity: Sensitivity,
    pub created_at: DateTime<Utc>,
    /// When the fact began holding in the world and when it stopped. Both None on most rows, which
    /// is why a page prints them beside the entry rather than in a column of its own.
    pub occurred_at: Option<DateTime<Utc>>,
    pub occurred_until: Option<DateTime<Utc>>,
    /// A later write replaced this one. Printed struck through, in place.
    pub retired: bool,
    /// The period closed and no supersession closed it, so no live read returns it and nothing
    /// replaced it. A third state beside live and retired, and the page names it.
    ///
    /// Read off `superseded_at` rather than the link. A restore that could not relink a successor
    /// leaves a row stamped by a supersession with a NULL link, and that row is retired with its
    /// successor missing rather than expired.
    pub expired: bool,
    /// The owner restated it, so the store counted it as confirmed.
    pub confirmed: bool,
    /// True for a sealed row. The page says why the content is absent rather than drawing a box
    /// with nothing in it.
    pub withheld: bool,
}

impl Entry {
    pub fn from_memory(m: &Memory) -> Self {
        let withheld = m.sensitivity == Sensitivity::Sealed;
        Self {
            id: m.id.clone(),
            namespace: m.namespace.clone(),
            // Belt and braces against the level itself: `memory_from_row` reads an unrecognised
            // sensitivity as sealed, so a row can arrive here at a level this console must not
            // print however it was written.
            content: if withheld { String::new() } else { m.content.clone() },
            tags: m.tags.clone(),
            source_client: m.source_client.clone(),
            sensitivity: m.sensitivity,
            created_at: m.created_at,
            occurred_at: m.occurred_at,
            occurred_until: m.occurred_until,
            retired: m.superseded_by.is_some(),
            expired: m.superseded_at.is_none()
                && m.occurred_until.is_some_and(|until| until <= Utc::now()),
            confirmed: m.last_confirmed_at.is_some(),
            withheld,
        }
    }

    /// `19 Aug`. The year appears only when the fact is from another one, because a dateline in the
    /// margin of every entry earns its width by staying narrow.
    pub fn dateline(&self, today: DateTime<Utc>) -> String {
        let d = self.created_at;
        if d.year() == today.year() {
            format!("{} {}", d.day(), month(d))
        } else {
            format!("{} {} {}", d.day(), month(d), d.year())
        }
    }

    /// `05:22`, for a row sitting under a daymark that already carries its date.
    pub fn timeline(&self) -> String {
        format!("{:02}:{:02}", self.created_at.hour(), self.created_at.minute())
    }

    /// The heading a run of entries sits under.
    pub fn daymark(&self) -> String {
        format!("{} {} {}", self.created_at.day(), month(self.created_at), self.created_at.year())
    }
}

fn month(d: DateTime<Utc>) -> &'static str {
    const NAMES: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    NAMES[(d.month0() as usize).min(11)]
}

/// One line in the contents rail.
#[derive(Debug, Clone)]
pub struct NamespaceLine {
    pub namespace: String,
    pub live: i64,
    pub retired: i64,
    pub above_open: i64,
    pub last_write: Option<DateTime<Utc>>,
}

/// The rail: what the store holds, per namespace, on both axes.
#[derive(Debug, Clone, Default)]
pub struct Contents {
    pub namespaces: Vec<NamespaceLine>,
    pub live: i64,
    pub retired: i64,
    pub last_write: Option<DateTime<Utc>>,
    /// Namespace to sealed item count. Counted, never opened.
    pub sealed: Vec<(String, i64)>,
}

/// The cursor a page hands to the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub at: DateTime<Utc>,
    pub id: uuid::Uuid,
}

impl Cursor {
    /// Microseconds since the epoch and the row id.
    ///
    /// Not RFC 3339: a `+00:00` offset in a query string decodes with the `+` read as a space, and
    /// the cursor comes back an hour of parsing away from what it was. Microseconds are also
    /// exactly what a Postgres `timestamptz` stores, so the value round-trips without rounding into
    /// a row the reader has already seen.
    pub fn encode(&self) -> String {
        format!("{}.{}", self.at.timestamp_micros(), self.id)
    }

    /// A cursor that does not parse is no cursor: the reader gets the first page rather than an
    /// error page about a query string they never typed.
    pub fn parse(raw: &str) -> Option<Self> {
        let (micros, id) = raw.split_once('.')?;
        Some(Self {
            at: DateTime::from_timestamp_micros(micros.parse().ok()?)?,
            id: uuid::Uuid::parse_str(id).ok()?,
        })
    }
}

/// A run of entries with the cursor that continues it.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub older: Option<Cursor>,
}

/// One value's life, as dated intervals. Oldest first, the live row last.
///
/// Two clocks, and the page keeps them apart. `occurred_at` and `occurred_until` say when the fact
/// held in the world, which is the timeline a reader came for. `created_at` and `retired_at` say
/// when this store learned it and when a correction landed, which is what stays true for the rows
/// carrying no date at all.
#[derive(Debug, Clone)]
pub struct Revision {
    pub id: String,
    pub content: String,
    pub source_client: String,
    pub created_at: DateTime<Utc>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub occurred_until: Option<DateTime<Utc>>,
    pub retired_at: Option<DateTime<Utc>>,
    /// When this version's period closed with no supersession closing it.
    ///
    /// Never set together with `retired_at`, and the reason is the predicate rather than the
    /// column: this is set only when `superseded_at` is absent, and `retired_at` is that column.
    pub expired_at: Option<DateTime<Utc>>,
    pub current: bool,
    pub withheld: bool,
}

/// One entry, with everything the page prints about it.
#[derive(Debug, Clone)]
pub struct Leaf {
    pub entry: Entry,
    pub access_count: i32,
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub last_confirmed_at: Option<DateTime<Utc>>,
    pub embedding_model: Option<String>,
    pub superseded_at: Option<DateTime<Utc>>,
    /// The whole chain, this entry included.
    pub revisions: Vec<Revision>,
}

/// A search answer, banded. The bands are printed words; no score reaches the page.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub query: String,
    pub close: Vec<Entry>,
    pub related: Vec<Entry>,
    /// Rows that came back under the band. Counted, never printed: their wording overlaps and
    /// their meaning does not.
    pub weak: usize,
    pub namespaces: Vec<String>,
}

/// The registry, grouped by namespace: exact facts with their canonical keys.
#[derive(Debug, Clone)]
pub struct RegistryGroup {
    pub namespace: String,
    pub entries: Vec<RegistryEntry>,
}

/// Every namespace this reader may reach, with the ceiling it holds for each.
///
/// Discovery runs first and the grant runs over its result, which is the contract
/// `namespace_counts` states: its counts are pre-policy and are dropped here. Only names survive,
/// and a name reaches a page only once a both-axes count has put a row behind it.
pub async fn readable(ctx: &Ctx) -> Result<Vec<NamespaceCeiling>> {
    let mut all: Vec<String> =
        ctx.repos.memories.namespace_counts(ctx.tenant()).await?.into_keys().collect();
    if let Some(store) = ctx.repos.sealed.as_ref() {
        // A `credentials:*` namespace holds sealed items and nothing else, so the memory table has
        // never heard of it and the sealed block would report nothing.
        match store.namespaces(ctx.tenant()).await {
            Ok(stored) => all.extend(stored),
            Err(e) => tracing::warn!(
                error = %e.log_message(),
                "could not list sealed namespaces for the console"
            ),
        }
    }
    all.sort();
    namespaces::dedupe(&mut all);
    Ok(filter_readable(&ctx.principal, &all))
}

/// The contents rail.
pub async fn contents(ctx: &Ctx, readable: &[NamespaceCeiling]) -> Result<Contents> {
    let summary = ctx.repos.memories.namespace_summary(ctx.tenant(), readable).await?;

    let mut out = Contents::default();
    for row in summary {
        out.live += row.live;
        out.retired += row.retired;
        out.last_write = later(out.last_write, row.last_write);
        out.namespaces.push(NamespaceLine {
            namespace: row.namespace,
            live: row.live,
            retired: row.retired,
            above_open: row.above_open,
            last_write: row.last_write,
        });
    }

    out.sealed = sealed_counts(ctx, readable).await;
    Ok(out)
}

/// Sealed counts, for the namespaces where this reader's ceiling reaches sealed.
///
/// A count is the whole answer. The bytes are encrypted by the client that stored them and the key
/// never reaches this server, so a page that showed anything more would be inventing it.
async fn sealed_counts(ctx: &Ctx, readable: &[NamespaceCeiling]) -> Vec<(String, i64)> {
    let Some(store) = ctx.repos.sealed.as_ref() else {
        return vec![];
    };
    let names: Vec<String> = readable
        .iter()
        .filter(|c| c.max >= Sensitivity::Sealed)
        .map(|c| c.namespace.clone())
        .collect();
    if names.is_empty() {
        return vec![];
    }
    match store.counts(ctx.tenant(), &names).await {
        Ok(rows) => {
            let mut rows: Vec<(String, i64)> = rows.into_iter().filter(|(_, n)| *n > 0).collect();
            rows.sort();
            rows
        }
        Err(e) => {
            // A missing block rather than a failed page: the rail is a summary and the entries are
            // what the reader came for.
            tracing::warn!(error = %e.log_message(), "could not count sealed items for the console");
            vec![]
        }
    }
}

/// One page of entries, newest first.
///
/// `namespace` narrows to one section of the document; absent reads every namespace this reader
/// may reach. The ceilings go into the query, so a row above them never enters this process.
pub async fn page(
    ctx: &Ctx,
    readable: &[NamespaceCeiling],
    namespace: Option<&str>,
    before: Option<Cursor>,
    limit: i64,
    include_superseded: bool,
) -> Result<Page> {
    // One more than the page, so the presence of a next page is an observation rather than a guess
    // that shows an empty page at the end.
    let rows = ctx
        .repos
        .memories
        .recent(RecentQuery {
            tenant_id: ctx.tenant().to_string(),
            readable: readable.to_vec(),
            namespace: namespace.map(str::to_string),
            before: before.map(|c| (c.at, c.id)),
            limit: limit + 1,
            include_superseded,
        })
        .await?;

    let mut rows = rows;
    let older = if rows.len() as i64 > limit {
        rows.truncate(limit as usize);
        rows.last().and_then(|m| {
            uuid::Uuid::parse_str(&m.id).ok().map(|id| Cursor { at: m.created_at, id })
        })
    } else {
        None
    };

    let rows = opened(ctx, rows).await;
    Ok(Page { entries: rows.iter().map(Entry::from_memory).collect(), older })
}

/// One entry with its provenance and the whole chain it belongs to.
pub async fn leaf(ctx: &Ctx, id: &str) -> Result<Option<Leaf>> {
    let Some(row) = one(ctx, id).await? else {
        return Ok(None);
    };

    let (chain, unopened) = chain(ctx, &row).await?;

    let revisions: Vec<Revision> = chain
        .versions
        .iter()
        .map(|m| {
            // A version this reader cannot open keeps its place in the timeline with no content.
            // Dropping it would sever the chain and report a short history as a complete one, which
            // is the failure the repository's own comment warns about.
            let withheld = m.sensitivity == Sensitivity::Sealed || unopened.contains(&m.id);
            // An expired version is not current, and it carries no `superseded_at` to date it by.
            let expired_at = match m.superseded_at.is_none() {
                true => m.occurred_until.filter(|until| *until <= Utc::now()),
                false => None,
            };
            Revision {
                id: m.id.clone(),
                content: if withheld { String::new() } else { m.content.clone() },
                source_client: m.source_client.clone(),
                created_at: m.created_at,
                occurred_at: m.occurred_at,
                occurred_until: m.occurred_until,
                retired_at: m.superseded_at,
                expired_at,
                current: m.superseded_by.is_none() && expired_at.is_none(),
                withheld,
            }
        })
        .collect();

    Ok(Some(Leaf {
        entry: Entry::from_memory(&row),
        access_count: row.access_count,
        last_accessed_at: row.last_accessed_at,
        last_confirmed_at: row.last_confirmed_at,
        embedding_model: row.embedding_model.clone(),
        superseded_at: row.superseded_at,
        revisions,
    }))
}

/// What got retired lately, newest first.
///
/// The window is a week because that is the cadence the owner reads the queue on, and a supersession
/// approved in a batch of forty is the one that needs finding before the next batch. A wrong
/// supersession is otherwise discovered by missing a fact, and by then nothing says which run did
/// it: the only undo runs off the delete plan, so it costs the successor row and a new id.
pub async fn retired(ctx: &Ctx, readable: &[NamespaceCeiling]) -> Result<Vec<Retired>> {
    let since = chrono::Utc::now() - chrono::Duration::days(7);
    ctx.repos.memories.retired_since(ctx.tenant(), readable, since, 100).await
}

/// Every version of this fact, oldest first, with the ids whose content would not open.
///
/// One recursive read rather than a walk over `find_by_id`. The walk followed `supersedes` and
/// `superseded_by` one row at a time and cost a round trip per version, and it had to carry its own
/// cycle guard; the repository already holds both.
///
/// The ceiling is this reader's ceiling for that one namespace, not the highest level they hold
/// anywhere. A version above it is absent from the answer and leaves a visible gap, which is the
/// contract `subject_history` states.
///
/// The read is scoped to one namespace, so a chain whose successor was written into a different one
/// stops at the boundary. `write::run` permits that supersede and nothing here can see past it. It
/// shows as a timeline shorter than the store holds rather than as an error, so a namespace rename
/// landing mid-chain is the case to watch.
async fn chain(ctx: &Ctx, row: &Memory) -> Result<(Timeline, Vec<String>)> {
    // A row linked in neither direction is its own whole timeline, and the query would return
    // exactly it. Most rows in the store are this row.
    if row.supersedes.is_none() && row.superseded_by.is_none() {
        return Ok((Timeline::single(row.clone()), vec![]));
    }
    // The capability is checked by `services::history`, and a refusal here is not an error: a
    // reader without it sees the fact and no past, which is what every other console page does with
    // something it may not show.
    if crate::services::history::assert_may_read(ctx).is_err() {
        return Ok((Timeline::single(row.clone()), vec![]));
    }
    let Ok(uuid) = uuid::Uuid::parse_str(row.id.trim()) else {
        return Ok((Timeline::single(row.clone()), vec![]));
    };

    // The whole grant, not one namespace's ceiling. A chain may cross a namespace boundary, and
    // the walk now filters each version against the grant rather than stopping at the first
    // version it cannot read: stopping there reports a short history as a complete one.
    let mut timeline = crate::services::history::of(ctx, uuid).await?;
    if timeline.is_empty() {
        return Ok((Timeline::single(row.clone()), vec![]));
    }
    let unopened = crate::services::decrypt(ctx, timeline.versions.iter_mut().collect()).await;
    Ok((timeline, unopened))
}

/// Search, through the service the tools use, banded for reading.
///
/// The service applies the ceilings inside the query, decrypts what this reader may open, and drops
/// what will not open. This function adds no filter of its own beyond the bands.
///
/// Every readable namespace is named rather than left to the default set. With no list the service
/// searches `user:me` and `global` first and everything else at a score penalty, which is right for
/// a model that forgot to say which project it is in and wrong for a reader who asked the whole
/// document a question: it would rank a personal fact below a global one for no reason the reader
/// can see.
pub async fn answer(
    ctx: &Ctx,
    readable: &[NamespaceCeiling],
    query: &str,
    limit: i64,
) -> Result<Answer> {
    let named: Vec<String> = readable.iter().map(|c| c.namespace.clone()).collect();
    let result = search::run(ctx, query, Some(named), Some(limit), None, Some(false), None).await?;

    let mut out =
        Answer { query: query.to_string(), namespaces: result.namespaces, ..Answer::default() };
    for hit in result.hits {
        let entry = Entry {
            id: hit.id,
            namespace: hit.namespace,
            content: if hit.sensitivity == Sensitivity::Sealed {
                String::new()
            } else {
                hit.content
            },
            tags: hit.tags,
            source_client: hit.source_client,
            sensitivity: hit.sensitivity,
            created_at: DateTime::parse_from_rfc3339(&hit.created_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            // The wire shape carries valid time now, skipped when absent, so a search result shows
            // the same period the reading page does rather than looking like an undated row.
            occurred_at: hit
                .occurred_at
                .as_deref()
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                .map(|d| d.with_timezone(&Utc)),
            occurred_until: hit
                .occurred_until
                .as_deref()
                .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
                .map(|d| d.with_timezone(&Utc)),
            retired: hit.superseded_by.is_some(),
            // A search answer never carries an expired row: `search::run` reads live rows and
            // this console asks it for live rows. The wire shape carries no `superseded_at`, so
            // the state cannot be computed here anyway, and inventing it from the link alone is
            // the mistake this predicate exists to avoid.
            expired: false,
            confirmed: false,
            withheld: hit.sensitivity == Sensitivity::Sealed,
        };
        if hit.similarity >= CLOSE_BAND {
            out.close.push(entry);
        } else if hit.similarity >= RELATED_BAND {
            out.related.push(entry);
        } else {
            out.weak += 1;
        }
    }
    Ok(out)
}

/// The registry, grouped by namespace.
pub async fn registry(ctx: &Ctx, readable: &[NamespaceCeiling]) -> Result<Vec<RegistryGroup>> {
    let entries = ctx.repos.registry.list(ctx.tenant(), readable).await?;

    let mut by_namespace: HashMap<String, Vec<RegistryEntry>> = HashMap::new();
    for entry in entries {
        by_namespace.entry(entry.namespace.clone()).or_default().push(entry);
    }
    let mut groups: Vec<RegistryGroup> = by_namespace
        .into_iter()
        .map(|(namespace, mut entries)| {
            entries.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.key.cmp(&b.key)));
            RegistryGroup { namespace, entries }
        })
        .collect();
    groups.sort_by(|a, b| a.namespace.cmp(&b.namespace));
    Ok(groups)
}

/// One proposal as the queue page prints it.
///
/// Everything the owner needs to decide without leaving the page: the claim, where it would land,
/// the frozen speaker, whether the auto gate opened, and the extractor's own read of it. `last_error`
/// travels because a refused proposal stays in the queue exactly so the refusal is visible, not a row
/// that silently stopped moving.
#[derive(Debug, Clone)]
pub struct QueueRow {
    pub id: String,
    pub content: String,
    pub namespace: String,
    pub tags: Vec<String>,
    pub speaker: String,
    pub auto: bool,
    pub state: String,
    pub extractor: String,
    /// The credential that posted the proposal. Beside `extractor` and `speaker`, which the poster
    /// chose for itself, this is the one line of provenance the poster could not write.
    pub posted_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

impl QueueRow {
    fn from_proposal(p: &Proposal) -> Self {
        Self {
            id: p.id.to_string(),
            content: p.content.clone(),
            namespace: p.namespace.clone(),
            tags: p.tags.clone(),
            speaker: p.speaker.clone(),
            auto: p.auto,
            state: p.state.clone(),
            extractor: p.extractor.clone(),
            posted_by: p.posted_by.clone(),
            created_at: p.created_at,
            last_error: p.last_error.clone(),
        }
    }
}

/// The queue, split by state so the page can answer "what is waiting" without the reader scanning
/// past everything already settled.
#[derive(Debug, Clone, Default)]
pub struct QueueView {
    pub proposed: Vec<QueueRow>,
    pub written: Vec<QueueRow>,
    pub rejected: Vec<QueueRow>,
}

impl QueueView {
    pub fn total(&self) -> usize {
        self.proposed.len() + self.written.len() + self.rejected.len()
    }
}

/// The proposal queue, newest first within each state.
///
/// Reads through `IngestRepository` directly rather than through `Ctx.repos`, because ingestion is
/// an operator surface `Repos` was never given a slot for. The grant runs inside the query, as it
/// does for every other read. This reader holds every namespace at sealed, so nothing is hidden
/// from the owner; a narrower reader would see the proposals its grant admits at the level their
/// namespace writes at.
pub async fn queue(ctx: &Ctx, repo: &dyn IngestRepository) -> Result<QueueView> {
    let filter = ProposalFilter {
        limit: QUEUE_LIMIT,
        reader: crate::services::ingest::reader(ctx),
        ..ProposalFilter::default()
    };
    let rows = repo.list_proposals(ctx.tenant(), filter).await?;

    let mut view = QueueView::default();
    for p in &rows {
        let row = QueueRow::from_proposal(p);
        match p.state.as_str() {
            "written" => view.written.push(row),
            "rejected" => view.rejected.push(row),
            // "proposed", and anything the state parser would not recognise: a row nobody has
            // decided belongs in front of the owner rather than dropped for want of a match arm.
            _ => view.proposed.push(row),
        }
    }
    Ok(view)
}

/// One row by id, if this reader may read it at its stored level, with its plaintext filled in.
///
/// `find_by_id` takes no ceiling, so the grant is applied here. The same shape `/admin/memory/{id}`
/// uses, and for the same reason: a row outside the grant is absent rather than refused.
async fn one(ctx: &Ctx, id: &str) -> Result<Option<Memory>> {
    let Ok(uuid) = uuid::Uuid::parse_str(id.trim()) else { return Ok(None) };
    let row = ctx.repos.memories.find_by_id(ctx.tenant(), uuid).await?;
    let mut row = row.filter(|m| can_read(&ctx.principal, &m.namespace, m.sensitivity));
    if let Some(m) = row.as_mut() {
        // A row that will not open keeps its empty content rather than disappearing. An entry the
        // owner asked for by id and cannot read is itself worth seeing.
        let _ = crate::services::decrypt(ctx, vec![m]).await;
    }
    Ok(row)
}

/// Fill in the plaintext of the private rows in a page, and drop the ones that will not open.
///
/// One call through the service helper, which opens each row once and reuses the plaintext for a
/// second copy of the same row. A page with a hole in it reads as a fact with no content, so an
/// unreadable row leaves rather than staying blank.
async fn opened(ctx: &Ctx, mut rows: Vec<Memory>) -> Vec<Memory> {
    let unopened = crate::services::decrypt(ctx, rows.iter_mut().collect()).await;
    if !unopened.is_empty() {
        rows.retain(|m| !unopened.contains(&m.id));
    }
    rows
}

fn later(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(content: &str, sensitivity: Sensitivity) -> Memory {
        Memory {
            occurred_at: None,
            occurred_until: None,
            id: "3f9c1d2a-6b41-4c07-9e55-1a2f8c4d0e77".into(),
            namespace: "user:me".into(),
            content: content.into(),
            tags: vec![],
            source_client: "claude-code-mac".into(),
            embedding_model: None,
            sensitivity,
            supersedes: None,
            superseded_by: None,
            superseded_at: None,
            access_count: 0,
            last_accessed_at: None,
            last_confirmed_at: None,
            created_at: "2026-08-19T14:02:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn a_page_size_stays_inside_what_a_page_can_render() {
        assert_eq!(page_size(None), DEFAULT_PAGE);
        assert_eq!(page_size(Some(10)), 10);
        assert_eq!(page_size(Some(0)), 1);
        assert_eq!(page_size(Some(-500)), 1);
        assert_eq!(page_size(Some(i64::MAX)), MAX_PAGE);
        assert_eq!(page_size(Some(MAX_PAGE + 1)), MAX_PAGE);
    }

    #[test]
    fn a_sealed_row_carries_no_content_past_this_layer() {
        let entry = Entry::from_memory(&memory(
            "ciphertext leaked into a text column",
            Sensitivity::Sealed,
        ));
        assert!(entry.withheld);
        assert!(entry.content.is_empty(), "a sealed row is counted, never shown");
    }

    #[test]
    fn a_private_row_keeps_its_content_because_the_owner_is_reading() {
        let entry = Entry::from_memory(&memory(
            "Hetzner renewal lands on 4 September.",
            Sensitivity::Private,
        ));
        assert!(!entry.withheld);
        assert_eq!(entry.content, "Hetzner renewal lands on 4 September.");
        assert_eq!(entry.sensitivity, Sensitivity::Private);
    }

    #[test]
    fn a_dateline_drops_the_year_only_inside_the_current_one() {
        let entry = Entry::from_memory(&memory("The port is 8787.", Sensitivity::Open));
        let same_year: DateTime<Utc> = "2026-08-20T09:00:00Z".parse().unwrap();
        let next_year: DateTime<Utc> = "2027-01-04T09:00:00Z".parse().unwrap();
        assert_eq!(entry.dateline(same_year), "19 Aug");
        assert_eq!(entry.dateline(next_year), "19 Aug 2026");
        assert_eq!(entry.daymark(), "19 Aug 2026");
    }

    #[test]
    fn a_cursor_survives_a_round_trip_through_a_query_string() {
        let cursor = Cursor {
            at: "2026-08-19T14:02:00.123456Z".parse().unwrap(),
            id: uuid::Uuid::parse_str("3f9c1d2a-6b41-4c07-9e55-1a2f8c4d0e77").unwrap(),
        };
        let encoded = cursor.encode();
        assert!(!encoded.contains('+'), "a plus in a query string decodes as a space");
        assert_eq!(Cursor::parse(&encoded), Some(cursor));
    }

    #[test]
    fn a_cursor_that_does_not_parse_is_no_cursor_rather_than_an_error() {
        for raw in ["", ".", "abc", "123", "123.not-a-uuid", "not-a-number.3f9c1d2a"] {
            assert!(Cursor::parse(raw).is_none(), "{raw:?} must not parse");
        }
    }

    fn proposal(state: &str) -> Proposal {
        Proposal {
            id: uuid::Uuid::parse_str("3f9c1d2a-6b41-4c07-9e55-1a2f8c4d0e77").unwrap(),
            fingerprint: "deadbeef".into(),
            content: "Dana renews the Hetzner box on 4 September.".into(),
            namespace: "user:me".into(),
            tags: vec![],
            supersedes: None,
            speaker: "dana".into(),
            quote: None,
            auto: true,
            extractor: "claude-code".into(),
            posted_by: Some("claude-code-mac".into()),
            state: state.into(),
            memory_id: None,
            last_error: None,
            last_error_at: None,
            decided_at: None,
            created_at: "2026-08-19T14:02:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn a_queue_row_sorts_into_the_state_its_own_field_names() {
        let mut view = QueueView::default();
        for state in ["proposed", "written", "rejected", "something a future migration adds"] {
            let p = proposal(state);
            let row = QueueRow::from_proposal(&p);
            match p.state.as_str() {
                "written" => view.written.push(row),
                "rejected" => view.rejected.push(row),
                _ => view.proposed.push(row),
            }
        }
        assert_eq!(view.written.len(), 1);
        assert_eq!(view.rejected.len(), 1);
        // proposed plus the unrecognised state: undecided stays in front of the owner.
        assert_eq!(view.proposed.len(), 2);
        assert_eq!(view.total(), 4);
    }

    /// Valid time reaches the page through the entry or it does not reach it at all, and most rows
    /// carry none. Both shapes are pinned because a default of `Utc::now()` on the absent one would
    /// print today's date on a fact nobody dated.
    #[test]
    fn a_valid_period_travels_to_the_entry_and_absence_stays_absent() {
        let mut dated = memory("The port is 8080.", Sensitivity::Open);
        dated.occurred_at = Some("2026-08-01T00:00:00Z".parse().unwrap());
        dated.occurred_until = Some("2026-08-20T00:00:00Z".parse().unwrap());
        let entry = Entry::from_memory(&dated);
        assert_eq!(entry.occurred_at, dated.occurred_at);
        assert_eq!(entry.occurred_until, dated.occurred_until);

        let undated = Entry::from_memory(&memory("Dana prefers plain prose.", Sensitivity::Open));
        assert!(undated.occurred_at.is_none());
        assert!(undated.occurred_until.is_none());
    }

    #[test]
    fn a_retired_row_is_marked_rather_than_hidden() {
        let mut m = memory("The port is 8080.", Sensitivity::Open);
        m.superseded_by = Some("11111111-1111-4111-8111-111111111111".into());
        assert!(Entry::from_memory(&m).retired);
        assert!(!Entry::from_memory(&memory("x", Sensitivity::Open)).retired);
    }
}
