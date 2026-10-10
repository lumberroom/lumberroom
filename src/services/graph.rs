//! A bounded walk over the store, for the question similarity cannot answer.
//!
//! Decision 0014 part 4. The failing query joins a held position to a named catalyst, and neither
//! phrase appears in the row that answers it. Measured twice on 25 August 2026: that row scores
//! 0.834 against its own name and does not reach the top twenty of the question describing it, on
//! the live store and on a replica. Nearest-neighbour search has no operator for a join.
//!
//! # Seeded from search, walked over structure
//!
//! Seeds come from the ordinary search, so the walk starts where the question actually points. Edges
//! come from structure the store already holds: supersession links, aliases, curated tags. No model
//! is called. 0014 assumed entity extraction and warned the graph has to earn that cost; these edges
//! cost nothing, and if a bounded walk over them answers no more than search does, the extractor was
//! never the missing piece.
//!
//! # The walk severs, and says nothing about what it withheld
//!
//! Every hop runs inside the caller's subgraph: both endpoints of every edge are readable, at their
//! stored sensitivity, with retired rows admitted only for a caller holding the history capability.
//! A node the caller may not read is not a node it walks through.
//!
//! Nothing reports what was withheld, and that is deliberate. `subject_history` does the opposite,
//! filtering after its recursion so a chain stays whole across a row it must hide, and it can report
//! the gap as a bare count because a chain has one subject and an anchor the caller already named. A
//! graph has neither past the first hop. Its shape **is** the answer, so a count of skipped edges,
//! or even a boolean saying the walk was truncated, is the answer leaking: vary the seed, watch the
//! flag flip, map the boundary.
//!
//! The cost is that a client cannot tell "no path exists" from "no path is yours". 0014 accepts
//! that and names the console, which reads as the owner, as where the whole graph is visible.

use serde::Serialize;

use super::Ctx;
use crate::domain::errors::Result;
use crate::domain::routing::{self, Route, Verdict};
use crate::domain::similarity;
use crate::ports::memory::{GraphEdge, WalkBounds};

/// The three numbers 0014 fixed. Design targets, pinned here and by a test rather than left in a
/// query for somebody to edit without noticing what they are for.
pub const SEEDS: i64 = 10;
pub const FAN_OUT: i64 = 25;
pub const DEPTH: usize = 2;

/// A node the walk reached, and how far out it sits.
#[derive(Debug, Clone, Serialize)]
pub struct Reached {
    pub id: String,
    pub content: String,
    pub namespace: String,
    /// 0 for a seed, 1 for its neighbour, 2 for the hop after that.
    pub hop: usize,
    /// Why it was reached. Absent on a seed, which search found rather than an edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// What one walk found.
#[derive(Debug, Clone, Serialize)]
pub struct Walk {
    pub query: String,
    /// Why this walked, or why it did not. Present whenever the router was asked; absent when the
    /// caller demanded a walk outright.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The rows search returned, which is where the walk started.
    pub seeds: usize,
    /// Everything reached, seeds first, then by hop.
    pub reached: Vec<Reached>,
    /// Edges traversed, for the console. A client sees these too, and they name only nodes it can
    /// already read, so they disclose nothing the rows do not.
    pub edges: Vec<GraphEdge>,
}

/// Walk out from what search found.
///
/// The capability check is the caller's grant, applied inside every query rather than as a pass over
/// results. `may_read_history` decides whether retired rows are walkable at all, which is the same
/// question `search` asks before it will honour `as_of`, and for the same reason: a supersession
/// edge reaches exactly what `memory_history` refuses, and that door has been opened by a second
/// spelling once before.
pub async fn walk(ctx: &Ctx, query: &str, degree_cap: i64) -> Result<Walk> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Walk {
            query: String::new(),
            verdict: None,
            seeds: 0,
            reached: vec![],
            edges: vec![],
        });
    }
    let found = super::search::run(ctx, query, None, Some(SEEDS), None, None, None).await?;
    from_hits(ctx, query, &found.hits, degree_cap).await
}

/// The walk, given seeds somebody already paid for.
///
/// `routed` runs the search to read its scores, so calling [`walk`] afterwards would run the same
/// search and the same embedding call twice for every walked question. This is the half that does
/// not repeat the work.
async fn from_hits(
    ctx: &Ctx,
    query: &str,
    hits: &[super::search::Hit],
    degree_cap: i64,
) -> Result<Walk> {
    let mut reached: Vec<Reached> = hits
        .iter()
        .map(|h| Reached {
            id: h.id.clone(),
            content: h.content.clone(),
            namespace: h.namespace.clone(),
            hop: 0,
            via: None,
        })
        .collect();
    let seeds = reached.len();

    let bounds = WalkBounds {
        fan_out: FAN_OUT,
        degree_cap,
        include_retired: ctx.principal.may_read_history,
    };

    let mut frontier: Vec<uuid::Uuid> =
        reached.iter().filter_map(|r| uuid::Uuid::parse_str(&r.id).ok()).collect();
    let mut seen: std::collections::HashSet<String> =
        reached.iter().map(|r| r.id.clone()).collect();
    let mut edges: Vec<GraphEdge> = Vec::new();

    for hop in 1..=DEPTH {
        if frontier.is_empty() {
            break;
        }
        let found_edges = ctx
            .repos
            .memories
            .graph_neighbours(ctx.tenant(), &ctx.principal.read, &frontier, bounds)
            .await?;

        let mut next: Vec<uuid::Uuid> = Vec::new();
        let mut wanted: Vec<uuid::Uuid> = Vec::new();
        for e in &found_edges {
            let id = e.to_id.to_string();
            if seen.insert(id.clone()) {
                wanted.push(e.to_id);
                next.push(e.to_id);
            }
        }
        edges.extend(found_edges);

        if !wanted.is_empty() {
            // One query per hop, not one per node. At the documented bounds a wide walk reaches
            // thousands of nodes, and a `find_by_id` each would be thousands of serial round trips
            // inside one request, which `force=true` lets any authenticated caller ask for.
            let rows = ctx.repos.memories.find_many(ctx.tenant(), &wanted).await?;
            for row in rows {
                let id = uuid::Uuid::parse_str(&row.id).ok();
                reached.push(Reached {
                    id: row.id.clone(),
                    content: row.content.clone(),
                    namespace: row.namespace.clone(),
                    hop,
                    via: id
                        .and_then(|id| edges.iter().find(|e| e.to_id == id))
                        .map(|e| e.relation.clone()),
                });
            }
        }
        frontier = next;
    }

    Ok(Walk { query: query.to_string(), verdict: None, seeds, reached, edges })
}

