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
use crate::domain::types::Memory;

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

/// A row this caller may both see and change. Resolving a conflict mutates rows, so it needs the
/// write grant, and one message covers "missing" and "not yours" so a refusal maps nothing.
///
/// `pub(super)`: `review_queue::decide` fetches every row through this before any write.
pub(super) async fn writable_row(ctx: &Ctx, id: &str) -> Result<(uuid::Uuid, Memory)> {
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
