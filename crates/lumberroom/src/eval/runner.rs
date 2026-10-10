//! Running the questions.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::client::{err, Client, Result};
use crate::eval::{corpus, dataset};
use crate::eval::{Protocol, Question, QuestionResult, RunReport};
use crate::wire;

#[derive(Debug, Clone)]
pub struct EvalArgs {
    pub dataset: Option<String>,
    pub protocol: Protocol,
    /// Stop after this many questions. The default is every one.
    pub limit: Option<usize>,
    /// Skip the 30 abstention variants. Off, because agentmemory's published run scored them.
    pub skip_abstention: bool,
    pub chunk_chars: usize,
    /// Where to write the report. Defaults beside the dataset.
    pub out: Option<String>,
    pub json: bool,
    /// Resume: skip a question whose namespace already holds rows.
    pub resume: bool,
    /// Delete each question's haystack once it is scored, so the next question meets an empty
    /// store. Reproduces agentmemory's fresh-index-per-question and measures ranking with the rest
    /// of the corpus taken away.
    pub isolate: bool,
    /// Prepend each session's dataset date to its text. A deviation from the published protocol,
    /// which carried no dates, and the cheap way to learn whether a temporal signal matters here.
    pub dates_in_text: bool,
    /// Run one question type only, so a 133-question category can be measured without the other
    /// 367.
    pub only_type: Option<String>,
    /// Search inside the question's own namespace. Off means corpus-wide: every unique session is
    /// written once into one namespace before the first search, and every question searches all
    /// of it, which is the configuration a large real store lives in.
    pub scoped: bool,
}

/// A search hit whose memory id is in no session's owner list. `QuestionResult` has one free-text
/// field, so the note lives in `write_failures` behind a prefix and the counters read the prefix
/// back out. An orphan means the corpus map and the store disagree, which is a harness bug.
pub const UNMAPPED_HIT: &str = "unmapped hit: ";

/// A haystack session that produced no row. Same field, same reason. This one is the counter that
/// decides whether a run is comparable at all.
pub const MISSING_SESSION: &str = "session not stored: ";

/// Map hits back to sessions, in rank order, first occurrence winning.
///
/// A session that contributed three chunks appears once, at the rank of its best chunk. Counting it
/// three times would let one session filling the top five read as five hits and inflate every
/// metric. One memory can own several sessions when the store deduplicated identical text, and all
/// of them enter at that rank.
///
/// An id with no owner keeps its rank as the raw memory id. It can never equal a gold session id,
/// so it costs a rank rather than promoting the hit below it: dropping it would silently improve
/// the score, which is the one direction a benchmark must never round in.
pub fn sessions_in_rank_order(
    hit_ids: &[String],
    owners: &HashMap<String, Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    let mut ordered: Vec<String> = Vec::new();
    let mut unmapped: Vec<String> = Vec::new();
    for id in hit_ids {
        match owners.get(id) {
            Some(sessions) => {
                for session in sessions {
                    if !ordered.iter().any(|s| s == session) {
                        ordered.push(session.clone());
                    }
                }
            }
            None => {
                unmapped.push(id.clone());
                if !ordered.iter().any(|s| s == id) {
                    ordered.push(id.clone());
                }
            }
        }
    }
    (ordered, unmapped)
}

/// Rebuild the id-to-session map from rows already in the store, writing nothing.
///
/// Resume needs owners without a write, so it matches on content. Exact equality covers
/// session-as-document. Containment covers the chunked protocol, where a row carries a slice of the
/// session, and it also covers a server that shortened the content it echoed back. Two sessions
/// rendering to the same text both own the row, which is what `corpus::build` records when the
/// store deduplicates them.
/// Drop a leading `role: ` written by the chunker, leaving the words somebody actually said.
fn strip_role_prefix(text: &str) -> &str {
    match text.split_once(": ") {
        Some((role, rest)) if !role.is_empty() && role.chars().all(|c| c.is_ascii_lowercase()) => {
            rest
        }
        _ => text,
    }
}

