//! Postgres implementation of MemoryRepository.
//!
//! All memory SQL lives here. Callers ask for ranked results; how they are ranked is this file's
//! business, which is the seam that makes a second storage implementation possible.
//!
//! Two rules run through every statement below.
//!
//! **The sensitivity ceiling is part of the query.** The caller hands over namespaces paired with
//! the ceiling it holds for each, they arrive as two parallel arrays, and every subquery joins
//! against them. A row this client may not see never enters this client's process memory, which is
//! a stronger guarantee than filtering results and is the only one worth having.
//!
//! **The repository never encrypts and never decrypts.** A private row arrives with its ciphertext
//! already sealed by the service and leaves as a `Memory` with an empty `content`. The KEK lives in
//! the service layer and nothing here can reach it, so no statement in this file can accidentally
//! turn ciphertext into a plaintext column or hand ciphertext bytes to a caller as if they were
//! text.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::collections::HashMap;

use crate::config::{Fusion, SearchConfig, DEFAULT_RRF_K};
use crate::crypto::envelope::SealedContent;
use crate::domain::errors::{DomainError, Result};
use crate::domain::policy::{NamespaceCeiling, NamespaceGrant};
use crate::domain::types::{ConflictCandidate, Memory, SearchHit, Sensitivity};
use crate::ports::memory::{
    ChainEdits, ChainLink, ChainNeighbours, DeleteOutcome, DeletePlan, GraphEdge, PairCounts,
    RestoreRow, Retired, Superseded, Timeline, WalkBounds,
};
use crate::ports::{
    ConflictPair, DigestData, DigestQuery, Emission, MemoryRepository, NamespaceRows,
    NamespaceSummary, NeighbourQuery, NewMemory, RecentQuery, RegistrySummary, SearchQuery,
    Staleness,
};

pub struct PgMemoryRepository {
    pool: PgPool,
    /// Which blend orders the candidate set. A repository built without the configuration ranks
    /// the way this server has always ranked, so a caller that forgets `with_search` loses the new
    /// option rather than the old behaviour.
    fusion: Fusion,
    rrf_k: f64,
}

impl PgMemoryRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool, fusion: Fusion::Linear, rrf_k: DEFAULT_RRF_K }
    }

    /// Take the ranking settings from the boot configuration.
    ///
    /// Separate from `new` because the tests and the operator tools build a repository from a pool
    /// alone, and because every setting is read in `config.rs`: this adapter learns the blend by
    /// being handed it, never by reading the environment.
    pub fn with_search(mut self, search: &SearchConfig) -> Self {
        self.fusion = search.fusion;
        self.rrf_k = search.rrf_k;
        self
    }

    /// How much of this tenant's valid time is a copy of its transaction time.
    ///
    /// The detector for the one failure a tool description cannot prevent. `occurred_at` is set by
    /// whoever writes the fact, and a model that stamps today on every write turns the column into
    /// a second `created_at`. Three months of that leaves noise no later pass can tell from signal,
    /// so the ratio has to be watched from the start rather than audited afterwards.
    ///
    /// Absolute difference, because both directions are the same failure: a skewed clock lands
    /// after the write and a careless backfill lands before it. Live rows only, since a retired row
    /// records what the store was once told rather than what it holds.
    ///
    /// `within_secs` is a parameter and never an environment read: this adapter learns its settings
    /// by being handed them.
    pub async fn occurred_at_compliance(
        &self,
        tenant: &str,
        within_secs: i64,
    ) -> Result<OccurredAtCompliance> {
        let row = sqlx::query(
            "SELECT count(*) FILTER (WHERE occurred_at IS NOT NULL) AS dated,
                    count(*) FILTER (
                      WHERE occurred_at IS NOT NULL
                        AND abs(extract(epoch FROM occurred_at - created_at))::float8 <= $2
                    ) AS near_created_at
               FROM memory
              WHERE tenant_id = $1 AND superseded_by IS NULL",
        )
        .bind(tenant)
        .bind(within_secs as f64)
        .fetch_one(&self.pool)
        .await?;
        Ok(OccurredAtCompliance {
            dated: row.get("dated"),
            near_created_at: row.get("near_created_at"),
        })
    }
}

/// The two numbers that say whether `occurred_at` still means anything.
///
/// A count on its own cannot be read. `near_created_at` approaching `dated` is the failure; both
/// small against a large store is a fill-rate problem instead, and the two want different fixes.
#[derive(Debug, Clone, Copy, serde::Serialize, Default)]
pub struct OccurredAtCompliance {
    /// Live rows carrying a valid-time start at all.
    pub dated: i64,
    /// Of those, the ones whose start sits within the window of when the store learned them.
    pub near_created_at: i64,
}

/// How deep a supersession chain is walked before the walk is abandoned.
///
/// A cycle already in the table would make an unbounded recursive CTE run until the statement
/// timeout, so every walk is capped. Sixty-four hops is far past any real correction chain: the
/// deepest plausible one is a fact corrected once a week for a year.
const MAX_CHAIN_DEPTH: i32 = 64;

/// The column list every read of `memory` selects, defined once.
///
/// `memory_from_row` reads columns by name and panics on one a query forgot, and there are nine
/// read sites. A macro rather than a constant because `concat!` only accepts literals, so the
/// whole statement has to be assembled inside one expansion.
macro_rules! select_memory {
    ($prefix:literal, $rest:expr) => {
        select_memory!("", $prefix, $rest)
    };
    // The leading form exists for the one statement that needs a data-modifying CTE ahead of the
    // SELECT: Postgres allows INSERT ... RETURNING inside WITH and nowhere else.
    ($pre:literal, $prefix:literal, $rest:expr) => {
        concat!(
            $pre,
            "SELECT ",
            $prefix,
            "id, ",
            $prefix,
            "namespace, ",
            $prefix,
            "content, ",
            $prefix,
            "tags, ",
            $prefix,
            "source_client, ",
            $prefix,
            "embedding_model, ",
            $prefix,
            "sensitivity, ",
            $prefix,
            "supersedes, ",
            $prefix,
            "superseded_by, ",
            $prefix,
            "superseded_at, ",
            $prefix,
            "access_count, ",
            $prefix,
            "last_accessed_at, ",
            $prefix,
            "last_confirmed_at, ",
            $prefix,
            "created_at, ",
            $prefix,
            "occurred_at, ",
            $prefix,
            "occurred_until ",
            $rest
        )
    };
}

/// Hybrid search, in four compile-time variants: two live-row predicates times two blends.
///
/// The live-rows predicate is a literal in all four rather than a bound boolean, and the live pair
/// takes it from `live!()`: the link test and the period test, spelled once for the whole adapter.
/// The link half as written matches the `memory_live` partial index from migration 005 and the
/// conjunct is stronger, so the planner can still prove the index applies. The same test spelled
/// `($n OR superseded_by IS NULL)` is weaker, leaves the planner unable to prove the predicate
/// holds under a generic plan, and quietly loses the index on a store where history outweighs live
/// rows.
///
/// The blend arrives as four literals: what each arm adds to its select list, what `merged` carries
/// through, and the score expression itself. `linear_search_sql!` passes empty strings for the first
/// three, so the default text is what it has always been down to the window functions it does not
/// contain. That matters more than it looks: `row_number()` puts a WindowAgg above the vector arm's
/// ordered index scan, and the plan that arm gets is what pgvector's iterative scan depends on.
///
/// Everything after this point is `concat!` over literals: no runtime string building reaches the
/// query text.
macro_rules! search_sql {
    (
        $live:expr,
        $vec_rank:literal,
        $lex_rank:literal,
        $merged_ranks:literal,
        $score:literal
    ) => {
        concat!(
            r#"
            WITH reachable AS (
                -- The grant, as a table. Namespaces and the ceiling each carries arrive as two
                -- parallel arrays and are joined in, so the sensitivity filter runs in the plan
                -- rather than over the results.
                --
                -- A namespace listed twice is a caller bug (policy::resolve unions the ceilings
                -- before this point). The lower ceiling wins, because collapsing a duplicate must
                -- never be the thing that raises one.
                SELECT namespace, min(sensitivity_rank(max)) AS max_rank
                  FROM (
                        SELECT * FROM unnest($2::text[], $3::text[]) AS p(namespace, max)
                        UNION ALL
                        SELECT * FROM unnest($4::text[], $5::text[]) AS s(namespace, max)
                       ) u
                 GROUP BY namespace
            ),
            vec AS (
                -- Both filters sit inside the arm, ahead of its LIMIT. Filtering after the LIMIT
                -- is the same failure as the HNSW truncation trap: the arm returns its quota,
                -- the filter empties it, and the caller is told nothing is known.
                SELECT m.id, 1 - (m.embedding <=> $6) AS similarity"#, $vec_rank, r#"
                  FROM memory m
                  JOIN reachable rg ON rg.namespace = m.namespace
                 WHERE m.tenant_id = $1
                   -- Redundant with the join, and kept on purpose. As a qual on the base relation
                   -- the namespace test can be pushed into the vector index scan; expressed only as
                   -- a join condition it sits above the scan, and the planner is free to abandon the
                   -- ordered index scan for a sequential scan and a sort. This arm is the one where
                   -- that matters, because the ordered index scan is what pgvector's iterative scan
                   -- resumes when the filters discard most of a candidate batch.
                   AND m.namespace = ANY($2 || $4)
                   AND sensitivity_rank(m.sensitivity) <= rg.max_rank
                   AND m.embedding IS NOT NULL
                   AND "#, $live, r#"
                 ORDER BY m.embedding <=> $6
                 LIMIT $7
            ),
            lex AS (
                SELECT m.id,
                       ts_rank(to_tsvector('english', m.content),
                               websearch_to_tsquery('english', $8)) AS lexical"#, $lex_rank, r#"
                  FROM memory m
                  JOIN reachable rg ON rg.namespace = m.namespace
                 WHERE m.tenant_id = $1
                   AND m.namespace = ANY($2 || $4)
                   AND sensitivity_rank(m.sensitivity) <= rg.max_rank
                   -- Spelled exactly as migration 004's partial index predicate so the index
                   -- matches. It is also the correct filter rather than an optimisation: a
                   -- tsvector is the document, so private content is not in the index and cannot
                   -- be reached from here at any ceiling.
                   AND m.sensitivity = 'open'
                   AND to_tsvector('english', m.content) @@ websearch_to_tsquery('english', $8)
                   AND "#, $live, r#"
                 ORDER BY lexical DESC
                 LIMIT $7
            ),
            merged AS (
                -- Both scores cast to float8 here rather than at the point of use. ts_rank
                -- returns real, and Postgres resolves `real * $n` by inferring float4 for the
                -- parameter, which the driver then refuses to bind against an f64.
                SELECT COALESCE(v.id, l.id) AS id,
                       COALESCE(v.similarity, 0)::float8 AS similarity,
                       LEAST(COALESCE(l.lexical, 0), 1)::float8 AS lexical"#, $merged_ranks, r#"
                  FROM vec v
                  FULL OUTER JOIN lex l ON l.id = v.id
            )
            SELECT m.id, m.namespace, m.content, m.tags, m.source_client, m.embedding_model,
                   m.sensitivity, m.supersedes, m.superseded_by, m.superseded_at,
                   m.access_count, m.last_accessed_at, m.last_confirmed_at, m.created_at,
                   m.occurred_at, m.occurred_until,
                   g.similarity::float8 AS similarity,
                   (m.namespace = ANY($2)) AS is_primary,
                   "#, $score, r#" AS score
              FROM merged g
              JOIN memory m ON m.id = g.id
             ORDER BY score DESC, m.created_at DESC
             LIMIT $12
            "#
        )
    };
}

/// The shipped blend: a weighted sum of cosine similarity and `ts_rank`.
///
/// The two arms are on different scales and this is the whole of its weakness. A strong three-term
/// lexical match scores 0.259, which the 0.35 weight turns into 0.091, against a cosine near 0.7 at
/// weight 1.0. The lexical arm supplies candidates and hardly reorders them.
macro_rules! linear_search_sql {
    ($live:expr) => {
        search_sql!(
            $live,
            "",
            "",
            "",
            r#"-- The use boost. Saturating at ten retrievals and multiplied by a weight the
                   -- config keeps an order of magnitude below the vector weight, so the whole term
                   -- is bounded by that weight. It also only ever reranks rows an arm already
                   -- returned, so a heavily used row that answers nothing cannot be pulled into a
                   -- result by it.
                   ((g.similarity * $9 + g.lexical * $10
                       + LEAST(ln(1 + m.access_count::float8) / ln(11::float8), 1.0) * $13)
                     * (CASE WHEN m.namespace = ANY($2)
                             THEN 1.0::float8 ELSE $11::float8 END))::float8"#
        )
    };
}

/// Reciprocal rank fusion, behind `SEARCH_FUSION=rrf`.
///
/// Each arm contributes `weight / (k + its own rank)`, so position decides and magnitude is
/// discarded. Both arms then argue on one scale by construction, which is what the linear blend
/// cannot do while a cosine similarity and a `ts_rank` mean different things.
///
/// This variant binds a fourteenth parameter, `k`.
macro_rules! rrf_search_sql {
    ($live:expr) => {
        search_sql!(
            $live,
            r#",
                       row_number() OVER (ORDER BY m.embedding <=> $6) AS rank"#,
            r#",
                       -- The window repeats the ts_rank expression because an OVER clause cannot
                       -- see the `lexical` alias. Edit one and edit both: a rank that describes a
                       -- different order than the arm returns is wrong in a way nothing reports.
                       row_number() OVER (
                           ORDER BY ts_rank(to_tsvector('english', m.content),
                                            websearch_to_tsquery('english', $8)) DESC) AS rank"#,
            r#",
                       -- NULL wherever an arm did not return the row, which is most rows: the two
                       -- arms agree on a handful of ids and diverge on the rest. Turning that NULL
                       -- into a zero contribution is the score expression's job.
                       v.rank AS rank_vec,
                       l.rank AS rank_lex"#,
            r#"-- COALESCE wraps each whole term rather than the rank inside it. `$14 + NULL` is
                   -- NULL, so the division is NULL, and NULL swallows the sum: every row that only
                   -- one arm returned would score NULL and fall back to created_at order. Most
                   -- rows are exactly that row.
                   ((COALESCE($9::float8 / ($14::float8 + g.rank_vec::float8), 0.0::float8)
                     + COALESCE($10::float8 / ($14::float8 + g.rank_lex::float8), 0.0::float8))
                    -- The use boost multiplies here instead of adding, and the shape change is the
                    -- point. A rank-1 row scores about 1/61, so the additive 0.05 the linear blend
                    -- carries is three times the entire score and would order the results by
                    -- access count. As a factor it buys at most five percent over an unused row.
                    * (1.0::float8 + $13::float8
                        * LEAST(ln(1 + m.access_count::float8) / ln(11::float8), 1.0::float8))
                    -- The cross-namespace penalty keeps the shape it already had.
                    * (CASE WHEN m.namespace = ANY($2)
                            THEN 1.0::float8 ELSE $11::float8 END))::float8"#
        )
    };
}

const SEARCH_LIVE: &str = linear_search_sql!(live!());
/// `include_superseded`. The decision log and `lumberroom review` read history by hand; nothing on a
/// request path uses this, so losing the partial index here costs nothing that matters.
const SEARCH_ALL: &str = linear_search_sql!("true");
const SEARCH_RRF_LIVE: &str = rrf_search_sql!(live!());
const SEARCH_RRF_ALL: &str = rrf_search_sql!("true");

/// What held at one instant, on the valid-time axis. One statement per blend.
///
/// `<=` on the left edge and `>` on the right, which is spec rule I1 and the reason the store
/// writes half-open periods at all. Writing `>=` on the right lets a predecessor ending at T and a
/// successor starting at T both match a query at T, so the read returns two contradictory answers
/// about one instant and nothing errors. The predicate is the shape the spec fixes for every as-of
/// read in this codebase, now and later, so it is copied rather than reworded.
///
/// NULL on either side widens the period: no known start reads as always held (rule N1), no known
/// end reads as still holding (N3).
///
/// **No `superseded_by` filter, on purpose.** A row retired last week is exactly the row that
/// answers a question about last month, and filtering it would leave the statement returning only
/// facts that both held then and still hold now, which is a question nobody asked. Cost: this pair
/// forfeits the `memory_live` partial index the same way `SEARCH_ALL` does, and it discards more
/// candidates inside each arm than the live predicate, so it leans harder on the `iterative_scan`
/// and `ef_search` settings migration 003 puts on the database. Both are acceptable on a read that
/// is not on the bootstrap path.
///
/// **The start falls back to `created_at`, and that is the fix for a row with no date at all.**
/// `occurred_at IS NULL OR occurred_at <= t` made an undated row match every instant, including
/// instants before the store existed. Most rows are undated, so an as-of read returned a retired
/// fact and its undated replacement together for any instant before the retirement, which is the
/// double answer this whole read exists to prevent. Falling back says the honest thing instead: the
/// store cannot claim a fact held before it learned it.
///
/// No extra column says which clock answered, because the row already tells you. A hit carries
/// `occurred_at` only when it is set, so its absence beside a `created_at` is the fallback speaking.
///
/// Two literals rather than one constant because `concat!` takes literals alone. They differ in one
/// character: the parameter number, which is the first number each blend has spare. Edit one and
/// edit the other.
const SEARCH_AS_OF: &str = linear_search_sql!(
    r#"(COALESCE(m.occurred_at, m.created_at) <= $14
                   AND (m.occurred_until IS NULL OR m.occurred_until >  $14))"#
);
/// Rank fusion already binds `k` as the fourteenth, so as-of lands on the fifteenth here.
///
/// Renumbering `k` would have given both blends the same as-of parameter and changed the text of
/// two statements that are not changing, which is the one thing this addition may not do.
const SEARCH_RRF_AS_OF: &str = rrf_search_sql!(
    r#"(COALESCE(m.occurred_at, m.created_at) <= $15
                   AND (m.occurred_until IS NULL OR m.occurred_until >  $15))"#
);

/// One page of facts, newest first, in two compile-time variants.
///
/// The live-rows predicate is a literal rather than a bound boolean, for the reason `search_sql!`
/// spells out, and the live variant takes `live!()`: the link test matches the `memory_live`
/// partial index from migration 005 and the period test rides above it as a filter, while
/// `($n OR superseded_by IS NULL)` would leave the planner unable to prove the index predicate
/// under a generic plan.
///
/// The keyset comparison is a row comparison, so it rides the `(created_at, id)` ordering instead
/// of counting rows the way an offset does. An offset page shifts by one whenever a write lands
/// between two reads, and the reader sees a fact twice or never.
macro_rules! recent_sql {
    ($live:expr) => {
        concat!(
            r#"
            WITH reachable AS (
                SELECT namespace, min(sensitivity_rank(max)) AS max_rank
                  FROM unnest($2::text[], $3::text[]) AS g(namespace, max)
                 GROUP BY namespace
            )
            SELECT m.id, m.namespace, m.content, m.tags, m.source_client, m.embedding_model,
                   m.sensitivity, m.supersedes, m.superseded_by, m.superseded_at,
                   m.access_count, m.last_accessed_at, m.last_confirmed_at, m.created_at,
                   m.occurred_at, m.occurred_until
              FROM memory m
              JOIN reachable rg ON rg.namespace = m.namespace
             WHERE m.tenant_id = $1
               AND sensitivity_rank(m.sensitivity) <= rg.max_rank
               AND ($4::text IS NULL OR m.namespace = $4)
               AND ($5::timestamptz IS NULL OR (m.created_at, m.id) < ($5, $6::uuid))
               AND "#,
            $live,
            r#"
             ORDER BY m.created_at DESC, m.id DESC
             LIMIT $7
            "#
        )
    };
}

const RECENT_LIVE: &str = recent_sql!(live!());
/// History alongside the live rows, so a correction reads as a revision in place.
const RECENT_ALL: &str = recent_sql!("true");

