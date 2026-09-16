//! The review queue, Phase 4 §3 and §1.
//!
//! Supersession only works if a model chooses to supersede rather than write afresh, and models
//! overwhelmingly write afresh. Conflict candidates on write are the first mechanism; this is the
//! second, and it is the one that catches everything the model did the easy thing about.
//!
//! Nothing here deletes on its own. A personal memory that silently forgets is worse than one that
//! gets cluttered, so the queue lists and a person decides: confirm, supersede, or delete.
//!
//! # Why every row is re-fetched by id
//!
//! `conflicts`, `stale` and `due_for_review` take no ceilings. The queue is an operator surface, but
//! "operator surface" is not a grant, so each row is fetched and checked against this caller's read
//! ceiling for its own namespace before it appears. That costs a round trip per pair on a list that
//! runs by hand with a small limit, and it means the review queue cannot become the convenience
//! surface that leaks.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::Ctx;
use crate::adapters::auth::{can_read, can_write};
use crate::domain::errors::{DomainError, Result};
use crate::domain::policy;
use crate::domain::types::{Memory, RegistryEntry, Sensitivity};
use crate::ports::Staleness;

/// How much of a row the queue prints. The point is to recognise the fact, not to read it.
const PREVIEW_CHARS: usize = 160;

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub id: String,
    pub namespace: String,
    pub sensitivity: Sensitivity,
    pub preview: String,
    pub created_at: String,
    pub access_count: i32,
    pub last_accessed_at: Option<String>,
    pub last_confirmed_at: Option<String>,
}

/// Two live rows close enough that one probably should have retired the other.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictItem {
    pub similarity: f64,
    pub older: Row,
    pub newer: Row,
    /// The command that resolves it, spelled out. The queue is only useful if acting on it is one
    /// copy and paste rather than a lookup.
    pub resolve_with: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StaleItem {
    #[serde(flatten)]
    pub row: Row,
    pub age_days: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryDue {
    pub namespace: String,
    pub kind: String,
    pub key: String,
    pub value: serde_json::Value,
    pub sensitivity: Sensitivity,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewQueue {
    pub conflicts: Vec<ConflictItem>,
    pub stale: Vec<StaleItem>,
    pub registry_due: Vec<RegistryDue>,
    /// The three decay numbers, and only for a caller whose grant reaches every row they count.
    ///
    /// `staleness` takes no ceilings and counts every row in the tenant, so it is a size and a shape
    /// of the store rather than of what this caller may read. A client granted `user:me` learned the
    /// live row count of the whole store from a queue that showed it two of its own rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staleness: Option<Staleness>,
    /// Rendered for `lumberroom review`.
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub action: &'static str,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded: Option<String>,
    /// The retired row kept an open end, so it still reads as holding at every instant. Absent on
    /// every action that retires nothing, and on the ordinary supersession that dated its end.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub end_left_open: bool,
}

/// A fact whose period the owner closed, and the instant that closed it.
///
/// The instant is what `unexpire` is guarded on, so a caller who wants the action back has to
/// carry it. Returning it is the whole of the undo contract.
#[derive(Debug, Clone, Serialize)]
pub struct Expired {
    pub id: String,
    pub until: DateTime<Utc>,
}

pub async fn queue(ctx: &Ctx, limit: Option<i64>) -> Result<ReviewQueue> {
    let limit = limit.unwrap_or(20).clamp(1, 200);

    let pairs = ctx
        .repos
        .memories
        .conflicts(ctx.tenant(), ctx.cfg.quality.conflict_threshold, limit)
        .await?;
    let mut conflicts = Vec::new();
    for pair in pairs {
        // Both halves have to be visible. Showing one side of a conflict is worse than showing
        // neither: it invites a supersede against a row the caller cannot see.
        let (Some(older), Some(newer)) =
            (visible(ctx, &pair.older.id).await?, visible(ctx, &pair.newer.id).await?)
        else {
            continue;
        };
        conflicts.push(ConflictItem {
            similarity: pair.similarity,
            resolve_with: format!("lumberroom supersede {} {}", older.id, newer.id),
            older: to_row(&older),
            newer: to_row(&newer),
        });
    }

    let mut stale_rows: Vec<Memory> = ctx
        .repos
        .memories
        .stale(ctx.tenant(), ctx.cfg.quality.stale_days, limit, &ctx.principal.read)
        .await?;
    let _ = super::decrypt(ctx, stale_rows.iter_mut().collect()).await;
    let stale: Vec<StaleItem> = stale_rows
        .iter()
        .map(|m| StaleItem {
            age_days: (chrono::Utc::now() - m.created_at).num_days(),
            row: to_row(m),
        })
        .collect();

    let registry_due = ctx
        .repos
        .registry
        .due_for_review(ctx.tenant(), limit, &ctx.principal.read)
        .await?
        .into_iter()
        .map(to_registry_due)
        .collect();

    // The three numbers that say whether the store is decaying, computed over every row in the
    // tenant and therefore published to nobody else. Best effort beyond that: a queue that fails
    // because a summary statistic did not compute is a queue nobody uses.
    let staleness = match super::reads_whole_store(&ctx.principal) {
        false => None,
        true => match ctx.repos.memories.staleness(ctx.tenant()).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e.log_message(), "could not compute staleness for the queue");
                Some(Staleness::default())
            }
        },
    };

    let mut queue = ReviewQueue { conflicts, stale, registry_due, staleness, text: String::new() };
    queue.text = render(&queue, ctx.cfg.quality.stale_days);
    Ok(queue)
}

