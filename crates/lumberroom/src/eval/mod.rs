//! LongMemEval, run against a live lumberroom server.
//!
//! The point is a retrieval number that can sit beside somebody else's. agentmemory publishes
//! LongMemEval-S retrieval scores, so this harness copies their protocol close enough to compare
//! and records every place it cannot.
//!
//! Two things make a comparison honest here. The metrics below were checked against agentmemory's
//! own checked-in per-question results and reproduce their published `recall_any@5`,
//! `recall_any@10` and `NDCG@10` to the digit, so the arithmetic is not the variable. And the
//! embedder is theirs: `all-MiniLM-L6-v2` at 384 dims, zero-padded into the 768-dim column, which
//! leaves cosine unchanged. A retrieval comparison run on a different embedder measures the
//! embedder.
//!
//! What this harness does NOT copy, and every one of these belongs in the report:
//!
//! - Their run scored BM25 plus brute-force cosine fused by RRF. This runs lumberroom's real search,
//!   Postgres full text search plus HNSW blended by a weighted sum.
//! - Their lexical side stems, expands synonyms and matches prefixes. Postgres FTS does none of
//!   that beyond the `english` configuration's own stemming.
//! - Their harness embedded only the first 512 characters of a session. bge and MiniLM both cut at
//!   512 tokens, which is a different bound and usually a longer one.
//! - Their harness replaced the store with an in-process map and built a fresh index per question.
//!   This writes through the real HTTP path into real Postgres.

pub mod corpus;
pub mod dataset;
pub mod hitlog;
pub mod report;
pub mod runner;
pub mod store;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a session becomes rows in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// One memory per session, the whole transcript as its content. What agentmemory's harness
    /// did, and the configuration whose number is comparable to theirs.
    SessionAsDocument,
    /// Sessions cut into chunks the store was built for. Closer to how lumberroom is actually used, and
    /// not comparable to their number.
    Chunked,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionAsDocument => "session-as-document",
            Self::Chunked => "chunked",
        }
    }
}

/// Deep enough for `recall_any@20`. `SEARCH_MAX_LIMIT` is 50, so this is inside it.
pub const RETRIEVE_DEPTH: i64 = 20;

/// One question's haystack lives in its own namespace, so a run is one corpus rather than 500
/// truncations. `project:` is the only prefix whose segment rules admit this shape.
pub fn question_namespace(index: usize) -> String {
    format!("project:lme-q{index:04}")
}

#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub question_id: String,
    pub question_type: String,
    pub question: String,
    #[serde(default)]
    pub question_date: Option<String>,
    pub haystack_session_ids: Vec<String>,
    pub haystack_sessions: Vec<Vec<Turn>>,
    #[serde(default)]
    pub haystack_dates: Vec<String>,
    pub answer_session_ids: Vec<String>,
}

