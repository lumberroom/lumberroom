//! The report, in the shape somebody can put beside agentmemory's table.

use crate::client::{err, Result};
use crate::eval::runner::UNMAPPED_HIT;
use crate::eval::RunReport;

/// agentmemory's published LongMemEval-S retrieval scores, transcribed from their
/// `benchmark/LONGMEMEVAL.md`, the BM25+Vector row.
///
/// A different retrieval stack produced these on a different harness. They are here so a reader can
/// see the gap without hunting for the source, and the DEVIATIONS block below says what a gap of a
/// given size does and does not mean. Do not copy either column into a claim without it.
const AGENTMEMORY: [(&str, f64); 5] = [
    ("recall_any@5", 95.2),
    ("recall_any@10", 98.6),
    ("recall_any@20", 99.4),
    ("NDCG@10", 87.9),
    ("MRR", 88.2),
];

/// The overall table, the per-type table, and the deviations that decide what the number means.
pub fn print(report: &RunReport) {
    crate::out(render(report).trim_end());
}

pub fn render(report: &RunReport) -> String {
    let mut s = String::new();
    s.push_str(&header(report));
    s.push('\n');
    s.push_str(&overall_table(report));
    s.push('\n');
    s.push_str(&per_type_table(report));
    s.push('\n');
    s.push_str(&deviations(report));
    s
}

fn header(report: &RunReport) -> String {
    format!(
        "LongMemEval-S · {} questions · {} · embedder {} · depth {} · {}s\n",
        report.overall.questions,
        report.protocol,
        report.embedding_model,
        report.retrieve_depth,
        report.wall_seconds,
    )
}

fn overall_table(report: &RunReport) -> String {
    let ours = [
        report.overall.recall_any_at_5,
        report.overall.recall_any_at_10,
        report.overall.recall_any_at_20,
        report.overall.ndcg_at_10,
        report.overall.mrr,
    ];
    // The right column is headed so nobody reads it as ours.
    let mut s =
        String::from("metric           lumberroom     agentmemory (published, their stack)\n");
    for (i, (name, theirs)) in AGENTMEMORY.iter().enumerate() {
        s.push_str(&format!("{:<15} {:>5.1}%   {:>5.1}%\n", name, 100.0 * ours[i], theirs));
    }
    s
}

fn per_type_table(report: &RunReport) -> String {
    let mut rows: Vec<(&String, &crate::eval::Aggregate)> = report.per_type.iter().collect();
    // Count descending, then name, so two types of equal size keep a stable order between runs.
    rows.sort_by(|a, b| b.1.questions.cmp(&a.1.questions).then_with(|| a.0.cmp(b.0)));

    let mut s = String::from("question type                  n     R@5     R@10\n");
    for (name, agg) in rows {
        s.push_str(&format!(
            "{:<28} {:>3}   {:>5.1}%   {:>5.1}%\n",
            name,
            agg.questions,
            100.0 * agg.recall_any_at_5,
            100.0 * agg.recall_any_at_10,
        ));
    }
    s
}

