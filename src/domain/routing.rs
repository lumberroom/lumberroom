//! Is this a question search can answer, or one that needs a walk?
//!
//! A walk is expensive. The first one measured cost 2,539 edge lookups and returned 256 rows out of
//! roughly 447 readable, against one statement for an ordinary search. Routing every question
//! through that replaces a cheap read with an expensive one for the large majority of questions that
//! never needed a join, so something has to decide.
//!
//! # Not a model, and not yet
//!
//! This reads the scores search already produced. Nothing here calls a provider, and that is the
//! point of starting here rather than with a classifier: the signal turned up in the measurements
//! that motivated the graph, so it can be checked before anything is trained.
//!
//! Two runs on 25 August 2026, same store, same ranker:
//!
//! - A question naming an entity the store holds returned that row first at **0.834**, with the
//!   next hit at 0.704. Sharp.
//! - The compositional question that needed the graph topped out at **0.570** and reached 0.512 at
//!   rank twenty, with unrelated rows scoring what the right one would have. Flat.
//!
//! A low best score beside a flat distribution is the shape of a question similarity cannot answer.
//! It says the store holds nothing that looks like the question, and that several things look
//! equally unlike it, which is what a join looks like from inside a single-vector search.
//!
//! # The thresholds are guesses, and they are meant to be replaced
//!
//! They are set from two observations. Two is not a calibration. This is the same position 0011 took
//! on the dedupe bands: publish the signal per decision, let the owner read real ones with their
//! numbers beside them, and move the thresholds when there is something to move them with.
//! [`Signals`] rides on every verdict for that reason, so a run over real questions is a calibration
//! run rather than an exercise to build later.
//!
//! # The thresholds are cosine values, and only cosine values
//!
//! Both numbers are absolute scores from the linear blend, where a good hit sits near 0.8 and a poor
//! one near 0.5. Under rank fusion the same hit scores about `1/(k + rank)`, so a rank-one row is
//! near 0.016 and the whole field spans less than 0.01. Compared against these thresholds every
//! question would read weak *and* flat, and every question would walk: the opposite of what this
//! module is for, arrived at silently.
//!
//! So the router declines to decide under a blend it was not calibrated against, and says so. It
//! does not invent a second pair of numbers, because there is no run behind them and a guessed
//! threshold that routes traffic is worse than no router at all.
//!
//! # Bias
//!
//! Toward not walking. A false negative costs the answer search would have given anyway, which is
//! the answer the caller gets today. A false positive costs thousands of edge lookups and hands back
//! half the store. The cheap failure is the one to prefer while the numbers are guesses.

use serde::Serialize;

/// What the scores looked like, published with every verdict so the thresholds can be calibrated
/// against real questions rather than argued about.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Signals {
    /// The best score search returned, or 0.0 when it returned nothing.
    pub top: f64,
    /// How far the best score stands above the field: `top` minus the score at [`SPREAD_RANK`], or
    /// minus the last hit when there are fewer. Zero when there is nothing to compare against.
    ///
    /// A sharp answer stands clear of the field. A flat one does not, and flatness is the part that
    /// says the ranking has no opinion rather than a weak one.
    pub spread: f64,
    /// How many rows came back. Zero means there is nothing to seed a walk from, whatever the shape.
    pub hits: usize,
    /// The question names something the store knows as an entity. When it does, the direct answer is
    /// usually reachable by name and the walk is unlikely to add anything.
    pub names_entity: bool,
}

/// Which rank the spread is measured against. Fifth, because the first four are where a
/// near-duplicate cluster sits and comparing against rank two would call every duplicated fact flat.
pub const SPREAD_RANK: usize = 5;

/// What to do with the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Search answered it, or there is nothing a walk could add.
    Search,
    /// The scores have the shape of a join. Worth the walk.
    Walk,
}

/// The verdict, with why.
///
/// `because` is not decoration. A router whose decisions cannot be read is a router nobody will
/// trust enough to leave switched on, and the reasons are what make a wrong threshold visible as a
/// wrong threshold rather than as bad luck.
#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub route: Route,
    pub signals: Signals,
    pub because: Vec<&'static str>,
}