/// Rows that left the live reads inside a window, newest first, with whatever retired them.
///
/// Two kinds of row. A supersession names the successor and stamps `superseded_at`. An expiry
/// closes the period with no successor and stamps nothing, so `expired` tells the page which it is
/// reading and `retired_at` takes whichever instant happened. Decision 0017 put the expired rows
/// here: they are off every live read, and a list of what left that showed only half of them would
/// send the owner looking in psql for the other half.
///
/// Ordered by that instant rather than `created_at`, which is the whole point: a fact written in
/// March and retired yesterday belongs at the top of this list and nowhere near the top of `recent`.
///
/// Both endpoints run through `reachable`. The successor is joined but never filtered on, because a
/// successor the caller cannot read would otherwise drop the retirement itself out of the list and
/// tell them nothing happened. Its content is not selected; the id and the namespace are what the
/// page needs, and the page reads as the owner.
///
/// `end_open` is the same-day case surfacing where somebody will see it: the row was retired and its
/// period never closed, so every as-of read still reports it as holding.
/// Edges from structure, in three statements that call no model.
///
/// Every one is `ON CONFLICT DO NOTHING`, so the whole rebuild is idempotent and safe hourly.
///
/// `shares_tag` excludes the tags every row carries. A tag on almost everything is not a subject,
/// it is a filing convention, and joining on it would make one hub of the entire store: the degree
/// cap would then skip it and the walk would answer nothing, which looks identical to a graph with
/// no edges. Excluding it at build time keeps the table honest about what it holds.
const SEED_SUPERSEDES_SQL: &str = r#"
    INSERT INTO memory_edge (tenant_id, src_id, dst_id, relation, produced_by)
    SELECT m.tenant_id, m.id, m.superseded_by, 'supersedes', 'structure'
      FROM memory m
     WHERE m.tenant_id = $1 AND m.superseded_by IS NOT NULL
    ON CONFLICT DO NOTHING
"#;

/// Two rows whose text uses two names the alias table says are one subject.
///
/// Only aliases that are current: `until IS NULL`. A name that stopped being current is exactly the
/// rename 0009 exists to record, and joining on it would tie a row to a subject it no longer names.
///
/// `least`/`greatest` normalises the stored pair so one row covers both directions. It cannot be
/// `a.id < b.id` here, which is the trap: that clause dedupes only when both sides are drawn from
/// the same predicate, and these are asymmetric. `a` must contain the alias and `b` the canonical
/// name, so an ordering filter would keep the pair only when the alias-mentioning row happened to
/// have the smaller uuid, dropping about half of all alias edges at random. `shares_tag` below can
/// use the ordering filter because both of its sides carry the identical predicate.
///
/// Both rows must be live and in the alias's own namespace. The content match is a substring, which
/// is loose; the degree cap is what stops a common word from making a hub, and `produced_by` names
/// this seeder so a bad pass can be undone by its own name.
const SEED_ALIAS_SQL: &str = r#"
    INSERT INTO memory_edge (tenant_id, src_id, dst_id, relation, produced_by)
    SELECT DISTINCT $1, least(a.id, b.id), greatest(a.id, b.id), 'shares_alias', 'structure'
      FROM entity_alias ea
      JOIN memory a ON a.tenant_id = $1 AND a.namespace = ea.namespace
                   AND a.superseded_by IS NULL
                   AND position(ea.alias in lower(a.content)) > 0
      JOIN memory b ON b.tenant_id = $1 AND b.namespace = ea.namespace
                   AND b.superseded_by IS NULL
                   AND position(lower(ea.canonical) in lower(b.content)) > 0
     WHERE ea.tenant_id = $1 AND ea.until IS NULL AND a.id <> b.id
    ON CONFLICT DO NOTHING
"#;

/// Pairs sharing a tag, with the ubiquitous tags left out.
///
/// `src_id < dst_id` writes one row per pair rather than two. The walk expands from either end, so
/// storing both directions would double the table to answer the same question.
const SEED_TAG_SQL: &str = r#"
    WITH common AS (
        SELECT t AS tag, count(*) AS n
          FROM memory m, unnest(m.tags) AS t
         WHERE m.tenant_id = $1 AND m.superseded_by IS NULL
         GROUP BY t
    ),
    usable AS (
        SELECT tag FROM common
         WHERE n >= 2 AND n <= $2
    )
    INSERT INTO memory_edge (tenant_id, src_id, dst_id, relation, produced_by)
    SELECT DISTINCT $1, a.id, b.id, 'shares_tag', 'structure'
      FROM usable u
      JOIN memory a ON a.tenant_id = $1 AND u.tag = ANY(a.tags) AND a.superseded_by IS NULL
      JOIN memory b ON b.tenant_id = $1 AND u.tag = ANY(b.tags) AND b.superseded_by IS NULL
     WHERE a.id < b.id
    ON CONFLICT DO NOTHING
"#;

/// A tag on more rows than this is a filing convention rather than a subject.
const TAG_HUB_LIMIT: i64 = 40;