pub fn owners_from_content(q: &Question, hits: &[wire::Hit]) -> HashMap<String, Vec<String>> {
    let rendered: Vec<(String, String)> = q
        .haystack_session_ids
        .iter()
        .zip(q.haystack_sessions.iter())
        .map(|(id, turns)| (id.clone(), corpus::render_session(turns)))
        .collect();

    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    for hit in hits {
        let exact: Vec<String> = rendered
            .iter()
            .filter(|(_, text)| *text == hit.content)
            .map(|(id, _)| id.clone())
            .collect();
        let matched = if exact.is_empty() {
            // A chunk that continues an oversized turn repeats the role prefix, and the rendered
            // session carries that prefix only where the turn began. Containment on the raw text
            // therefore fails on every continuation piece, and 29% of turns in this dataset exceed
            // the default chunk budget, so it fails often rather than rarely. Try the body too.
            let body = strip_role_prefix(&hit.content);
            rendered
                .iter()
                .filter(|(_, text)| {
                    !hit.content.is_empty()
                        && (text.contains(&hit.content)
                            || (!body.is_empty() && text.contains(body)))
                })
                .map(|(id, _)| id.clone())
                .collect()
        } else {
            exact
        };
        if !matched.is_empty() {
            owners.insert(hit.id.clone(), matched);
        }
    }
    owners
}

async fn search(c: &Client, query: &str, namespace: &str, limit: i64) -> Result<Vec<wire::Hit>> {
    let req = wire::SearchArgsRequest {
        // Live search. An as-of read is a different question and no benchmark question asks it.
        as_of: None,
        query: query.to_string(),
        // Named explicitly. Sending none means "the caller's defaults", which are user, global and
        // the active project, and those hold nothing here: the first attempt at this returned zero
        // hits on every question and read as a score of zero rather than as the mistake it was.
        namespaces: Some(vec![namespace.to_string()]),
        limit: Some(limit),
        project: None,
    };
    let output = c.call_tool("memory_search", serde_json::to_value(req).unwrap()).await?;
    let result: wire::SearchResult = serde_json::from_value(output.structured.clone())
        .map_err(|e| err(format!("memory_search response is not the expected shape ({e})")))?;
    Ok(result.hits)
}

/// The namespace a corpus-wide run writes every session into. One project holding the whole corpus
/// is the shape of a large real store, where a search meets every row the owner ever wrote.
pub const CORPUS_NAMESPACE: &str = "project:lme-corpus";

/// Sessions per write batch in a corpus-wide run, so a write phase of tens of thousands of rows
/// prints progress instead of going quiet for an hour.
const CORPUS_BATCH: usize = 500;

/// Every unique session written once, and the map from memory id to the sessions it holds.
pub struct SharedCorpus {
    pub owners: HashMap<String, Vec<String>>,
    pub rows_written: usize,
    /// Session id to the reason it never landed, so each question reports its own losses.
    pub missing: HashMap<String, String>,
}

/// Write each session the selected questions mention exactly once, before any search runs.
///
/// Writing per question and searching straight away meant question N competed against only the N
/// haystacks written so far, so early questions took an easier test than late ones and no question
/// saw the whole pool until the last. LongMemEval-S repeats sessions across haystacks (23,867
/// references to 19,195 ids); a session id always carries the same turns, so the first occurrence
/// stands for all of them. Its date is the first one seen, which matters only with --dates-in-text.
async fn write_corpus(
    c: &Client,
    selected: &[(usize, &Question)],
    args: &EvalArgs,
) -> Result<SharedCorpus> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut ids: Vec<String> = Vec::new();
    let mut sessions: Vec<Vec<crate::eval::Turn>> = Vec::new();
    let mut dates: Vec<String> = Vec::new();
    for (_, q) in selected {
        for (i, id) in q.haystack_session_ids.iter().enumerate() {
            if !seen.insert(id.as_str()) {
                continue;
            }
            ids.push(id.clone());
            sessions.push(q.haystack_sessions.get(i).cloned().unwrap_or_default());
            dates.push(q.haystack_dates.get(i).cloned().unwrap_or_default());
        }
    }
    crate::out(&format!(
        "corpus-wide: writing {} unique sessions into {CORPUS_NAMESPACE} before the first search",
        ids.len()
    ));

    let mut shared =
        SharedCorpus { owners: HashMap::new(), rows_written: 0, missing: HashMap::new() };
    let started = Instant::now();
    let mut start = 0usize;
    while start < ids.len() {
        let end = (start + CORPUS_BATCH).min(ids.len());
        let batch = Question {
            question_id: "corpus".into(),
            question_type: "corpus".into(),
            question: String::new(),
            question_date: None,
            haystack_session_ids: ids[start..end].to_vec(),
            haystack_sessions: sessions[start..end].to_vec(),
            haystack_dates: dates[start..end].to_vec(),
            answer_session_ids: vec![],
        };
        let built = corpus::build(
            c,
            &batch,
            CORPUS_NAMESPACE,
            args.protocol,
            args.chunk_chars,
            args.dates_in_text,
        )
        .await?;
        for (id, owners) in built.owners {
            shared.owners.entry(id).or_default().extend(owners);
        }
        shared.rows_written += built.rows_written;
        for session in built.missing_sessions {
            let reason = built
                .failures
                .iter()
                .find(|f| f.starts_with(&format!("{session}:")))
                .cloned()
                .unwrap_or_else(|| "no row".into());
            shared.missing.insert(session, reason);
        }
        start = end;
        crate::out(&format!(
            "  {end}/{} sessions written  {:>6.0}s",
            ids.len(),
            started.elapsed().as_secs_f64()
        ));
    }
    Ok(shared)
}

