//! Turning one question's haystack into rows in the store.
//!
//! The mapping back out is the part that matters. A search returns memory ids; the metric needs
//! session ids. Two things break a naive map. A chunked protocol writes several rows per session,
//! and `write::run` deduplicates, so two sessions holding the same text collapse to one id that is
//! the answer for both.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::task::Poll;

use crate::client::{err, Client, Result};
use crate::eval::{Protocol, Question, Turn};
use crate::wire;

/// Writes in flight at once. 500 questions times 53 sessions is 26,500 writes and every one runs
/// the embedder, which decides whether a run takes twenty minutes or two hours. The client keeps
/// its token and request counter in `RefCell`, so it is not `Sync` and cannot be handed to
/// `tokio::spawn`; these futures are polled inside the one task instead.
const IN_FLIGHT: usize = 4;

/// On every row, so a human can find the eval corpus in a store that also holds real facts.
const EVAL_TAG: &str = "longmemeval";

/// One question's haystack, written and mapped.
#[derive(Debug, Default)]
pub struct Corpus {
    /// Memory id to every session id that produced it. A dedupe collapse puts two here.
    pub owners: HashMap<String, Vec<String>>,
    /// Writes the server accepted. A collapse counts: the content reached the store, on a row that
    /// was already there. `owners.len()` is the number of distinct rows the haystack occupies.
    pub rows_written: usize,
    /// Reason per refused write. A session that never landed cannot be retrieved, and a question
    /// whose gold session is in this list is unanswerable for a reason that is not ranking.
    pub failures: Vec<String>,
    /// Sessions with no row at all.
    pub missing_sessions: Vec<String>,
}

/// LongMemEval stamps each session `2023/02/01 (Wed) 10:20`. Nothing standard parses that, and the
/// weekday is decoration the date already implies.
pub fn parse_haystack_date(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    let mut parts = raw.split_whitespace();
    let ymd = parts.next()?;
    let hm = parts.last()?;
    let mut d = ymd.split('/');
    let (y, m, day) = (
        d.next()?.parse::<i32>().ok()?,
        d.next()?.parse::<u32>().ok()?,
        d.next()?.parse::<u32>().ok()?,
    );
    let mut t = hm.split(':');
    let (h, min) = (t.next()?.parse::<u32>().ok()?, t.next()?.parse::<u32>().ok()?);
    chrono::Utc.with_ymd_and_hms(y, m, day, h, min, 0).single()
}

/// Render one session as the text a memory carries. Roles are kept, because "the assistant said
/// it" and "the user said it" are different facts and LongMemEval asks about both.
pub fn render_session(turns: &[Turn]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(turns.len());
    for turn in turns {
        let content = turn.content.trim();
        if content.is_empty() {
            continue;
        }
        lines.push(line(turn.role.trim(), content));
    }
    lines.join("\n")
}

fn line(role: &str, content: &str) -> String {
    match role.is_empty() {
        true => content.to_string(),
        false => format!("{role}: {content}"),
    }
}