/// "This fact is still true." The cheapest of the three actions and the one that should be used
/// most: most of what lands in the queue is correct and simply unvisited.
pub async fn confirm(ctx: &Ctx, id: &str) -> Result<Resolved> {
    let (uuid, _) = writable_row(ctx, id).await?;
    ctx.repos.memories.confirm(ctx.tenant(), uuid).await?;
    Ok(Resolved { action: "confirm", id: uuid.to_string(), superseded: None, end_left_open: false })
}

/// Retire `old` in favour of `new`. The chain and cycle rules live in the repository, because a
/// two-row cycle makes both rows invisible and that has to be refused inside the transaction that
/// would create it.
pub async fn supersede(ctx: &Ctx, old: &str, new: &str) -> Result<Resolved> {
    // The same validation `memory_write` runs, so the queue cannot become a second, laxer path to
    // the same mutation.
    let old_id = super::write::validate_supersedes(ctx, old).await?;
    let (new_id, new_row) = writable_row(ctx, new).await?;
    if old_id == new_id {
        return Err(DomainError::validation("a row cannot supersede itself"));
    }
    if !new_row.is_live() {
        return Err(DomainError::conflict(format!(
            "memory {new} does not hold now, because something superseded it or its period closed, \
             so it cannot be the replacement"
        )));
    }

    let done = ctx.repos.memories.supersede(ctx.tenant(), old_id, new_id).await?;
    super::bootstrap::clear_cache();
    Ok(Resolved {
        action: "supersede",
        id: new_id.to_string(),
        superseded: Some(old_id.to_string()),
        end_left_open: done.end_left_open,
    })
}

/// One undated row and the day its own text names.
#[derive(Debug, Clone, Serialize)]
pub struct DateCandidate {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub created_at: String,
    /// The single day the content states. Absent when it names none, or more than one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed: Option<String>,
    /// Every day the text names, when it names more than one. The owner picks; nothing here does.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ambiguous: Vec<String>,
}