/// One corpus-wide question: search the shared pool, map hits through the corpus-wide owners, score.
async fn search_corpus(c: &Client, q: &Question, corpus: &SharedCorpus) -> Result<QuestionResult> {
    let started = Instant::now();
    let hits = search(c, &q.question, CORPUS_NAMESPACE, crate::eval::RETRIEVE_DEPTH).await?;
    let latency_ms = started.elapsed().as_millis() as u64;

    let hit_ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
    let (retrieved, unmapped) = sessions_in_rank_order(&hit_ids, &corpus.owners);
    // A question owns the losses of its own haystack, so a run's sessions_never_stored keeps
    // meaning what it means in the other modes.
    let mut failures: Vec<String> = q
        .haystack_session_ids
        .iter()
        .filter_map(|id| corpus.missing.get(id).map(|why| format!("{MISSING_SESSION}{id} ({why})")))
        .collect();
    for id in &unmapped {
        failures.push(format!("{UNMAPPED_HIT}{id}"));
    }

    let mut result = QuestionResult::score(
        q.question_id.clone(),
        q.question_type.clone(),
        q.answer_session_ids.clone(),
        retrieved,
    );
    result.write_failures = failures;
    result.rows_written = 0;
    result.latency_ms = latency_ms;
    Ok(result)
}

/// What configuration produced a report. Printed at the top of it, because a parity number and a
/// scale number look identical on the page and mean different things.
pub fn mode_name(args: &EvalArgs) -> String {
    if args.isolate {
        "isolated".into()
    } else if args.scoped {
        "scoped".into()
    } else {
        "corpus-wide".into()
    }
}

/// One question: write its haystack, search with its text, map hits back to sessions, score.
pub async fn run_question(
    c: &Client,
    q: &Question,
    index: usize,
    args: &EvalArgs,
) -> Result<QuestionResult> {
    let namespace = crate::eval::question_namespace(index);

    // Resume trades a guarantee of freshness for the ability to finish a run that was interrupted.
    // Rows already in the namespace are taken as this question's haystack without checking that
    // they came from this dataset, this protocol or this chunk size. A resumed run also cannot
    // report sessions_never_stored for the questions it reused, because it never attempted the
    // writes that would have failed.
    let mut probe: Vec<wire::Hit> = Vec::new();
    let mut probe_ms = 0u64;
    let mut resumed = false;
    if args.resume {
        let started = Instant::now();
        probe = search(c, &q.question, &namespace, crate::eval::RETRIEVE_DEPTH).await?;
        probe_ms = started.elapsed().as_millis() as u64;
        resumed = !probe.is_empty();
    }

    let (owners, mut failures, rows_written, missing) = if resumed {
        (owners_from_content(q, &probe), Vec::new(), 0usize, Vec::new())
    } else {
        let built =
            corpus::build(c, q, &namespace, args.protocol, args.chunk_chars, args.dates_in_text)
                .await?;
        (built.owners, built.failures, built.rows_written, built.missing_sessions)
    };

    for session in &missing {
        failures.push(format!("{MISSING_SESSION}{session}"));
    }

    // The clock covers the search alone. Writing the haystack is ingest, and folding it in would
    // report a number nobody can compare to a retrieval latency.
    let started = Instant::now();
    let (hits, latency_ms) = if resumed {
        (probe, probe_ms)
    } else {
        let hits = search(c, &q.question, &namespace, crate::eval::RETRIEVE_DEPTH).await?;
        (hits, started.elapsed().as_millis() as u64)
    };

    let hit_ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
    let (retrieved, unmapped) = sessions_in_rank_order(&hit_ids, &owners);
    for id in &unmapped {
        failures.push(format!("{UNMAPPED_HIT}{id}"));
    }

    let mut result = QuestionResult::score(
        q.question_id.clone(),
        q.question_type.clone(),
        q.answer_session_ids.clone(),
        retrieved,
    );
    result.write_failures = failures;
    result.rows_written = rows_written;
    result.latency_ms = latency_ms;

    // Isolation is a parity device and nothing more. agentmemory built a fresh index holding one
    // question's sessions, so matching their number means matching that. It removes every
    // distractor the rest of the corpus would have supplied, which is the opposite of a scale
    // test, so it is off unless somebody asks for the comparable number.
    if args.isolate {
        for id in owners.keys() {
            let (status, body) = c
                .http_request(reqwest::Method::DELETE, &format!("/admin/memory/{id}"), None)
                .await?;
            if status != 200 {
                let detail =
                    body.get("detail").and_then(|v| v.as_str()).unwrap_or("no detail").to_string();
                return Err(err(format!(
                    "isolation needs delete and the server refused it: HTTP {status}, {detail}.                      The eval credential needs mayDelete."
                )));
            }
        }
    }
    Ok(result)
}

