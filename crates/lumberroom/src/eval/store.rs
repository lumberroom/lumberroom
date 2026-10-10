//! Rebuilding the corpus map from rows already in the store, for a search-only run.
//!
//! A fusion sweep reruns the same searches against one corpus under several blends. Writing the
//! corpus again for each blend costs an embedding pass per session and measures nothing new, so a
//! search-only run reads the rows a previous run left and maps them back to sessions instead.
//!
//! Two keys find a row's sessions. Every row `corpus::build` writes carries its session id as a
//! tag, lowercased by the server. A row that a later write collapsed into carries only the first
//! writer's tag, so the text is the second key: a session whose rendered text equals a stored row
//! owns that row too. A near-duplicate collapse matches neither, because the stored text is the
//! other session's, and that session lands in the missing count with a reason that says so. The
//! write run would have scored it through the id the write returned, so a nonzero count here is a
//! difference from that run, and the report shows it rather than guessing.

use std::collections::{HashMap, HashSet};

use crate::client::{err, Client, Result};
use crate::eval::corpus::record_owner;
use crate::wire;

/// One row as the export returns it, kept to the fields the map reads.
#[derive(Debug, Clone)]
pub struct StoredRow {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub tags: Vec<String>,
}

/// One session as the harness would have written it: its id and the text of each of its rows.
#[derive(Debug, Clone)]
pub struct ExpectedSession {
    pub id: String,
    pub pieces: Vec<String>,
}

/// The map a write run would have built, as far as the store can still answer it.
#[derive(Debug, Default)]
pub struct Rebuilt {
    pub owners: HashMap<String, Vec<String>>,
    /// Sessions no stored row could be traced to, in the order the caller listed them.
    pub missing: Vec<String>,
}

/// The reason a search-only run gives for a session it could not find.
pub const NOT_IN_STORE: &str =
    "no stored row carries its tag or its text; a near-duplicate collapse lands here too";

/// Match stored rows to the sessions that produced them.
pub fn owners_from_store(sessions: &[ExpectedSession], rows: &[&StoredRow]) -> Rebuilt {
    let mut by_tag: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        by_tag.entry(s.id.to_ascii_lowercase()).or_default().push(i);
    }

    let mut rebuilt = Rebuilt::default();
    let mut owned: HashSet<usize> = HashSet::new();

    for row in rows {
        for tag in &row.tags {
            let Some(candidates) = by_tag.get(tag.as_str()) else { continue };
            // Two session ids that differ only in case share one lowercased tag. The text breaks
            // the tie; with no text match both keep the row, which errs toward a hit the way a
            // dedupe collapse does in the write run.
            let by_text: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&i| sessions[i].pieces.contains(&row.content))
                .collect();
            let chosen = if candidates.len() > 1 && !by_text.is_empty() {
                by_text
            } else {
                candidates.clone()
            };
            for i in chosen {
                record_owner(&mut rebuilt.owners, &row.id, &sessions[i].id);
                owned.insert(i);
            }
        }
    }

    let mut by_content: HashMap<&str, Vec<&str>> = HashMap::new();
    for row in rows {
        by_content.entry(row.content.as_str()).or_default().push(row.id.as_str());
    }
    for (i, s) in sessions.iter().enumerate() {
        for piece in &s.pieces {
            if let Some(ids) = by_content.get(piece.as_str()) {
                for id in ids {
                    record_owner(&mut rebuilt.owners, id, &s.id);
                }
                owned.insert(i);
            }
        }
    }

    rebuilt.missing = sessions
        .iter()
        .enumerate()
        .filter(|(i, _)| !owned.contains(i))
        .map(|(_, s)| s.id.clone())
        .collect();
    rebuilt
}

/// Rows per export page, the most the server hands out at once.
const PAGE: usize = 1000;

/// Every live open row this credential may read, through the export the CLI already uses.
///
/// The eval server holds nothing but the corpus, so the whole tenant is the corpus. Open alone,
/// because the harness writes nothing else and an encrypted row could not be matched by its text.
pub async fn read_store(c: &Client) -> Result<Vec<StoredRow>> {
    let mut rows: Vec<StoredRow> = Vec::new();
    let mut offset = 0usize;
    loop {
        let path = format!("/admin/export?max_sensitivity=open&limit={PAGE}&offset={offset}");
        let (status, body) = c.http_get(&path).await?;
        if status != 200 {
            return Err(err(format!(
                "search-only needs to read the stored corpus and /admin/export answered HTTP \
                 {status}: {}",
                body.get("detail").and_then(|v| v.as_str()).unwrap_or("no detail")
            )));
        }
        let page: wire::ExportPage = serde_json::from_value(body)
            .map_err(|e| err(format!("/admin/export response is not the expected shape ({e})")))?;
        let count = page.rows.len();
        rows.extend(page.rows.into_iter().map(|m| StoredRow {
            id: m.id,
            namespace: m.namespace,
            content: m.content,
            tags: m.tags,
        }));
        if count < PAGE {
            break;
        }
        offset += PAGE;
    }
    Ok(rows)
}

