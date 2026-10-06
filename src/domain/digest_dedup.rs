//! Near-duplicate collapse for the bootstrap digest.
//!
//! The digest ranks by recency and nothing else (decision 0025). Embeddings get one job: a row that
//! restates a newer row spends a slot on nothing new, so it is dropped and the next candidate takes
//! the slot. Restatements include corrections, "port 5432" beside "port 5433", which is why the
//! newer row always wins, whichever section either one would have printed in.
//!
//! No I/O and no vectors. The adapter compares the stored vectors and hands over the pairs at or
//! above the threshold; this module decides which member of each pair survives and fills the
//! sections from what is left.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

/// The rows every section's pool holds, after the duplicates are dropped, and the rows chosen so far.
#[derive(Debug)]
pub struct Selection {
    dropped: HashSet<String>,
    chosen: HashSet<String>,
}

impl Selection {
    /// `rows` is every pooled row with its age, in any order and with repeats: a row can sit in the
    /// profile pool and the recent pool at once. `pairs` holds the near-duplicate pairs, either way
    /// round.
    ///
    /// Newest first, ties broken by id so two runs agree, each row survives unless a survivor
    /// already restates it. A dropped row drops nothing: when b restates a and c restates b alone,
    /// a and c both stay.
    pub fn new<'a>(
        rows: impl IntoIterator<Item = (&'a str, DateTime<Utc>)>,
        pairs: &[(String, String)],
    ) -> Self {
        let mut ordered: Vec<(&str, DateTime<Utc>)> = rows.into_iter().collect();
        ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ordered.dedup_by(|a, b| a.0 == b.0);

        let mut twins: HashMap<&str, Vec<&str>> = HashMap::new();
        for (a, b) in pairs {
            twins.entry(a.as_str()).or_default().push(b.as_str());
            twins.entry(b.as_str()).or_default().push(a.as_str());
        }

        let mut kept: HashSet<&str> = HashSet::new();
        let mut dropped = HashSet::new();
        for (id, _) in ordered {
            let restates_a_survivor =
                twins.get(id).is_some_and(|t| t.iter().any(|other| kept.contains(other)));
            if restates_a_survivor {
                dropped.insert(id.to_string());
            } else {
                kept.insert(id);
            }
        }
        Self { dropped, chosen: HashSet::new() }
    }

    /// Rows dropped as the older half of a near-duplicate pair.
    pub fn dropped(&self) -> usize {
        self.dropped.len()
    }

    /// Indices into `pool` of the rows one section prints, in pool order, at most `limit` of them.
    /// A row an earlier call chose is skipped, so recent never repeats profile or project.
    pub fn pick(&mut self, pool: &[&str], limit: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for (i, id) in pool.iter().enumerate() {
            if out.len() >= limit {
                break;
            }
            if self.chosen.contains(*id) || self.dropped.contains(*id) {
                continue;
            }
            self.chosen.insert((*id).to_string());
            out.push(i);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours_ago: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 - hours_ago * 3600, 0).unwrap()
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn the_newer_of_two_near_duplicates_keeps_the_slot_and_the_next_row_fills_it() {
        let rows = [("new", at(1)), ("old", at(2)), ("other", at(3))];
        let mut s = Selection::new(rows, &[pair("old", "new")]);
        assert_eq!(s.pick(&["new", "old", "other"], 2), vec![0, 2]);
        assert_eq!(s.dropped(), 1);
    }

    #[test]
    fn rows_in_no_pair_all_stay() {
        let rows = [("a", at(1)), ("b", at(2))];
        let mut s = Selection::new(rows, &[]);
        assert_eq!(s.pick(&["a", "b"], 5), vec![0, 1]);
        assert_eq!(s.dropped(), 0);
    }

    #[test]
    fn a_newer_row_in_a_later_section_beats_its_older_twin_in_an_earlier_one() {
        // A correction filed under a project restates a profile rule. The profile copy is stale.
        let rows = [("rule", at(5)), ("fix", at(1)), ("other", at(6))];
        let mut s = Selection::new(rows, &[pair("rule", "fix")]);
        assert_eq!(s.pick(&["rule", "other"], 10), vec![1], "profile");
        assert_eq!(s.pick(&["fix"], 10), vec![0], "project");
    }

    #[test]
    fn a_dropped_row_does_not_drop_the_rows_it_restates() {
        // b restates a, c restates b, and c does not restate a. b goes; c answers to a alone.
        let rows = [("a", at(1)), ("b", at(2)), ("c", at(3))];
        let mut s = Selection::new(rows, &[pair("a", "b"), pair("b", "c")]);
        assert_eq!(s.pick(&["a", "b", "c"], 10), vec![0, 2]);
    }

    #[test]
    fn a_tie_on_age_goes_to_the_smaller_id_in_either_order() {
        for rows in [[("x", at(1)), ("y", at(1))], [("y", at(1)), ("x", at(1))]] {
            let mut s = Selection::new(rows, &[pair("y", "x")]);
            assert_eq!(s.pick(&["x", "y"], 10), vec![0]);
        }
    }

    #[test]
    fn a_row_in_two_pools_counts_once() {
        let rows = [("a", at(1)), ("a", at(1)), ("b", at(2))];
        let mut s = Selection::new(rows, &[pair("a", "b")]);
        assert_eq!(s.dropped(), 1);
        assert_eq!(s.pick(&["a"], 10), vec![0]);
        assert_eq!(s.pick(&["a", "b"], 10), Vec::<usize>::new(), "recent repeats nothing");
    }

    #[test]
    fn a_later_section_skips_rows_an_earlier_one_chose() {
        let rows = [("rule", at(3)), ("fresh", at(1))];
        let mut s = Selection::new(rows, &[]);
        assert_eq!(s.pick(&["rule"], 10), vec![0]);
        assert_eq!(s.pick(&["fresh", "rule"], 10), vec![0]);
    }

    #[test]
    fn the_limit_counts_chosen_rows_and_not_dropped_ones() {
        let rows = [("a1", at(1)), ("a2", at(2)), ("a3", at(3)), ("b", at(4))];
        let mut s = Selection::new(rows, &[pair("a1", "a2"), pair("a1", "a3"), pair("a2", "a3")]);
        assert_eq!(s.pick(&["a1", "a2", "a3", "b"], 2), vec![0, 3]);
    }

    #[test]
    fn a_pair_naming_a_row_outside_the_pools_changes_nothing() {
        let rows = [("a", at(2))];
        let mut s = Selection::new(rows, &[pair("ghost", "a")]);
        assert_eq!(s.pick(&["a"], 10), vec![0]);
        assert_eq!(s.dropped(), 0);
    }
}