/// What the server says it embeds with. A retrieval comparison run on a different embedder measures
/// the embedder, so the report names the one that produced the number.
async fn embedder_id(c: &Client) -> String {
    match c.http_get("/readyz").await {
        Ok((_, body)) => body
            .get("embedder")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Does this entry count against the run, or is it a note about the mapping.
fn is_write_failure(entry: &str) -> bool {
    !entry.starts_with(UNMAPPED_HIT)
}

/// Every question, in order, with progress on stdout.
pub async fn run(c: &Client, args: &EvalArgs) -> Result<RunReport> {
    let path = dataset_path(args)?;
    let questions = dataset::load(&path)?;
    let embedding_model = embedder_id(c).await;

    // Enumerate before filtering. The namespace follows a question's position in the dataset, so a
    // --skip-abstention run and a full run have to agree on it or --resume reuses another
    // question's haystack.
    let selected: Vec<(usize, &Question)> = questions
        .iter()
        .enumerate()
        .filter(|(_, q)| !(args.skip_abstention && q.is_abstention()))
        .filter(|(_, q)| args.only_type.as_deref().is_none_or(|t| q.question_type == t))
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();

    crate::out(&format!(
        "{} questions · {} · embedder {} · depth {}",
        selected.len(),
        args.protocol.as_str(),
        embedding_model,
        crate::eval::RETRIEVE_DEPTH
    ));

    let wall = Instant::now();
    let corpus_wide = !args.scoped && !args.isolate;
    if corpus_wide && args.resume {
        return Err(err(
            "--resume does not apply to --corpus-wide: the corpus is written once up front, so \
             rerun it whole against a fresh database",
        ));
    }
    let shared = match corpus_wide {
        true => Some(write_corpus(c, &selected, args).await?),
        false => None,
    };

    let mut results: Vec<QuestionResult> = Vec::with_capacity(selected.len());
    let mut hits_at_5 = 0.0f64;
    for (index, q) in selected {
        // A failed question stops the run rather than scoring itself zero, because a transient HTTP
        // error is not a retrieval miss and averaging it in would quietly lower the number. The
        // haystacks already written stay in the store, so `--resume` picks the run back up.
        let result = match &shared {
            Some(corpus) => search_corpus(c, q, corpus).await?,
            None => run_question(c, q, index, args).await?,
        };
        hits_at_5 += result.recall_any_at_5;
        results.push(result);
        let last = results.last().expect("just pushed");
        // A 500 question run is long, and a harness that prints nothing for an hour is one nobody
        // trusts enough to leave running.
        crate::out(&format!(
            "{:>4} {:<26} {}  R@5 {:>5.1}%  {:>6.0}s",
            index,
            truncate_type(&last.question_type),
            if last.recall_any_at_5 > 0.0 { "hit " } else { "miss" },
            100.0 * hits_at_5 / results.len() as f64,
            wall.elapsed().as_secs_f64(),
        ));
    }

    let mut per_type: std::collections::BTreeMap<String, crate::eval::Aggregate> =
        std::collections::BTreeMap::new();
    let mut types: Vec<String> = results.iter().map(|r| r.question_type.clone()).collect();
    types.sort();
    types.dedup();
    for ty in types {
        let of_type: Vec<QuestionResult> =
            results.iter().filter(|r| r.question_type == ty).cloned().collect();
        per_type.insert(ty, crate::eval::aggregate(&of_type));
    }

    let questions_with_write_failures =
        results.iter().filter(|r| r.write_failures.iter().any(|f| is_write_failure(f))).count();
    let sessions_never_stored = results
        .iter()
        .flat_map(|r| r.write_failures.iter())
        .filter(|f| f.starts_with(MISSING_SESSION))
        .count();

    let rows_at_end = if let Some(corpus) = &shared {
        corpus.rows_written
    } else if args.isolate {
        // Isolation deletes each haystack once it is scored, so the store never holds more than
        // one question's worth. Reporting a corpus size here would flatter the run.
        results.last().map_or(0, |r| r.rows_written)
    } else {
        results.iter().map(|r| r.rows_written).sum()
    };

    Ok(RunReport {
        protocol: args.protocol.as_str().to_string(),
        mode: mode_name(args),
        rows_at_end,
        embedding_model,
        retrieve_depth: crate::eval::RETRIEVE_DEPTH,
        overall: crate::eval::aggregate(&results),
        per_type,
        questions_with_write_failures,
        sessions_never_stored,
        wall_seconds: wall.elapsed().as_secs(),
        per_question: results,
    })
}

/// Keep the progress line inside a terminal. `single-session-preference` is the long one.
fn truncate_type(ty: &str) -> String {
    if ty.chars().count() <= 26 {
        return ty.to_string();
    }
    ty.chars().take(25).collect::<String>() + "…"
}

fn dataset_path(args: &EvalArgs) -> Result<PathBuf> {
    match &args.dataset {
        Some(p) => Ok(PathBuf::from(p)),
        None => dataset::default_path(),
    }
}

/// Where the JSON lands when nobody passed `--out`. Beside the dataset, named for the protocol, so
/// two protocols on one machine do not overwrite each other.
pub fn default_report_path(args: &EvalArgs) -> Result<PathBuf> {
    if let Some(out) = &args.out {
        return Ok(PathBuf::from(out));
    }
    let dataset = dataset_path(args)?;
    let dir = dataset.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    Ok(dir.join(format!("lumberroom-lme-{}.json", args.protocol.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Aggregate;

    fn owners(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(id, sessions)| {
                (id.to_string(), sessions.iter().map(|s| s.to_string()).collect())
            })
            .collect()
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_session_with_several_chunks_lands_once_at_its_best_rank() {
        let map = owners(&[
            ("m1", &["s_a"][..]),
            ("m2", &["s_a"][..]),
            ("m3", &["s_b"][..]),
            ("m4", &["s_a"][..]),
        ]);
        let (ordered, unmapped) = sessions_in_rank_order(&ids(&["m1", "m2", "m3", "m4"]), &map);
        assert_eq!(ordered, ids(&["s_a", "s_b"]));
        assert!(unmapped.is_empty());
    }

    #[test]
    fn the_rank_of_a_session_is_the_rank_of_its_first_chunk() {
        let map = owners(&[("m1", &["s_b"][..]), ("m2", &["s_a"][..]), ("m3", &["s_a"][..])]);
        let (ordered, _) = sessions_in_rank_order(&ids(&["m1", "m2", "m3"]), &map);
        assert_eq!(ordered, ids(&["s_b", "s_a"]));
    }

    #[test]
    fn a_deduplicated_row_enters_every_session_it_owns() {
        let map = owners(&[("m1", &["s_a", "s_b"][..])]);
        let (ordered, _) = sessions_in_rank_order(&ids(&["m1"]), &map);
        assert_eq!(ordered, ids(&["s_a", "s_b"]));
    }

    #[test]
    fn a_hit_with_no_owner_is_counted_and_keeps_its_rank() {
        let map = owners(&[("m2", &["s_a"][..])]);
        let (ordered, unmapped) = sessions_in_rank_order(&ids(&["m1", "m2"]), &map);
        assert_eq!(unmapped, ids(&["m1"]));
        // The orphan holds rank 1, so the gold session it displaced scores as rank 2 rather than
        // being promoted into the rank the orphan actually took.
        assert_eq!(ordered, ids(&["m1", "s_a"]));
        assert_eq!(crate::eval::mrr(&ordered, &ids(&["s_a"])), 0.5);
    }

    #[test]
    fn an_orphan_note_is_not_a_write_failure() {
        assert!(!is_write_failure(&format!("{UNMAPPED_HIT}abc")));
        assert!(is_write_failure(&format!("{MISSING_SESSION}answer_1")));
        assert!(is_write_failure("write refused: content too long"));
    }

    fn result(ty: &str, gold: &[&str], retrieved: &[&str]) -> QuestionResult {
        QuestionResult::score(
            format!("q-{ty}-{}", retrieved.len()),
            ty.to_string(),
            ids(gold),
            ids(retrieved),
        )
    }

    #[test]
    fn the_aggregate_averages_the_hand_built_results() {
        let results = vec![
            result("single-session-user", &["g"], &["g", "x"]),
            result("single-session-user", &["g"], &["x", "y", "z", "a", "b", "g"]),
            result("multi-session", &["g"], &["x", "y"]),
        ];
        let overall: Aggregate = crate::eval::aggregate(&results);
        assert_eq!(overall.questions, 3);
        // One at rank 1, one at rank 6, one absent.
        assert!((overall.recall_any_at_5 - 1.0 / 3.0).abs() < 1e-12);
        assert!((overall.recall_any_at_10 - 2.0 / 3.0).abs() < 1e-12);
        assert!((overall.mrr - (1.0 + 1.0 / 6.0) / 3.0).abs() < 1e-12);

        let single: Vec<QuestionResult> =
            results.iter().filter(|r| r.question_type == "single-session-user").cloned().collect();
        let agg = crate::eval::aggregate(&single);
        assert_eq!(agg.questions, 2);
        assert!((agg.recall_any_at_5 - 0.5).abs() < 1e-12);
    }

    #[test]
    fn the_default_report_lands_beside_the_dataset() {
        let args = EvalArgs {
            dataset: Some("/data/lme_s.json".to_string()),
            protocol: Protocol::Chunked,
            limit: None,
            skip_abstention: false,
            chunk_chars: 1200,
            out: None,
            json: false,
            resume: false,
            isolate: false,
            scoped: true,
            dates_in_text: false,
            only_type: None,
        };
        assert_eq!(
            default_report_path(&args).unwrap(),
            PathBuf::from("/data/lumberroom-lme-chunked.json")
        );
    }
}

#[cfg(test)]
mod resume_mapping {
    use super::*;
    use crate::eval::Turn;

    fn question(turns: Vec<Turn>) -> Question {
        Question {
            question_id: "q".into(),
            question_type: "multi-session".into(),
            question: String::new(),
            question_date: None,
            haystack_session_ids: vec!["s1".into()],
            haystack_sessions: vec![turns],
            haystack_dates: vec![],
            answer_session_ids: vec!["s1".into()],
        }
    }

    fn hit(id: &str, content: &str) -> wire::Hit {
        wire::Hit {
            id: id.into(),
            namespace: "project:lme-q0000".into(),
            content: content.into(),
            score: 1.0,
        }
    }

    #[test]
    fn a_continuation_chunk_still_maps_to_its_session() {
        // The chunker repeats the role on a split turn; the rendered session carries it once.
        let long = "z".repeat(4000);
        let q = question(vec![Turn { role: "user".into(), content: long.clone() }]);
        let tail = format!("user: {}", &long[2000..3000]);
        let owners = owners_from_content(&q, &[hit("m1", &tail)]);
        assert_eq!(owners.get("m1"), Some(&vec!["s1".to_string()]));
    }

    #[test]
    fn a_whole_session_still_maps_by_exact_match() {
        let q = question(vec![Turn { role: "user".into(), content: "the port is 5433".into() }]);
        let owners = owners_from_content(&q, &[hit("m1", "user: the port is 5433")]);
        assert_eq!(owners.get("m1"), Some(&vec!["s1".to_string()]));
    }

    #[test]
    fn a_hit_from_nowhere_owns_nothing_rather_than_guessing() {
        let q = question(vec![Turn { role: "user".into(), content: "the port is 5433".into() }]);
        assert!(owners_from_content(&q, &[hit("m1", "unrelated text")]).is_empty());
    }

    #[test]
    fn stripping_a_role_leaves_text_that_carries_a_colon_alone() {
        assert_eq!(strip_role_prefix("user: hello"), "hello");
        assert_eq!(strip_role_prefix("Error: something"), "Error: something");
        assert_eq!(strip_role_prefix("no colon here"), "no colon here");
    }
}