impl Question {
    /// LongMemEval marks an abstention variant by suffixing the id. agentmemory's published run
    /// scored 30 of these among its 500, so they stay in by default.
    pub fn is_abstention(&self) -> bool {
        self.question_id.ends_with("_abs")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub question_id: String,
    pub question_type: String,
    pub gold_session_ids: Vec<String>,
    /// In rank order, deduplicated, one entry per session however many rows it contributed.
    pub retrieved_session_ids: Vec<String>,
    pub recall_any_at_5: f64,
    pub recall_any_at_10: f64,
    pub recall_any_at_20: f64,
    pub ndcg_at_10: f64,
    pub mrr: f64,
    /// Rows this question's haystack could not store, with the reason. A refused write removes a
    /// session from the haystack, and a question whose gold session never landed is unanswerable
    /// for a reason that has nothing to do with ranking.
    pub write_failures: Vec<String>,
    pub rows_written: usize,
    pub latency_ms: u64,
}

impl QuestionResult {
    pub fn score(
        question_id: String,
        question_type: String,
        gold: Vec<String>,
        retrieved: Vec<String>,
    ) -> Self {
        Self {
            recall_any_at_5: recall_any(&retrieved, &gold, 5),
            recall_any_at_10: recall_any(&retrieved, &gold, 10),
            recall_any_at_20: recall_any(&retrieved, &gold, 20),
            ndcg_at_10: ndcg(&retrieved, &gold, 10),
            mrr: mrr(&retrieved, &gold),
            question_id,
            question_type,
            gold_session_ids: gold,
            retrieved_session_ids: retrieved,
            write_failures: vec![],
            rows_written: 0,
            latency_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Aggregate {
    pub questions: usize,
    pub recall_any_at_5: f64,
    pub recall_any_at_10: f64,
    pub recall_any_at_20: f64,
    pub ndcg_at_10: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub protocol: String,
    /// `isolated`, `scoped` or `corpus-wide`. Only `isolated` is comparable to a published run
    /// that built a fresh index per question, and it is the one that says least about scale.
    pub mode: String,
    /// Rows the store held when the last question searched. The whole point of the two harder
    /// modes, and meaningless in `isolated`, where it is one question's haystack. A search-only run
    /// counts the rows it found in the searched namespaces, where a writing run counts accepted
    /// writes, collapses included.
    pub rows_at_end: usize,
    /// True when this run wrote nothing and searched a corpus an earlier run left in the store.
    pub search_only: bool,
    pub embedding_model: String,
    pub retrieve_depth: i64,
    pub overall: Aggregate,
    pub per_type: BTreeMap<String, Aggregate>,
    pub questions_with_write_failures: usize,
    pub sessions_never_stored: usize,
    pub wall_seconds: u64,
    pub per_question: Vec<QuestionResult>,
}

// ---- the metrics ----
//
// Checked on 20 August 2026 against agentmemory's `longmemeval_results_hybrid.json` and
// `longmemeval_results_bm25.json`: recomputing their 500 stored rows with these three functions
// reproduces their published recall_any@5, recall_any@10 and NDCG@10 exactly, on both runs.
// `recall_any@20` and MRR could not be checked that way, because those files store only the first
// ten retrieved ids. `scripts/eval-metrics-check.sh` runs that comparison.

/// Does any gold session appear in the top k. Binary per question, averaged across questions.
pub fn recall_any(retrieved: &[String], gold: &[String], k: usize) -> f64 {
    let take = retrieved.len().min(k);
    if retrieved[..take].iter().any(|s| gold.contains(s)) {
        1.0
    } else {
        0.0
    }
}

fn dcg(relevances: &[bool], k: usize) -> f64 {
    relevances
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &rel)| if rel { 1.0 / ((i + 2) as f64).log2() } else { 0.0 })
        .sum()
}

/// Binary relevance, ideal ranking being every gold session first.
pub fn ndcg(retrieved: &[String], gold: &[String], k: usize) -> f64 {
    let relevances: Vec<bool> = retrieved.iter().take(k).map(|s| gold.contains(s)).collect();
    let ideal = vec![true; gold.len().min(k)];
    let idcg = dcg(&ideal, k);
    if idcg <= 0.0 {
        return 0.0;
    }
    dcg(&relevances, k) / idcg
}

/// Reciprocal rank of the first gold session, over the whole retrieved list.
pub fn mrr(retrieved: &[String], gold: &[String]) -> f64 {
    retrieved.iter().position(|s| gold.contains(s)).map_or(0.0, |i| 1.0 / (i + 1) as f64)
}

pub fn aggregate(results: &[QuestionResult]) -> Aggregate {
    let n = results.len();
    if n == 0 {
        return Aggregate::default();
    }
    let f = n as f64;
    Aggregate {
        questions: n,
        recall_any_at_5: results.iter().map(|r| r.recall_any_at_5).sum::<f64>() / f,
        recall_any_at_10: results.iter().map(|r| r.recall_any_at_10).sum::<f64>() / f,
        recall_any_at_20: results.iter().map(|r| r.recall_any_at_20).sum::<f64>() / f,
        ndcg_at_10: results.iter().map(|r| r.ndcg_at_10).sum::<f64>() / f,
        mrr: results.iter().map(|r| r.mrr).sum::<f64>() / f,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recall_any_is_binary_and_respects_the_cut() {
        let ret = ids(&["a", "b", "c", "d", "e", "gold"]);
        let gold = ids(&["gold"]);
        assert_eq!(recall_any(&ret, &gold, 5), 0.0);
        assert_eq!(recall_any(&ret, &gold, 10), 1.0);
    }

    #[test]
    fn recall_any_handles_a_list_shorter_than_the_cut() {
        assert_eq!(recall_any(&ids(&["gold"]), &ids(&["gold"]), 20), 1.0);
        assert_eq!(recall_any(&[], &ids(&["gold"]), 5), 0.0);
    }

    #[test]
    fn ndcg_is_one_when_every_gold_leads_the_list() {
        let gold = ids(&["g1", "g2"]);
        let ret = ids(&["g1", "g2", "x", "y"]);
        assert!((ndcg(&ret, &gold, 10) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn ndcg_discounts_a_gold_found_late() {
        let gold = ids(&["g1"]);
        let early = ndcg(&ids(&["g1", "x"]), &gold, 10);
        let late = ndcg(&ids(&["x", "g1"]), &gold, 10);
        assert!(late < early, "rank 2 has to score below rank 1");
        // 1/log2(3) over an ideal of 1/log2(2).
        assert!((late - (1.0 / 3f64.log2())).abs() < 1e-12);
    }

    #[test]
    fn ndcg_is_zero_when_the_question_has_no_gold() {
        assert_eq!(ndcg(&ids(&["x"]), &[], 10), 0.0);
    }

    #[test]
    fn mrr_reads_the_first_gold_and_nothing_after_it() {
        let gold = ids(&["g1", "g2"]);
        assert_eq!(mrr(&ids(&["x", "g1", "g2"]), &gold), 0.5);
        assert_eq!(mrr(&ids(&["g2", "g1"]), &gold), 1.0);
        assert_eq!(mrr(&ids(&["x", "y"]), &gold), 0.0);
    }

    #[test]
    fn a_namespace_per_question_is_a_shape_the_store_accepts() {
        let ns = question_namespace(7);
        assert_eq!(ns, "project:lme-q0007");
        assert!(ns
            .strip_prefix("project:")
            .unwrap()
            .starts_with(|c: char| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn an_abstention_question_is_recognised_by_its_id() {
        let q = |id: &str| Question {
            question_id: id.into(),
            question_type: "multi-session".into(),
            question: String::new(),
            question_date: None,
            haystack_session_ids: vec![],
            haystack_sessions: vec![],
            haystack_dates: vec![],
            answer_session_ids: vec![],
        };
        assert!(q("abc_abs").is_abstention());
        assert!(!q("abc").is_abstention());
    }
}

/// `lumberroom eval-longmemeval [flags]`.
///
/// A separate command from `lumberroom eval`, which measures a curated fixture the owner wrote. That one
/// asks whether the store answers the owner's own questions. This one asks how the ranking places
/// against a public haystack, and the two answer different things.
pub async fn dispatch(
    c: &crate::client::Client,
    args: &crate::args::Args,
) -> crate::client::Result<()> {
    let protocol = match args.value("protocol").unwrap_or("session-as-document") {
        "session-as-document" | "session" => Protocol::SessionAsDocument,
        "chunked" | "chunk" => Protocol::Chunked,
        other => {
            return Err(crate::client::err(format!(
                "unknown protocol {other:?}. Use session-as-document, which is the one comparable \
                 to a published run, or chunked."
            )))
        }
    };

    let a = runner::EvalArgs {
        dataset: args.value("dataset").map(str::to_string),
        protocol,
        limit: args.value("limit").and_then(|v| v.parse().ok()),
        skip_abstention: args.present("skip-abstention"),
        chunk_chars: args.value("chunk-chars").and_then(|v| v.parse().ok()).unwrap_or(2000),
        out: args.value("out").map(str::to_string),
        json: args.present("json"),
        resume: args.present("resume"),
        isolate: args.present("isolate"),
        search_only: args.present("search-only"),
        dates_in_text: args.present("dates-in-text"),
        only_type: args.value("type").map(str::to_string),
        // Scoped by default. `--corpus-wide` is the hard configuration.
        scoped: !args.present("corpus-wide"),
    };

    let report = runner::run(c, &a).await?;
    if a.json {
        crate::out_json(&serde_json::to_value(&report).unwrap_or(serde_json::Value::Null));
    } else {
        report::print(&report);
    }
    if let Some(path) = &a.out {
        report::write_json(&report, std::path::Path::new(path))?;
        crate::out(&format!("report written to {path}"));
    }
    Ok(())
}
