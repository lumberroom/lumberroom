//! Orders the digest's profile pool and decides whether a model's floor may gate the related
//! section (decision 0034).
//!
//! No I/O and no vectors: the adapter computes each pooled row's cosine to the query and hands over
//! the scores by id, and the service says which rows carry a standing-rule tag.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::similarity::Source;

/// One profile-pool row as the ranking sees it.
#[derive(Debug, Clone, Copy)]
pub struct ProfileRow<'a> {
    pub id: &'a str,
    pub created_at: DateTime<Utc>,
    /// Carries one of `BOOTSTRAP_RULE_TAGS`.
    pub standing: bool,
}

/// True when `tags` holds any of `rule_tags`. Both sides are in the store's normalised spelling.
pub fn is_standing(tags: &[String], rule_tags: &[String]) -> bool {
    tags.iter().any(|t| rule_tags.contains(t))
}

/// Indices into `rows` in print order. Standing rules lead: the profile is where a session learns
/// the rules that bind it, and a burst of personal facts from another surface used to push every
/// rule out. Inside each group scored rows come first, highest score first, then newest, then id.
/// With no scores, which is every call without a query, each group is newest first.
pub fn profile_order(rows: &[ProfileRow<'_>], scores: &HashMap<String, f64>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| {
        let (ra, rb) = (rows[a], rows[b]);
        let (sa, sb) = (scores.get(ra.id), scores.get(rb.id));
        // A NaN never reaches here (the adapter filters it), and if one did it would compare equal
        // and fall through to recency rather than panic.
        rb.standing
            .cmp(&ra.standing)
            .then_with(|| sb.is_some().cmp(&sa.is_some()))
            .then_with(|| match (sa, sb) {
                (Some(x), Some(y)) => y.partial_cmp(x).unwrap_or(std::cmp::Ordering::Equal),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| rb.created_at.cmp(&ra.created_at))
            .then_with(|| ra.id.cmp(rb.id))
    });
    order
}

/// Whether a floor from this source may gate the related section. A value an operator set, or one
/// tuned or measured for this model, qualifies. `Carried` and `Guessed` put bge's number on another
/// model's scale, which can fill the section on every query or leave it empty.
pub fn floor_is_trusted(source: Source) -> bool {
    match source {
        Source::Override | Source::Legacy | Source::Study | Source::Sweep => true,
        // bge-base's route_max_top. Tuned rather than measured, and production ran the router on
        // it before decision 0029.
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
        ProfileRow { id, created_at: at(hours_ago), standing: false }
    }
    fn rule(id: &str, hours_ago: i64) -> ProfileRow<'_> {
        ProfileRow { id, created_at: at(hours_ago), standing: true }
    }

    #[test]
    fn scored_rows_sort_by_score_then_newest() {
        let rows = [row("a", 1), row("b", 2), row("c", 3)];
        assert_eq!(
            profile_order(&rows, &scores(&[("a", 0.2), ("b", 0.9), ("c", 0.9)])),
            vec![1, 2, 0]
        );
    }

    #[test]
    fn rows_without_a_score_follow_every_scored_row_newest_first() {
        let rows = [row("new", 1), row("mid", 2), row("old", 3)];
        assert_eq!(profile_order(&rows, &scores(&[("old", 0.1)])), vec![2, 0, 1]);
    }

    #[test]
    fn no_scores_and_no_rules_keep_recency_order() {
        let rows = [row("x", 1), row("y", 2)];
        assert_eq!(profile_order(&rows, &HashMap::new()), vec![0, 1]);
    }

    #[test]
    fn equal_scores_and_ages_break_ties_by_id() {
        let rows = [row("b", 1), row("a", 1)];
        assert_eq!(profile_order(&rows, &scores(&[("a", 0.5), ("b", 0.5)])), vec![1, 0]);
    }

    /// The incident: a burst of newer personal facts and two older rules, no query.
    #[test]
    fn standing_rules_lead_newer_personal_rows_without_a_query() {
        let mut rows: Vec<ProfileRow> = ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"]
            .iter()
            .enumerate()
            .map(|(i, id)| row(id, i as i64 + 1))
            .collect();
        rows.push(rule("r-new", 40));
        rows.push(rule("r-old", 90));
        let order = profile_order(&rows, &HashMap::new());
        assert_eq!(&order[..3], &[8, 9, 0], "the rules, newest first, then the newest fact");
    }

    #[test]
    fn a_query_orders_inside_each_group_and_never_lifts_a_fact_over_a_rule() {
        let rows = [row("fact", 1), rule("far", 2), rule("near", 3)];
        let s = scores(&[("fact", 0.99), ("far", 0.1), ("near", 0.8)]);
        assert_eq!(profile_order(&rows, &s), vec![2, 1, 0]);
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