/// Printed every run. A retrieval number without this list reads as a like-for-like comparison and
/// it is not one.
fn deviations(report: &RunReport) -> String {
    let unmapped: usize = report
        .per_question
        .iter()
        .flat_map(|r| r.write_failures.iter())
        .filter(|f| f.starts_with(UNMAPPED_HIT))
        .count();
    // A question that wrote nothing either resumed onto rows already in the store or built an
    // empty haystack. Either way its sessions_never_stored is not something this run observed.
    let reused = report
        .per_question
        .iter()
        .filter(|r| r.rows_written == 0 && r.writes_collapsed == 0)
        .count();

    let mut s = String::from("DEVIATIONS from agentmemory's published run\n");
    for line in [
        "  ranking     lumberroom blends Postgres full text search and HNSW by a weighted sum. Theirs",
        "              fused BM25 and brute-force cosine by RRF.",
        "  lexical     their lexical side stems, expands synonyms and matches prefixes. Postgres",
        "              FTS does none of that beyond the english configuration's own stemming.",
        "  truncation  their harness embedded the first 512 characters of a session. This cuts at",
        "              the model's 512 tokens, a different bound and usually a longer one.",
        "  storage     their harness replaced the store with an in-process map and built a fresh",
        "              index per question. This writes through HTTP into real Postgres.",
    ] {
        s.push_str(line);
        s.push('\n');
    }
    s.push_str(&format!(
        "  writes      {} of {} questions had a haystack that did not store completely.\n",
        report.questions_with_write_failures, report.overall.questions,
    ));
    s.push_str(&format!(
        "  sessions    {} haystack sessions never reached the store.\n",
        report.sessions_never_stored,
    ));
    if report.writes_collapsed > 0 {
        s.push_str(&format!(
            "  collapses   {} accepted writes landed on a row already stored, so the store holds\n",
            report.writes_collapsed,
        ));
        s.push_str(
            "              that many fewer rows than writes. Each such session maps to that row.\n",
        );
    }
    // In corpus-wide the same counter means the opposite thing. A hit from another question's
    // haystack is a distractor beating this question's sessions to a slot, which is the whole
    // point of that mode. Reading it as a harness fault there would hide the finding.
    s.push_str(&if report.mode == "corpus-wide" {
        format!(
            "  distractors {unmapped} of the retrieved rows belonged to another question. In this \
             mode that is the measurement rather than a fault.\n"
        )
    } else {
        format!("  mapping     {unmapped} search hits carried an id no session owned.\n")
    });
    if reused > 0 {
        s.push_str(&format!(
            "  resume      {reused} question{} wrote no rows, so this run never observed whether\n",
            if reused == 1 { "" } else { "s" },
        ));
        s.push_str("              their haystacks stored completely.\n");
    }

    if report.sessions_never_stored > 0 {
        s.push('\n');
        s.push_str(&format!(
            "WARNING: {} haystack sessions never reached the store. A question whose gold session\n",
            report.sessions_never_stored,
        ));
        s.push_str(
            "is one of them cannot be answered for a reason that has nothing to do with ranking.\n",
        );
        s.push_str("This number is not comparable to agentmemory's until that count is zero.\n");
    }
    if unmapped > 0 {
        s.push_str(&format!(
            "WARNING: {unmapped} hits had no owning session. The corpus map and the store disagree,\n"
        ));
        s.push_str("which is a harness bug, and every one of those hits scored as a miss.\n");
    }
    s
}

