//! Orders the digest's profile pool and decides whether a model's floor may gate the related
//! section (decision 0034).
//!
//! No I/O and no vectors: the adapter scores each pooled row against the anchor and hands the
//! scores over by id, and the service says which rows are preferences.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::similarity::Source;

/// One profile-pool row as the ranking sees it.
#[derive(Debug, Clone, Copy)]
pub struct ProfileRow<'a> {
    pub id: &'a str,
    pub created_at: DateTime<Utc>,
    /// A `user:me` row carrying one of `BOOTSTRAP_RULE_TAGS`, or the survivor that dropped one.
    pub preference: bool,
    /// The near-duplicate pass dropped this row, and the section's pick will skip it.
    pub dropped: bool,
}

/// True when `tags` holds any of `rule_tags`. Both sides are in the store's normalised spelling.
pub fn is_standing(tags: &[String], rule_tags: &[String]) -> bool {
    tags.iter().any(|t| rule_tags.contains(t))
}

/// Indices into `rows` in print order (decision 0034, amended 21 October 2026). Up to
/// `preference_limit` preferences lead, ranked the way the rest are: highest score first, then
/// newest, then id. Any writer can tag a row `preference`, so age alone let a newer personal row
/// take a slot from an older rule near the session's work. A dropped twin keeps its place in the
/// lead, where the section notes it as a row it would have printed, and takes no slot, because
/// the pick skips it. Rows without a score follow every scored row, newest first. With no anchor
/// no row has a score, so the lead falls back to the newest preferences.
pub fn profile_order(
    rows: &[ProfileRow<'_>],
    scores: &HashMap<String, f64>,
    preference_limit: usize,
) -> Vec<usize> {
    let ranked = |a: usize, b: usize| {
        let (sa, sb) = (scores.get(rows[a].id), scores.get(rows[b].id));
        // A NaN never reaches here (the adapter filters it), and if one did it would compare equal
        // and fall through to recency rather than panic.
        sb.is_some()
            .cmp(&sa.is_some())
            .then_with(|| match (sa, sb) {
                (Some(x), Some(y)) => y.partial_cmp(x).unwrap_or(std::cmp::Ordering::Equal),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| rows[b].created_at.cmp(&rows[a].created_at))
            .then_with(|| rows[a].id.cmp(rows[b].id))
    };
    let mut candidates: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].preference).collect();
    candidates.sort_by(|&a, &b| ranked(a, b));
    let mut lead = Vec::new();
    let mut slots = 0;
    for i in candidates {
        if slots == preference_limit {
            break;
        }
        if !rows[i].dropped {
            slots += 1;
        }
        lead.push(i);
    }
    let mut rest: Vec<usize> = (0..rows.len()).filter(|i| !lead.contains(i)).collect();
    rest.sort_by(|&a, &b| ranked(a, b));
    lead.extend(rest);
    lead
}