/// Rows grouped by namespace, borrowing from the export.
pub fn by_namespace(rows: &[StoredRow]) -> HashMap<&str, Vec<&StoredRow>> {
    let mut out: HashMap<&str, Vec<&StoredRow>> = HashMap::new();
    for row in rows {
        out.entry(row.namespace.as_str()).or_default().push(row);
    }
    out
}

/// The namespaces a search-only run needs that hold nothing, so the refusal can name them all.
pub fn empty_namespaces<'a>(
    wanted: impl IntoIterator<Item = &'a str>,
    stored: &HashMap<&str, Vec<&StoredRow>>,
) -> Vec<String> {
    wanted
        .into_iter()
        .filter(|ns| stored.get(ns).is_none_or(|rows| rows.is_empty()))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, content: &str, tags: &[&str]) -> StoredRow {
        StoredRow {
            id: id.into(),
            namespace: "project:lme-q0000".into(),
            content: content.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn session(id: &str, pieces: &[&str]) -> ExpectedSession {
        ExpectedSession { id: id.into(), pieces: pieces.iter().map(|p| p.to_string()).collect() }
    }

    #[test]
    fn a_row_maps_to_its_session_through_the_lowercased_tag() {
        let sessions = [session("sharegpt_yywfIrx_0", &["user: hi"])];
        let stored =
            [row("m1", "user: hi, edited by nobody", &["longmemeval", "sharegpt_yywfirx_0"])];
        let rebuilt = owners_from_store(&sessions, &stored.iter().collect::<Vec<_>>());
        assert_eq!(rebuilt.owners["m1"], vec!["sharegpt_yywfIrx_0".to_string()]);
        assert!(rebuilt.missing.is_empty());
    }

    /// The exact-text collapse: the second session's write came back with the first row's id, and
    /// that row carries only the first session's tag.
    #[test]
    fn an_exact_collapse_leaves_one_row_owning_both_sessions() {
        let sessions = [session("s_a", &["user: same"]), session("s_b", &["user: same"])];
        let stored = [row("m1", "user: same", &["longmemeval", "s_a"])];
        let rebuilt = owners_from_store(&sessions, &stored.iter().collect::<Vec<_>>());
        assert_eq!(rebuilt.owners["m1"], vec!["s_a".to_string(), "s_b".to_string()]);
        assert!(rebuilt.missing.is_empty());
    }

    #[test]
    fn a_session_with_neither_tag_nor_text_in_the_store_is_missing() {
        let sessions = [session("s_a", &["user: a"]), session("s_b", &["user: b, reworded"])];
        let stored = [row("m1", "user: a", &["s_a"]), row("m2", "user: b", &["s_c"])];
        let rebuilt = owners_from_store(&sessions, &stored.iter().collect::<Vec<_>>());
        assert_eq!(rebuilt.missing, vec!["s_b".to_string()]);
        assert!(!rebuilt.owners.contains_key("m2"), "a row nobody wrote maps to nobody");
    }

    #[test]
    fn every_chunk_of_a_session_maps_back_to_it() {
        let sessions = [session("s_a", &["user: one", "user: two"])];
        let stored = [row("m1", "user: one", &["s_a"]), row("m2", "user: two", &["s_a"])];
        let rebuilt = owners_from_store(&sessions, &stored.iter().collect::<Vec<_>>());
        assert_eq!(rebuilt.owners.len(), 2);
        assert_eq!(rebuilt.owners["m2"], vec!["s_a".to_string()]);
    }

    #[test]
    fn two_ids_that_differ_only_in_case_split_on_the_text() {
        let sessions = [session("s_Ab", &["user: upper"]), session("s_aB", &["user: lower"])];
        let stored = [row("m1", "user: upper", &["s_ab"]), row("m2", "user: lower", &["s_ab"])];
        let rebuilt = owners_from_store(&sessions, &stored.iter().collect::<Vec<_>>());
        assert_eq!(rebuilt.owners["m1"], vec!["s_Ab".to_string()]);
        assert_eq!(rebuilt.owners["m2"], vec!["s_aB".to_string()]);
    }

    #[test]
    fn an_empty_or_absent_namespace_is_named() {
        let rows = [row("m1", "x", &[])];
        let grouped = by_namespace(&rows);
        let empty = empty_namespaces(["project:lme-q0000", "project:lme-q0001"], &grouped);
        assert_eq!(empty, vec!["project:lme-q0001".to_string()]);
    }
}