/// Live rows with no start date, paired with the day each one states about itself.
///
/// A review, never a filler. It proposes nothing where the text names nothing, and where the text
/// names two days it reports both rather than picking: "approved on 4 March after the panel met on
/// 9 January" has two real dates and only the owner knows which one the fact is about.
///
/// Rows that name no day at all are dropped rather than listed. Most of the store is undated
/// because most facts are timeless, and a list of every preference the owner ever stated is not a
/// review, it is the store.
pub async fn date_candidates(ctx: &Ctx, limit: Option<i64>) -> Result<Vec<DateCandidate>> {
    let limit = limit.unwrap_or(50).clamp(1, 500);
    // Every namespace this caller may read, resolved from their grants the way `search` resolves a
    // requested list. The store supplies the names; the grant decides which survive and at what
    // ceiling, and the ceiling then runs inside the query.
    let names: Vec<String> =
        ctx.repos.memories.namespace_counts(ctx.tenant()).await?.into_keys().collect();
    let readable = policy::resolve(&ctx.principal.read, &names);
    // Scan wider than the answer. Undated rows are the common case and only a few name a day, so a
    // page sized to the answer would return almost nothing.
    let mut rows = ctx.repos.memories.undated(ctx.tenant(), &readable, limit * 20).await?;

    // A private row arrives with empty content, because the repository will not render ciphertext
    // as text. Without this the scan reads those rows as naming no day and drops them in silence,
    // so the facts most worth dating would be the ones it never mentions. A row that will not open
    // stays dropped, which is the same answer every other reader gives it.
    super::decrypt(ctx, rows.iter_mut().collect()).await;

    let today = Utc::now().date_naive();
    let mut out = Vec::new();
    for row in rows {
        let mut days = crate::domain::dates::extract(&row.content);
        // A day still ahead is a plan, not a record, and `fill_date` would refuse it anyway.
        days.retain(|d| *d <= today);
        if days.is_empty() {
            continue;
        }
        let (proposed, ambiguous) = if days.len() == 1 {
            (Some(days[0].to_string()), vec![])
        } else {
            (None, days.iter().map(|d| d.to_string()).collect())
        };
        out.push(DateCandidate {
            id: row.id,
            namespace: row.namespace,
            content: row.content,
            created_at: row.created_at.to_rfc3339(),
            proposed,
            ambiguous,
        });
        if out.len() as i64 >= limit {
            break;
        }
    }
    Ok(out)
}

/// "This fact described a situation, and the situation has passed."
///
/// The third thing the queue can do to a row, beside confirming it and superseding it, and the
/// first that retires one with nothing to retire it into. `CleanupKind::Stale` deletes for exactly
/// this reason; this is the answer that keeps the text, the history and every as-of read.
///
/// The write grant at the row's own level and no capability flag. `may_delete` guards loss nothing
/// brings back, and `unexpire` is one statement away.
pub async fn expire(ctx: &Ctx, id: &str) -> Result<Expired> {
    let (uuid, row) = writable_row(ctx, id).await?;
    // The period first. `is_live` answers both clocks, so a row this path already closed would
    // otherwise be reported as superseded by something that does not exist.
    //
    // Two ways a period is already closed, and the refusal says which. A stamp means a supersession
    // ended it, even where the successor went missing in a restore; no stamp means this path did.
    if let Some(until) = row.occurred_until {
        return Err(DomainError::validation(match row.superseded_at {
            None => format!("memory {id} already expired at {}", until.to_rfc3339()),
            Some(_) => format!(
                "memory {id} was retired by a supersession that ended its period at {}",
                until.to_rfc3339()
            ),
        }));
    }
    if !row.is_live() {
        return Err(DomainError::conflict(format!(
            "memory {id} is already superseded, so its end is the supersession's to write"
        )));
    }
    // Everything the row said is checked above, so a statement that moves nothing here means the
    // row changed between the read and the write rather than that it was already closed.
    let Some(until) = ctx.repos.memories.expire(ctx.tenant(), uuid).await? else {
        return Err(DomainError::conflict(format!("memory {id} changed while this ran")));
    };
    super::bootstrap::clear_cache();
    Ok(Expired { id: uuid.to_string(), until })
}

/// Reopen a fact this instant closed, and say whether the statement moved anything.
///
/// A false return is a row somebody else has since changed, which the caller reports rather than
/// overrides.
pub async fn unexpire(ctx: &Ctx, id: &str, until: DateTime<Utc>) -> Result<bool> {
    let (uuid, _) = writable_row(ctx, id).await?;
    let done = ctx.repos.memories.unexpire(ctx.tenant(), uuid, until).await?;
    if done {
        super::bootstrap::clear_cache();
    }
    Ok(done)
}

