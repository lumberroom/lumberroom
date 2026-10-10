//! The per-hit log: one JSON line per question, beside the report.
//!
//! The report says how often a gold session reached the top five. A sweep over blends needs to see
//! why: the cosine and keyword score that put each hit where it landed, and where the gold one sat.
//! Those numbers come from the server only under `SEARCH_DEBUG_SCORES=true`; without it each hit
//! still carries its rank, its sessions and its gold flag, and the three score fields are null.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::client::{err, Result};
use crate::eval::Question;
use crate::wire;

/// `report.json` becomes `report.hits.jsonl`. A path with no `.json` suffix gains the suffix.
pub fn hits_path(report: &Path) -> PathBuf {
    let name = report.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = name.strip_suffix(".json").unwrap_or(&name);
    report.with_file_name(format!("{stem}.hits.jsonl"))
}

/// One question's line.
///
/// `rank` is the hit's position in the store's answer, before hits are folded into sessions, so it
/// is the rank the fused score decided. A hit whose memory id owns no session lists none, and is
/// never gold.
pub fn question_line(
    q: &Question,
    hits: &[wire::Hit],
    owners: &HashMap<String, Vec<String>>,
) -> Value {
    let lines: Vec<Value> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let sessions: Vec<String> = owners.get(&h.id).cloned().unwrap_or_default();
            let gold = sessions.iter().any(|s| q.answer_session_ids.contains(s));
            let mut line = json!({
                "rank": i + 1,
                "memory_id": h.id,
                "session_ids": sessions,
                "score": h.score,
                "cosine": h.scores.map(|s| s.cosine),
                "keyword": h.scores.map(|s| s.keyword),
                "fused": h.scores.map(|s| s.fused),
                "gold": gold,
            });
            if let Some(s) = h.scores {
                for (key, value) in [
                    ("cosine_norm", s.cosine_norm.map(Value::from)),
                    ("vector_rank", s.vector_rank.map(Value::from)),
                    ("keyword_rank", s.keyword_rank.map(Value::from)),
                ] {
                    if let Some(v) = value {
                        line[key] = v;
                    }
                }
            }
            line
        })
        .collect();
    json!({
        "question_id": q.question_id,
        "question_type": q.question_type,
        "gold_session_ids": q.answer_session_ids,
        "hits": lines,
    })
}

/// The open log. Each line is flushed as it is written, so a run that dies at question 300 leaves
/// 300 lines rather than a buffer nobody saw.
///
/// Off when the run writes no report. The default report path sits beside the dataset, which the
/// script mounts read-only, so creating a log there would stop a run that never asked for a file.
pub struct HitLog {
    path: PathBuf,
    file: Option<std::io::BufWriter<std::fs::File>>,
}

impl HitLog {
    /// Truncates. A log from an earlier run under the same name would otherwise carry its lines
    /// into this one and double every question.
    pub fn create(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
        }
        let file = std::fs::File::create(&path)
            .map_err(|e| err(format!("cannot write {}: {e}", path.display())))?;
        Ok(Self { path, file: Some(std::io::BufWriter::new(file)) })
    }

    /// A log that writes nothing.
    pub fn off() -> Self {
        Self { path: PathBuf::new(), file: None }
    }

    /// Where the lines go, or `None` when the log is off.
    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|_| self.path.as_path())
    }

    pub fn write(&mut self, line: &Value) -> Result<()> {
        let Some(file) = self.file.as_mut() else { return Ok(()) };
        let fail = |e: std::io::Error| err(format!("cannot write {}: {e}", self.path.display()));
        serde_json::to_writer(&mut *file, line)
            .map_err(|e| err(format!("cannot serialise a hit line: {e}")))?;
        file.write_all(b"\n").map_err(fail)?;
        file.flush().map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> Question {
        Question {
            question_id: "q1".into(),
            question_type: "multi-session".into(),
            question: "where".into(),
            question_date: None,
            haystack_session_ids: vec!["s_gold".into(), "s_other".into()],
            haystack_sessions: vec![vec![], vec![]],
            haystack_dates: vec![],
            answer_session_ids: vec!["s_gold".into()],
        }
    }

    fn hit(id: &str, scores: Option<wire::HitScores>) -> wire::Hit {
        wire::Hit {
            id: id.into(),
            namespace: "project:lme-q0000".into(),
            content: String::new(),
            score: 0.5,
            scores,
        }
    }

    #[test]
    fn the_log_sits_beside_the_report() {
        assert_eq!(hits_path(Path::new("/r/run.json")), PathBuf::from("/r/run.hits.jsonl"));
        assert_eq!(hits_path(Path::new("/r/run")), PathBuf::from("/r/run.hits.jsonl"));
    }

    #[test]
    fn a_line_carries_rank_sessions_scores_and_the_gold_flag() {
        let owners: HashMap<String, Vec<String>> = [
            ("m1".to_string(), vec!["s_other".to_string()]),
            ("m2".to_string(), vec!["s_gold".to_string()]),
        ]
        .into();
        let parts = wire::HitScores {
            cosine: 0.81,
            keyword: 0.2,
            fused: 0.88,
            cosine_norm: Some(1.0),
            vector_rank: None,
            keyword_rank: None,
        };
        let hits = [hit("m1", Some(parts)), hit("m2", Some(parts)), hit("orphan", None)];
        let line = question_line(&question(), &hits, &owners);

        assert_eq!(line["question_id"], "q1");
        assert_eq!(line["question_type"], "multi-session");
        let h = line["hits"].as_array().unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[0]["rank"], 1);
        assert_eq!(h[0]["gold"], false);
        assert_eq!(h[1]["rank"], 2);
        assert_eq!(h[1]["gold"], true);
        assert_eq!(h[1]["session_ids"], json!(["s_gold"]));
        assert_eq!(h[1]["cosine"], 0.81);
        assert_eq!(h[1]["keyword"], 0.2);
        assert_eq!(h[1]["fused"], 0.88);
        assert_eq!(h[1]["cosine_norm"], 1.0);
        assert!(h[1].get("vector_rank").is_none(), "an absent part stays absent");
        // A server without debug scores still logs the rank and the gold flag.
        assert_eq!(h[2]["cosine"], Value::Null);
        assert_eq!(h[2]["session_ids"], json!([]));
        assert_eq!(h[2]["gold"], false);
    }

    #[test]
    fn the_log_writes_one_line_per_question() {
        let dir = std::env::temp_dir().join(format!("lr-hitlog-{}", std::process::id()));
        let path = dir.join("run.hits.jsonl");
        let mut log = HitLog::create(path.clone()).unwrap();
        log.write(&json!({"question_id": "a"})).unwrap();
        log.write(&json!({"question_id": "b"})).unwrap();
        drop(log);
        let text = std::fs::read_to_string(&path).unwrap();
        let ids: Vec<String> = text
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["question_id"].as_str().unwrap().into()
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_log_that_is_off_accepts_lines_and_writes_nothing() {
        let mut log = HitLog::off();
        assert!(log.path().is_none());
        log.write(&json!({"question_id": "a"})).unwrap();
    }
}