/// Which score scale the thresholds describe. Only one is calibrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    /// Absolute cosine, the linear blend. What the numbers below were measured against.
    Cosine,
    /// Rank fusion, or the linear blend over a min-max rescaled cosine. Rank fusion scores sit near
    /// `1/(k + rank)`, and the rescaled blend puts the top hit at 1.0 on every query. Neither shares
    /// a scale with the above, so the router stands down rather than comparing across scales.
    Ranked,
}

/// The thresholds. Every one is a design target rather than a measurement, and each says so.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// The blend these numbers describe. Set from `SEARCH_FUSION` at boot.
    pub scale: Scale,
    /// A best score at or above this reads as "the store holds something that looks like the
    /// question". Set from 0.834 on one side and 0.570 on the other; the midpoint is not evidence.
    pub max_top: f64,
    /// A spread at or above this reads as a ranking with an opinion. The compositional case spanned
    /// 0.058 across twenty rows, so anything above that is a distribution with structure in it.
    pub max_spread: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { scale: Scale::Cosine, max_top: 0.65, max_spread: 0.08 }
    }
}

/// Read the shape of a result set.
///
/// `scores` arrives in rank order. Nothing here sorts it: search already decided the order, and
/// re-sorting would hide a ranker that returned rows out of order.
pub fn signals(scores: &[f64], names_entity: bool) -> Signals {
    let Some(&top) = scores.first() else {
        return Signals { names_entity, ..Signals::default() };
    };
    let against = scores.get(SPREAD_RANK - 1).or_else(|| scores.last()).copied().unwrap_or(top);
    Signals { top, spread: (top - against).max(0.0), hits: scores.len(), names_entity }
}

