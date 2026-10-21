//! Near-duplicate collapse for the bootstrap digest.
//!
//! A row that restates a newer row spends a slot on nothing new, so it is dropped and the next
//! candidate takes the slot (decision 0025). Restatements include corrections, "port 5432" beside
//! "port 5433", which is why the newer row always wins, whichever section either one would have
//! printed in. The pass runs over every row the digest can print: the profile, project, recent and
//! related pools (decision 0034).
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
    /// Each survivor that dropped an older twin, with the twins it dropped.
    displaced: HashMap<String, Vec<String>>,
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
        let mut displaced: HashMap<String, Vec<String>> = HashMap::new();
        for (id, _) in ordered {
            let survivors: Vec<&str> = twins
                .get(id)
                .map(|t| t.iter().copied().filter(|other| kept.contains(other)).collect())
                .unwrap_or_default();
            if survivors.is_empty() {
                kept.insert(id);
            } else {
                for s in survivors {
                    displaced.entry(s.to_string()).or_default().push(id.to_string());
                }
                dropped.insert(id.to_string());
            }
        }
        Self { dropped, chosen: HashSet::new(), displaced }
    }

    /// The dropped rows that one section would have printed with no near-duplicate pass: those
    /// among the first `limit` rows of `pool` that no earlier section chose. Call it before that
    /// section's `pick`, which marks rows chosen.
    pub fn lost_from(&self, pool: &[&str], limit: usize) -> Vec<String> {
        pool.iter()
            .filter(|id| !self.chosen.contains(**id))
            .take(limit)
            .filter(|id| self.dropped.contains(**id))
            .map(|id| (*id).to_string())
            .collect()
    }

    /// True when `id` survived and dropped a twin named in `lost`. Such a row restates a fact the
    /// digest would have printed, so the related section that picks it first keeps the fact on
    /// the page. A twin that would never have printed, such as a related-only row or one past its
    /// section's limit, promotes nothing.
    pub fn displaced_one_of(&self, id: &str, lost: &HashSet<String>) -> bool {
        self.displaced.get(id).is_some_and(|twins| twins.iter().any(|t| lost.contains(t)))
    }

    /// The twins `id` dropped, empty for a dropped row and for one that dropped nothing. The
    /// profile reads it so a survivor carries the standing-rule mark of a tagged twin it replaced.
    pub fn displaced_by(&self, id: &str) -> &[String] {
        self.displaced.get(id).map_or(&[], Vec::as_slice)
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

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_survivor_counts_only_for_a_twin_a_section_would_have_printed() {
        let rows = [("new", at(1)), ("old", at(2)), ("lone", at(3))];
        let s = Selection::new(rows, &[pair("old", "new")]);
        assert!(s.displaced_one_of("new", &set(&["old"])));
        assert!(!s.displaced_one_of("new", &set(&[])), "a related-only twin promotes nothing");
        assert!(
            !s.displaced_one_of("old", &set(&["old"]))
                && !s.displaced_one_of("lone", &set(&["old"]))
        );
    }

    #[test]
    fn a_dropped_row_marks_nothing() {
        // c restates b alone, b restates a: b drops, and a and c both stay. Only a dropped b.
        let rows = [("a", at(1)), ("b", at(2)), ("c", at(3))];
        let s = Selection::new(rows, &[pair("a", "b"), pair("b", "c")]);
        assert!(s.displaced_one_of("a", &set(&["b"])));
        assert!(!s.displaced_one_of("c", &set(&["b"])));
    }

    #[test]
    fn a_survivor_names_the_twins_it_dropped_and_no_other_row_names_any() {
        // b restates a and drops; c restates b alone and stays.
        let rows = [("a", at(1)), ("b", at(2)), ("c", at(3))];
        let s = Selection::new(rows, &[pair("a", "b"), pair("b", "c")]);
        assert_eq!(s.displaced_by("a"), ["b".to_string()]);
        assert!(s.displaced_by("b").is_empty(), "a dropped row");
        assert!(s.displaced_by("c").is_empty(), "a survivor that dropped nothing");
        assert!(s.displaced_by("ghost").is_empty());
    }

    #[test]
    fn a_section_loses_only_the_dropped_rows_inside_its_limit() {
        // c restates a and drops. Project's pool is b, c, d.
        let rows = [("a", at(1)), ("b", at(2)), ("c", at(3)), ("d", at(4))];
        let s = Selection::new(rows, &[pair("a", "c")]);
        assert_eq!(s.lost_from(&["b", "c", "d"], 2), vec!["c".to_string()]);
        assert!(s.lost_from(&["b", "c", "d"], 1).is_empty(), "c sat past the limit");
    }

    #[test]
    fn a_row_an_earlier_section_chose_holds_no_slot_in_the_window() {
        let rows = [("a", at(1)), ("b", at(2)), ("c", at(3))];
        let mut s = Selection::new(rows, &[pair("a", "c")]);
        assert_eq!(s.pick(&["b"], 1), vec![0]);
        assert_eq!(s.lost_from(&["b", "c"], 1), vec!["c".to_string()]);
    }

    #[test]
    fn a_pair_naming_a_row_outside_the_pools_changes_nothing() {
        let rows = [("a", at(2))];
        let mut s = Selection::new(rows, &[pair("ghost", "a")]);
        assert_eq!(s.pick(&["a"], 10), vec![0]);
        assert_eq!(s.dropped(), 0);
    }
}