/// One hop out, inside the caller's subgraph, with the grant applied to both endpoints.
///
/// `readable` is the subgraph: rows this caller may see, at their stored sensitivity, with retired
/// rows admitted only when the caller holds the history capability. Everything below joins through
/// it, so an edge touching a row the caller cannot read does not exist for this walk. That is the
/// severing 0014 mandates, and it is why nothing here reports what was withheld.
///
/// `deg` counts edges **within that subgraph**. A global count would answer partly from rows the
/// caller cannot read, which turns a hub's degree into an oracle on private write volume.
///
/// The fan-out cap is a window function outside any recursion, which is the reason this walks one
/// hop per call rather than recursing: Postgres forbids LIMIT and window functions in a recursive
/// term, so a per-parent cap cannot be expressed there. Depth two is two calls.
const GRAPH_NEIGHBOURS_SQL: &str = concat!(
    r#"
    WITH granted AS (
        SELECT prefix, exact, sensitivity_rank(max) AS max_rank
          FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
    ),
    readable AS (
        SELECT m.id
          FROM memory m
         WHERE m.tenant_id = $1
           AND ($8 OR ("#,
    live!(),
    r#"))
           AND EXISTS (
                 SELECT 1 FROM granted g
                  WHERE CASE WHEN g.exact
                             THEN m.namespace = g.prefix
                             ELSE left(m.namespace, length(g.prefix)) = g.prefix
                        END
                    AND sensitivity_rank(m.sensitivity) <= g.max_rank
               )
    ),
    sub AS (
        SELECT e.src_id, e.dst_id, e.relation
          FROM memory_edge e
          JOIN readable s ON s.id = e.src_id
          JOIN readable d ON d.id = e.dst_id
         WHERE e.tenant_id = $1
    ),
    deg AS (
        SELECT id, count(*) AS d
          FROM (SELECT src_id AS id FROM sub UNION ALL SELECT dst_id AS id FROM sub) x
         GROUP BY id
    ),
    expanded AS (
        SELECT s.src_id AS from_id, s.dst_id AS to_id, s.relation
          FROM sub s JOIN deg ON deg.id = s.src_id
         WHERE s.src_id = ANY($2) AND deg.d <= $6
        UNION ALL
        SELECT s.dst_id AS from_id, s.src_id AS to_id, s.relation
          FROM sub s JOIN deg ON deg.id = s.dst_id
         WHERE s.dst_id = ANY($2) AND deg.d <= $6
    )
    SELECT from_id, to_id, relation
      FROM (
            SELECT from_id, to_id, relation,
                   row_number() OVER (PARTITION BY from_id ORDER BY relation, to_id) AS rn
              FROM expanded
           ) t
     WHERE rn <= $7
"#
);

/// What supersession did to the periods it closed.
///
/// The successor is joined and filtered on the same axes as the predecessor, so a pair whose live
/// half the caller cannot read is not counted. Counting it would report a closed interval the caller
/// has no way to observe, which makes the measure describe somebody else's store.
///
/// `dated_but_open` is the one worth watching. Those rows were replaced and still read as holding at
/// every instant after their start, so an as-of query returns the fact and its replacement together.
const PAIR_COUNTS_SQL: &str = r#"
    WITH granted AS (
        SELECT prefix, exact, sensitivity_rank(max) AS max_rank
          FROM unnest($2::text[], $3::bool[], $4::text[]) AS g(prefix, exact, max)
    )
    SELECT count(*)                                             AS pairs,
           count(*) FILTER (WHERE m.occurred_until IS NOT NULL) AS closed,
           count(*) FILTER (WHERE m.occurred_at IS NOT NULL
                              AND m.occurred_until IS NULL)     AS dated_but_open,
           count(*) FILTER (WHERE m.occurred_at IS NOT NULL
                              AND s.occurred_at IS NOT NULL)    AS both_dated
      FROM memory m
      JOIN memory s ON s.id = m.superseded_by AND s.tenant_id = m.tenant_id
     WHERE m.tenant_id = $1
       AND m.superseded_by IS NOT NULL
       AND EXISTS (
             SELECT 1 FROM granted g
              WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                         ELSE left(m.namespace, length(g.prefix)) = g.prefix END
                AND sensitivity_rank(m.sensitivity) <= g.max_rank
           )
       AND EXISTS (
             SELECT 1 FROM granted g
              WHERE CASE WHEN g.exact THEN s.namespace = g.prefix
                         ELSE left(s.namespace, length(g.prefix)) = g.prefix END
                AND sensitivity_rank(s.sensitivity) <= g.max_rank
           )
"#;

/// Live rows carrying no start date, for the date review.
///
/// Live only. A retired row's missing start is not worth the owner's attention: nothing reads it as
/// current, and filling it would move a boundary inside a chain that is already closed.
const UNDATED_SQL: &str = concat!(
    r#"
    WITH reachable AS (
        SELECT namespace, min(sensitivity_rank(max)) AS max_rank
          FROM unnest($2::text[], $3::text[]) AS g(namespace, max)
         GROUP BY namespace
    )
    SELECT m.id, m.namespace, m.content, m.tags, m.source_client, m.embedding_model,
           m.sensitivity, m.supersedes, m.superseded_by, m.superseded_at,
           m.access_count, m.last_accessed_at, m.last_confirmed_at, m.created_at,
           m.occurred_at, m.occurred_until
      FROM memory m
      JOIN reachable rg ON rg.namespace = m.namespace
     WHERE m.tenant_id = $1
       AND sensitivity_rank(m.sensitivity) <= rg.max_rank
       AND m.occurred_at IS NULL
       AND "#,
    live!(),
    r#"
     ORDER BY m.created_at DESC, m.id DESC
     LIMIT $4
"#
);

const RETIRED_SQL: &str = r#"
    WITH reachable AS (
        SELECT namespace, min(sensitivity_rank(max)) AS max_rank
          FROM unnest($2::text[], $3::text[]) AS g(namespace, max)
         GROUP BY namespace
    )
    SELECT m.id, m.namespace, m.content, m.sensitivity, m.occurred_at,
           m.occurred_until,
           -- Two ways a fact leaves the live reads, and one list. A supersession stamps
           -- `superseded_at`; an expiry closes the period and stamps nothing, so the instant this
           -- page dates the row by comes from whichever of the two happened.
           COALESCE(m.superseded_at, m.occurred_until) AS retired_at,
           -- Expired reads off `superseded_at` rather than `superseded_by`. A restore that could
           -- not relink a successor leaves a row carrying `superseded_at` and `occurred_until`
           -- with a NULL link, and that row was retired by a supersession whose successor is
           -- missing. Reading the link would file it as expired on every surface, take away its
           -- replace form and refuse a supersession over it.
           (m.superseded_at IS NULL AND m.occurred_until IS NOT NULL
              AND m.occurred_until <= now()) AS expired,
           (m.occurred_at IS NOT NULL AND m.occurred_until IS NULL) AS end_open,
           s.id AS successor_id, s.namespace AS successor_namespace
      FROM memory m
      JOIN reachable rg ON rg.namespace = m.namespace
      LEFT JOIN memory s ON s.id = m.superseded_by AND s.tenant_id = m.tenant_id
     WHERE m.tenant_id = $1
       AND sensitivity_rank(m.sensitivity) <= rg.max_rank
       AND ((m.superseded_at IS NOT NULL AND m.superseded_at >= $4)
         -- The expired arm. `occurred_until <= now()` is the negation of the period half of
         -- `live!()`, so a row is on this page exactly when it is off every live read. A future
         -- end is a fact that still holds and stays off the list. `superseded_at IS NULL` is what
         -- keeps a supersession out of this arm; it is already in the one above.
         OR (m.superseded_at IS NULL AND m.occurred_until IS NOT NULL
             AND m.occurred_until <= now() AND m.occurred_until >= $4))
     ORDER BY COALESCE(m.superseded_at, m.occurred_until) DESC, m.id DESC
     LIMIT $5
"#;

/// Per-namespace counts and the last write, on both axes.
///
/// The same `reachable` join every other read carries. Without it this statement publishes a
/// namespace name and a row count for a namespace the caller may not read, which is the digest
/// inventory bug under a different name: the content refused, the name and the number handed over.
const NAMESPACE_SUMMARY_SQL: &str = concat!(
    r#"
    WITH reachable AS (
        SELECT namespace, min(sensitivity_rank(max)) AS max_rank
          FROM unnest($2::text[], $3::text[]) AS g(namespace, max)
         GROUP BY namespace
    )
    SELECT m.namespace,
           -- `live` is `live!()`, so it agrees with the digest's `by_namespace` arm. A row whose
           -- period closed with no successor lands in neither `live` nor `retired`: the rail prints
           -- two numbers per namespace in a column the width of a phrase, and a third would cost
           -- more room than the state is worth. The retired page lists those rows and names them.
           count(*) FILTER (WHERE "#,
    live!(),
    r#") AS live,
           count(*) FILTER (WHERE m.superseded_by IS NOT NULL) AS retired,
           count(*) FILTER (WHERE "#,
    live!(),
    r#"
                              AND sensitivity_rank(m.sensitivity) > sensitivity_rank('open'))
             AS above_open,
           max(m.created_at) AS last_write
      FROM memory m
      JOIN reachable rg ON rg.namespace = m.namespace
     WHERE m.tenant_id = $1
       AND sensitivity_rank(m.sensitivity) <= rg.max_rank
     GROUP BY m.namespace
     ORDER BY m.namespace
"#
);

/// The recall monitor's probe sample: plaintext rows this caller may read.
///
/// The same `reachable` join every other read uses, because the monitor's report quotes the opening
/// characters of each probe and `/admin/recall` is reached with a bearer token and no scope. Without
/// the join it read open content out of every namespace in the tenant to whoever asked.
const SAMPLE_CONTENT_SQL: &str = r#"
    WITH reachable AS (
        SELECT namespace, min(sensitivity_rank(max)) AS max_rank
          FROM unnest($2::text[], $3::text[]) AS g(namespace, max)
         GROUP BY namespace
    )
    SELECT m.content
      FROM memory m
      JOIN reachable rg ON rg.namespace = m.namespace
     WHERE m.tenant_id = $1
       AND sensitivity_rank(m.sensitivity) <= rg.max_rank
       AND m.content IS NOT NULL
       AND m.superseded_by IS NULL
     ORDER BY random() LIMIT $4
"#;

/// The digest, as one statement with seven subqueries.
///
/// Deliberately not decomposed: the bootstrap latency budget depends on this staying a single round
/// trip. Every one of the seven joins `reachable` and compares `sensitivity_rank`, including the
/// two counts and the namespace inventory. Phase 1 shipped a version where the profile and project
/// subqueries skipped the namespace filter, and the leak path in a memory system is the convenience
/// surface rather than the obvious one; the unit test at the bottom of this file counts the joins so
/// a later edit cannot drop one silently.
const DIGEST_SQL: &str = concat!(
    r#"
    WITH reachable AS (
        SELECT namespace, min(sensitivity_rank(max)) AS max_rank
          FROM unnest($6::text[], $7::text[]) AS g(namespace, max)
         GROUP BY namespace
    )
    SELECT json_build_object(
        'profile', COALESCE((
            SELECT json_agg(f) FROM (
              SELECT m.id, m.namespace, m.content, m.tags, m.source_client, m.embedding_model,
                     m.sensitivity, m.supersedes, m.superseded_by, m.superseded_at,
                     m.access_count, m.last_accessed_at, m.last_confirmed_at, m.created_at,
                     m.occurred_at, m.occurred_until
                FROM memory m
                JOIN reachable rg ON rg.namespace = m.namespace
               WHERE m.tenant_id = $1
                 AND sensitivity_rank(m.sensitivity) <= rg.max_rank
                 AND "#,
    live!(),
    r#"
                 -- 'global' is a namespace like any other and has to be granted. The join is what
                 -- enforces that; this line only narrows which granted namespaces are profile.
                 AND m.namespace IN ($2, 'global')
               ORDER BY (m.tags && ARRAY['profile','preference','identity']) DESC, m.created_at DESC
               LIMIT $3
            ) f), '[]'::json),
        'project_context', COALESCE((
            SELECT json_agg(f) FROM (
              SELECT m.id, m.namespace, m.content, m.tags, m.source_client, m.embedding_model,
                     m.sensitivity, m.supersedes, m.superseded_by, m.superseded_at,
                     m.access_count, m.last_accessed_at, m.last_confirmed_at, m.created_at,
                     m.occurred_at, m.occurred_until
                FROM memory m
                JOIN reachable rg ON rg.namespace = m.namespace
               WHERE m.tenant_id = $1
                 AND sensitivity_rank(m.sensitivity) <= rg.max_rank
                 AND "#,
    live!(),
    r#"
                 AND $4::text IS NOT NULL AND m.namespace = $4
               ORDER BY m.created_at DESC
               LIMIT $5
            ) f), '[]'::json),
        'recent', COALESCE((
            SELECT json_agg(f) FROM (
              SELECT m.id, m.namespace, m.content, m.tags, m.source_client, m.embedding_model,
                     m.sensitivity, m.supersedes, m.superseded_by, m.superseded_at,
                     m.access_count, m.last_accessed_at, m.last_confirmed_at, m.created_at,
                     m.occurred_at, m.occurred_until
                FROM memory m
                JOIN reachable rg ON rg.namespace = m.namespace
               WHERE m.tenant_id = $1
                 AND sensitivity_rank(m.sensitivity) <= rg.max_rank
                 AND "#,
    live!(),
    r#"
                 AND m.created_at > now() - ($8 || ' days')::interval
               ORDER BY m.created_at DESC
               LIMIT $9
            ) f), '[]'::json),
        'registry', COALESCE((
            SELECT json_agg(r) FROM (
              -- The registry carries its own sensitivity column and holds credential locations.
              -- A digest arm that skipped the ceiling here would be the leak, and this is exactly
              -- the convenience surface where it would go unnoticed.
              SELECT e.namespace, e.kind, e.key, e.value
                FROM registry e
                JOIN reachable rg ON rg.namespace = e.namespace
               WHERE e.tenant_id = $1
                 AND sensitivity_rank(e.sensitivity) <= rg.max_rank
               ORDER BY e.kind, e.key
               LIMIT $10
            ) r), '[]'::json),
        'memories_count', (
            SELECT count(*) FROM memory m
              JOIN reachable rg ON rg.namespace = m.namespace
             WHERE m.tenant_id = $1
               AND sensitivity_rank(m.sensitivity) <= rg.max_rank
               AND "#,
    live!(),
    r#"),
        'registry_count', (
            SELECT count(*) FROM registry e
              JOIN reachable rg ON rg.namespace = e.namespace
             WHERE e.tenant_id = $1
               AND sensitivity_rank(e.sensitivity) <= rg.max_rank),
        'by_namespace', COALESCE((
            SELECT json_object_agg(namespace, n) FROM (
              -- The inventory line. It names namespaces and row counts, which is enough to leak
              -- the existence of a namespace this client cannot read, so it carries the same
              -- filter as every arm above it.
              SELECT m.namespace, count(*) AS n
                FROM memory m
                JOIN reachable rg ON rg.namespace = m.namespace
               WHERE m.tenant_id = $1
                 AND sensitivity_rank(m.sensitivity) <= rg.max_rank
                 AND "#,
    live!(),
    r#"
               GROUP BY m.namespace
            ) c), '{}'::json)
    )
"#
);

/// Chain walk shared by the cycle check and by `supersession_head`.
///
/// Depth-capped, so a cycle that is already in the table produces a bounded answer rather than a
/// statement that runs until the timeout.
const CHAIN_IDS_SQL: &str = r#"
    WITH RECURSIVE chain AS (
        SELECT id, superseded_by, 1 AS depth
          FROM memory WHERE tenant_id = $1 AND id = $2
        UNION ALL
        SELECT m.id, m.superseded_by, c.depth + 1
          FROM memory m
          JOIN chain c ON m.id = c.superseded_by
         WHERE m.tenant_id = $1 AND c.depth < $3
    )
    SELECT id FROM chain
"#;

/// The chain walk that answers "which row is live now", hoisted out of the method it serves.
///
/// A constant rather than an inline literal so the column-list test below can read it. This is the
/// sixth hand-written list mirroring `select_memory!`, it cannot use the macro because it sits
/// inside a recursive CTE, and `memory_from_row` panics on a column a query forgot.
const SUPERSESSION_HEAD_SQL: &str = r#"
    WITH RECURSIVE chain AS (
        SELECT id, superseded_by, 1 AS depth
          FROM memory WHERE tenant_id = $1 AND id = $2
        UNION ALL
        SELECT m.id, m.superseded_by, c.depth + 1
          FROM memory m
          JOIN chain c ON m.id = c.superseded_by
         WHERE m.tenant_id = $1 AND c.depth < $3
    )
    SELECT m.id, m.namespace, m.content, m.tags, m.source_client, m.embedding_model,
           m.sensitivity, m.supersedes, m.superseded_by, m.superseded_at,
           m.access_count, m.last_accessed_at, m.last_confirmed_at, m.created_at,
           m.occurred_at, m.occurred_until
      FROM chain c
      JOIN memory m ON m.id = c.id
     ORDER BY c.depth DESC
     LIMIT 1
"#;

/// One subject's timeline: the anchor row and every row on its supersession chain, oldest first.
///
/// Two walks of the shape `CHAIN_IDS_SQL` already uses, because the anchor can sit anywhere on the
/// chain. `forward` follows `superseded_by` towards the row that is live now; `backward` follows it
/// in reverse, towards the first version. Both are depth-capped for the reason the other walks are:
/// a cycle already in the table would otherwise run until the statement timeout.
///
/// Depth is the ordering and the answer is read from it. Negating the backward leg puts the oldest
/// version at the most negative depth, the anchor at zero and the live row last, so `ORDER BY depth`
/// says "was 8080, then 8787" without comparing dates that may be NULL. `created_at` and `id` break
/// the tie, because a row that retired two predecessors gives the backward walk two rows at one
/// depth and an unordered answer is a different timeline on every call.
///
/// `min(depth)` collapses the union to one row per id. A chain with no cycle gives the anchor twice
/// at zero and everything else once; a chain with a cycle gives an id several depths, and one row
/// per id beats a timeline that lists the same version four times.
///
/// **The walk crosses namespaces and the grant filters the rows.** `write::run` permits a
/// supersede whose successor sits in another namespace, so a real chain can cross one. A walk
/// scoped to a single namespace stopped at that boundary and handed back a short history with
/// nothing saying so, which is the same failure as filtering inside the recursion and arrives
/// through the other axis. Both axes now filter at the final join and neither steers the walk.
///
/// Filtering either axis inside the recursion would sever the chain at the first row the caller
/// may not read and hide every version behind it, reporting a partial timeline as a complete one.
/// Filtering here drops that version, keeps the versions past it, and counts what was dropped, so
/// the gap is something the caller is told about rather than something they cannot see.
///
/// **The grant is matched in SQL, which is the one place in this file that has to be.** Every
/// other read resolves globs to concrete namespaces with `policy::resolve` before the query runs.
/// That is impossible here: which namespaces a chain visits is the answer, not the question, so
/// there is nothing to resolve the globs against until the walk has already run. The `EXISTS` is
/// `policy::admits` spelled in SQL, union across matching patterns like `policy::ceiling`, and
/// `split_grants` below turns each pattern into the prefix comparison that mirrors
/// `namespaces::matches`. The digest's `min()` collapse is a different rule for a different input:
/// it deduplicates already-resolved concrete ceilings.
///
/// The anchor is gated on being readable itself. Without that, a caller naming an id they may not
/// read would learn the shape of its chain from the rows around it, where the contract says an id
/// above the grant answers nothing at all. The gate cannot be `depth = 0`, because a cycle can
/// hand the anchor a negative depth through the backward leg.
const SUBJECT_HISTORY_SQL: &str = select_memory!(
    r#"
    WITH RECURSIVE forward AS (
        SELECT id, superseded_by, 0 AS depth
          FROM memory WHERE tenant_id = $1 AND id = $2
        UNION ALL
        SELECT m.id, m.superseded_by, f.depth + 1
          FROM memory m
          JOIN forward f ON m.id = f.superseded_by
         WHERE m.tenant_id = $1 AND f.depth < $3
    ),
    backward AS (
        SELECT id, 0 AS depth
          FROM memory WHERE tenant_id = $1 AND id = $2
        UNION ALL
        SELECT m.id, b.depth + 1
          FROM memory m
          JOIN backward b ON m.superseded_by = b.id
         WHERE m.tenant_id = $1 AND b.depth < $3
    ),
    chain AS (
        SELECT id, min(depth) AS depth
          FROM (
                SELECT id, depth FROM forward
                UNION ALL
                SELECT id, -depth FROM backward
               ) u
         GROUP BY id
    ),
    granted AS (
        -- The grant as a table: a prefix, whether the match is exact, and the ceiling it carries.
        SELECT prefix, exact, sensitivity_rank(max) AS max_rank
          FROM unnest($4::text[], $5::bool[], $6::text[]) AS g(prefix, exact, max)
    ),
    walked AS (
        -- Every version on the chain with the verdict on it. Unreadable rows stay here so the
        -- count can see them and go no further: nothing below selects their content.
        SELECT c.id, c.depth,
               EXISTS (
                   SELECT 1
                     FROM granted g
                    WHERE CASE WHEN g.exact
                               THEN m.namespace = g.prefix
                               ELSE left(m.namespace, length(g.prefix)) = g.prefix
                          END
                      AND sensitivity_rank(m.sensitivity) <= g.max_rank
               ) AS readable
          FROM chain c
          JOIN memory m ON m.id = c.id
         WHERE m.tenant_id = $1
    ),
    gap AS (
        -- What the caller is owed about what they cannot see: how many versions, and whether the
        -- walk stopped because it ran out of hops rather than out of chain.
        SELECT count(*) FILTER (WHERE NOT readable) AS withheld,
               coalesce(bool_or(abs(depth) >= $3), false) AS depth_capped
          FROM walked
    )
    "#,
    "m.",
    r#", gap.withheld, gap.depth_capped
      FROM walked w
      JOIN memory m ON m.id = w.id
      CROSS JOIN gap
     WHERE m.tenant_id = $1
       AND w.readable
       AND EXISTS (SELECT 1 FROM walked a WHERE a.id = $2 AND a.readable)
     ORDER BY w.depth, m.created_at, m.id"#
);

/// Retire one row in favour of another, and end its validity in the same statement.
///
/// One constant for both supersession paths: the write that carries `supersedes` and the standalone
/// link. They wrote the same UPDATE with one difference before this, and the difference was the
/// kind nobody notices.
///
/// `COALESCE` on the column, not on the parameter. A predecessor that already carried an end keeps
/// it: the end of a period is a fact about the world, and a later correction to the link does not
/// move it. A NULL `$4` therefore leaves the column alone, which is how the flagged case below
/// writes nothing.
const RETIRE_PREDECESSOR_SQL: &str = r#"
    UPDATE memory
       SET superseded_by  = $3,
           superseded_at  = now(),
           occurred_until = COALESCE(occurred_until, $4::timestamptz)
     WHERE tenant_id = $1 AND id = $2 AND superseded_by IS NULL
       -- An expired row takes no successor. `write::validate_supersedes_target` refuses it first
       -- and this is the same test inside the statement, spelled the way the state is defined
       -- everywhere else: a row carrying `superseded_at` was retired by a supersession and is
       -- still replaceable, a row carrying an end and no stamp expired and is not.
       AND (superseded_at IS NOT NULL OR occurred_until IS NULL)
"#;

/// Close a row's validity with no successor.
///
/// `superseded_at` stays NULL, and that is the whole shape of the state. The column means "a
/// successor retired this row", so writing it here made the retired page print "replaced by a row
/// that has since been deleted" about a fact nothing had replaced. An expired row is neither live
/// nor retired, and every surface that reads it says so in its own words.
///
/// `superseded_by IS NULL` keeps this path away from a supersession: a row a successor already
/// retired has an end that supersession wrote, and moving it would rewrite the timeline under the
/// row that replaced it. `occurred_until IS NULL` makes the statement idempotent, so a second call
/// moves nothing and returns no row.
const EXPIRE_SQL: &str = r#"
    UPDATE memory
       SET occurred_until = now()
     WHERE tenant_id = $1 AND id = $2
       AND superseded_by IS NULL AND occurred_until IS NULL
    RETURNING occurred_until
"#;

/// Reopen a row, guarded on the instant that closed it.
///
/// One column, because `expire` wrote one. A statement that also cleared `superseded_at` would
/// erase a supersession's own stamp if the guard ever admitted a superseded row.
///
/// `occurred_until = $3` does here what `superseded_by = $3` does in a guarded revive: a close
/// somebody else wrote is not this caller's to undo, so the statement moves nothing rather than
/// reopening a fact a second decision ended.
const UNEXPIRE_SQL: &str = r#"
    UPDATE memory
       SET occurred_until = NULL
     WHERE tenant_id = $1 AND id = $2
       AND superseded_by IS NULL AND occurred_until = $3
    RETURNING id
"#;

/// Everything one tenant holds, live and retired, ordered by id so the keyset cursor is total.
///
/// Keyset rather than `LIMIT`/`OFFSET`: an archive of a large store takes many pages, and a write
/// landing mid-read shifts every later offset by one, so the archive gains a duplicate row or drops
/// one. Ordering by the primary key also gives two archives of an unchanged store the same row
/// order, which is what makes them comparable.
///
/// `$2::uuid IS NULL` rather than two statements. The cast is load-bearing: without it Postgres
/// cannot infer a type for the first page's NULL cursor.
const LIST_WHOLE_STORE_SQL: &str = select_memory!(
    "",
    "FROM memory
      WHERE tenant_id = $1
        AND ($2::uuid IS NULL OR id > $2)
      ORDER BY id
      LIMIT $3"
);

/// A row put back exactly as it was recorded, for restore alone.
///
/// The only INSERT in this file besides `insert`, and the difference is the tail of the column
/// list: `created_at`, `access_count`, `last_accessed_at`, `last_confirmed_at`, `occurred_until`
/// and `superseded_at` are all bound rather than defaulted. A restore that let those default would
/// hand back a store whose every row was learned the day it was imported.
///
/// `kek_id` is bound from this install's `kek_state` and never from the caller. No archive carries
/// one, and a row wrapped by a key named in an archive would be a row this deployment can never
/// open.
///
/// `supersedes` and `superseded_by` are both foreign keys into this table. Bind them here only when
/// the target row already exists; `relink_restored` is the second pass for the general case.
const RESTORE_ROW_SQL: &str = r#"
    INSERT INTO memory (id, tenant_id, namespace, content, embedding, tags, supersedes,
                        source_client, embedding_model, sensitivity,
                        content_ct, content_nonce, dek_wrapped, dek_nonce, enc_alg, kek_id,
                        occurred_at, occurred_until, superseded_by, superseded_at,
                        access_count, last_accessed_at, last_confirmed_at, created_at)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
            $17, $18, $19, $20, $21, $22, $23, $24)
"#;

/// The second pass of a restore: the chain links, once every row they point at is in the table.
///
/// It writes both columns unconditionally rather than coalescing. A restored row starts with two
/// NULLs and this is the only statement that fills them, so there is nothing to preserve, and a
/// COALESCE would make an archive that genuinely records "no predecessor" unrepresentable.
const RELINK_RESTORED_SQL: &str = r#"
    UPDATE memory
       SET supersedes = $3, superseded_by = $4
     WHERE tenant_id = $1 AND id = $2
"#;

/// What a supersession writes into the predecessor's `occurred_until`, and when it refuses.
///
/// The rule, in one place, for both callers.
///
/// A change ends a period and never moves its start, so this is the only valid-time column a
/// supersession touches. The value is the successor's own start where it has one and its
/// `created_at` otherwise. That second arm admits ignorance rather than measuring anything: it
/// says when the store learned of the replacement, which is transaction time standing in for valid
/// time. Ingestion fills `occurred_at` before a proposal reaches this path, so the rows this phase
/// was built for never take it.
///
/// Two cases write no end at all.
///
/// **A strict inversion is refused.** A successor that became true before the fact it replaces
/// would end that fact a month before it started, and the CHECK from migration 011 would reject
/// the write somewhere the caller cannot read. The refusal names both dates so the owner can see
/// which two rows disagree. The live case is a backfill: July facts approved after August ones.
///
/// **An end at the predecessor's own start is dropped.** Under the half-open convention `[T, T)`
/// is an empty period, which is the idiom for "this was never true", and a caller who asked to
/// replace a fact did not ask to erase it. The link is still written; only the end stays unknown,
/// and the caller logs it.
fn supersession_until(
    predecessor_occurred_at: Option<DateTime<Utc>>,
    successor_occurred_at: Option<DateTime<Utc>>,
    successor_created_at: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let until = successor_occurred_at.unwrap_or(successor_created_at);
    let Some(start) = predecessor_occurred_at else {
        return Ok(Some(until));
    };
    if let Some(successor_start) = successor_occurred_at {
        if successor_start < start {
            return Err(DomainError::validation(format!(
                "this fact became true at {}, before {} when the fact it replaces became true, \
                 so it cannot be what ended that fact: write the older fact first, or correct \
                 the dates",
                successor_start.to_rfc3339(),
                start.to_rfc3339()
            )));
        }
    }
    Ok((until > start).then_some(until))
}

/// The trace for a supersession that could not date the end of what it retired.
///
/// A warn rather than a refusal, because the link itself is right and only the valid-time end is
/// unknowable from what arrived. Both ids and the date are in the line: without them the operator
/// knows a timeline has a hole and not where.
fn warn_on_open_validity(
    predecessor: uuid::Uuid,
    successor: uuid::Uuid,
    predecessor_occurred_at: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) {
    if let (Some(start), None) = (predecessor_occurred_at, until) {
        tracing::warn!(
            %predecessor,
            %successor,
            predecessor_occurred_at = %start.to_rfc3339(),
            "the replacement does not start after the fact it retires, so that fact keeps an open \
             end rather than one that would be earlier than its start"
        );
    }
}

fn memory_from_row(r: &sqlx::postgres::PgRow) -> Memory {
    Memory {
        id: r.get::<uuid::Uuid, _>("id").to_string(),
        namespace: r.get("namespace"),
        // NULL means the row is encrypted. It leaves here empty and stays empty: a ciphertext blob
        // rendered into a content string is the corruption this seam exists to prevent, and only
        // the service holds the KEK that could fill it in honestly.
        content: r.get::<Option<String>, _>("content").unwrap_or_default(),
        tags: r.get::<Vec<String>, _>("tags"),
        source_client: r.get("source_client"),
        embedding_model: r.get("embedding_model"),
        // An unrecognised level reads as the most restrictive rather than as `open`. The query
        // filters on sensitivity_rank, which scores an unknown level 99 and admits it under no
        // ceiling, so a row here has already passed; this is the second line of the same rule.
        sensitivity: Sensitivity::parse(r.get::<&str, _>("sensitivity"))
            .unwrap_or(Sensitivity::Sealed),
        supersedes: r.get::<Option<uuid::Uuid>, _>("supersedes").map(|u| u.to_string()),
        superseded_by: r.get::<Option<uuid::Uuid>, _>("superseded_by").map(|u| u.to_string()),
        superseded_at: r.get("superseded_at"),
        access_count: r.get("access_count"),
        last_accessed_at: r.get("last_accessed_at"),
        last_confirmed_at: r.get("last_confirmed_at"),
        created_at: r.get::<DateTime<Utc>, _>("created_at"),
        // Valid time. Every select list in this file carries both columns, and it has to: `get`
        // panics on a column the query did not ask for, so a list that misses one breaks the
        // request path it serves while every other path keeps working.
        occurred_at: r.get("occurred_at"),
        occurred_until: r.get("occurred_until"),
    }
}

/// A grant as three parallel arrays: the text to compare, whether the comparison is exact, and the
/// ceiling that pattern carries.
///
/// `namespaces::matches` rewritten as something Postgres can compare. It has three cases and so
/// does this: `*` matches everything, which is the empty prefix; a trailing star is a prefix match
/// on what precedes it; anything else is the whole name. Trimmed and lowercased here for the
/// reason `matches` does it, because a grant is operator-written text and stored namespaces are
/// normalised on the way in.
///
/// Only the timeline walk uses this. Every other read resolves globs against a requested namespace
/// list before the query, and should keep doing so; a chain has no such list, which is the whole
/// argument for matching a glob in SQL at all.
fn split_grants(grants: &[NamespaceGrant]) -> (Vec<String>, Vec<bool>, Vec<String>) {
    let mut prefixes = Vec::with_capacity(grants.len());
    let mut exact = Vec::with_capacity(grants.len());
    let mut maxima = Vec::with_capacity(grants.len());
    for g in grants {
        let pattern = g.namespace.trim().to_ascii_lowercase();
        match pattern.strip_suffix('*') {
            Some(prefix) => {
                prefixes.push(prefix.to_string());
                exact.push(false);
            }
            None => {
                prefixes.push(pattern);
                exact.push(true);
            }
        }
        maxima.push(g.max.as_str().to_string());
    }
    (prefixes, exact, maxima)
}

/// Namespaces and ceilings as two parallel arrays, which is the only form the `unnest` join takes.
fn split_ceilings(ceilings: &[NamespaceCeiling]) -> (Vec<String>, Vec<String>) {
    let mut namespaces = Vec::with_capacity(ceilings.len());
    let mut maxima = Vec::with_capacity(ceilings.len());
    for c in ceilings {
        namespaces.push(c.namespace.clone());
        maxima.push(c.max.as_str().to_string());
    }
    (namespaces, maxima)
}

/// The constraint name when a statement failed on a foreign key, SQLSTATE 23503.
///
/// A delete blocked by a reference is the caller's problem to look at, and "internal error" sends
/// them to the server log for a fact the database already stated in one word.
fn foreign_key_blocker(e: &sqlx::Error) -> Option<String> {
    let db = e.as_database_error()?;
    if db.code().as_deref() != Some("23503") {
        return None;
    }
    Some(db.constraint().unwrap_or("a foreign key").to_string())
}

/// A foreign key tripped while a delete edited the chain: a neighbour appeared or vanished between
/// the plan and the transaction. A conflict, so the caller plans again; anything else stays the
/// internal error it is.
fn chain_conflict(id: uuid::Uuid, e: sqlx::Error) -> DomainError {
    match foreign_key_blocker(&e) {
        Some(constraint) => DomainError::conflict(format!(
            "memory {id} could not be deleted: {constraint} changed while the delete was being \
             planned. Run the delete again."
        ))
        .with_source(e),
        None => DomainError::from(e),
    }
}

/// What `enc_alg` says, mapped onto the one label this build implements.
///
/// The column is text and anything with database write access can set it. Recognised means the
/// constant; everything else means a marker, because `envelope::open` refuses a label that is not
/// its own and that is the wanted behaviour. Leaking a `&'static str` out of an attacker-writable
/// column to satisfy the lifetime would not be.
fn known_alg(row_id: uuid::Uuid, stored: &str) -> &'static str {
    if stored == crate::crypto::envelope::ALG {
        crate::crypto::envelope::ALG
    } else {
        tracing::error!(
            row_id = %row_id,
            stored,
            expected = crate::crypto::envelope::ALG,
            "row carries an encryption algorithm this build does not implement"
        );
        UNRECOGNISED_ALG
    }
}

const UNRECOGNISED_ALG: &str = "unrecognised";

#[async_trait]
impl MemoryRepository for PgMemoryRepository {
    /// The ciphertext columns for rows the caller already holds, so the service can decrypt them.
    ///
    /// Kept apart from every other read rather than folded into one: `Memory` has no ciphertext
    /// field, the KEK lives one layer up, and a read path that returned both would make it possible
    /// to hand a caller ciphertext in a text field by mistake. Batched by id because the shape that
    /// needs it is a search result rather than a single row.
    async fn sealed_batch(
        &self,
        tenant: &str,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, SealedContent, Option<String>)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let rows = sqlx::query(
            "SELECT id, content_ct, content_nonce, dek_wrapped, dek_nonce, enc_alg, kek_id
               FROM memory
              WHERE tenant_id = $1 AND id = ANY($2) AND content_ct IS NOT NULL",
        )
        .bind(tenant)
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| {
                let id: uuid::Uuid = r.get("id");
                let stored: String = r.get::<Option<String>, _>("enc_alg").unwrap_or_default();
                (
                    id,
                    SealedContent {
                        content_ct: r.get("content_ct"),
                        content_nonce: r
                            .get::<Option<Vec<u8>>, _>("content_nonce")
                            .unwrap_or_default(),
                        dek_wrapped: r.get::<Option<Vec<u8>>, _>("dek_wrapped").unwrap_or_default(),
                        dek_nonce: r.get::<Option<Vec<u8>>, _>("dek_nonce").unwrap_or_default(),
                        enc_alg: known_alg(id, &stored),
                    },
                    r.get::<Option<String>, _>("kek_id"),
                )
            })
            .collect())
    }

    /// One row's ciphertext. Convenience over [`Self::sealed_batch`] for `memory_get`.
    async fn sealed_one(
        &self,
        tenant: &str,
        id: uuid::Uuid,
    ) -> Result<Option<(SealedContent, Option<String>)>> {
        Ok(MemoryRepository::sealed_batch(self, tenant, &[id])
            .await?
            .pop()
            .map(|(_, sealed, kek_id)| (sealed, kek_id)))
    }

    async fn search(&self, q: SearchQuery) -> Result<Vec<SearchHit>> {
        if q.primary.is_empty() && q.secondary.is_empty() {
            return Ok(vec![]);
        }
        let (primary_ns, primary_max) = split_ceilings(&q.primary);
        let (secondary_ns, secondary_max) = split_ceilings(&q.secondary);
        let embedding = pgvector::Vector::from(q.embedding);
        // Over-fetch each arm so the blend, the penalty and the use boost have something to rerank.
        let candidates = (q.limit * 4).max(20);

        // `as_of` decides first and `include_superseded` says nothing under it: the period
        // predicate already reaches retired rows, which is the whole reason to ask.
        let sql = match (self.fusion, q.as_of.is_some(), q.include_superseded) {
            (Fusion::Linear, true, _) => SEARCH_AS_OF,
            (Fusion::Rrf, true, _) => SEARCH_RRF_AS_OF,
            (Fusion::Linear, false, false) => SEARCH_LIVE,
            (Fusion::Linear, false, true) => SEARCH_ALL,
            (Fusion::Rrf, false, false) => SEARCH_RRF_LIVE,
            (Fusion::Rrf, false, true) => SEARCH_RRF_ALL,
        };
        let mut stmt = sqlx::query(sql)
            .bind(&q.tenant_id)
            .bind(&primary_ns)
            .bind(&primary_max)
            .bind(&secondary_ns)
            .bind(&secondary_max)
            .bind(&embedding)
            .bind(candidates)
            .bind(&q.text)
            .bind(q.weights.vector)
            .bind(q.weights.lexical)
            .bind(q.weights.secondary_penalty)
            .bind(q.limit)
            .bind(q.weights.usage);
        // Postgres refuses a bind message whose count disagrees with the statement, so `k` goes on
        // only for the text that mentions $14. Binding it against the linear query would turn every
        // search into a protocol error.
        //
        // The two conditionals run in this order because that is the order the parameters are
        // numbered in: as-of is $14 in the linear text, where nothing else claims it, and $15 under
        // rank fusion, which already binds `k` as the fourteenth. Renumbering `k` would have given
        // both blends the same number and rewritten two statements that are not changing.
        if self.fusion == Fusion::Rrf {
            stmt = stmt.bind(self.rrf_k);
        }
        if let Some(as_of) = q.as_of {
            stmt = stmt.bind(as_of);
        }
        let rows = stmt.fetch_all(&self.pool).await?;

        Ok(rows
            .iter()
            .map(|r| SearchHit {
                memory: memory_from_row(r),
                score: round4(r.get::<f64, _>("score")),
                similarity: round4(r.get::<f64, _>("similarity")),
                primary: r.get("is_primary"),
            })
            .collect())
    }

    async fn insert(&self, m: NewMemory) -> Result<Memory> {
        // Which KEK wrapped this row, read from `kek_state` rather than taken on trust.
        //
        // Migration 008's CHECK enforces that exactly one content representation is present, but it
        // says nothing about kek_id: a sealed row written without one is valid SQL and unrotatable
        // afterwards, and a rewrap that cannot find the rows it has to rewrap is indistinguishable
        // from data loss.
        //
        // `kek_state` holds one row per tenant, written by the boot check only after it unwrapped
        // the live key and matched the recorded fingerprint. Reading it here is therefore also the
        // last line of the rule migration 008 states in a comment and the Phase 3 spec puts at step
        // 4: do not write an encrypted row until a restart has proved the key can be recovered.
        // One extra indexed lookup, on the private write path only.
        let kek_id: Option<String> = match &m.sealed {
            None => None,
            Some(_) => Some(
                sqlx::query_scalar::<_, String>(
                    "SELECT kek_id FROM kek_state WHERE tenant_id = $1",
                )
                .bind(&m.tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    DomainError::unavailable(
                        "no verified encryption key is recorded, so an encrypted row cannot be \
                         written: it could not be rewrapped or read after a rotation",
                    )
                })?,
            ),
        };

        let embedding = pgvector::Vector::from(m.embedding);
        let sealed = m.sealed.as_ref();
        // The embedding stays plaintext for a private row, because search has to work. That trade
        // is stated and defended in docs/research/encryption-and-sensitivity.md rather than made
        // quietly here.
        //
        // `content` is dropped rather than stored when the row arrives sealed, which is the port's
        // documented contract. It is also the single line that decides whether this seam leaks:
        // there is no branch below that can write a plaintext column for an encrypted row.
        let content = sealed.is_none().then_some(m.content.as_str());

        // A write carrying `supersedes` is the correction case, and both halves of the link belong
        // in one transaction. Phase 1 wrote only this row's `supersedes` and left the row it
        // corrected live, so search returned the fact and its correction side by side; running the
        // done-when test four times left four contradictory answers, each written by a model acting
        // correctly. Nothing here can leave that state half-applied.
        //
        // No cycle check: the row being inserted is new, so nothing in the table points at it and it
        // cannot already be part of the chain it is about to close.
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query(select_memory!(
            "WITH inserted AS (
                INSERT INTO memory (id, tenant_id, namespace, content, embedding, tags, supersedes,
                                    source_client, embedding_model, sensitivity,
                                    content_ct, content_nonce, dek_wrapped, dek_nonce, enc_alg,
                                    kek_id, occurred_at)
                VALUES (COALESCE($1::uuid, gen_random_uuid()), $2, $3, $4, $5, $6, $7, $8, $9, $10,
                        $11, $12, $13, $14, $15, $16, $17)
                RETURNING *
             ) ",
            "",
            "FROM inserted"
        ))
        .bind(m.id)
        .bind(&m.tenant_id)
        .bind(&m.namespace)
        .bind(content)
        .bind(&embedding)
        .bind(&m.tags)
        .bind(m.supersedes)
        .bind(&m.source_client)
        .bind(&m.embedding_model)
        .bind(m.sensitivity.as_str())
        .bind(sealed.map(|s| s.content_ct.as_slice()))
        .bind(sealed.map(|s| s.content_nonce.as_slice()))
        .bind(sealed.map(|s| s.dek_wrapped.as_slice()))
        .bind(sealed.map(|s| s.dek_nonce.as_slice()))
        .bind(sealed.map(|s| s.enc_alg))
        .bind(kek_id.as_deref())
        // Valid time stays a plaintext column for a private row, as `created_at`, `tags` and
        // `namespace` already do. A date is metadata about a fact rather than the fact, so it is
        // outside what the encryption seam covers, and sealing it would take the ordering the
        // column exists to support with it.
        .bind(m.occurred_at)
        .fetch_one(&mut *tx)
        .await
        // `supersedes` is a foreign key into this table, so a target that does not exist arrives
        // here as a constraint violation. That is the caller's mistake rather than ours, and it
        // reads as one instead of as "internal error".
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(code) if code == "23503" => {
                DomainError::validation("the memory this write supersedes does not exist")
            }
            _ => DomainError::from(e),
        })?;
        let inserted = memory_from_row(&row);

        if let Some(old) = m.supersedes {
            let id = row.get::<uuid::Uuid, _>("id");

            // The predecessor's start, read under the lock the UPDATE below takes anyway, so the
            // date cannot move between the decision and the write. Reading it here rather than
            // deriving the end in SQL keeps one spelling of the rule: `supersession_until` decides
            // for both this path and `supersede`, and a refusal can name both dates.
            let predecessor_occurred_at: Option<DateTime<Utc>> =
                sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
                    "SELECT occurred_at FROM memory WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
                )
                .bind(&m.tenant_id)
                .bind(old)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();

            // Dropping the transaction on this `?` rolls it back, which is what the refusal wants
            // and what every other early return in this file relies on.
            let until =
                supersession_until(predecessor_occurred_at, m.occurred_at, inserted.created_at)?;
            warn_on_open_validity(old, id, predecessor_occurred_at, until);

            let retired = sqlx::query(RETIRE_PREDECESSOR_SQL)
                .bind(&m.tenant_id)
                .bind(old)
                .bind(id)
                .bind(until)
                .execute(&mut *tx)
                .await?
                .rows_affected();

            if retired == 0 {
                // The insert already proved the target exists, so the only way to update nothing is
                // that something else retired it first. Roll back rather than store a correction
                // pointing at a row that is no longer the current one, and name the live head so the
                // caller can retry against it.
                tx.rollback().await?;
                let head = self.supersession_head(&m.tenant_id, old).await?;
                let head_id = head.map(|h| h.id).unwrap_or_else(|| old.to_string());
                return Err(DomainError::conflict(format!(
                    "memory {old} was already superseded; the live row is {head_id}"
                )));
            }
        }

        tx.commit().await?;
        Ok(inserted)
    }

    /// Live rows only, on both clocks. Collapsing a new write into a row that was already retired
    /// would revive the fact that retirement was correcting, and collapsing it into an expired row
    /// is worse: the owner restates a fact the store closed, the write is counted as a duplicate,
    /// and the fact stays absent from every live read.
    async fn find_exact(
        &self,
        tenant: &str,
        namespace: &str,
        content: &str,
    ) -> Result<Option<Memory>> {
        let row = sqlx::query(select_memory!(
            "m.",
            concat!(
                "FROM memory m
              WHERE m.tenant_id = $1 AND m.namespace = $2 AND m.content = $3
                AND ",
                live!(),
                "
              ORDER BY m.created_at DESC LIMIT 1"
            )
        ))
        .bind(tenant)
        .bind(namespace)
        .bind(content)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(memory_from_row))
    }

    /// No live filter and no ceiling: this is lookup by an id the caller already holds, and the
    /// paths that use it (supersede, delete, the decision log) all need retired rows to be
    /// reachable. Authorization for those happens against the row this returns.
    async fn find_by_id(&self, tenant: &str, id: uuid::Uuid) -> Result<Option<Memory>> {
        let row = sqlx::query(select_memory!("", "FROM memory WHERE id = $1 AND tenant_id = $2"))
            .bind(id)
            .bind(tenant)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(memory_from_row))
    }

    /// Many rows by id, in one query.
    async fn find_many(&self, tenant: &str, ids: &[uuid::Uuid]) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let rows =
            sqlx::query(select_memory!("", "FROM memory WHERE tenant_id = $1 AND id = ANY($2)"))
                .bind(tenant)
                .bind(ids)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(memory_from_row).collect())
    }

    async fn digest(&self, q: DigestQuery) -> Result<DigestData> {
        let (readable_ns, readable_max) = split_ceilings(&q.readable);
        let payload: serde_json::Value = sqlx::query_scalar(DIGEST_SQL)
            .bind(&q.tenant_id)
            .bind(&q.user_namespace)
            .bind(q.profile_limit)
            .bind(&q.project_namespace)
            .bind(q.project_limit)
            .bind(&readable_ns)
            .bind(&readable_max)
            .bind(q.recent_days.to_string())
            .bind(q.recent_limit)
            .bind(q.registry_limit)
            .fetch_one(&self.pool)
            .await?;

        let parse = |key: &str| -> Vec<Memory> {
            payload
                .get(key)
                .and_then(|v| serde_json::from_value::<Vec<JsonMemory>>(v.clone()).ok())
                .unwrap_or_default()
                .into_iter()
                .map(Into::into)
                .collect()
        };

        Ok(DigestData {
            profile: parse("profile"),
            project_context: parse("project_context"),
            recent: parse("recent"),
            registry: payload
                .get("registry")
                .and_then(|v| serde_json::from_value::<Vec<RegistrySummary>>(v.clone()).ok())
                .unwrap_or_default(),
            memories_count: payload.get("memories_count").and_then(|v| v.as_i64()).unwrap_or(0),
            registry_count: payload.get("registry_count").and_then(|v| v.as_i64()).unwrap_or(0),
            by_namespace: payload
                .get("by_namespace")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
        })
    }

    /// Live rows per namespace, plus the namespaces that exist only in the registry.
    ///
    /// The zero-count arm is not decoration: grant resolution needs to know a namespace exists even
    /// when no memory has landed in it yet.
    ///
    /// No grant and no ceiling, by contract rather than by omission. The port states what a caller
    /// may do with the result, and the short version is that this answer is for resolving globs and
    /// never for a response. The filtered count lives in `DIGEST_SQL`'s `by_namespace` arm.
    async fn namespace_counts(&self, tenant: &str) -> Result<HashMap<String, i64>> {
        let rows = sqlx::query(
            "SELECT namespace, count(*) AS n
               FROM memory
              WHERE tenant_id = $1 AND superseded_by IS NULL
              GROUP BY namespace
             UNION ALL
             SELECT namespace, 0::bigint FROM registry WHERE tenant_id = $1 GROUP BY namespace",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await?;
        let mut counts: HashMap<String, i64> = HashMap::new();
        for r in &rows {
            *counts.entry(r.get("namespace")).or_insert(0) += r.get::<i64, _>("n");
        }
        Ok(counts)
    }

    /// Every `user:` row this store holds, per table.
    ///
    /// Eight tables key on namespace and all eight are counted, superseded memory rows included.
    /// The guard that reads this is looking for rows an upgrade left unreachable, and a namespace
    /// holding nothing but a registry entry or nothing but retired rows is exactly that case.
    ///
    /// `tool_calls` carries a namespace and is left out. It records what a client asked for at the
    /// time it asked, and an audit row rewritten to a name the call never used is a falsified log.
    async fn user_namespace_rows(&self, tenant: &str) -> Result<Vec<NamespaceRows>> {
        let rows = sqlx::query(
            "SELECT 'memory'::text AS t, namespace, count(*) AS n FROM memory
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             UNION ALL
             SELECT 'registry'::text, namespace, count(*) FROM registry
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             UNION ALL
             SELECT 'registry_history'::text, namespace, count(*) FROM registry_history
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             UNION ALL
             SELECT 'registry_alias'::text, namespace, count(*) FROM registry_alias
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             UNION ALL
             SELECT 'entity_alias'::text, namespace, count(*) FROM entity_alias
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             UNION ALL
             SELECT 'ingest_proposal'::text, namespace, count(*) FROM ingest_proposal
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             UNION ALL
             SELECT 'cleanup_proposal'::text, namespace, count(*) FROM cleanup_proposal
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             UNION ALL
             SELECT 'sealed_item'::text, namespace, count(*) FROM sealed_item
              WHERE tenant_id = $1 AND namespace LIKE 'user:%' GROUP BY namespace
             ORDER BY 2, 1",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| NamespaceRows {
                namespace: r.get("namespace"),
                table: r.get("t"),
                rows: r.get::<i64, _>("n"),
            })
            .collect())
    }

    /// One page of facts, newest first, filtered on both axes inside the query.
    async fn recent(&self, q: RecentQuery) -> Result<Vec<Memory>> {
        if q.readable.is_empty() {
            return Ok(vec![]);
        }
        let (readable_ns, readable_max) = split_ceilings(&q.readable);
        let (before_at, before_id) = match q.before {
            Some((at, id)) => (Some(at), Some(id)),
            None => (None, None),
        };
        let sql = if q.include_superseded { RECENT_ALL } else { RECENT_LIVE };
        let rows = sqlx::query(sql)
            .bind(&q.tenant_id)
            .bind(&readable_ns)
            .bind(&readable_max)
            .bind(&q.namespace)
            .bind(before_at)
            .bind(before_id)
            .bind(q.limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(memory_from_row).collect())
    }

    /// Rows retired inside a window, newest retirement first.
    async fn retired_since(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
        since: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<Vec<Retired>> {
        if readable.is_empty() {
            return Ok(vec![]);
        }
        let (readable_ns, readable_max) = split_ceilings(readable);
        let rows = sqlx::query(RETIRED_SQL)
            .bind(tenant)
            .bind(&readable_ns)
            .bind(&readable_max)
            .bind(since)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| Retired {
                id: r.get("id"),
                namespace: r.get("namespace"),
                content: r.get("content"),
                // Same rule as `memory_from_row`: an unrecognised level reads as the most
                // restrictive, and the query already filtered on `sensitivity_rank`.
                sensitivity: Sensitivity::parse(r.get::<&str, _>("sensitivity"))
                    .unwrap_or(Sensitivity::Sealed),
                retired_at: r.get("retired_at"),
                expired: r.get("expired"),
                occurred_at: r.get("occurred_at"),
                occurred_until: r.get("occurred_until"),
                end_open: r.get("end_open"),
                successor_id: r.get("successor_id"),
                successor_namespace: r.get("successor_namespace"),
            })
            .collect())
    }

    /// Per-namespace counts and the last write, on both axes.
    async fn namespace_summary(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
    ) -> Result<Vec<NamespaceSummary>> {
        if readable.is_empty() {
            return Ok(vec![]);
        }
        let (readable_ns, readable_max) = split_ceilings(readable);
        let rows = sqlx::query(NAMESPACE_SUMMARY_SQL)
            .bind(tenant)
            .bind(&readable_ns)
            .bind(&readable_max)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| NamespaceSummary {
                namespace: r.get("namespace"),
                live: r.get("live"),
                retired: r.get("retired"),
                above_open: r.get("above_open"),
                last_write: r.get("last_write"),
            })
            .collect())
    }

    /// Plaintext rows only. The recall monitor embeds what it samples, and the repository cannot
    /// read a private row, so the monitor measures the open store. Say so in what it reports rather
    /// than letting the sample look complete.
    ///
    /// The ceiling is in the query rather than in a pass over the result, for the reason every
    /// other read here gives: the report quotes what it sampled, so a row outside the caller's
    /// grant must never enter the process that answers that caller.
    async fn sample_content(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
        n: i64,
    ) -> Result<Vec<String>> {
        if readable.is_empty() {
            return Ok(vec![]);
        }
        let (readable_ns, readable_max) = split_ceilings(readable);
        let rows = sqlx::query_scalar::<_, String>(SAMPLE_CONTENT_SQL)
            .bind(tenant)
            .bind(&readable_ns)
            .bind(&readable_max)
            .bind(n)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// `exact` forces a sequential scan so the distances are true rather than approximate. That
    /// comparison is the whole point of the recall monitor.
    ///
    /// The settings go on a transaction on purpose. `SET LOCAL` outside a transaction block is a
    /// no-op that Postgres answers with a warning and nothing else, so the previous version of this
    /// function ran its "exact" arm straight through the HNSW index and compared the index against
    /// itself. A recall monitor that cannot fail is worse than no recall monitor.
    async fn nearest_ids(
        &self,
        tenant: &str,
        embedding: &[f32],
        k: i64,
        exact: bool,
    ) -> Result<Vec<String>> {
        let vector = pgvector::Vector::from(embedding.to_vec());
        let mut tx = self.pool.begin().await?;
        if exact {
            sqlx::query("SET LOCAL enable_indexscan = off").execute(&mut *tx).await?;
            sqlx::query("SET LOCAL enable_indexonlyscan = off").execute(&mut *tx).await?;
        }
        let ids = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT id FROM memory
              WHERE tenant_id = $1 AND embedding IS NOT NULL
              ORDER BY embedding <=> $2 LIMIT $3",
        )
        .bind(tenant)
        .bind(&vector)
        .bind(k)
        .fetch_all(&mut *tx)
        .await?;
        // Read-only, so a rollback is as correct as a commit and cannot fail on a conflict.
        tx.rollback().await?;
        Ok(ids.into_iter().map(|u| u.to_string()).collect())
    }

    /// Live rows in one namespace above a similarity floor, under the caller's ceiling.
    ///
    /// The HNSW trap applies here as much as to search: the namespace, ceiling and live filters are
    /// all inside the statement that carries the LIMIT, and migration 003's `strict_order` iterative
    /// scan is what keeps the arm from returning an empty set for a sparse namespace. The floor
    /// bounds this in the other direction: when nothing is similar, the scan runs until pgvector's
    /// own scan cap stops it rather than until the limit is filled.
    async fn neighbours(&self, q: NeighbourQuery) -> Result<Vec<ConflictCandidate>> {
        let embedding = pgvector::Vector::from(q.embedding);
        let rows = sqlx::query(
            "SELECT id, namespace, COALESCE(content, '') AS content,
                    (1 - (embedding <=> $3))::float8 AS similarity
               FROM memory
              WHERE tenant_id = $1
                AND namespace = $2
                AND sensitivity_rank(sensitivity) <= sensitivity_rank($4)
                -- `live!()` under this statement's own spelling: the table carries no alias here.
                AND superseded_by IS NULL
                AND (occurred_until IS NULL OR occurred_until > now())
                AND embedding IS NOT NULL
                AND 1 - (embedding <=> $3) >= $5
              ORDER BY embedding <=> $3
              LIMIT $6",
        )
        .bind(&q.tenant_id)
        .bind(&q.namespace)
        .bind(&embedding)
        .bind(q.max_sensitivity.as_str())
        .bind(q.min_similarity)
        .bind(q.limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| ConflictCandidate {
                id: r.get::<uuid::Uuid, _>("id").to_string(),
                namespace: r.get("namespace"),
                // Empty for a private neighbour. The service decrypts what it may show; a
                // repository that filled this in would be decrypting, which it cannot do.
                content: r.get("content"),
                similarity: round4(r.get::<f64, _>("similarity")),
            })
            .collect())
    }

    /// Both links and the retired row's valid-time end, one transaction, four refusals.
    ///
    /// The refusals are the feature. A self-reference and a cycle both make rows invisible to every
    /// live read, which is data loss dressed as a correction, and an already-superseded target
    /// means the caller is holding a stale id: the error names the current head so the retry has
    /// somewhere to go. The fourth is `supersession_until`, which refuses a successor that became
    /// true before the fact it replaces.
    async fn supersede(
        &self,
        tenant: &str,
        old: uuid::Uuid,
        new: uuid::Uuid,
    ) -> Result<Superseded> {
        if old == new {
            return Err(DomainError::validation("a memory cannot supersede itself"));
        }
        let mut tx = self.pool.begin().await?;

        // Locked in id order so two concurrent supersessions cannot deadlock, and locked at all so
        // the cycle check below cannot be invalidated between the check and the write.
        let (first, second) = if old < new { (old, new) } else { (new, old) };
        // The two valid-time columns ride along on the lock query. The dates have to be read under
        // the same lock as the cycle check, and reading them here costs nothing a second statement
        // would not.
        let locked = sqlx::query(
            "SELECT id, superseded_by, occurred_at, occurred_until, created_at FROM memory
                                  WHERE tenant_id = $1 AND id IN ($2, $3)
                                  ORDER BY id FOR UPDATE",
        )
        .bind(tenant)
        .bind(first)
        .bind(second)
        .fetch_all(&mut *tx)
        .await?;
        if locked.len() < 2 {
            return Err(DomainError::not_found("memory not found"));
        }

        let old_superseded_by = locked
            .iter()
            .find(|r| r.get::<uuid::Uuid, _>("id") == old)
            .and_then(|r| r.get::<Option<uuid::Uuid>, _>("superseded_by"));
        // Writing the link that is already there is not a failure. `insert` writes both halves for
        // a write that carries `supersedes`, so a service that then calls this to be sure must get
        // an answer it can proceed on rather than a conflict it has to interpret.
        if old_superseded_by == Some(new) {
            tx.rollback().await?;
            // The answer describes the row as it stands, not as this call left it. A caller that
            // re-asserts an existing link still needs to know the period behind it never closed.
            let row = locked.iter().find(|r| r.get::<uuid::Uuid, _>("id") == old);
            let open = row.is_some_and(|r| {
                r.get::<Option<DateTime<Utc>>, _>("occurred_at").is_some()
                    && r.get::<Option<DateTime<Utc>>, _>("occurred_until").is_none()
            });
            return Ok(Superseded { end_left_open: open });
        }

        if let Some(next) = old_superseded_by {
            // Release the locks before the head walk: it takes a second connection from the pool,
            // and holding row locks while waiting for one is how a busy pool turns into a stall.
            tx.rollback().await?;
            let head = self.supersession_head(tenant, next).await?;
            let head_id = head.map(|m| m.id).unwrap_or_else(|| next.to_string());
            return Err(DomainError::conflict(format!(
                "memory {old} was already superseded; the live row is {head_id}"
            )));
        }

        // Walking forward from `new`: reaching `old` means the link about to be written closes a
        // loop. Under the lock, so the answer is still true when the UPDATE lands.
        let chain: Vec<uuid::Uuid> = sqlx::query_scalar(CHAIN_IDS_SQL)
            .bind(tenant)
            .bind(new)
            .bind(MAX_CHAIN_DEPTH)
            .fetch_all(&mut *tx)
            .await?;
        if chain.contains(&old) {
            return Err(DomainError::conflict(format!(
                "memory {new} is already part of the chain that supersedes {old}; \
                 linking them would make both invisible"
            )));
        }

        // S2's value, decided in Rust rather than as a CASE inside the UPDATE. Two reasons, and the
        // second is the one that settled it: both rows are already locked and already read, so the
        // SQL would buy nothing; and a strict inversion has to be refused with a sentence naming
        // both dates, which is a sentence this layer can write and a constraint violation is not.
        let successor = locked
            .iter()
            .find(|r| r.get::<uuid::Uuid, _>("id") == new)
            .ok_or_else(|| DomainError::not_found("memory not found"))?;
        let predecessor_occurred_at = locked
            .iter()
            .find(|r| r.get::<uuid::Uuid, _>("id") == old)
            .and_then(|r| r.get::<Option<DateTime<Utc>>, _>("occurred_at"));
        let until = supersession_until(
            predecessor_occurred_at,
            successor.get::<Option<DateTime<Utc>>, _>("occurred_at"),
            successor.get::<DateTime<Utc>, _>("created_at"),
        )?;
        warn_on_open_validity(old, new, predecessor_occurred_at, until);

        sqlx::query(RETIRE_PREDECESSOR_SQL)
            .bind(tenant)
            .bind(old)
            .bind(new)
            .bind(until)
            .execute(&mut *tx)
            .await?;

        // The mirror, and only when it is empty. `superseded_by` on the retired row is the
        // authoritative link and the one every read filters on; `supersedes` on the live row is the
        // convenience direction and it holds one value, so a row that retires two predecessors can
        // only name the first. Overwriting it would quietly drop the record of the other.
        sqlx::query(
            "UPDATE memory SET supersedes = $3
              WHERE tenant_id = $1 AND id = $2 AND supersedes IS NULL",
        )
        .bind(tenant)
        .bind(new)
        .bind(old)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Superseded { end_left_open: until.is_none() })
    }

    /// Rebuild edges from structure, idempotently.
    async fn rebuild_edges(&self, tenant: &str) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        // Rebuild means rebuild. Appending alone cannot remove an edge whose reason has gone: an
        // alias closed with an `until`, a tag taken off a row, or a tag that grew past the hub limit
        // would all leave their edges traversable forever, and the walk would keep crossing them
        // while the comments above claimed the opposite. Only this seeder's own rows are cleared,
        // so anything a future seeder writes under a different `produced_by` survives.
        sqlx::query("DELETE FROM memory_edge WHERE tenant_id = $1 AND produced_by = 'structure'")
            .bind(tenant)
            .execute(&mut *tx)
            .await?;
        sqlx::query(SEED_SUPERSEDES_SQL).bind(tenant).execute(&mut *tx).await?;
        sqlx::query(SEED_ALIAS_SQL).bind(tenant).execute(&mut *tx).await?;
        sqlx::query(SEED_TAG_SQL).bind(tenant).bind(TAG_HUB_LIMIT).execute(&mut *tx).await?;
        let total: i64 =
            sqlx::query_scalar("SELECT count(*) FROM memory_edge WHERE tenant_id = $1")
                .bind(tenant)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(total)
    }

    /// One hop out, inside the caller's subgraph.
    async fn graph_neighbours(
        &self,
        tenant: &str,
        grants: &[NamespaceGrant],
        from: &[uuid::Uuid],
        bounds: WalkBounds,
    ) -> Result<Vec<GraphEdge>> {
        if from.is_empty() || grants.is_empty() {
            return Ok(vec![]);
        }
        let (prefixes, exact, maxima) = split_grants(grants);
        let rows = sqlx::query(GRAPH_NEIGHBOURS_SQL)
            .bind(tenant)
            .bind(from)
            .bind(&prefixes)
            .bind(&exact)
            .bind(&maxima)
            .bind(bounds.degree_cap)
            .bind(bounds.fan_out)
            .bind(bounds.include_retired)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| GraphEdge {
                from_id: r.get("from_id"),
                to_id: r.get("to_id"),
                relation: r.get("relation"),
            })
            .collect())
    }

    /// Count what supersession did to the periods it closed.
    async fn pair_counts(&self, tenant: &str, grants: &[NamespaceGrant]) -> Result<PairCounts> {
        if grants.is_empty() {
            return Ok(PairCounts::default());
        }
        let (prefixes, exact, maxima) = split_grants(grants);
        let row = sqlx::query(PAIR_COUNTS_SQL)
            .bind(tenant)
            .bind(&prefixes)
            .bind(&exact)
            .bind(&maxima)
            .fetch_one(&self.pool)
            .await?;
        Ok(PairCounts {
            pairs: row.get("pairs"),
            closed: row.get("closed"),
            dated_but_open: row.get("dated_but_open"),
            both_dated: row.get("both_dated"),
        })
    }

    /// Live rows with no start date, newest first.
    async fn undated(
        &self,
        tenant: &str,
        readable: &[NamespaceCeiling],
        limit: i64,
    ) -> Result<Vec<Memory>> {
        if readable.is_empty() {
            return Ok(vec![]);
        }
        let (readable_ns, readable_max) = split_ceilings(readable);
        let rows = sqlx::query(UNDATED_SQL)
            .bind(tenant)
            .bind(&readable_ns)
            .bind(&readable_max)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(memory_from_row).collect())
    }

    /// Fill a start date that was never recorded, and refuse to move one that was.
    ///
    /// `occurred_at IS NULL` in the WHERE rather than a read-then-write: two callers filling the
    /// same row race, and the loser has to lose inside the statement rather than after reading a
    /// NULL that stopped being true. The row count answers which one this was.
    async fn fill_occurred_at(
        &self,
        tenant: &str,
        id: uuid::Uuid,
        when: DateTime<Utc>,
    ) -> Result<bool> {
        let done = sqlx::query(
            "UPDATE memory SET occurred_at = $3
              WHERE tenant_id = $1 AND id = $2 AND occurred_at IS NULL",
        )
        .bind(tenant)
        .bind(id)
        .bind(when)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(done == 1)
    }

    async fn expire(&self, tenant: &str, id: uuid::Uuid) -> Result<Option<DateTime<Utc>>> {
        let row = sqlx::query(EXPIRE_SQL).bind(tenant).bind(id).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| r.get("occurred_until")))
    }

    async fn unexpire(&self, tenant: &str, id: uuid::Uuid, until: DateTime<Utc>) -> Result<bool> {
        let row = sqlx::query(UNEXPIRE_SQL)
            .bind(tenant)
            .bind(id)
            .bind(until)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// The last row on the chain from `id`. Depth-capped like every other walk, so a table that
    /// already contains a cycle answers with a bounded row rather than hanging.
    async fn supersession_head(&self, tenant: &str, id: uuid::Uuid) -> Result<Option<Memory>> {
        let row = sqlx::query(SUPERSESSION_HEAD_SQL)
            .bind(tenant)
            .bind(id)
            .bind(MAX_CHAIN_DEPTH)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(memory_from_row))
    }

    /// The whole chain around `id`, oldest first, in one statement, however many namespaces it
    /// crosses.
    ///
    /// An id that does not exist or that the grant does not admit gives an empty timeline rather
    /// than an error, and reports no gap with it. That is the same answer `search` gives for the
    /// same row, and the reason is the same: a count of withheld versions on a row the caller
    /// cannot read at all would confirm the row exists.
    ///
    /// `withheld` and `depth_capped` ride on every returned row and are read off the first. They
    /// are aggregates over the whole chain, so the value is the same on all of them.
    async fn subject_history(
        &self,
        tenant: &str,
        grants: &[NamespaceGrant],
        id: uuid::Uuid,
    ) -> Result<Timeline> {
        // A grant that admits nothing needs no round trip, and the statement would answer the same
        // thing the long way: an empty `granted` table makes every row on the chain unreadable.
        if grants.is_empty() {
            return Ok(Timeline::default());
        }
        let (prefixes, exact, maxima) = split_grants(grants);
        let rows = sqlx::query(SUBJECT_HISTORY_SQL)
            .bind(tenant)
            .bind(id)
            .bind(MAX_CHAIN_DEPTH)
            .bind(&prefixes)
            .bind(&exact)
            .bind(&maxima)
            .fetch_all(&self.pool)
            .await?;
        let withheld = rows.first().map(|r| r.get::<i64, _>("withheld")).unwrap_or(0);
        let depth_capped = rows.first().map(|r| r.get::<bool, _>("depth_capped")).unwrap_or(false);
        Ok(Timeline {
            versions: rows.iter().map(memory_from_row).collect(),
            withheld,
            depth_capped,
        })
    }

    /// One UPDATE for the whole result set, on a spawned task.
    ///
    /// A search must not wait for this and must not fail because of it, so nothing here is
    /// awaited by the caller and a failure is a log line. One statement per result set rather than
    /// per row: a ten-row search that wrote ten times would turn every read into a write storm.
    /// A collector coalescing across requests would be the next step and is not worth it at
    /// single-user query rates.
    fn touch_accessed(&self, tenant: &str, ids: Vec<uuid::Uuid>) {
        if ids.is_empty() {
            return;
        }
        let pool = self.pool.clone();
        let tenant = tenant.to_string();
        tokio::spawn(async move {
            let result = sqlx::query(
                "UPDATE memory
                    SET access_count = access_count + 1, last_accessed_at = now()
                  WHERE tenant_id = $1 AND id = ANY($2)",
            )
            .bind(&tenant)
            .bind(&ids)
            .execute(&pool)
            .await;
            if let Err(e) = result {
                tracing::warn!(rows = ids.len(), error = %e, "touch_accessed failed");
            }
        });
    }

    /// The anti-loop record, written the same way and for the same reasons as `touch_accessed`.
    ///
    /// One statement for the whole result set, on a spawned task, nothing awaited by the caller. A
    /// search that failed because this insert failed would trade the loop this table exists to
    /// catch for an outage on every read.
    ///
    /// `first_emitted_at` never moves. The echo test asks whether the store handed the content out
    /// *before* the transcript recorded it, so the first emission is the one that could have caused
    /// the echo and a later read must not push the answer forward. A repeat bumps the count and
    /// `last_emitted_at`, which is what keeps a fact read a thousand times at one row.
    ///
    /// `recall_emission` also has a writer in `ingest.rs`, for the one-row path the ingest service
    /// uses. The batched one lives here because this is where the rows being emitted are, and
    /// because the spec puts it beside the touch it rides along with.
    fn record_emissions(
        &self,
        tenant: &str,
        tool: &'static str,
        session_id: Option<String>,
        rows: Vec<Emission>,
    ) {
        if rows.is_empty() {
            return;
        }
        let pool = self.pool.clone();
        let tenant = tenant.to_string();
        tokio::spawn(async move {
            let hashes: Vec<String> = rows.iter().map(|r| r.content_sha256.clone()).collect();
            let ids: Vec<uuid::Uuid> = rows.iter().map(|r| r.memory_id).collect();
            // Two parallel arrays into one `unnest`, which is what makes a ten-row digest one
            // round trip. DISTINCT because two rows in one result set can carry the same content,
            // and Postgres refuses a statement that touches the same key twice.
            let result = sqlx::query(
                "INSERT INTO recall_emission
                     (tenant_id, content_sha256, memory_id, tool, session_id)
                 SELECT DISTINCT $1, h, m, $4, $5
                   FROM unnest($2::text[], $3::uuid[]) AS t(h, m)
                 ON CONFLICT (tenant_id, content_sha256, memory_id, tool) DO UPDATE
                    SET last_emitted_at = now(),
                        emit_count      = recall_emission.emit_count + 1,
                        session_id      = COALESCE(EXCLUDED.session_id, recall_emission.session_id)",
            )
            .bind(&tenant)
            .bind(&hashes)
            .bind(&ids)
            .bind(tool)
            .bind(session_id)
            .execute(&pool)
            .await;
            if let Err(e) = result {
                tracing::warn!(rows = ids.len(), tool, error = %e, "record_emissions failed");
            }
        });
    }

    async fn confirm(&self, tenant: &str, id: uuid::Uuid) -> Result<()> {
        sqlx::query("UPDATE memory SET last_confirmed_at = now() WHERE tenant_id = $1 AND id = $2")
            .bind(tenant)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn chain_neighbours(&self, tenant: &str, id: uuid::Uuid) -> Result<ChainNeighbours> {
        let row = sqlx::query(
            "SELECT supersedes, superseded_by FROM memory WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(ChainNeighbours::default());
        };
        let own = (
            row.get::<Option<uuid::Uuid>, _>("supersedes"),
            row.get::<Option<uuid::Uuid>, _>("superseded_by"),
        );
        // One statement for both directions. The direction column is what tells a predecessor
        // from a successor. A row that is both (a two-row cycle, which the write path refuses)
        // lands in `predecessors` once, because the boolean is tested first and the row is
        // pushed to one bucket.
        let links = sqlx::query(
            "SELECT id, namespace, sensitivity, (superseded_by = $2) AS predecessor
               FROM memory
              WHERE tenant_id = $1 AND (superseded_by = $2 OR supersedes = $2)
              ORDER BY created_at, id",
        )
        .bind(tenant)
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = ChainNeighbours { row: Some(own), ..Default::default() };
        for r in &links {
            let link = ChainLink {
                id: r.get("id"),
                namespace: r.get("namespace"),
                // An unrecognised level reads as the most restrictive, the rule every row
                // constructor in this file follows.
                sensitivity: Sensitivity::parse(r.get::<&str, _>("sensitivity"))
                    .unwrap_or(Sensitivity::Sealed),
            };
            if r.get::<Option<bool>, _>("predecessor").unwrap_or(false) {
                out.predecessors.push(link);
            } else {
                out.successors.push(link);
            }
        }
        Ok(out)
    }

    /// Hard delete, with the chain edits the service planned applied first, all in one
    /// transaction.
    ///
    /// `supersedes` and `superseded_by` are foreign keys into this table and stay `NO ACTION` on
    /// purpose: an `ON DELETE SET NULL` on `superseded_by` would revive every row this one had
    /// retired, in namespaces the caller may never have been granted, with no service in the loop
    /// to say no. So the links are edited here, under the plan, and a predecessor the plan did not
    /// account for makes the DELETE fail on the constraint. That failure is mapped to a conflict
    /// rather than to an internal error, because it names the one thing the caller can do about
    /// it, which is to look again.
    ///
    /// The doomed row is locked before the edits. Two deletes of one row then serialise, and the
    /// loser finds nothing to delete and reports `Missing`.
    async fn delete(
        &self,
        tenant: &str,
        id: uuid::Uuid,
        plan: &DeletePlan,
    ) -> Result<DeleteOutcome> {
        let mut tx = self.pool.begin().await?;
        let doomed = sqlx::query(
            "SELECT supersedes FROM memory WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(doomed) = doomed else {
            return Ok(DeleteOutcome::Missing);
        };
        let own_predecessor: Option<uuid::Uuid> = doomed.get("supersedes");

        let mut edits = ChainEdits::default();
        if !plan.revive.is_empty() {
            // `occurred_until` goes back to NULL with the rest of the retirement. Two statements
            // write that column, `RETIRE_PREDECESSOR_SQL` and `EXPIRE_SQL`, and only the first can
            // have written it on a row that has a successor: `write::validate_supersedes_target`
            // refuses a target whose period is already closed, so an expired row never gains one.
            // The value cleared here is therefore the one that supersession wrote. Leaving it
            // set stranded the row: live search filters on `superseded_by IS NULL` and returned it,
            // every as-of read filters on `occurred_until` and did not, and the COALESCE in the
            // retire statement meant a later supersession kept the stale end rather than correcting
            // it. The owner's only repair was psql.
            let rows = sqlx::query(
                "UPDATE memory SET superseded_by = NULL, superseded_at = NULL, occurred_until = NULL
                  WHERE tenant_id = $1 AND superseded_by = $2 AND id = ANY($3)
              RETURNING id",
            )
            .bind(tenant)
            .bind(id)
            .bind(&plan.revive)
            .fetch_all(&mut *tx)
            .await?;
            edits.revived = rows.iter().map(|r| r.get("id")).collect();
        }
        if let Some(successor) = plan.splice_to {
            // `superseded_at` is kept. The predecessor was retired when it was retired; which row
            // did the retiring is the only thing that changed.
            let rows = sqlx::query(
                "UPDATE memory SET superseded_by = $3
                  WHERE tenant_id = $1 AND superseded_by = $2
              RETURNING id",
            )
            .bind(tenant)
            .bind(id)
            .bind(successor)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| chain_conflict(id, e))?;
            edits.spliced = rows.iter().map(|r| r.get("id")).collect();
        }
        // The successor's provenance link moves to whatever the doomed row had replaced, so a
        // chain of three with the middle removed still reads as a chain of two.
        let rows = sqlx::query(
            "UPDATE memory SET supersedes = $3
              WHERE tenant_id = $1 AND supersedes = $2
          RETURNING id",
        )
        .bind(tenant)
        .bind(id)
        .bind(own_predecessor)
        .fetch_all(&mut *tx)
        .await?;
        edits.relinked = rows.iter().map(|r| r.get("id")).collect();

        let deleted = sqlx::query("DELETE FROM memory WHERE tenant_id = $1 AND id = $2")
            .bind(tenant)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| chain_conflict(id, e))?
            .rows_affected();
        tx.commit().await?;
        Ok(match deleted {
            0 => DeleteOutcome::Missing,
            _ => DeleteOutcome::Deleted(edits),
        })
    }

    /// The review queue, not a reaper. Matches the `memory_never_accessed` partial index.
    async fn stale(
        &self,
        tenant: &str,
        older_than_days: i32,
        limit: i64,
        reader: &[NamespaceGrant],
    ) -> Result<Vec<Memory>> {
        let (g_prefix, g_exact, g_max) = crate::adapters::postgres::cleanup::grant_arrays(reader);
        let rows = sqlx::query(select_memory!(
            "m.",
            concat!(
                "FROM memory m
              WHERE m.tenant_id = $1
                AND ",
                live!(),
                "
                AND m.last_accessed_at IS NULL
                AND m.created_at < now() - ($2 || ' days')::interval
                AND EXISTS (
                      SELECT 1
                        FROM unnest($4::text[], $5::bool[], $6::text[]) AS g(prefix, exact, max)
                       WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                                  ELSE left(m.namespace, length(g.prefix)) = g.prefix END
                         AND sensitivity_rank(g.max) >= sensitivity_rank(m.sensitivity)
                    )
              ORDER BY m.created_at ASC
              LIMIT $3"
            )
        ))
        .bind(tenant)
        .bind(older_than_days.to_string())
        .bind(limit)
        .bind(&g_prefix)
        .bind(&g_exact)
        .bind(&g_max)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(memory_from_row).collect())
    }

    /// One pass over the tenant's rows. Age is measured from `created_at`: the question is how old
    /// the facts being retrieved are, not how recently someone looked at them.
    async fn staleness(&self, tenant: &str) -> Result<Staleness> {
        let row = sqlx::query(
            // `live` is `live!()` under this statement's own spelling, computed once and read by
            // four counters. `superseded_rows` reads the other axis and stays as it was, so a row
            // whose period closed with no successor counts in neither. That is the honest answer:
            // it is not live and nothing replaced it.
            "SELECT
               count(*) FILTER (WHERE live) AS live_rows,
               count(*) FILTER (WHERE live AND last_accessed_at IS NULL)
                 AS never_retrieved,
               count(*) FILTER (WHERE superseded_by IS NOT NULL) AS superseded_rows,
               percentile_cont(0.5) WITHIN GROUP (
                 ORDER BY (extract(epoch FROM now() - created_at) / 86400.0)::float8)
                 FILTER (WHERE live AND last_accessed_at IS NOT NULL)
                 AS median_age_days_retrieved,
               max((extract(epoch FROM now() - created_at) / 86400.0)::float8)
                 FILTER (WHERE live AND last_accessed_at IS NULL)
                 AS oldest_never_retrieved_days
             FROM (
               SELECT superseded_by, last_accessed_at, created_at,
                      superseded_by IS NULL
                        AND (occurred_until IS NULL OR occurred_until > now()) AS live
                 FROM memory WHERE tenant_id = $1
             ) m",
        )
        .bind(tenant)
        .fetch_one(&self.pool)
        .await?;

        let live_rows: i64 = row.get("live_rows");
        let never_retrieved: i64 = row.get("never_retrieved");
        Ok(Staleness {
            live_rows,
            never_retrieved,
            never_retrieved_pct: percentage(never_retrieved, live_rows),
            median_age_days_retrieved: row
                .get::<Option<f64>, _>("median_age_days_retrieved")
                .map(round2),
            superseded_rows: row.get("superseded_rows"),
            oldest_never_retrieved_days: row
                .get::<Option<f64>, _>("oldest_never_retrieved_days")
                .map(round2),
        })
    }

    /// Near-duplicate live pairs in one namespace, each reported once, older row first.
    ///
    /// This is a self-join on vector distance, so it is O(n^2) in the rows of a namespace with no
    /// index able to help: HNSW answers "near this vector", not "all pairs near each other". The
    /// LIMIT is the only bound, and Postgres has to compute the distances before it can apply it.
    /// At a few thousand rows per namespace that is seconds; somewhere around fifty thousand it
    /// stops being a command you can run interactively and needs either a blocking pre-filter or a
    /// per-row nearest-neighbour probe instead. It runs from `lumberroom review` by hand, never on a
    /// request path, which is what makes the trade acceptable today.
    async fn conflicts(
        &self,
        tenant: &str,
        min_similarity: f64,
        limit: i64,
    ) -> Result<Vec<ConflictPair>> {
        let rows = sqlx::query(
            "SELECT a.id AS older_id, a.namespace AS older_namespace,
                    COALESCE(a.content, '') AS older_content,
                    b.id AS newer_id, b.namespace AS newer_namespace,
                    COALESCE(b.content, '') AS newer_content,
                    (1 - (a.embedding <=> b.embedding))::float8 AS similarity
               FROM memory a
               JOIN memory b
                 ON b.tenant_id = a.tenant_id
                AND b.namespace = a.namespace
                -- Row comparison rather than created_at alone, so a pair written in the same
                -- transaction is still reported exactly once.
                AND (a.created_at, a.id) < (b.created_at, b.id)
              WHERE a.tenant_id = $1
                -- `live!()` on both sides, under this statement's own aliases. A pair is a finding
                -- only while both facts still hold.
                AND a.superseded_by IS NULL
                AND (a.occurred_until IS NULL OR a.occurred_until > now())
                AND b.superseded_by IS NULL
                AND (b.occurred_until IS NULL OR b.occurred_until > now())
                AND a.embedding IS NOT NULL AND b.embedding IS NOT NULL
                AND 1 - (a.embedding <=> b.embedding) >= $2
              ORDER BY similarity DESC
              LIMIT $3",
        )
        .bind(tenant)
        .bind(min_similarity)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| {
                let similarity = round4(r.get::<f64, _>("similarity"));
                ConflictPair {
                    older: ConflictCandidate {
                        id: r.get::<uuid::Uuid, _>("older_id").to_string(),
                        namespace: r.get("older_namespace"),
                        content: r.get("older_content"),
                        similarity,
                    },
                    newer: ConflictCandidate {
                        id: r.get::<uuid::Uuid, _>("newer_id").to_string(),
                        namespace: r.get("newer_namespace"),
                        content: r.get("newer_content"),
                        similarity,
                    },
                    similarity,
                }
            })
            .collect())
    }

    /// Live rows only, oldest first, bounded by sensitivity.
    ///
    /// Live only because the port offers no `include_superseded` here and the consumer is the
    /// Obsidian mirror, which the system PRD calls a window onto current truth. A retired "the port
    /// is 8080" note sitting beside the live "the port is 8787" note in a vault is the exact
    /// contradiction Phase 4 exists to remove; history stays reachable through `find_by_id` and
    /// `supersession_head`.
    async fn list_for_export(
        &self,
        tenant: &str,
        max_sensitivity: Sensitivity,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Memory>> {
        let rows = sqlx::query(select_memory!(
            "m.",
            concat!(
                "FROM memory m
              WHERE m.tenant_id = $1
                AND sensitivity_rank(m.sensitivity) <= sensitivity_rank($2)
                AND ",
                live!(),
                "
              ORDER BY m.created_at ASC, m.id ASC
              LIMIT $3 OFFSET $4"
            )
        ))
        .bind(tenant)
        .bind(max_sensitivity.as_str())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(memory_from_row).collect())
    }

    /// Live and retired alike, with no grant and no ceiling anywhere in the statement.
    ///
    /// The one read here that filters on tenant alone, and the gate sits above it: the archive
    /// service refuses a caller who cannot read the whole store before it reaches this method. A
    /// ceiling applied here would turn that refusal into a partial archive nobody could tell from a
    /// complete one, which is the failure the whole feature exists to avoid.
    ///
    /// Private rows arrive with an empty `content`, as they do from every other read in this file.
    /// The service opens them through the same helper the export uses.
    async fn list_whole_store(
        &self,
        tenant: &str,
        limit: i64,
        after: Option<uuid::Uuid>,
    ) -> Result<Vec<Memory>> {
        let rows = sqlx::query(LIST_WHOLE_STORE_SQL)
            .bind(tenant)
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(memory_from_row).collect())
    }

    /// One row back, as recorded.
    ///
    /// Which KEK wrapped this row is read from `kek_state`, for the reason `insert` gives at
    /// length: `kek_state` holds a key the boot check has already unwrapped and matched, so a row
    /// written against it can be rewrapped later. An archive names no key and could not be trusted
    /// about one if it did.
    ///
    /// Nothing here decides whether a row should be encrypted. The service resolved the level,
    /// sealed the content under this install's key, and this statement stores what it was handed.
    async fn restore_row(&self, row: RestoreRow) -> Result<()> {
        let kek_id: Option<String> = match &row.sealed {
            None => None,
            Some(_) => Some(
                sqlx::query_scalar::<_, String>(
                    "SELECT kek_id FROM kek_state WHERE tenant_id = $1",
                )
                .bind(&row.tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| {
                    DomainError::unavailable(
                        "no verified encryption key is recorded, so an encrypted row cannot be \
                         restored: it could not be rewrapped or read after a rotation",
                    )
                })?,
            ),
        };

        let embedding = pgvector::Vector::from(row.embedding);
        let sealed = row.sealed.as_ref();
        // The same single line `insert` relies on: a sealed row has no plaintext column to write,
        // and there is no branch below that could give it one.
        let content = sealed.is_none().then_some(row.content.as_str());

        sqlx::query(RESTORE_ROW_SQL)
            .bind(row.id)
            .bind(&row.tenant_id)
            .bind(&row.namespace)
            .bind(content)
            .bind(&embedding)
            .bind(&row.tags)
            .bind(row.supersedes)
            .bind(&row.source_client)
            .bind(&row.embedding_model)
            .bind(row.sensitivity.as_str())
            .bind(sealed.map(|s| s.content_ct.as_slice()))
            .bind(sealed.map(|s| s.content_nonce.as_slice()))
            .bind(sealed.map(|s| s.dek_wrapped.as_slice()))
            .bind(sealed.map(|s| s.dek_nonce.as_slice()))
            .bind(sealed.map(|s| s.enc_alg))
            .bind(kek_id.as_deref())
            .bind(row.occurred_at)
            .bind(row.occurred_until)
            .bind(row.superseded_by)
            .bind(row.superseded_at)
            .bind(row.access_count)
            .bind(row.last_accessed_at)
            .bind(row.last_confirmed_at)
            .bind(row.created_at)
            .execute(&self.pool)
            .await
            .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
                // A chain link whose target has not landed yet. The caller reaches this by binding
                // the links on the insert instead of leaving them for `relink_restored`, and the
                // message says so rather than reading as an internal error.
                Some(code) if code == "23503" => DomainError::validation(
                    "a row this restore links to is not in the store yet: write the chain links \
                     after every row has landed",
                ),
                Some(code) if code == "23505" => DomainError::validation(
                    "this row id is already in the store, so the restore would overwrite what is \
                     here",
                ),
                _ => DomainError::from(e),
            })?;
        Ok(())
    }

    /// The chain links, written after every restored row exists.
    ///
    /// Separate from `restore_row` because both columns are foreign keys into this table and
    /// neither is deferrable: `superseded_by` names a newer row, so a restore walking rows in id
    /// order always reaches the target second. One statement per row rather than one per store,
    /// because the caller already holds the archive's records and a batch would mean two arrays
    /// that have to stay aligned.
    async fn relink_restored(
        &self,
        tenant: &str,
        id: uuid::Uuid,
        supersedes: Option<uuid::Uuid>,
        superseded_by: Option<uuid::Uuid>,
    ) -> Result<()> {
        if supersedes.is_none() && superseded_by.is_none() {
            return Ok(());
        }
        sqlx::query(RELINK_RESTORED_SQL)
            .bind(tenant)
            .bind(id)
            .bind(supersedes)
            .bind(superseded_by)
            .execute(&self.pool)
            .await
            .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
                Some(code) if code == "23503" => DomainError::validation(
                    "this row's supersession chain names a memory the archive did not carry",
                ),
                _ => DomainError::from(e),
            })?;
        Ok(())
    }
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Zero rows is zero percent, not a division by zero and not a missing number: an empty store is
/// not stale, it is empty.
fn percentage(part: i64, whole: i64) -> f64 {
    if whole <= 0 {
        return 0.0;
    }
    round2(part as f64 * 100.0 / whole as f64)
}