/// Whether a floor from this source may gate the related section. A value an operator set, or one
/// tuned or measured for this model, qualifies. `Carried` and `Guessed` put bge's number on another
/// model's scale, which can fill the section on every query or leave it empty.
pub fn floor_is_trusted(source: Source) -> bool {
    match source {
        Source::Override | Source::Legacy | Source::Study | Source::Sweep => true,
        // bge-base's route_max_top. Tuned rather than measured; decision 0029 gives it the basis
        // `shipped`, the value the router ran on before that record.
        Source::Shipped => true,
        Source::Carried | Source::Guessed => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 - h * 3600, 0).unwrap()
    }
    fn scores(v: &[(&str, f64)]) -> HashMap<String, f64> {
        v.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }
    fn row(id: &str, hours_ago: i64) -> ProfileRow<'_> {
        ProfileRow { id, created_at: at(hours_ago), preference: false, dropped: false }
    }
    fn pref(id: &str, hours_ago: i64) -> ProfileRow<'_> {
        ProfileRow { id, created_at: at(hours_ago), preference: true, dropped: false }
    }
    /// A preference the near-duplicate pass dropped.
    fn gone(id: &str, hours_ago: i64) -> ProfileRow<'_> {
        ProfileRow { id, created_at: at(hours_ago), preference: true, dropped: true }
    }

    #[test]
    fn scored_rows_sort_by_score_then_newest() {
        let rows = [row("a", 1), row("b", 2), row("c", 3)];
        assert_eq!(
            profile_order(&rows, &scores(&[("a", 0.2), ("b", 0.9), ("c", 0.9)]), 5),
            vec![1, 2, 0]
        );
    }

    #[test]
    fn rows_without_a_score_follow_every_scored_row_newest_first() {
        let rows = [row("new", 1), row("mid", 2), row("old", 3)];
        assert_eq!(profile_order(&rows, &scores(&[("old", 0.1)]), 5), vec![2, 0, 1]);
    }

    #[test]
    fn no_scores_and_no_rules_keep_recency_order() {
        let rows = [row("x", 1), row("y", 2)];
        assert_eq!(profile_order(&rows, &HashMap::new(), 5), vec![0, 1]);
    }

    #[test]
    fn equal_scores_and_ages_break_ties_by_id() {
        let rows = [row("b", 1), row("a", 1)];
        assert_eq!(profile_order(&rows, &scores(&[("a", 0.5), ("b", 0.5)]), 5), vec![1, 0]);
    }

    /// The incident: a burst of newer personal facts and two older preferences, no anchor.
    #[test]
    fn preferences_lead_newer_personal_rows_without_scores() {
        let mut rows: Vec<ProfileRow> = ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"]
            .iter()
            .enumerate()
            .map(|(i, id)| row(id, i as i64 + 1))
            .collect();
        rows.push(pref("p-new", 40));
        rows.push(pref("p-old", 90));
        let order = profile_order(&rows, &HashMap::new(), 5);
        assert_eq!(&order[..3], &[8, 9, 0], "the preferences, newest first, then the newest fact");
    }

    #[test]
    fn the_lead_ranks_by_score_ahead_of_age() {
        let rows = [row("fact", 1), pref("far", 2), pref("near", 3)];
        let s = scores(&[("fact", 0.99), ("far", 0.1), ("near", 0.8)]);
        assert_eq!(profile_order(&rows, &s, 5), vec![2, 1, 0]);
    }

    /// Two newer personal rows tagged as preferences, two older rules near the session's work, and
    /// a lead of two. The two rules tie, and the newer one carries the larger id, so only the age
    /// tie-break puts it first.
    #[test]
    fn the_lead_takes_the_preferences_nearest_the_anchor_over_newer_ones() {
        let rows = [
            pref("plan-a", 1),
            pref("plan-b", 2),
            pref("rule-a", 101),
            pref("rule-b", 100),
            row("fact", 0),
        ];
        let s = scores(&[
            ("plan-a", 0.05),
            ("plan-b", 0.04),
            ("rule-a", 0.7),
            ("rule-b", 0.7),
            ("fact", 0.9),
        ]);
        assert_eq!(profile_order(&rows, &s, 2), vec![3, 2, 4, 0, 1]);
    }

    /// Passes before and after the change: it guards the fallback, which the change leaves alone.
    #[test]
    fn without_an_anchor_the_lead_keeps_the_newest_preferences() {
        let rows = [pref("p-new", 1), pref("p-mid", 5), pref("p-old", 9), row("fact", 0)];
        assert_eq!(profile_order(&rows, &HashMap::new(), 2), vec![0, 1, 3, 2]);
    }

    #[test]
    fn a_preference_without_a_score_follows_every_scored_preference_in_the_lead() {
        let rows = [pref("no-vector", 1), pref("scored", 50)];
        assert_eq!(profile_order(&rows, &scores(&[("scored", 0.01)]), 1), vec![1, 0]);
    }

    #[test]
    fn preferences_past_the_limit_rank_on_score_with_the_rest() {
        let rows = [pref("p0", 1), pref("p1", 2), row("near", 3), pref("p2", 4)];
        let s = scores(&[("p0", 0.1), ("p1", 0.1), ("near", 0.9), ("p2", 0.2)]);
        assert_eq!(profile_order(&rows, &s, 2), vec![3, 0, 2, 1]);
    }

    /// The pass dropped the tagged twin, and its survivor scores next to it. The twin keeps its
    /// place, so the section still notes it as a row it would have printed, and the two slots go
    /// to the survivor and the next preference.
    #[test]
    fn a_dropped_twin_keeps_its_place_in_the_lead_and_takes_no_slot() {
        let rows =
            [gone("twin", 9), pref("survivor", 5), pref("p0", 1), pref("p1", 2), row("near", 3)];
        let s =
            scores(&[("twin", 0.8), ("survivor", 0.79), ("p0", 0.1), ("p1", 0.1), ("near", 0.5)]);
        assert_eq!(profile_order(&rows, &s, 2), vec![0, 1, 2, 4, 3]);
    }

    #[test]
    fn a_preference_limit_of_zero_leads_with_nothing() {
        let rows = [pref("p", 1), row("near", 2)];
        let s = scores(&[("p", 0.1), ("near", 0.9)]);
        assert_eq!(profile_order(&rows, &s, 0), vec![1, 0]);
    }

    #[test]
    fn a_tag_counts_only_when_it_is_a_rule_tag() {
        let rule_tags = vec!["preference".to_string(), "rule".to_string()];
        assert!(is_standing(&["tooling".into(), "rule".into()], &rule_tags));
        assert!(!is_standing(&["household".into()], &rule_tags));
        assert!(!is_standing(&[], &rule_tags));
        assert!(!is_standing(&["rule".into()], &[]));
    }

    #[test]
    fn a_carried_or_guessed_floor_never_gates_the_related_section() {
        assert!(!floor_is_trusted(Source::Carried));
        assert!(!floor_is_trusted(Source::Guessed));
    }

    #[test]
    fn a_set_tuned_or_measured_floor_gates_the_related_section() {
        for s in [Source::Override, Source::Legacy, Source::Shipped, Source::Study, Source::Sweep] {
            assert!(floor_is_trusted(s), "{s:?}");
        }
    }
}