pub fn write_json(report: &RunReport, path: &std::path::Path) -> Result<()> {
    // The whole report including per_question, so a later run can be diffed question by question.
    // The aggregate alone tells you the score moved and never which questions moved it.
    let body = serde_json::to_string_pretty(report)
        .map_err(|e| err(format!("cannot serialise the report: {e}")))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
    }
    std::fs::write(path, body).map_err(|e| err(format!("cannot write {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::runner::MISSING_SESSION;
    use crate::eval::{aggregate, Aggregate, QuestionResult, RunReport};
    use std::collections::BTreeMap;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn built(missing: usize, unmapped: usize) -> RunReport {
        let mut results = vec![
            QuestionResult::score(
                "q1".into(),
                "single-session-user".into(),
                ids(&["g1"]),
                ids(&["g1", "x"]),
            ),
            QuestionResult::score(
                "q2".into(),
                "single-session-user".into(),
                ids(&["g2"]),
                ids(&["x", "y"]),
            ),
            QuestionResult::score("q3".into(), "multi-session".into(), ids(&["g3"]), ids(&["g3"])),
        ];
        for r in results.iter_mut() {
            r.rows_written = 53;
        }
        for i in 0..missing {
            results[0].write_failures.push(format!("{MISSING_SESSION}s{i}"));
        }
        for i in 0..unmapped {
            results[1].write_failures.push(format!("{UNMAPPED_HIT}m{i}"));
        }

        let mut per_type: BTreeMap<String, Aggregate> = BTreeMap::new();
        per_type.insert("single-session-user".into(), aggregate(&results[..2]));
        per_type.insert("multi-session".into(), aggregate(&results[2..]));

        RunReport {
            protocol: "session-as-document".into(),
            mode: "scoped".into(),
            rows_at_end: 159,
            writes_collapsed: 0,
            embedding_model: "hash-768".into(),
            retrieve_depth: 20,
            overall: aggregate(&results),
            per_type,
            questions_with_write_failures: if missing > 0 { 1 } else { 0 },
            sessions_never_stored: missing,
            wall_seconds: 42,
            per_question: results,
        }
    }

    #[test]
    fn the_overall_table_shows_both_columns_and_labels_whose_is_whose() {
        let text = render(&built(0, 0));
        assert!(text.contains("agentmemory (published, their stack)"), "{text}");
        assert!(text.contains("95.2%"), "their recall_any@5 has to be on the page: {text}");
        // Two of three questions found gold in the top five.
        assert!(text.contains(" 66.7%"), "{text}");
    }

    #[test]
    fn the_per_type_table_sorts_by_count_descending() {
        let text = render(&built(0, 0));
        let single = text.find("single-session-user   ").expect("per-type row");
        let multi = text.find("multi-session         ").expect("per-type row");
        assert!(single < multi, "the two-question type comes first:\n{text}");
    }

    #[test]
    fn the_deviations_block_is_printed_every_time() {
        let text = render(&built(0, 0));
        assert!(text.contains("DEVIATIONS from agentmemory's published run"), "{text}");
        assert!(text.contains("RRF"), "{text}");
        assert!(text.contains("512 characters"), "{text}");
        assert!(text.contains("512 tokens"), "{text}");
        assert!(text.contains("real Postgres"), "{text}");
        assert!(text.contains("did not store completely"), "{text}");
        assert!(!text.contains("WARNING"), "a clean run has nothing to warn about:\n{text}");
    }

    #[test]
    fn a_session_that_never_stored_prints_the_warning() {
        let text = render(&built(4, 0));
        assert!(text.contains("WARNING: 4 haystack sessions never reached the store"), "{text}");
        assert!(text.contains("not comparable to agentmemory's"), "{text}");
    }

    #[test]
    fn an_unmapped_hit_prints_its_own_warning_and_its_count() {
        let text = render(&built(0, 3));
        assert!(text.contains("3 search hits carried an id no session owned"), "{text}");
        assert!(text.contains("WARNING: 3 hits had no owning session"), "{text}");
    }

    #[test]
    fn a_question_that_wrote_nothing_is_disclosed() {
        let mut report = built(0, 0);
        assert!(!render(&report).contains("resume      "), "every question wrote rows");
        report.per_question[0].rows_written = 0;
        assert!(render(&report).contains("resume      1 question wrote no rows"));
    }

    #[test]
    fn collapsed_writes_are_reported_beside_the_rows() {
        let mut report = built(0, 0);
        assert!(!render(&report).contains("collapses"), "no collapse, no line");
        report.writes_collapsed = 13;
        let text = render(&report);
        assert!(text.contains("collapses   13 accepted writes"), "{text}");
    }

    #[test]
    fn a_question_whose_every_write_collapsed_still_wrote() {
        let mut report = built(0, 0);
        report.per_question[0].rows_written = 0;
        report.per_question[0].writes_collapsed = 2;
        assert!(!render(&report).contains("resume      "), "a collapse is an accepted write");
    }

    #[test]
    fn the_json_carries_every_question() {
        let dir = std::env::temp_dir().join(format!("lumberroom-eval-{}", std::process::id()));
        let path = dir.join("report.json");
        write_json(&built(1, 0), &path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["per_question"].as_array().unwrap().len(), 3);
        assert_eq!(v["per_question"][0]["question_id"], "q1");
        assert_eq!(v["sessions_never_stored"], 1);
        assert_eq!(v["writes_collapsed"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