/// Fill a start date on a row that never carried one.
///
/// Three refusals, and each one exists because the alternative stores a date nobody can check.
///
/// **The content has to state the day.** The same rule the near-now fence uses, and it is the whole
/// reason this is safe to expose: a date written in the row's own text can be checked against that
/// row forever, by anyone, long after whoever proposed it is gone. Without that rule this is an
/// endpoint for writing arbitrary history.
///
/// **A date already there is never moved.** The repository refuses it in the statement. Filling a
/// gap adds what was missing; overwriting rewrites what the store already believed.
///
/// **Nothing in the future.** A future start reads live and never reads as-of, so the row would
/// answer one query and not the other.
pub async fn fill_date(ctx: &Ctx, id: &str, when: DateTime<Utc>) -> Result<Resolved> {
    let (uuid, row) = writable_row(ctx, id).await?;
    if when > Utc::now() {
        return Err(DomainError::validation(
            "occurred_at cannot be in the future: a fact does not become true later than now",
        ));
    }
    if row.occurred_at.is_some() {
        return Err(DomainError::conflict(format!(
            "memory {id} already carries a start date. This fills a gap and never moves a start"
        )));
    }
    if !crate::domain::dates::states(&row.content, when.date_naive()) {
        return Err(DomainError::validation(format!(
            "the content of memory {id} does not name {}, so this date cannot be checked against \
             the row later. Only a date the fact itself states can be filled in",
            when.date_naive()
        )));
    }
    if !ctx.repos.memories.fill_occurred_at(ctx.tenant(), uuid, when).await? {
        return Err(DomainError::conflict(format!(
            "memory {id} gained a start date while this ran"
        )));
    }
    super::bootstrap::clear_cache();
    Ok(Resolved {
        action: "fill_date",
        id: uuid.to_string(),
        superseded: None,
        end_left_open: false,
    })
}

/// Deleting goes through the delete path, grant flag included. A second entry point with its own
/// checks is how the two drift apart.
pub async fn delete(
    ctx: &Ctx,
    id: &str,
    reason: Option<&str>,
) -> Result<super::forget::ForgetOutcome> {
    super::forget::by_id(ctx, id, reason, false).await
}

/// The row, if this caller may read it at its stored level.
async fn visible(ctx: &Ctx, id: &str) -> Result<Option<Memory>> {
    let Ok(uuid) = uuid::Uuid::parse_str(id) else { return Ok(None) };
    let row = ctx.repos.memories.find_by_id(ctx.tenant(), uuid).await?;
    let mut row = row.filter(|m| can_read(&ctx.principal, &m.namespace, m.sensitivity));
    if let Some(m) = row.as_mut() {
        // Kept even when it will not open. A conflict pair where one side is unreadable is still
        // something a person should look at, and hiding it would hide the reason.
        let _ = super::decrypt(ctx, vec![m]).await;
    }
    Ok(row)
}

/// A row this caller may both see and change. Resolving a conflict mutates rows, so it needs the
/// write grant, and one message covers "missing" and "not yours" so a refusal maps nothing.
async fn writable_row(ctx: &Ctx, id: &str) -> Result<(uuid::Uuid, Memory)> {
    let uuid = uuid::Uuid::parse_str(id.trim())
        .map_err(|_| DomainError::validation(format!("{id:?} is not a uuid")))?;
    match ctx.repos.memories.find_by_id(ctx.tenant(), uuid).await? {
        Some(m)
            if can_read(&ctx.principal, &m.namespace, m.sensitivity)
                && can_write(&ctx.principal, &m.namespace, m.sensitivity) =>
        {
            Ok((uuid, m))
        }
        _ => Err(DomainError::not_found(format!(
            "memory {id} does not exist or is not yours to change"
        ))),
    }
}

fn to_row(m: &Memory) -> Row {
    Row {
        id: m.id.clone(),
        namespace: m.namespace.clone(),
        sensitivity: m.sensitivity,
        preview: preview(&m.content),
        created_at: m.created_at.to_rfc3339(),
        access_count: m.access_count,
        last_accessed_at: m.last_accessed_at.map(|t| t.to_rfc3339()),
        last_confirmed_at: m.last_confirmed_at.map(|t| t.to_rfc3339()),
    }
}

fn to_registry_due(e: RegistryEntry) -> RegistryDue {
    RegistryDue {
        namespace: e.namespace,
        kind: e.kind,
        key: e.key,
        value: e.value,
        sensitivity: e.sensitivity,
        version: e.version,
    }
}

/// An empty preview is a private row that would not decrypt. It stays in the queue: an unreadable
/// row is a review item in its own right.
fn preview(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "(encrypted; this server could not read it)".to_string();
    }
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    format!("{}…", flat.chars().take(PREVIEW_CHARS).collect::<String>())
}