/// Decide.
pub fn route(s: Signals, t: Thresholds) -> Verdict {
    let mut because = Vec::new();

    // A scale these numbers were never measured on. Comparing 0.016 against 0.65 would call every
    // question weak and flat and walk all of them, so the router stands down instead.
    if t.scale != Scale::Cosine {
        return Verdict {
            route: Route::Search,
            signals: s,
            because: vec![
                "the router's thresholds are cosine values and this search ranks by fusion, \
                 which shares no scale with them",
            ],
        };
    }

    // Nothing to seed from. A walk starts at what search found, so no hits is no walk, whatever the
    // question looks like.
    if s.hits == 0 {
        return Verdict {
            route: Route::Search,
            signals: s,
            because: vec!["search returned nothing, and a walk starts from what search found"],
        };
    }

    // The question names something the store knows by name. Asking by name is the case that already
    // works, and the walk would be paying to rediscover a row search put first.
    //
    // **This short-circuit is the weakest rule here, and a counterexample is already recorded.**
    // The evidence for it was a by-name question scoring 0.834. On the same store, "what happened
    // with butler", naming a recorded alias, scored 0.474 with a spread of 0.034: weak and flat,
    // the shape that otherwise warrants a walk. It was refused anyway. Naming an entity is not the
    // same as search having answered.
    //
    // Kept for now because the bias is toward not walking while every threshold is a guess, and a
    // refused walk costs the answer search already gave. Demote it to a tiebreaker when a
    // calibration run says how often it refuses a walk that would have helped.
    if s.names_entity {
        return Verdict {
            route: Route::Search,
            signals: s,
            because: vec![
                "the question names an entity the store holds, which search answers directly",
            ],
        };
    }

    let weak = s.top < t.max_top;
    let flat = s.spread < t.max_spread;
    if weak {
        because.push("nothing scored like an answer");
    }
    if flat {
        because.push("the field is flat, so the ranking has no opinion rather than a weak one");
    }

    // Both, not either. A weak-but-sharp result is a poor answer that is still an answer; a
    // strong-but-flat one is a cluster of near-duplicates. Only the pair says "no single row
    // resembles this question", which is what a join looks like from inside one vector.
    if weak && flat {
        Verdict { route: Route::Walk, signals: s, because }
    } else {
        if because.is_empty() {
            because.push("search found something that stands clear of the field");
        }
        because.push("not enough to pay for a walk");
        Verdict { route: Route::Search, signals: s, because }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Thresholds {
        Thresholds::default()
    }

    /// The two runs this is built from, replayed. If the thresholds ever stop separating these,
    /// they have been moved past the only evidence there is.
    #[test]
    fn the_two_measured_questions_land_on_opposite_sides() {
        // Asked by name: 0.834 first, 0.704 next. Sharp, and it names an entity.
        let by_name = signals(&[0.834, 0.704, 0.666], true);
        assert_eq!(route(by_name, t()).route, Route::Search);

        // The compositional question: 0.570 down to 0.512 across twenty, naming no entity.
        let compositional =
            signals(&[0.570, 0.558, 0.547, 0.546, 0.543, 0.543, 0.536, 0.531, 0.530, 0.526], false);
        let v = route(compositional, t());
        assert_eq!(v.route, Route::Walk, "{v:?}");
        assert!(v.signals.spread < 0.08, "the field was flat: {}", v.signals.spread);
    }

    /// Under rank fusion every score is near 1/61, so the cosine thresholds would call every
    /// question weak and flat and walk all of them. That is the failure this guard exists for.
    #[test]
    fn rank_fusion_stands_the_router_down_rather_than_walking_everything() {
        let ranked = Thresholds { scale: Scale::Ranked, ..Thresholds::default() };
        // The exact shape an RRF result set has: tiny scores, tiny spread.
        let rrf = signals(&[0.0163, 0.0161, 0.0159, 0.0157, 0.0156], false);
        assert!(rrf.top < 0.65 && rrf.spread < 0.08, "this is what would have walked");
        let v = route(rrf, ranked);
        assert_eq!(v.route, Route::Search, "{v:?}");
        assert!(v.because[0].contains("shares no scale"), "{v:?}");

        // The same numbers on the scale they were measured for still walk.
        assert_eq!(route(rrf, Thresholds::default()).route, Route::Walk);
    }

    #[test]
    fn nothing_found_is_never_a_walk_because_there_is_nothing_to_start_from() {
        let v = route(signals(&[], false), t());
        assert_eq!(v.route, Route::Search);
        assert!(v.because[0].contains("starts from what search found"));
    }

    /// A weak answer that stands clear of the field is still an answer. Walking would pay to
    /// improve on something search already ordered.
    #[test]
    fn weak_but_sharp_does_not_walk() {
        let v = route(signals(&[0.60, 0.30, 0.28, 0.27, 0.26], false), t());
        assert_eq!(v.route, Route::Search, "{v:?}");
    }

    /// A tight cluster of near-duplicates is flat and strong. That is a dedupe finding, not a join.
    #[test]
    fn strong_but_flat_does_not_walk() {
        let v = route(signals(&[0.91, 0.90, 0.90, 0.89, 0.89], false), t());
        assert_eq!(v.route, Route::Search, "{v:?}");
    }

    /// Naming an entity short-circuits, because that is the case search demonstrably answers.
    /// The counterexample from the dev store, pinned so the weakness stays visible in the tests
    /// rather than only in a comment. Weak, flat, and refused because it named an alias.
    #[test]
    fn naming_an_entity_refuses_a_walk_the_scores_would_otherwise_warrant() {
        let real = signals(&[0.474, 0.462, 0.451, 0.444, 0.440], true);
        let v = route(real, t());
        assert_eq!(v.route, Route::Search);
        // Without the name, the identical distribution walks. That difference is the rule.
        let unnamed = signals(&[0.474, 0.462, 0.451, 0.444, 0.440], false);
        assert_eq!(route(unnamed, t()).route, Route::Walk);
    }

    #[test]
    fn naming_an_entity_beats_a_flat_field() {
        let flat_but_named = signals(&[0.55, 0.54, 0.54, 0.53, 0.53], true);
        assert_eq!(route(flat_but_named, t()).route, Route::Search);
    }

    #[test]
    fn the_spread_is_measured_against_the_fifth_and_falls_back_to_the_last() {
        assert_eq!(SPREAD_RANK, 5);
        let short = signals(&[0.9, 0.5], false);
        assert!((short.spread - 0.4).abs() < 1e-9, "two hits compare against the last");
        let full = signals(&[0.9, 0.8, 0.7, 0.6, 0.5, 0.1], false);
        assert!(
            (full.spread - 0.4).abs() < 1e-9,
            "six hits compare against the fifth, not the sixth"
        );
    }

    /// Every verdict carries its reasons, because a router nobody can read is one nobody leaves on.
    #[test]
    fn a_verdict_always_says_why() {
        for scores in [vec![], vec![0.9, 0.1], vec![0.5, 0.49, 0.49, 0.48, 0.48]] {
            for named in [true, false] {
                let v = route(signals(&scores, named), t());
                assert!(!v.because.is_empty(), "{v:?}");
            }
        }
    }
}