/// Ask the router first, and walk only if it says so.
///
/// The classifier reads scores search already produced, and those hits are handed to the walk
/// rather than fetched again, so a walked question costs one search and a refused one costs the
/// same search and nothing more. That ordering is the whole design: a walk
/// measured at 2,539 edge lookups against one statement for a search cannot be the default path.
///
/// Returning the verdict on a refusal matters as much as on a walk. A caller told only "no results
/// from the graph" learns nothing about whether the graph was even consulted, and an owner
/// calibrating the thresholds needs the numbers behind every decision, not only the affirmative
/// ones.
pub async fn routed(ctx: &Ctx, query: &str, degree_cap: i64) -> Result<Walk> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Walk {
            query: String::new(),
            verdict: None,
            seeds: 0,
            reached: vec![],
            edges: vec![],
        });
    }

    // One unit for the search and the verdict: the scores come from its model, so the route
    // thresholds have to as well.
    let u = ctx.embedders.for_unit(ctx.tenant())?;
    let found =
        super::search::run_with(ctx, &u, query, None, Some(SEEDS), None, None, None, &[]).await?;
    let scores: Vec<f64> = found.hits.iter().map(|h| h.score).collect();
    let names_entity = names_known_entity(ctx, query).await?;
    let thresholds = routing::Thresholds {
        scale: ctx.cfg.search.graph_route_scale,
        max_top: u.thresholds.get(similarity::ROUTE_MAX_TOP),
        max_spread: u.thresholds.get(similarity::ROUTE_MAX_SPREAD),
    };
    let verdict = routing::route(routing::signals(&scores, names_entity), thresholds);

    if verdict.route == Route::Search {
        // Search already answered, and its hits are what the caller would have got anyway. Handing
        // them back as seeds keeps one shape for both outcomes.
        let reached = found
            .hits
            .iter()
            .map(|h| Reached {
                id: h.id.clone(),
                content: h.content.clone(),
                namespace: h.namespace.clone(),
                hop: 0,
                via: None,
            })
            .collect::<Vec<_>>();
        return Ok(Walk {
            query: query.to_string(),
            seeds: reached.len(),
            verdict: Some(verdict),
            reached,
            edges: vec![],
        });
    }

    let mut walked = from_hits(ctx, query, &found.hits, degree_cap).await?;
    walked.verdict = Some(verdict);
    Ok(walked)
}

/// Does the question name something the store knows as an entity?
///
/// Read off `entity_alias`, which is the table that already decides two names denote one subject.
/// A question naming a known entity is the case search demonstrably answers: on 25 August 2026 the
/// row came back first at 0.834 when asked by name.
///
/// Substring rather than tokenised matching, and lowercased on both sides because the alias table
/// stores lowercase by contract. Loose in the direction of finding a name, which biases toward not
/// walking, which is the cheap failure.
async fn names_known_entity(ctx: &Ctx, query: &str) -> Result<bool> {
    let haystack = query.to_lowercase();
    let aliases = ctx.repos.aliases.list(ctx.tenant(), None).await?;
    Ok(aliases.iter().any(|a| {
        // A name the owner recorded as no longer current is exactly the rename 0009 exists to
        // capture. Letting it suppress a walk would be the opposite of what the edge seeder does
        // with the same table, and it would suppress on the strength of a name nothing uses.
        if a.until.is_some() {
            return false;
        }
        let alias = a.alias.to_lowercase();
        let canonical = a.canonical.to_lowercase();
        // Four, not three. `api`, `ops` and `kek` are real aliases and each appears inside ordinary
        // words, so a three-letter floor lets one short name switch the graph off for every question
        // that happens to contain it.
        const SHORTEST: usize = 4;
        (alias.len() >= SHORTEST && haystack.contains(&alias))
            || (canonical.len() >= SHORTEST && haystack.contains(&canonical))
    }))
}

/// Rebuild the edge table from structure.
pub async fn rebuild(ctx: &Ctx) -> Result<i64> {
    ctx.repos.memories.rebuild_edges(ctx.tenant()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three numbers are 0014's, and a test is where they belong rather than inside a query.
    /// Ten seeds at twenty-five per hop to depth two touches 250 edges on the first hop and 6,250 on
    /// the second, 6,500 in total. The record said "six thousand", which is the right order and
    /// matches no exact reading; this is the arithmetic it should have carried.
    #[test]
    fn the_bounds_are_the_ones_the_record_fixed() {
        assert_eq!((SEEDS, FAN_OUT, DEPTH), (10, 25, 2));
        let first = SEEDS * FAN_OUT;
        let second = first * FAN_OUT;
        assert_eq!(first, 250);
        assert_eq!(first + second, 6_500);
    }
}