/// Cut a session into pieces of at most `budget` characters, breaking between turns.
///
/// The budget counts characters rather than bytes because the server's own limit does, and a piece
/// measured in bytes would be refused for a length the client thinks it respected.
pub fn chunk_session(turns: &[Turn], budget: usize) -> Vec<String> {
    let budget = budget.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for turn in turns {
        let content = turn.content.trim();
        if content.is_empty() {
            continue;
        }
        for piece in split_turn(turn.role.trim(), content, budget) {
            let chars = piece.chars().count();
            let separator = usize::from(!current.is_empty());
            if !current.is_empty() && current_chars + separator + chars > budget {
                chunks.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            if !current.is_empty() {
                current.push('\n');
                current_chars += 1;
            }
            current.push_str(&piece);
            current_chars += chars;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// One turn as one line, or as several when the turn alone exceeds the budget.
///
/// The longest single turn in LongMemEval-S is 76,560 characters, so this path runs. Every piece
/// carries the role again: a continuation that reads as nobody's words breaks the two question
/// types that ask who said something.
fn split_turn(role: &str, content: &str, budget: usize) -> Vec<String> {
    let whole = line(role, content);
    if whole.chars().count() <= budget {
        return vec![whole];
    }

    let prefix = match role.is_empty() {
        true => String::new(),
        false => format!("{role}: "),
    };
    // A budget too small to hold the prefix and a character of text drops the prefix. Losing the
    // role beats losing the text, and a budget that small is a caller error either way.
    let (prefix, room) = match prefix.chars().count() + 1 > budget {
        true => (String::new(), budget),
        false => {
            let room = budget - prefix.chars().count();
            (prefix, room)
        }
    };

    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut chars = 0usize;
    for ch in content.chars() {
        piece.push(ch);
        chars += 1;
        if chars == room {
            pieces.push(format!("{prefix}{piece}"));
            piece.clear();
            chars = 0;
        }
    }
    if !piece.is_empty() {
        pieces.push(format!("{prefix}{piece}"));
    }
    pieces
}

/// The text of every row one session becomes, exactly as `build` writes it.
///
/// One function for the write and for a search-only run that matches stored rows back to sessions
/// by their text. Two copies would agree until somebody edited one, and every row the other then
/// failed to recognise would read as a session the store never held.
pub fn session_pieces(
    turns: &[Turn],
    protocol: Protocol,
    chunk_chars: usize,
    date: Option<&str>,
) -> Vec<String> {
    let pieces = match protocol {
        Protocol::SessionAsDocument => {
            let text = render_session(turns);
            match text.is_empty() {
                true => vec![],
                false => vec![text],
            }
        }
        Protocol::Chunked => chunk_session(turns, chunk_chars),
    };
    // A published run rendered `role: content` and nothing else, and declared the session dates
    // in its types without ever reading them. Prepending the date is therefore an advantage over
    // that protocol rather than a match for it, so it stays behind a flag and the report names
    // it. It exists to answer one question cheaply: does a temporal signal move retrieval at all,
    // before anybody migrates a schema to carry one.
    match date {
        Some(d) => pieces.into_iter().map(|p| format!("date: {d}\n{p}")).collect(),
        None => pieces,
    }
}

/// Record one accepted write.
///
/// `memory_write` deduplicates, so two sessions holding the same text come back as one id and that
/// id is the right answer for both. Dropping the second loses a gold mapping, and it shows up later
/// as a recall miss with no explanation.
pub fn record_owner(owners: &mut HashMap<String, Vec<String>>, memory_id: &str, session_id: &str) {
    let sessions = owners.entry(memory_id.to_string()).or_default();
    if !sessions.iter().any(|s| s == session_id) {
        sessions.push(session_id.to_string());
    }
}

struct Job {
    session_id: String,
    content: String,
}

/// Write the haystack into `namespace`, one memory per session or chunked.
pub async fn build(
    c: &Client,
    q: &Question,
    namespace: &str,
    protocol: Protocol,
    chunk_chars: usize,
    dates_in_text: bool,
) -> Result<Corpus> {
    let mut corpus = Corpus::default();
    let mut jobs: Vec<Job> = Vec::new();

    for (index, turns) in q.haystack_sessions.iter().enumerate() {
        let Some(session_id) = q.haystack_session_ids.get(index) else {
            corpus.failures.push(format!(
                "session at index {index} has no id: the haystack holds {} sessions and {} ids",
                q.haystack_sessions.len(),
                q.haystack_session_ids.len()
            ));
            continue;
        };
        let date = match dates_in_text {
            true => q.haystack_dates.get(index).map(String::as_str),
            false => None,
        };
        let pieces = session_pieces(turns, protocol, chunk_chars, date);
        if pieces.is_empty() {
            corpus.missing_sessions.push(session_id.clone());
            continue;
        }
        for content in pieces {
            jobs.push(Job { session_id: session_id.clone(), content });
        }
    }

    let mut landed: HashSet<&str> = HashSet::new();
    let mut start = 0usize;
    while start < jobs.len() {
        let end = (start + IN_FLIGHT).min(jobs.len());
        let wave: Vec<_> =
            jobs[start..end].iter().map(|job| write_one(c, namespace, job)).collect();
        for (job, outcome) in jobs[start..end].iter().zip(join_all(wave).await) {
            match outcome {
                Ok(id) => {
                    corpus.rows_written += 1;
                    record_owner(&mut corpus.owners, &id, &job.session_id);
                    landed.insert(job.session_id.as_str());
                }
                // Auth is the one failure that is not about this row. Every remaining write fails
                // the same way, and 26,500 copies of it would bury the reason.
                Err(e) if e.code == 2 => return Err(e),
                Err(e) => corpus.failures.push(format!("{}: {}", job.session_id, e.message)),
            }
        }
        start = end;
    }

    for id in &q.haystack_session_ids {
        if !landed.contains(id.as_str()) && !corpus.missing_sessions.iter().any(|s| s == id) {
            corpus.missing_sessions.push(id.clone());
        }
    }
    Ok(corpus)
}

/// One row through the real tool path, so the eval exercises what a client exercises.
async fn write_one(c: &Client, namespace: &str, job: &Job) -> Result<String> {
    let req = wire::WriteArgsRequest {
        // The benchmark's own dates stay out of the write. Their harness carried none, so sending
        // one here would make the comparable number incomparable.
        occurred_at: None,
        content: job.content.clone(),
        namespace: namespace.to_string(),
        // The server lowercases a tag, so `sharegpt_yywfIrx_0` does not come back verbatim. This is
        // for a human tracing a row by eye; the owners map is what the scoring reads.
        tags: Some(vec![EVAL_TAG.to_string(), job.session_id.clone()]),
        supersedes: None,
    };
    let output = c.call_tool("memory_write", serde_json::to_value(req).unwrap()).await?;
    let outcome: wire::WriteOutcome = serde_json::from_value(output.structured.clone())
        .map_err(|e| err(format!("memory_write response is not the expected shape ({e})")))?;
    Ok(outcome.id)
}

/// Poll a wave of futures to completion together, keeping the input order.
///
/// `futures::join_all` under another name. The crate is not a dependency here and this is the only
/// place that wants it, so it is twenty lines rather than a tree.
async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut pending: Vec<_> = futures.into_iter().map(Box::pin).collect();
    let mut done: Vec<Option<F::Output>> = pending.iter().map(|_| None).collect();
    let mut left = pending.len();
    std::future::poll_fn(move |cx| {
        for (slot, future) in done.iter_mut().zip(pending.iter_mut()) {
            // A future that has already returned is never polled again, which is the rule that
            // makes this safe rather than the ordering.
            if slot.is_some() {
                continue;
            }
            if let Poll::Ready(value) = future.as_mut().poll(cx) {
                *slot = Some(value);
                left -= 1;
            }
        }
        match left {
            0 => Poll::Ready(std::mem::take(&mut done).into_iter().flatten().collect()),
            _ => Poll::Pending,
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, content: &str) -> Turn {
        Turn { role: role.to_string(), content: content.to_string() }
    }

    #[test]
    fn render_keeps_the_role_on_every_turn_and_drops_an_empty_one() {
        let turns = vec![
            turn("user", "  where did I graduate  "),
            turn("assistant", "  "),
            turn("assistant", "Business Administration"),
        ];
        assert_eq!(
            render_session(&turns),
            "user: where did I graduate\nassistant: Business Administration"
        );
    }

    #[test]
    fn render_of_a_session_with_nothing_in_it_is_empty() {
        assert_eq!(render_session(&[turn("user", "   ")]), "");
        assert_eq!(render_session(&[]), "");
    }

    #[test]
    fn the_chunker_breaks_between_turns_rather_than_inside_one() {
        // "user: aaaa" and "user: bbbb" are 10 each, so 21 holds both and 20 does not.
        let turns = vec![turn("user", "aaaa"), turn("user", "bbbb")];
        assert_eq!(chunk_session(&turns, 21), vec!["user: aaaa\nuser: bbbb"]);
        assert_eq!(chunk_session(&turns, 20), vec!["user: aaaa", "user: bbbb"]);
    }

    #[test]
    fn a_session_exactly_at_the_budget_stays_one_chunk() {
        let turns = vec![turn("user", "aaaa"), turn("user", "bbbb")];
        let rendered = render_session(&turns);
        let budget = rendered.chars().count();
        assert_eq!(chunk_session(&turns, budget), vec![rendered]);
    }

    #[test]
    fn an_oversized_turn_splits_and_every_piece_keeps_the_role() {
        // "user: " leaves four characters of room in a budget of ten.
        let chunks = chunk_session(&[turn("user", &"x".repeat(9))], 10);
        assert_eq!(chunks, vec!["user: xxxx", "user: xxxx", "user: x"]);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 10, "{chunk} is over the budget");
            assert!(chunk.starts_with("user: "), "{chunk} lost the role");
        }
    }

    #[test]
    fn a_split_turn_loses_nothing() {
        let content: String = (0..300).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let chunks = chunk_session(&[turn("assistant", &content)], 64);
        let rejoined: String = chunks
            .iter()
            .map(|c| c.strip_prefix("assistant: ").unwrap())
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(rejoined, content);
    }

    #[test]
    fn a_budget_smaller_than_the_role_prefix_keeps_the_text() {
        let chunks = chunk_session(&[turn("assistant", "abcdef")], 3);
        assert_eq!(chunks, vec!["abc", "def"]);
    }

    #[test]
    fn multibyte_text_splits_on_characters_and_never_panics() {
        let chunks = chunk_session(&[turn("user", "héllo wörld")], 8);
        assert!(chunks.iter().all(|c| c.chars().count() <= 8));
        let rejoined: String =
            chunks.iter().map(|c| c.strip_prefix("user: ").unwrap()).collect::<Vec<_>>().concat();
        assert_eq!(rejoined, "héllo wörld");
    }

    #[test]
    fn a_dedupe_collapse_leaves_one_id_owning_both_sessions() {
        let mut owners: HashMap<String, Vec<String>> = HashMap::new();
        record_owner(&mut owners, "row-1", "session-a");
        // The server saw the same text again and returned the id it collapsed into.
        record_owner(&mut owners, "row-1", "session-b");
        record_owner(&mut owners, "row-2", "session-c");
        assert_eq!(owners["row-1"], vec!["session-a".to_string(), "session-b".to_string()]);
        assert_eq!(owners["row-2"], vec!["session-c".to_string()]);
    }

    #[test]
    fn two_chunks_of_one_session_collapsing_do_not_list_it_twice() {
        let mut owners: HashMap<String, Vec<String>> = HashMap::new();
        record_owner(&mut owners, "row-1", "session-a");
        record_owner(&mut owners, "row-1", "session-a");
        assert_eq!(owners["row-1"], vec!["session-a".to_string()]);
    }

    async fn number(n: i32, yields: usize) -> i32 {
        for _ in 0..yields {
            tokio::task::yield_now().await;
        }
        n
    }

    #[tokio::test]
    async fn join_all_returns_results_in_the_input_order_whatever_finishes_first() {
        let wave = vec![number(1, 3), number(2, 0), number(3, 1)];
        assert_eq!(join_all(wave).await, vec![1, 2, 3]);
    }
}

#[cfg(test)]
mod haystack_dates {
    use super::parse_haystack_date;

    #[test]
    fn the_dataset_stamp_parses_and_the_weekday_is_ignored() {
        let d = parse_haystack_date("2023/02/01 (Wed) 10:20").expect("a real stamp parses");
        assert_eq!(d.to_rfc3339(), "2023-02-01T10:20:00+00:00");
    }

    #[test]
    fn a_stamp_with_no_weekday_still_parses() {
        let d = parse_haystack_date("2022/12/19 12:04").expect("the weekday is optional");
        assert_eq!(d.to_rfc3339(), "2022-12-19T12:04:00+00:00");
    }

    #[test]
    fn nonsense_is_none_rather_than_an_epoch() {
        for raw in ["", "not a date", "2023/13/40 (Xxx) 99:99", "2023/02/01"] {
            assert!(parse_haystack_date(raw).is_none(), "{raw:?} must not parse");
        }
    }
}
