//! `review_queue`'s structured answer: brief rows, every free-text value fenced (decision 0031).

use crate::services::review_queue::{Queue, Source};

/// Items an MCP call reads in all when it names no `limit`, split across the sources it reads.
pub const MCP_QUEUE_BUDGET: i64 = 10;

/// Verdicts remove items from the live queue, so the hint names no offset.
pub const QUEUE_MORE: &str =
    "items you leave stay first; offset N starts after the first N items of every source.";

pub fn default_limit(sources_read: usize) -> i64 {
    let _ = sources_read;
    todo!("task 3")
}

pub fn sources_read(asked: Option<&[Source]>, proposal_sources: usize) -> usize {
    let _ = (asked, proposal_sources);
    todo!("task 3")
}

pub fn queue(q: &Queue) -> serde_json::Value {
    let _ = q;
    todo!("task 3")
}