/// Plain text, because this is read in a terminal. Every section names the action that clears it.
pub fn render(q: &ReviewQueue, stale_days: i32) -> String {
    let mut lines = Vec::new();
    lines.push("# Review".to_string());
    // Absent rather than zeroed. Printing "0 live rows" above a queue holding rows would read as a
    // broken store, and the number belongs to the whole tenant rather than to this caller.
    if let Some(s) = &q.staleness {
        lines.push(format!(
            "{} live rows, {} never retrieved ({:.0}%), {} superseded.",
            s.live_rows, s.never_retrieved, s.never_retrieved_pct, s.superseded_rows
        ));
    }

    if !q.conflicts.is_empty() {
        lines.push(String::new());
        lines.push(format!("## Near-duplicates ({})", q.conflicts.len()));
        lines.push(
            "Two live rows saying nearly the same thing. Keep the newer one and retire the older."
                .to_string(),
        );
        for c in &q.conflicts {
            lines.push(String::new());
            lines.push(format!("{:.3}  [{}]", c.similarity, c.older.namespace));
            lines.push(format!("  older {}  {}", c.older.id, c.older.preview));
            lines.push(format!("  newer {}  {}", c.newer.id, c.newer.preview));
            lines.push(format!("  {}", c.resolve_with));
        }
    }

    if !q.stale.is_empty() {
        lines.push(String::new());
        lines.push(format!("## Never retrieved, older than {stale_days} days ({})", q.stale.len()));
        lines.push(
            "Nothing here is deleted automatically. Confirm what is still true, supersede what \
             changed, delete what is dead."
                .to_string(),
        );
        for s in &q.stale {
            lines.push(format!(
                "- {} [{}] {}d  {}",
                s.row.id, s.row.namespace, s.age_days, s.row.preview
            ));
        }
    }

    if !q.registry_due.is_empty() {
        lines.push(String::new());
        lines.push(format!("## Registry entries due for review ({})", q.registry_due.len()));
        lines.push(
            "A per-kind expectation expired. Expiry marks a row for review and never removes it."
                .to_string(),
        );
        for r in &q.registry_due {
            lines.push(format!("- {}/{} = {} [{}]", r.kind, r.key, r.value, r.namespace));
        }
    }

    if q.conflicts.is_empty() && q.stale.is_empty() && q.registry_due.is_empty() {
        lines.push(String::new());
        lines.push("Nothing to review.".to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, preview: &str) -> Row {
        Row {
            id: id.into(),
            namespace: "user:me".into(),
            sensitivity: Sensitivity::Open,
            preview: preview.into(),
            created_at: "2026-01-01T00:00:00+00:00".into(),
            access_count: 0,
            last_accessed_at: None,
            last_confirmed_at: None,
        }
    }

    fn empty() -> ReviewQueue {
        ReviewQueue {
            conflicts: vec![],
            stale: vec![],
            registry_due: vec![],
            staleness: Some(Staleness::default()),
            text: String::new(),
        }
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_printing_three_empty_headings() {
        let text = render(&empty(), 180);
        assert!(text.contains("Nothing to review."));
        assert!(!text.contains("## Near-duplicates"));
    }

    #[test]
    fn a_conflict_prints_the_command_that_resolves_it() {
        let mut q = empty();
        q.conflicts = vec![ConflictItem {
            similarity: 0.942,
            older: row("aaa", "The port is 8080"),
            newer: row("bbb", "The port is 8787"),
            resolve_with: "lumberroom supersede aaa bbb".into(),
        }];
        let text = render(&q, 180);
        assert!(text.contains("0.942"));
        assert!(text.contains("lumberroom supersede aaa bbb"));
        assert!(text.contains("older aaa"));
    }

    #[test]
    fn the_stale_section_names_the_threshold_it_used() {
        let mut q = empty();
        q.stale = vec![StaleItem { row: row("aaa", "An old fact"), age_days: 400 }];
        assert!(render(&q, 180).contains("older than 180 days"));
    }

    #[test]
    fn the_stale_section_promises_nothing_is_deleted_automatically() {
        let mut q = empty();
        q.stale = vec![StaleItem { row: row("aaa", "An old fact"), age_days: 400 }];
        assert!(render(&q, 180).contains("deleted automatically"));
    }

    #[test]
    fn a_preview_is_flattened_and_bounded() {
        assert_eq!(preview("two\n lines"), "two lines");
        assert_eq!(preview(&"x".repeat(400)).chars().count(), PREVIEW_CHARS + 1);
    }
}
