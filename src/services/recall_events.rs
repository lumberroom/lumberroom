//! The per-call recall log, decision 0024. `RECALL_EVENT_LOG` turns it on.
//!
//! `recall_emission` aggregates, so it cannot say which rows one search or one digest returned.
//! This log can: one row per returned memory, with the call it came from and the position it held.
//! It exists for offline evaluation of ranking, and nothing in the server reads it.
//!
//! The rows travel to the store inside `record_emissions`, which writes them in the same statement
//! as the emissions. Building them here costs one pass over a result set the caller already holds.

use super::Ctx;
use crate::domain::errors::Result;
use crate::ports::{MemoryRepository, RecallCall, RecallEvent};

/// Rows one retention batch deletes. Small enough that a backlog after a long outage never holds a
/// long lock on the table, large enough that a month of a busy store clears in a few statements.
pub const PURGE_BATCH: i64 = 5_000;

/// The log record for one call, or `None` when the log is off.
///
/// `sections` arrives in the order the caller received it: one section with no name for a search,
/// or the digest's sections by name. The rank counts from 1 inside each section and counts every
/// position, so a row whose id does not parse leaves a gap rather than shifting the rows after it.
///
/// Private rows are logged like any other. `recall_emission` leaves them out because it stores a
/// keyed digest of their content, which would be a second copy of a private row's plaintext to
/// probe. This log stores ids and no content. It does add what `touch_accessed` leaves on the row,
/// a count and a last time, does not: which client read a private row, when, and alongside which
/// other rows. That is use metadata, decision 0024 names it as a disclosure cost, and it exposes no
/// content. Leaving private rows out would make the evaluation blind to every private fact.
pub(crate) fn call<'a, I>(
    ctx: &Ctx,
    project: Option<&str>,
    sections: impl IntoIterator<Item = (Option<&'static str>, I)>,
) -> Option<RecallCall>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    if !ctx.cfg.recall_events.enabled {
        return None;
    }
    let mut events = Vec::new();
    for (section, rows) in sections {
        for (i, (id, namespace)) in rows.into_iter().enumerate() {
            let Ok(memory_id) = uuid::Uuid::parse_str(id) else { continue };
            events.push(RecallEvent {
                memory_id,
                namespace: namespace.to_string(),
                section,
                rank: i32::try_from(i + 1).unwrap_or(i32::MAX),
            });
        }
    }
    Some(RecallCall {
        call_id: uuid::Uuid::new_v4(),
        client: ctx.principal.client.clone(),
        project: project.map(str::to_string),
        events,
    })
}

/// The log record for one digest: its three memory sections, in the order the payload carries them.
pub(crate) fn digest_call(ctx: &Ctx, d: &super::bootstrap::Digest) -> Option<RecallCall> {
    if !ctx.cfg.recall_events.enabled {
        return None;
    }
    fn rows(facts: &[super::bootstrap::Fact]) -> impl Iterator<Item = (&str, &str)> {
        facts.iter().map(|f| (f.id.as_str(), f.namespace.as_str()))
    }
    call(
        ctx,
        d.project.as_deref(),
        [
            (Some("profile"), rows(&d.profile)),
            (Some("project"), rows(&d.project_context)),
            (Some("recent"), rows(&d.recent)),
        ],
    )
}

/// Delete this tenant's events older than `retention_days`, `batch` rows at a time, and return how
/// many went. A batch that comes back short means the backlog is clear.
pub async fn purge(
    repo: &dyn MemoryRepository,
    tenant: &str,
    retention_days: u32,
    batch: i64,
) -> Result<u64> {
    // A batch of zero deletes nothing and never comes back short, which would loop forever.
    let batch = batch.max(1);
    let mut total = 0u64;
    loop {
        let gone = repo.purge_recall_events(tenant, retention_days, batch).await?;
        total += gone;
        if (gone as i64) < batch {
            return Ok(total);
        }
    }
}