/// Postgres serialises timestamptz inside json_agg as ISO 8601, which chrono parses. The earlier
/// TypeScript build used to_char with a "+00" offset that JavaScript's Date rejected.
#[derive(serde::Deserialize)]
struct JsonMemory {
    id: uuid::Uuid,
    namespace: String,
    /// NULL for an encrypted row, and the digest carries no ciphertext, so a private row arrives
    /// here with nothing to show. The service hydrates it if the caller may read it.
    content: Option<String>,
    tags: Vec<String>,
    source_client: String,
    embedding_model: Option<String>,
    sensitivity: String,
    supersedes: Option<uuid::Uuid>,
    superseded_by: Option<uuid::Uuid>,
    superseded_at: Option<DateTime<Utc>>,
    access_count: i32,
    last_accessed_at: Option<DateTime<Utc>>,
    last_confirmed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    /// `default` because `serde_json::from_value` here fails whole rather than per field, and the
    /// caller turns a failure into an empty digest. A column dropped from one of the three digest
    /// select lists would otherwise read as "this client has no facts".
    #[serde(default)]
    occurred_at: Option<DateTime<Utc>>,
    #[serde(default)]
    occurred_until: Option<DateTime<Utc>>,
}

impl From<JsonMemory> for Memory {
    fn from(j: JsonMemory) -> Self {
        Memory {
            id: j.id.to_string(),
            namespace: j.namespace,
            content: j.content.unwrap_or_default(),
            tags: j.tags,
            source_client: j.source_client,
            embedding_model: j.embedding_model,
            sensitivity: Sensitivity::parse(&j.sensitivity).unwrap_or(Sensitivity::Sealed),
            supersedes: j.supersedes.map(|u| u.to_string()),
            superseded_by: j.superseded_by.map(|u| u.to_string()),
            superseded_at: j.superseded_at,
            access_count: j.access_count,
            last_accessed_at: j.last_accessed_at,
            last_confirmed_at: j.last_confirmed_at,
            created_at: j.created_at,
            occurred_at: j.occurred_at,
            occurred_until: j.occurred_until,
        }
    }
}

/// The read half of the encryption seam, for the service layer.
///
/// `services::SealedReader` is declared by the consumer rather than in `ports` on purpose (its doc
/// comment says why), and the composition root hands the same `PgMemoryRepository` up as both
/// handles. Spelled through `MemoryRepository` because this type now carries a `sealed_batch` on two
/// traits and an inferred receiver would be ambiguous.
#[async_trait]
impl crate::services::SealedReader for PgMemoryRepository {
    async fn sealed_batch(
        &self,
        tenant: &str,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, SealedContent, Option<String>)>> {
        MemoryRepository::sealed_batch(self, tenant, ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The statements are constants, so the structural guarantees this file is responsible for can
    /// be asserted without a database. These tests exist because the failure they catch is a
    /// missing line in a long string that nothing else notices: a leak, not a crash.
    #[test]
    fn every_digest_subquery_filters_on_the_callers_ceiling() {
        assert_eq!(
            DIGEST_SQL.matches("JOIN reachable rg").count(),
            7,
            "seven subqueries: profile, project, recent, registry, both counts, the inventory"
        );
        assert_eq!(DIGEST_SQL.matches("<= rg.max_rank").count(), 7);
    }

    #[test]
    fn every_digest_memory_subquery_returns_live_rows_only() {
        // Four memory arms plus the inventory: the two registry arms have no supersession.
        assert_eq!(DIGEST_SQL.matches("m.superseded_by IS NULL").count(), 5);
    }

    #[test]
    fn both_reading_pages_filter_on_the_callers_ceiling() {
        for sql in [RECENT_LIVE, RECENT_ALL] {
            assert_eq!(sql.matches("JOIN reachable rg").count(), 1);
            assert_eq!(sql.matches("<= rg.max_rank").count(), 1);
        }
    }

    #[test]
    fn a_reading_page_returns_live_rows_only_unless_history_was_asked_for() {
        assert_eq!(RECENT_LIVE.matches("m.superseded_by IS NULL").count(), 1);
        assert!(!RECENT_ALL.contains("superseded_by IS NULL"));
    }

    /// The summary publishes a namespace name and a row count, which is enough to disclose that a
    /// namespace this caller may not read exists.
    #[test]
    fn the_namespace_summary_filters_on_the_callers_ceiling() {
        assert_eq!(NAMESPACE_SUMMARY_SQL.matches("JOIN reachable rg").count(), 1);
        assert_eq!(NAMESPACE_SUMMARY_SQL.matches("<= rg.max_rank").count(), 1);
    }

    /// The recall monitor publishes what it sampled, so this statement is a read path like any
    /// other and the same count guards it.
    #[test]
    fn the_recall_sample_filters_on_the_callers_ceiling() {
        assert_eq!(SAMPLE_CONTENT_SQL.matches("JOIN reachable rg").count(), 1);
        assert_eq!(SAMPLE_CONTENT_SQL.matches("<= rg.max_rank").count(), 1);
    }

    /// Every search statement this file can hand to Postgres. The leak guards below run over all
    /// six, because a blend is a reason to rewrite the score expression, a time filter is a reason
    /// to rewrite the WHERE clause, and neither is a reason to rewrite the policy filters.
    const EVERY_SEARCH_SQL: [&str; 6] =
        [SEARCH_LIVE, SEARCH_ALL, SEARCH_RRF_LIVE, SEARCH_RRF_ALL, SEARCH_AS_OF, SEARCH_RRF_AS_OF];

    /// The four statements that ask about now. They carry no period predicate and no parameter for
    /// one, which is what makes a search with no `as_of` byte for byte the search this server has
    /// always run.
    const NO_AS_OF_SQL: [&str; 4] = [SEARCH_LIVE, SEARCH_ALL, SEARCH_RRF_LIVE, SEARCH_RRF_ALL];

    /// The two that ask about an instant.
    const AS_OF_SQL: [&str; 2] = [SEARCH_AS_OF, SEARCH_RRF_AS_OF];

    #[test]
    fn both_search_arms_filter_on_the_callers_ceiling() {
        for sql in EVERY_SEARCH_SQL {
            assert_eq!(
                sql.matches("<= rg.max_rank").count(),
                2,
                "the vector arm and the lexical arm"
            );
        }
    }

    #[test]
    fn search_filters_superseded_rows_inside_each_arm_unless_history_was_asked_for() {
        assert_eq!(SEARCH_LIVE.matches("m.superseded_by IS NULL").count(), 2);
        assert_eq!(SEARCH_RRF_LIVE.matches("m.superseded_by IS NULL").count(), 2);
        assert!(!SEARCH_ALL.contains("superseded_by IS NULL"));
        assert!(!SEARCH_RRF_ALL.contains("superseded_by IS NULL"));
    }

    /// A row retired last week is exactly the row that answers a question about last month, so an
    /// as-of statement filters no supersession at all. Adding the filter here would leave the
    /// statement returning only facts that held then and still hold now, which answers nothing.
    #[test]
    fn an_as_of_search_reaches_retired_rows_because_that_is_what_it_is_for() {
        for sql in AS_OF_SQL {
            assert!(!sql.contains("superseded_by IS NULL"), "as-of must not hide history");
        }
    }

    /// The half-open edges, quoted. `<=` on the left, `>` on the right, spec rule I1.
    ///
    /// `>=` on the right is the bug the convention exists to prevent: a predecessor ending at T and
    /// a successor starting at T would both match a query at T, and the read returns two
    /// contradictory answers about one instant with nothing to report.
    #[test]
    fn the_as_of_predicate_excludes_the_end_instant_on_both_arms() {
        for (sql, n) in [(SEARCH_AS_OF, "$14"), (SEARCH_RRF_AS_OF, "$15")] {
            let predicate = format!(
                "(COALESCE(m.occurred_at, m.created_at) <= {n}
                   AND (m.occurred_until IS NULL OR m.occurred_until >  {n}))"
            );
            assert_eq!(sql.matches(&predicate).count(), 2, "the vector arm and the lexical arm");
            assert!(!sql.contains("occurred_until >="), "the end instant is outside the period");
            // The start must never read `occurred_at IS NULL OR`: that spelling made an undated row
            // match every instant, and most rows are undated.
            assert!(
                !sql.contains("m.occurred_at    IS NULL OR"),
                "an undated row would hold at every instant again"
            );
        }
    }

    /// As-of is a fifth and sixth statement, so the parameter count changed for those two alone.
    /// Linear was thirteen and its as-of sibling binds fourteen; rank fusion was fourteen with `k`
    /// as the fourteenth, and its as-of sibling binds fifteen. The four statements above keep the
    /// counts they had.
    #[test]
    fn only_the_as_of_statements_gained_a_parameter() {
        assert!(SEARCH_AS_OF.contains("$14"));
        assert!(!SEARCH_AS_OF.contains("$15"), "the linear as-of statement binds fourteen");
        assert!(SEARCH_RRF_AS_OF.contains("$15"));
        assert!(!SEARCH_RRF_AS_OF.contains("$16"), "the rank-fusion as-of statement binds fifteen");
        // `k` did not move. Renumbering it would have rewritten two statements that are not
        // changing, and requirement 3 is that a search without `as_of` runs the same text.
        assert!(SEARCH_RRF_AS_OF.contains("$14::float8 + g.rank_vec::float8"));
    }

    #[test]
    fn the_lexical_arm_never_reaches_anything_but_open_content() {
        for sql in EVERY_SEARCH_SQL {
            assert!(sql.contains("m.sensitivity = 'open'"));
        }
    }

    /// The namespace qual on the base relation is what lets the planner push the test into the
    /// vector index scan, which is what pgvector's iterative scan resumes from. Both blends carry
    /// it in both arms.
    #[test]
    fn every_arm_quals_the_base_relation_on_namespace() {
        for sql in EVERY_SEARCH_SQL {
            assert_eq!(sql.matches("m.namespace = ANY($2 || $4)").count(), 2);
        }
    }

    /// The default path has to be the statement this server has always run. A window function in
    /// the vector arm changes its plan, and an extra placeholder changes its bind count, so both
    /// are pinned out of the linear text rather than left absent by accident.
    ///
    /// The linear blend now has an as-of sibling that does bind a fourteenth. These two are not it,
    /// and they stay at thirteen. The sibling keeps the plan: it discards more candidates inside
    /// the vector arm than the live predicate does, so the ordered index scan pgvector's iterative
    /// scan resumes matters to it more, not less.
    #[test]
    fn the_linear_blend_gains_no_window_function_and_no_new_parameter() {
        for sql in [SEARCH_LIVE, SEARCH_ALL] {
            assert!(!sql.contains("row_number"), "the shipped plan must not gain a WindowAgg");
            assert!(!sql.contains("$14"), "the shipped statement binds thirteen parameters");
            assert!(sql.contains("g.similarity * $9 + g.lexical * $10"));
        }
        assert!(!SEARCH_AS_OF.contains("row_number"), "the as-of plan is the linear plan");
    }

    #[test]
    fn rank_fusion_ranks_both_arms_and_carries_both_ranks_through() {
        for sql in [SEARCH_RRF_LIVE, SEARCH_RRF_ALL] {
            assert_eq!(sql.matches("row_number() OVER").count(), 2, "one rank per arm");
            assert!(sql.contains("v.rank AS rank_vec"));
            assert!(sql.contains("l.rank AS rank_lex"));
        }
    }

    /// A missing arm contributes zero. `$14 + NULL` is NULL, the division is NULL, and NULL
    /// swallows the sum, so each reciprocal is wrapped whole. Wrapping the rank instead would score
    /// an absent arm as rank zero, which is better than the best real rank.
    #[test]
    fn rank_fusion_zeroes_a_missing_arm_rather_than_letting_null_eat_the_score() {
        for sql in [SEARCH_RRF_LIVE, SEARCH_RRF_ALL] {
            assert!(sql.contains(
                "COALESCE($9::float8 / ($14::float8 + g.rank_vec::float8), 0.0::float8)"
            ));
            assert!(sql.contains(
                "COALESCE($10::float8 / ($14::float8 + g.rank_lex::float8), 0.0::float8)"
            ));
        }
    }

    /// An RRF score is about 1/61. The linear blend's additive 0.05 use boost is three times that,
    /// so under rank fusion the term multiplies and the cross-namespace penalty stays a factor.
    #[test]
    fn rank_fusion_multiplies_the_use_boost_instead_of_adding_it() {
        for sql in [SEARCH_RRF_LIVE, SEARCH_RRF_ALL] {
            assert!(sql.contains("* (1.0::float8 + $13::float8"));
            assert!(!sql.contains("1.0) * $13"), "the additive form belongs to the linear blend");
            assert!(sql.contains("THEN 1.0::float8 ELSE $11::float8 END))::float8"));
        }
    }

    /// The score expression in Rust, so the arithmetic can be asserted without a database.
    ///
    /// It mirrors the SQL and mirrors drift. The text assertions above are what tie it to the
    /// statement that actually runs; this function only settles what the formula does.
    #[allow(clippy::too_many_arguments)]
    fn rrf_score(
        rank_vec: Option<f64>,
        rank_lex: Option<f64>,
        k: f64,
        w_vec: f64,
        w_lex: f64,
        usage_weight: f64,
        access_count: f64,
        primary: bool,
        penalty: f64,
    ) -> f64 {
        let base =
            rank_vec.map_or(0.0, |r| w_vec / (k + r)) + rank_lex.map_or(0.0, |r| w_lex / (k + r));
        let use_term = ((1.0 + access_count).ln() / 11.0f64.ln()).min(1.0);
        base * (1.0 + usage_weight * use_term) * if primary { 1.0 } else { penalty }
    }

    fn plain(rank_vec: Option<f64>, rank_lex: Option<f64>) -> f64 {
        rrf_score(rank_vec, rank_lex, 60.0, 1.0, 0.35, 0.05, 0.0, true, 0.85)
    }

    #[test]
    fn a_row_one_arm_missed_scores_what_the_other_arm_earned() {
        let vector_only = plain(Some(1.0), None);
        assert!((vector_only - 1.0 / 61.0).abs() < 1e-12, "got {vector_only}");
        let lexical_only = plain(None, Some(1.0));
        assert!((lexical_only - 0.35 / 61.0).abs() < 1e-12, "got {lexical_only}");
        assert!(plain(None, None) == 0.0, "a row neither arm returned cannot be scored");
    }

    /// The reason for the change. Under the linear blend a lexical match worth 0.259 arrives as
    /// 0.091 against a cosine of 0.7 and moves nothing. Here a row both arms found at rank 3 beats
    /// a row only the vector arm found at rank 1.
    #[test]
    fn agreement_between_the_arms_outranks_a_single_strong_arm() {
        assert!(plain(Some(3.0), Some(3.0)) > plain(Some(1.0), None));
    }

    #[test]
    fn a_better_rank_always_scores_higher_within_one_arm() {
        let mut previous = f64::INFINITY;
        for rank in 1..=20 {
            let score = plain(Some(rank as f64), None);
            assert!(score < previous, "rank {rank} did not fall below the rank above it");
            previous = score;
        }
    }

    /// The bound that makes the multiplicative form safe: an infinitely used row is worth five
    /// percent more than an identical unused one, so the boost decides between neighbours and
    /// never drags a distant row to the top.
    #[test]
    fn the_use_boost_is_worth_at_most_five_percent() {
        let unused = plain(Some(4.0), None);
        let hammered = rrf_score(Some(4.0), None, 60.0, 1.0, 0.35, 0.05, 1e9, true, 0.85);
        assert!(hammered / unused <= 1.05 + 1e-12, "ratio {}", hammered / unused);
        assert!(hammered > unused);

        // Five ranks is already out of its reach.
        assert!(
            rrf_score(Some(6.0), None, 60.0, 1.0, 0.35, 0.05, 1e9, true, 0.85)
                < plain(Some(1.0), None)
        );
    }

    #[test]
    fn the_cross_namespace_penalty_scales_the_whole_score() {
        let here = plain(Some(2.0), Some(5.0));
        let elsewhere = rrf_score(Some(2.0), Some(5.0), 60.0, 1.0, 0.35, 0.05, 0.0, false, 0.85);
        assert!((elsewhere - here * 0.85).abs() < 1e-12);
    }

    /// Larger k flattens the difference between the top ranks, which is the only thing the setting
    /// does. It is a knob for an operator running a comparison, not a correctness lever.
    #[test]
    fn a_larger_k_narrows_the_gap_between_the_first_two_ranks() {
        let gap = |k: f64| {
            rrf_score(Some(1.0), None, k, 1.0, 0.35, 0.05, 0.0, true, 0.85)
                - rrf_score(Some(2.0), None, k, 1.0, 0.35, 0.05, 0.0, true, 0.85)
        };
        assert!(gap(600.0) < gap(60.0));
        assert!(gap(60.0) < gap(6.0));
    }

    #[test]
    fn a_ceiling_list_becomes_two_arrays_in_the_same_order() {
        let ceilings = vec![
            NamespaceCeiling { namespace: "user:me".into(), max: Sensitivity::Private },
            NamespaceCeiling { namespace: "global".into(), max: Sensitivity::Open },
        ];
        let (namespaces, maxima) = split_ceilings(&ceilings);
        assert_eq!(namespaces, vec!["user:me", "global"]);
        assert_eq!(maxima, vec!["private", "open"]);
    }

    #[test]
    fn an_empty_grant_produces_two_empty_arrays_rather_than_a_wildcard() {
        let (namespaces, maxima) = split_ceilings(&[]);
        assert!(namespaces.is_empty() && maxima.is_empty());
    }

    #[test]
    fn only_the_algorithm_this_build_implements_survives_the_enc_alg_column() {
        let id = uuid::Uuid::nil();
        assert_eq!(known_alg(id, crate::crypto::envelope::ALG), crate::crypto::envelope::ALG);
        assert_eq!(known_alg(id, "rot13"), UNRECOGNISED_ALG);
        assert_eq!(known_alg(id, ""), UNRECOGNISED_ALG);
    }

    /// Every statement whose rows reach `memory_from_row`, with how many `Memory` column lists it
    /// carries. `SAMPLE_CONTENT_SQL` selects `m.content` and is absent on purpose: it returns
    /// strings and builds no `Memory`.
    const EVERY_MEMORY_LIST_SQL: [(&str, &str, usize); 11] = [
        ("SEARCH_LIVE", SEARCH_LIVE, 1),
        ("SEARCH_ALL", SEARCH_ALL, 1),
        ("SEARCH_RRF_LIVE", SEARCH_RRF_LIVE, 1),
        ("SEARCH_RRF_ALL", SEARCH_RRF_ALL, 1),
        ("SEARCH_AS_OF", SEARCH_AS_OF, 1),
        ("SEARCH_RRF_AS_OF", SEARCH_RRF_AS_OF, 1),
        ("RECENT_LIVE", RECENT_LIVE, 1),
        ("RECENT_ALL", RECENT_ALL, 1),
        ("DIGEST_SQL", DIGEST_SQL, 3),
        ("SUPERSESSION_HEAD_SQL", SUPERSESSION_HEAD_SQL, 1),
        ("SUBJECT_HISTORY_SQL", SUBJECT_HISTORY_SQL, 1),
    ];

    /// The cheapest guard against the worst failure mode this change has. `memory_from_row` reads
    /// by name and panics on a column the query did not select, so a list that gained the valid-time
    /// pair everywhere but one place breaks one request path and leaves the rest working.
    #[test]
    fn every_list_that_reads_content_reads_valid_time_beside_it() {
        for (name, sql, lists) in EVERY_MEMORY_LIST_SQL {
            assert!(sql.contains("m.content"), "{name} does not read content");
            assert_eq!(
                sql.matches("m.last_confirmed_at").count(),
                lists,
                "{name} carries {lists} column lists"
            );
            assert_eq!(
                sql.matches("m.occurred_at, m.occurred_until").count(),
                lists,
                "{name} has a column list without the valid-time pair"
            );
        }
    }

    /// The timeline read, pinned on the four things that make it one.
    ///
    /// It walks both ways, because the id a caller holds can be any version. It caps both walks,
    /// because a cycle in the table would otherwise run to the statement timeout. It orders by
    /// depth with a tiebreak, because a row that retired two predecessors puts two rows at one
    /// depth. It crosses namespaces, because a supersede may retire a row in one namespace in
    /// favour of a row in another and a walk that stopped there called a partial history complete.
    #[test]
    fn the_timeline_walks_both_ways_and_crosses_namespaces() {
        assert!(SUBJECT_HISTORY_SQL.contains("JOIN forward f ON m.id = f.superseded_by"));
        assert!(SUBJECT_HISTORY_SQL.contains("JOIN backward b ON m.superseded_by = b.id"));
        assert_eq!(SUBJECT_HISTORY_SQL.matches("depth < $3").count(), 2, "both walks are capped");
        assert!(SUBJECT_HISTORY_SQL.contains("ORDER BY w.depth, m.created_at, m.id"));
        assert!(
            !SUBJECT_HISTORY_SQL.contains("namespace = $"),
            "no step of this statement is scoped to one namespace"
        );
    }

    /// Six parameters, the same way the search statements pin theirs. A seventh would mean a bind
    /// nobody added on the Rust side, and the arrays are where that would land.
    #[test]
    fn the_timeline_binds_six_parameters() {
        assert!(SUBJECT_HISTORY_SQL.contains("$6::text[]"));
        assert!(!SUBJECT_HISTORY_SQL.contains("$7"));
    }

    /// Both policy axes filter the rows and neither steers the walk. A version the grant refuses is
    /// dropped after the chain is assembled, so the timeline shows a gap; filtering inside the
    /// recursion would cut the chain at that version and report a short timeline as a complete one.
    #[test]
    fn the_timeline_filters_after_the_walk_rather_than_inside_it() {
        let recursion = SUBJECT_HISTORY_SQL.split_once("chain AS (").unwrap().0;
        assert!(
            !recursion.contains("sensitivity"),
            "the recursion must not stop at a row above the ceiling"
        );
        assert!(
            !recursion.contains("namespace"),
            "the recursion must not stop at a namespace boundary"
        );
        assert_eq!(
            recursion.matches("tenant_id = $1").count(),
            4,
            "two anchors and two recursive steps stay inside one tenant"
        );
    }

    /// A dropped version is counted, and a walk that ran out of hops says so. Silence about either
    /// is the failure this statement was rewritten to end.
    #[test]
    fn the_timeline_counts_what_it_withheld_and_admits_when_it_stopped_short() {
        assert!(SUBJECT_HISTORY_SQL.contains("count(*) FILTER (WHERE NOT readable) AS withheld"));
        assert!(SUBJECT_HISTORY_SQL.contains("bool_or(abs(depth) >= $3)"));
        assert!(SUBJECT_HISTORY_SQL.contains(", gap.withheld, gap.depth_capped"));
    }

    /// An anchor the grant refuses answers nothing, gap included. The gate reads the anchor's own
    /// verdict rather than its depth, because a cycle can reach the anchor backwards and give it a
    /// negative one.
    #[test]
    fn the_timeline_gates_on_the_anchor_being_readable() {
        assert!(SUBJECT_HISTORY_SQL
            .contains("EXISTS (SELECT 1 FROM walked a WHERE a.id = $2 AND a.readable)"));
        assert!(!SUBJECT_HISTORY_SQL.contains("depth = 0"));
    }

    /// A timeline is retired rows and nothing else would make it one.
    #[test]
    fn the_timeline_returns_retired_rows() {
        assert!(!SUBJECT_HISTORY_SQL.contains("superseded_by IS NULL"));
    }

    /// The grant predicate in the timeline statement, written in Rust so the matrix below can hold
    /// it against the domain rule. `left(ns, length(prefix)) = prefix` and `starts_with` are the
    /// same comparison: for valid UTF-8 a byte prefix and a character prefix are the same prefix.
    fn sql_admits_namespace(prefix: &str, exact: bool, namespace: &str) -> bool {
        if exact {
            namespace == prefix
        } else {
            namespace.starts_with(prefix)
        }
    }

    /// The one place a glob is matched in SQL, held against `namespaces::matches`. Two spellings of
    /// one rule is the risk this translation takes on, and drift between them would show up as a
    /// caller reading a namespace their grant never named.
    #[test]
    fn the_sql_grant_match_agrees_with_the_namespace_rule() {
        let patterns = [
            "*",
            "**",
            "project:*",
            "project:lumberroom",
            "credentials:*",
            "global",
            " Project:* ",
        ];
        let namespaces = [
            "global",
            "user:me",
            "project:lumberroom",
            "project:lumberrooms",
            "credentials:aws",
            "personal:finance",
        ];
        for pattern in patterns {
            let (prefixes, exact, _) = split_grants(&[NamespaceGrant::open(pattern)]);
            for ns in namespaces {
                assert_eq!(
                    sql_admits_namespace(&prefixes[0], exact[0], ns),
                    crate::domain::namespaces::matches(pattern, ns),
                    "{pattern:?} against {ns:?}"
                );
            }
        }
    }

    /// The ceiling travels with its pattern and in its order. Three arrays that disagree on order
    /// would hand one namespace another's ceiling, which is the quiet half of this translation.
    #[test]
    fn each_pattern_keeps_its_own_ceiling_and_position() {
        let (prefixes, exact, maxima) = split_grants(&[
            NamespaceGrant::new("project:*", Sensitivity::Private),
            NamespaceGrant::new("global", Sensitivity::Open),
            NamespaceGrant::new("*", Sensitivity::Sealed),
        ]);
        assert_eq!(prefixes, vec!["project:", "global", ""]);
        assert_eq!(exact, vec![false, true, false]);
        assert_eq!(maxima, vec!["private", "open", "sealed"]);
    }

    #[test]
    fn an_empty_grant_produces_three_empty_arrays() {
        let (prefixes, exact, maxima) = split_grants(&[]);
        assert!(prefixes.is_empty() && exact.is_empty() && maxima.is_empty());
    }

    /// The macro is the read site the hand-written lists are copied from.
    #[test]
    fn the_shared_column_list_carries_valid_time() {
        const SQL: &str = select_memory!("", "FROM memory");
        assert!(SQL.contains("content, "));
        assert!(SQL.contains("occurred_at, "));
        assert!(SQL.contains("occurred_until "));
    }

    /// Every statement in the adapter that reads or writes the supersession link, classified.
    ///
    /// The live readers splice `live!()` and carry the period test beside the link test. The
    /// statements below read or write history on purpose, each with the reason, and the test after
    /// this refuses any occurrence that is neither. A reader added to either file has to be
    /// classified before the suite goes green, which is the whole point: the first version of
    /// decision 0017 left five statements behind and nothing said so.
    const HISTORY_OWNERS: [(&str, &str); 9] = [
        ("occurred_at_compliance", "counts dating discipline, which an expiry does not change"),
        (
            "namespace_counts",
            "discovery: a namespace does not stop existing when a fact in it expires, and this \
             count never reaches a response",
        ),
        ("SEED_ALIAS_SQL", "structural edges, rebuilt hourly and severed by the reader's grant"),
        ("SEED_TAG_SQL", "structural edges, rebuilt hourly and severed by the reader's grant"),
        ("SAMPLE_CONTENT_SQL", "the recall monitor samples what search could reach at all"),
        ("RETIRED_SQL", "the list of what left the live reads, both kinds, by definition"),
        ("RETIRE_PREDECESSOR_SQL", "a writer, and its guard reads the state it refuses"),
        ("EXPIRE_SQL", "a writer, and its guard reads both columns already"),
        ("UNEXPIRE_SQL", "a writer, and its guard reads both columns already"),
    ];

    /// A new reader cannot reach Postgres without somebody deciding which clock it answers.
    ///
    /// Both adapter files read their own source, because the thing being guarded is the text of
    /// statements written in five shapes: a macro call, a `concat!`, a raw string, an inline
    /// literal inside a method, and a fragment spliced into another statement. A test over a list
    /// of constants misses every statement somebody writes inline, and those are the ones that got
    /// missed.
    ///
    /// Matching is exact text, not parsing. A line counts as live when the three lines starting at
    /// it contain the literal `occurred_until IS NULL OR`, which is the opening of the period test
    /// in `live!()` and in the three statements that spell it under their own aliases. Any other
    /// mention of `occurred_until` nearby proves nothing: `EXPIRE_SQL` carries one in a guard that
    /// is not a period test at all.
    #[test]
    fn every_link_test_in_the_adapter_is_classified_live_or_history() {
        // Up to the test module in each file, so the assertions below are not their own evidence.
        const SOURCES: [(&str, &str); 2] =
            [("memory.rs", include_str!("memory.rs")), ("cleanup.rs", include_str!("cleanup.rs"))];

        let mut unclassified: Vec<String> = vec![];
        for (file, source) in SOURCES {
            let head = source.split("#[cfg(test)]").next().unwrap();
            let lines: Vec<&str> = head.lines().collect();
            let mut owner = "the top of the file";
            for (i, line) in lines.iter().enumerate() {
                if let Some(name) = declared_name(line) {
                    owner = name;
                }
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || !trimmed.contains("superseded_by IS NULL") {
                    continue;
                }
                if HISTORY_OWNERS.iter().any(|(name, _)| *name == owner) {
                    continue;
                }
                let window = lines[i..(i + 3).min(lines.len())].join(" ");
                if window.contains("occurred_until IS NULL OR") {
                    continue;
                }
                unclassified.push(format!("{file}: {owner}, line {}", i + 1));
            }
        }

        assert!(
            unclassified.is_empty(),
            "these read the supersession link and answer neither clock question. Splice `live!()` \
             or name them in HISTORY_OWNERS with a reason: {unclassified:?}"
        );
    }

    /// The constant or function a line declares, for the scan above.
    ///
    /// Indentation is stripped first. A `const` nested inside a function is its own owner, and
    /// reading it as part of the function around it would file its statement under a name the
    /// allow list might already carry.
    fn declared_name(line: &str) -> Option<&str> {
        let trimmed = line.trim_start();
        let after = |kw: &str| -> Option<&str> {
            let rest = trimmed.split_once(kw)?.1.trim_start();
            let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
            Some(&rest[..end]).filter(|n| !n.is_empty())
        };
        for kw in ["const ", "static "] {
            if trimmed.starts_with(kw) || trimmed.starts_with(&format!("pub {kw}")) {
                return after(kw);
            }
        }
        for kw in ["fn ", "async fn ", "pub fn ", "pub async fn ", "pub(super) async fn "] {
            if trimmed.starts_with(kw) {
                return after("fn ");
            }
        }
        None
    }

    /// A live search answers both clocks, and it binds what it always bound.
    ///
    /// Valid time reached a result through the select list alone until decision 0017, which put the
    /// period test into the live predicate as a conjunct. The two live statements carry it, the two
    /// history statements carry neither clock, and none of the four gained a parameter. The vector
    /// arm's plan is what pgvector's iterative scan depends on, so the shape that stays fixed is
    /// the one that matters: no range filter, no new bind, one more boolean term.
    #[test]
    fn a_live_search_answers_both_clocks_and_binds_what_it_always_did() {
        for sql in NO_AS_OF_SQL {
            assert!(sql.contains("m.occurred_at, m.occurred_until,"));
            assert!(!sql.contains("occurred_at <="), "no range filter on a live read");
        }
        for sql in [SEARCH_LIVE, SEARCH_RRF_LIVE] {
            assert_eq!(sql.matches(live!()).count(), 2, "the vector arm and the lexical arm");
        }
        for sql in [SEARCH_ALL, SEARCH_RRF_ALL] {
            assert!(!sql.contains("superseded_by IS NULL"));
            assert!(!sql.contains("occurred_until IS NULL"), "history asks about neither clock");
        }
        for sql in [SEARCH_LIVE, SEARCH_ALL] {
            assert!(sql.contains("$13"), "the shipped statement still binds thirteen parameters");
            assert!(!sql.contains("$14"));
        }
        for sql in [SEARCH_RRF_LIVE, SEARCH_RRF_ALL] {
            assert!(sql.contains("$14"), "rank fusion still binds k as the fourteenth");
            assert!(!sql.contains("$15"));
        }
        // The as-of pair reads the same two columns into its result and adds the predicate.
        for sql in AS_OF_SQL {
            assert!(sql.contains("m.occurred_at, m.occurred_until,"));
            assert!(sql.contains("m.occurred_until IS NULL OR m.occurred_until >"));
        }
    }

    /// The statements that answer "what does the store hold now", in one list.
    ///
    /// A reader added to this file is live or it is history, and the test below is where a new one
    /// gets classified. The statements the adapter builds inline (`neighbours`, `conflicts`,
    /// `staleness`) carry the same two clauses under their own prefixes and cannot join a list of
    /// constants.
    const LIVE_SQL: [&str; 6] =
        [SEARCH_LIVE, SEARCH_RRF_LIVE, RECENT_LIVE, DIGEST_SQL, GRAPH_NEIGHBOURS_SQL, UNDATED_SQL];

    /// A live read answers both clocks: no successor replaced the row, and its period is open.
    ///
    /// Decision 0017 reversed the ruling that a live read carries no period predicate. The history
    /// readers keep what they had, because a closed period is exactly what they are asked about.
    #[test]
    fn every_live_statement_carries_the_period_conjunct() {
        for sql in LIVE_SQL {
            assert!(sql.contains("m.superseded_by IS NULL"));
            assert!(sql.contains("m.occurred_until IS NULL OR m.occurred_until > now()"));
        }
        // The history readers keep what they had.
        for sql in [SEARCH_ALL, SEARCH_RRF_ALL] {
            assert!(!sql.contains("occurred_until > now()"));
        }
        assert!(!RETIRED_SQL.contains("occurred_until > now()"));
        for sql in AS_OF_SQL {
            assert!(!sql.contains("occurred_until > now()"), "as-of reads an instant, not now");
        }
    }

    /// Three digest select lists gained two columns each. The join count is what says no arm lost
    /// its grant filter on the way.
    #[test]
    fn the_digest_gains_columns_and_keeps_its_seven_filtered_subqueries() {
        assert_eq!(DIGEST_SQL.matches("m.occurred_at, m.occurred_until").count(), 3);
        assert_eq!(DIGEST_SQL.matches("JOIN reachable rg").count(), 7);
        assert_eq!(DIGEST_SQL.matches("m.superseded_by IS NULL").count(), 5);
    }

    /// One statement retires a predecessor, and it ends that row's validity in the same breath.
    /// Two statements doing this diverged once already.
    #[test]
    fn retiring_a_row_ends_its_validity_in_the_same_statement() {
        assert!(RETIRE_PREDECESSOR_SQL.contains("superseded_by  = $3"));
        assert!(RETIRE_PREDECESSOR_SQL.contains("occurred_until = COALESCE(occurred_until, $4"));
        assert!(RETIRE_PREDECESSOR_SQL.contains("AND superseded_by IS NULL"));
        // The start is never rewritten. A change ends a period; moving its start is a correction.
        assert!(!RETIRE_PREDECESSOR_SQL.contains("occurred_at ="));
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().into()
    }

    #[test]
    fn a_dated_successor_ends_its_predecessor_at_its_own_start() {
        let until = supersession_until(
            Some(at("2026-03-01T00:00:00Z")),
            Some(at("2026-07-04T09:00:00Z")),
            at("2026-08-20T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(until, Some(at("2026-07-04T09:00:00Z")));
    }

    /// The undated arm. It stores when the store learned of the replacement, which is an admission
    /// of ignorance rather than a measurement, and ingestion is what keeps it rare.
    #[test]
    fn an_undated_successor_falls_back_to_when_the_store_learned_it() {
        let until =
            supersession_until(Some(at("2026-03-01T00:00:00Z")), None, at("2026-08-20T12:00:00Z"))
                .unwrap();
        assert_eq!(until, Some(at("2026-08-20T12:00:00Z")));
    }

    #[test]
    fn an_undated_predecessor_gains_an_end_and_keeps_its_unknown_start() {
        let until =
            supersession_until(None, Some(at("2026-07-04T09:00:00Z")), at("2026-08-20T12:00:00Z"))
                .unwrap();
        assert_eq!(until, Some(at("2026-07-04T09:00:00Z")));
    }

    /// The case the 222 queued proposals produce: a July fact approved after an August one. Left
    /// alone it ends the August fact a month before that fact began.
    #[test]
    fn a_successor_that_became_true_first_is_refused_naming_both_dates() {
        let err = supersession_until(
            Some(at("2026-08-01T00:00:00Z")),
            Some(at("2026-07-04T09:00:00Z")),
            at("2026-08-20T12:00:00Z"),
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("2026-07-04"), "{message}");
        assert!(message.contains("2026-08-01"), "{message}");
    }

    /// Equality proceeds and writes no end. `[T, T)` is an empty period, which says the fact was
    /// never true, and a caller replacing a fact did not ask to erase it.
    #[test]
    fn a_successor_starting_at_the_same_instant_leaves_the_end_unknown() {
        let t = at("2026-07-04T09:00:00Z");
        assert_eq!(supersession_until(Some(t), Some(t), at("2026-08-20T12:00:00Z")).unwrap(), None);
    }

    /// The future-date hole, closed here rather than left to the CHECK constraint. A predecessor
    /// holding a legal future start against an undated successor would otherwise take an end at
    /// today, which is earlier than its own start.
    #[test]
    fn a_future_dated_predecessor_keeps_an_open_end_rather_than_an_inverted_one() {
        let until =
            supersession_until(Some(at("2027-01-01T00:00:00Z")), None, at("2026-08-20T12:00:00Z"))
                .unwrap();
        assert_eq!(until, None);
    }

    #[test]
    fn an_empty_store_is_empty_rather_than_stale() {
        assert_eq!(percentage(0, 0), 0.0);
        assert_eq!(percentage(1, 3), 33.33);
        assert_eq!(percentage(3, 3), 100.0);
    }

    /// What only a database can answer about the timeline: whether a real chain crossing a real
    /// namespace boundary comes back whole.
    ///
    /// Its own database, `lumberroom_timeline_test`. `lumberroom` holds the owner's memories and
    /// `lumberroom_rust_test` belongs to the integration suite, which truncates it; a concurrent run of
    /// either has nothing to lose here. Every row carries a tenant no other suite uses, and setup
    /// deletes that tenant's rows rather than truncating the table.
    ///
    /// Skipped when no database is reachable, so `cargo test` on a machine without one passes with
    /// these tests never having run. A count of zero here is not a green run.
    mod timeline {
        use super::*;
        use sqlx::PgPool;

        const TEST_DB: &str = "lumberroom_timeline_test";
        const TENANT: &str = "timeline_track_c";

        /// Each test writes and deletes rows in one shared database, so they take turns. The guard
        /// lives as long as the test that holds it.
        static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

        async fn setup(
        ) -> Option<(PgMemoryRepository, PgPool, tokio::sync::MutexGuard<'static, ()>)> {
            let guard = SERIAL.lock().await;
            let admin_url = std::env::var("DATABASE_URL").ok()?;
            let base = admin_url.rsplit_once('/')?.0.to_string();
            let admin = PgPool::connect(&admin_url).await.ok()?;
            let exists: Option<i32> =
                sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
                    .bind(TEST_DB)
                    .fetch_optional(&admin)
                    .await
                    .ok()?;
            if exists.is_none() {
                // DDL takes no bind parameter, so this statement is built as a string and sqlx
                // requires the audit to be written down. Audited: `TEST_DB` is a compile-time
                // constant and no input of any kind reaches it.
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {TEST_DB}")))
                    .execute(&admin)
                    .await
                    .ok()?;
            }
            admin.close().await;

            let pool =
                crate::adapters::postgres::connect(&format!("{base}/{TEST_DB}")).await.ok()?;
            crate::adapters::postgres::migrate(&pool).await.ok()?;
            sqlx::query("DELETE FROM memory WHERE tenant_id = $1")
                .bind(TENANT)
                .execute(&pool)
                .await
                .ok()?;
            Some((PgMemoryRepository::new(pool.clone()), pool, guard))
        }

        /// One plaintext row. `minute` spaces the rows in creation order, so the ordering tiebreak
        /// is fixed rather than whatever `now()` returned twice in the same microsecond.
        async fn insert(
            pool: &PgPool,
            namespace: &str,
            level: &str,
            content: &str,
            minute: i32,
        ) -> uuid::Uuid {
            let id = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO memory
                     (id, tenant_id, namespace, content, source_client, sensitivity, created_at)
                 VALUES ($1, $2, $3, $4, 'track-c-test', $5, now() + $6::int * interval '1 minute')",
            )
            .bind(id)
            .bind(TENANT)
            .bind(namespace)
            .bind(content)
            .bind(level)
            .bind(minute)
            .execute(pool)
            .await
            .expect("insert");
            id
        }

        /// Retire `old` in favour of `new`, both directions, the way `supersede` writes it. Written
        /// here rather than called through the repository because two of these tests build shapes
        /// the repository refuses on purpose, a cycle among them.
        async fn link(pool: &PgPool, old: uuid::Uuid, new: uuid::Uuid) {
            sqlx::query(
                "UPDATE memory SET superseded_by = $2, superseded_at = now()
                  WHERE tenant_id = $1 AND id = $3",
            )
            .bind(TENANT)
            .bind(new)
            .bind(old)
            .execute(pool)
            .await
            .expect("retire");
            sqlx::query("UPDATE memory SET supersedes = $2 WHERE tenant_id = $1 AND id = $3")
                .bind(TENANT)
                .bind(old)
                .bind(new)
                .execute(pool)
                .await
                .expect("mirror");
        }

        fn contents(t: &Timeline) -> Vec<&str> {
            t.versions.iter().map(|m| m.content.as_str()).collect()
        }

        /// The bug, in the shape it was reported: a chain whose successor was written into another
        /// namespace used to stop at the boundary and call three versions two.
        #[tokio::test]
        async fn a_chain_crossing_two_readable_namespaces_returns_every_version() {
            let Some((repo, pool, _guard)) = setup().await else { return };
            let first = insert(&pool, "project:alpha", "open", "port 8080", 0).await;
            let second = insert(&pool, "project:beta", "open", "port 8787", 1).await;
            let third = insert(&pool, "project:beta", "open", "port 9000", 2).await;
            link(&pool, first, second).await;
            link(&pool, second, third).await;

            // Anchored on the middle version, so both walks have to work to answer at all.
            let grants = vec![NamespaceGrant::open("project:*")];
            let timeline = repo.subject_history(TENANT, &grants, second).await.unwrap();

            assert_eq!(contents(&timeline), vec!["port 8080", "port 8787", "port 9000"]);
            assert_eq!(timeline.withheld, 0);
            assert!(!timeline.depth_capped);
        }

        /// The other half of the same rule. The version the grant refuses is absent, the versions
        /// behind it survive, and the count says one is missing.
        #[tokio::test]
        async fn a_version_the_grant_refuses_leaves_a_counted_gap() {
            let Some((repo, pool, _guard)) = setup().await else { return };
            let first = insert(&pool, "project:alpha", "open", "salary was public", 0).await;
            let middle = insert(&pool, "personal:finance", "private", "the actual number", 1).await;
            let last = insert(&pool, "project:alpha", "open", "public again", 2).await;
            link(&pool, first, middle).await;
            link(&pool, middle, last).await;

            let grants = vec![NamespaceGrant::open("project:*")];
            let timeline = repo.subject_history(TENANT, &grants, first).await.unwrap();

            assert_eq!(contents(&timeline), vec!["salary was public", "public again"]);
            assert_eq!(timeline.withheld, 1, "the gap is reported rather than papered over");
        }

        /// A cycle terminates and says it was cut short. Nothing writes this shape through the
        /// repository, which refuses it; a table that already holds one still has to be readable.
        #[tokio::test]
        async fn the_depth_cap_still_terminates_a_cycle() {
            let Some((repo, pool, _guard)) = setup().await else { return };
            let one = insert(&pool, "project:alpha", "open", "round and round", 0).await;
            let two = insert(&pool, "project:alpha", "open", "and back again", 1).await;
            link(&pool, one, two).await;
            link(&pool, two, one).await;

            let grants = vec![NamespaceGrant::open("project:*")];
            let timeline = repo.subject_history(TENANT, &grants, one).await.unwrap();

            assert_eq!(timeline.versions.len(), 2, "one row per version, however many hops");
            assert!(timeline.depth_capped, "the walk ran out of hops rather than out of chain");
        }

        /// The common row in the store: no predecessor, no successor, its own whole history.
        #[tokio::test]
        async fn a_single_row_chain_is_the_row_itself() {
            let Some((repo, pool, _guard)) = setup().await else { return };
            let only = insert(&pool, "global", "open", "the only version", 0).await;

            let grants = vec![NamespaceGrant::open("*")];
            let timeline = repo.subject_history(TENANT, &grants, only).await.unwrap();

            assert_eq!(contents(&timeline), vec!["the only version"]);
            assert_eq!(timeline.withheld, 0);
            assert!(!timeline.depth_capped);
        }

        /// An id above the grant answers nothing at all, gap included. A count here would confirm
        /// the row exists to a caller who may not know that.
        #[tokio::test]
        async fn an_anchor_the_grant_refuses_answers_nothing() {
            let Some((repo, pool, _guard)) = setup().await else { return };
            let public = insert(&pool, "project:alpha", "open", "the readable half", 0).await;
            let secret = insert(&pool, "personal:finance", "private", "the other half", 1).await;
            link(&pool, public, secret).await;

            let grants = vec![NamespaceGrant::open("project:*")];
            let timeline = repo.subject_history(TENANT, &grants, secret).await.unwrap();

            assert!(timeline.is_empty());
            assert_eq!(timeline.withheld, 0);
        }
    }
}
