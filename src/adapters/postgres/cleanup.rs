//! Postgres implementation of `CleanupRepository`.
//!
//! Three candidate queries and a small queue. The candidate queries are where the care is, and two
//! properties carry them.
//!
//! **The window anchors one side.** A run reads rows created since its watermark and compares each
//! against every live row in scope, windowed or not. Filtering both sides is the obvious reading of
//! "read what changed" and it is wrong: restating a fact the store learned in July produces one new
//! row and one old one, and a window holding only the new one has nothing to compare it against.
//! That case is the most common duplicate there is.
//!
//! **The caller's grant is a term in every candidate query.** It arrives as three parallel arrays
//! and joins in as a table, the way the timeline walk in `memory.rs` does it, because the rows a
//! pass may look at are the answer rather than the question and there is no namespace list to
//! resolve the globs against beforehand. A row the caller may not read never leaves the database.
//!
//! **Nothing here touches the HNSW index.** Every query filters by namespace, by sensitivity and to
//! live rows, and filtered HNSW is the trap this repository has already paid for: ten rows asked
//! for, zero returned, forty candidates pulled and all forty filtered away, no error. A pass whose
//! job is to notice what the store holds must never be able to answer "nothing" because an index
//! truncated, so the distance here is computed exactly. The work is new rows times live rows, which
//! a personal store does not make expensive.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::domain::cleanup::{cluster_key, CleanupKind, Disposition};
use crate::domain::errors::{DomainError, Result};
use crate::domain::policy::NamespaceGrant;
use crate::domain::types::Sensitivity;
use crate::ports::cleanup::{
    Arity, Candidate, CandidatePair, CandidateQuery, Cardinality, CleanupRepository, Member,
    NewProposal, Proposal, QueueOutcome, Watermark,
};

pub struct PgCleanupRepository {
    pool: PgPool,
}

impl PgCleanupRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// A namespace glob as SQL, matching `domain::namespaces::matches`.
///
/// `None` means every namespace, a trailing `*` means a prefix, anything else is the whole name.
/// Two bound parameters rather than a `format!`, because dynamic SQL built by string concatenation
/// is refused by the type system here on purpose.
fn ns_bounds(namespace: Option<&str>) -> (String, bool) {
    match namespace {
        None => (String::new(), false),
        Some(raw) => {
            let pattern = raw.trim().to_ascii_lowercase();
            match pattern.strip_suffix('*') {
                Some(prefix) => (prefix.to_string(), false),
                None => (pattern, true),
            }
        }
    }
}

/// The predicate every candidate query shares, spelled out in each of them rather than assembled.
///
/// It reads: this tenant, live, in scope, at or below the ceiling, admitted by the grant, and
/// holding readable text. It is written four times because `sqlx::query` refuses a string built at
/// runtime, and rightly: a predicate assembled with `format!` is one edit away from carrying data.
/// The live half is the one part every candidate statement in this file splices from `live!()`
/// rather than spells, so a row whose period closed is a candidate for no cleanup kind.
/// `$1` tenant, `$2` ceiling, `$3` namespace pattern, `$4` whether that pattern is exact, `$5` to
/// `$7` the grant as prefix, exactness and ceiling.
///
/// `content IS NOT NULL` drops sealed rows whose bytes the server cannot read. A sealed row has no
/// plaintext to compare and no plaintext to show anyone, so it is not a cleanup candidate at all.
///
/// A test at the bottom of this file asserts the four copies stay identical, because four copies
/// of a security predicate is exactly the shape where one gets edited and the others do not.
#[cfg_attr(not(test), allow(dead_code))]
const IN_SCOPE_PREDICATE: &str = "AND ($3::text = '' OR ($4::bool AND m.namespace = $3) OR (NOT $4::bool AND m.namespace LIKE $3 || '%'))";

/// `policy::admits` in SQL: some grant entry matches the namespace and its ceiling is at or above
/// the row's stored level. Union across entries, like `policy::ceiling`. An empty grant admits
/// nothing, which is the right reading of a caller that was granted nothing.
#[cfg_attr(not(test), allow(dead_code))]
const GRANT_PREDICATE: &str = r#"AND EXISTS (
      SELECT 1 FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
       WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                  ELSE left(m.namespace, length(g.prefix)) = g.prefix END
         AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
    )"#;

/// Live rows whose normalised content is byte-identical, grouped, anchored on what changed.
const EXACT_DUPLICATES_SQL: &str = concat!(
    r#"
            WITH scoped AS (
              SELECT m.id, m.namespace, m.sensitivity, m.content, m.created_at, m.access_count,
                     m.namespace || '\x1f' ||
                     lower(regexp_replace(btrim(m.content), '\s+', ' ', 'g')) AS norm
                FROM memory m
               WHERE     m.tenant_id = $1
    AND "#,
    live!(),
    r#"
    AND m.content IS NOT NULL
    AND (
      CASE $2::text
        WHEN 'open' THEN m.sensitivity = 'open'
        WHEN 'private' THEN m.sensitivity IN ('open', 'private')
        ELSE true
      END
    )
    AND ($3::text = '' OR ($4::bool AND m.namespace = $3) OR (NOT $4::bool AND m.namespace LIKE $3 || '%'))
    AND EXISTS (
      SELECT 1 FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
       WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                  ELSE left(m.namespace, length(g.prefix)) = g.prefix END
         AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
    )
            ),
            -- The anchor: groups holding at least one row this run is responsible for. A group
            -- entirely older than the watermark was already proposed by an earlier run.
            touched AS (
              SELECT norm FROM scoped
               WHERE $8::timestamptz IS NULL OR created_at >= $8
               GROUP BY norm
            )
            SELECT s.* FROM scoped s
             WHERE s.norm IN (SELECT norm FROM touched)
               AND s.norm IN (SELECT norm FROM scoped GROUP BY norm HAVING count(*) > 1)
             ORDER BY s.norm, s.created_at
             LIMIT $9
            "#
);

/// Live pairs within a cosine band, anchored on one side and open on the other.
///
/// `b.namespace = a.namespace`: a pair straddling two namespaces produced a proposal whose own
/// `namespace` named only one of them, and the queue's grant check read that one field. Exact
/// duplicates were already grouped per namespace by the `norm` prefix; this brings the cosine band
/// to the same rule. The cost is that a fact restated under a second namespace is not a finding,
/// which `docs/cleanup-schedule.md` had been promising all along.
const SIMILAR_PAIRS_SQL: &str = concat!(
    r#"
            WITH scoped AS (
              SELECT m.id, m.namespace, m.sensitivity, m.content, m.created_at, m.access_count,
                     m.embedding
                FROM memory m
               WHERE     m.tenant_id = $1
    AND "#,
    live!(),
    r#"
    AND m.content IS NOT NULL
    AND (
      CASE $2::text
        WHEN 'open' THEN m.sensitivity = 'open'
        WHEN 'private' THEN m.sensitivity IN ('open', 'private')
        ELSE true
      END
    )
    AND ($3::text = '' OR ($4::bool AND m.namespace = $3) OR (NOT $4::bool AND m.namespace LIKE $3 || '%'))
    AND EXISTS (
      SELECT 1 FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
       WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                  ELSE left(m.namespace, length(g.prefix)) = g.prefix END
         AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
    ) AND m.embedding IS NOT NULL
            )
            SELECT a.id AS a_id, a.namespace AS a_namespace, a.sensitivity AS a_sensitivity,
                   a.content AS a_content, a.created_at AS a_created_at,
                   a.access_count AS a_access_count,
                   b.id AS b_id, b.namespace AS b_namespace, b.sensitivity AS b_sensitivity,
                   b.content AS b_content, b.created_at AS b_created_at,
                   b.access_count AS b_access_count,
                   1 - (a.embedding <=> b.embedding) AS similarity
              FROM scoped a
              JOIN scoped b ON b.id <> a.id AND a.id < b.id AND b.namespace = a.namespace
             WHERE ($8::timestamptz IS NULL OR a.created_at >= $8 OR b.created_at >= $8)
               AND 1 - (a.embedding <=> b.embedding) >= $9
             ORDER BY similarity DESC
             LIMIT $10
            "#
);

/// The newest live row in scope. No window: the question is what the store now holds.
const NEWEST_SQL: &str = concat!(
    r#"
            SELECT max(m.created_at) AS newest
              FROM memory m
             WHERE     m.tenant_id = $1
    AND "#,
    live!(),
    r#"
    AND m.content IS NOT NULL
    AND (
      CASE $2::text
        WHEN 'open' THEN m.sensitivity = 'open'
        WHEN 'private' THEN m.sensitivity IN ('open', 'private')
        ELSE true
      END
    )
    AND ($3::text = '' OR ($4::bool AND m.namespace = $3) OR (NOT $4::bool AND m.namespace LIKE $3 || '%'))
    AND EXISTS (
      SELECT 1 FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
       WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                  ELSE left(m.namespace, length(g.prefix)) = g.prefix END
         AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
    )
"#
);

/// Live rows nothing has read, older than the interval.
///
/// The live test is `live!()`, so a fact whose period closed with no successor is not a stale
/// candidate. Decision 0017 carries the reasoning.
const UNREAD_SQL: &str = concat!(
    r#"
            SELECT m.id, m.namespace, m.sensitivity, m.content, m.created_at, m.access_count
              FROM memory m
             WHERE     m.tenant_id = $1
    AND "#,
    live!(),
    r#"
    AND m.content IS NOT NULL
    AND (
      CASE $2::text
        WHEN 'open' THEN m.sensitivity = 'open'
        WHEN 'private' THEN m.sensitivity IN ('open', 'private')
        ELSE true
      END
    )
    AND ($3::text = '' OR ($4::bool AND m.namespace = $3) OR (NOT $4::bool AND m.namespace LIKE $3 || '%'))
    AND EXISTS (
      SELECT 1 FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
       WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                  ELSE left(m.namespace, length(g.prefix)) = g.prefix END
         AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
    )
               AND m.access_count = 0
               AND m.last_confirmed_at IS NULL
               AND m.created_at < now() - make_interval(days => $8::int)
             ORDER BY m.created_at
             LIMIT $9
            "#
);

/// A proposal this caller may read: its own namespace matched by some grant entry, and every
/// member row readable at the level it is stored at. `$3` to `$5` are the grant arrays.
///
/// The member check is a NOT EXISTS over the members rather than a test on `p.namespace`, because
/// the poster chose `p.namespace` and the members are what the proposal would act on. A member
/// whose memory row is gone has nothing left to read and does not block; in practice the member
/// row cascades away with the memory.
#[cfg_attr(not(test), allow(dead_code))]
const PROPOSAL_VISIBLE_PREDICATE: &str = r#"
              AND EXISTS (
                    SELECT 1 FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
                     WHERE CASE WHEN g.exact THEN p.namespace = g.prefix
                                ELSE left(p.namespace, length(g.prefix)) = g.prefix END
                  )
              AND NOT EXISTS (
                    SELECT 1
                      FROM cleanup_proposal_member cm
                      JOIN memory m ON m.id = cm.memory_id
                     WHERE cm.proposal_id = p.id
                       AND NOT EXISTS (
                             SELECT 1
                               FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
                              WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                                         ELSE left(m.namespace, length(g.prefix)) = g.prefix END
                                AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
                           )
                  )"#;

const LIST_SQL: &str = r#"
            SELECT p.id, p.kind, p.namespace, p.keep_id, p.rationale, p.produced_by, p.posted_by,
                   p.similarity, p.state, p.reason, p.decided_at, p.created_at
              FROM cleanup_proposal p
             WHERE p.tenant_id = $1 AND ($2::text IS NULL OR p.state = $2)
              AND EXISTS (
                    SELECT 1 FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
                     WHERE CASE WHEN g.exact THEN p.namespace = g.prefix
                                ELSE left(p.namespace, length(g.prefix)) = g.prefix END
                  )
              AND NOT EXISTS (
                    SELECT 1
                      FROM cleanup_proposal_member cm
                      JOIN memory m ON m.id = cm.memory_id
                     WHERE cm.proposal_id = p.id
                       AND NOT EXISTS (
                             SELECT 1
                               FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
                              WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                                         ELSE left(m.namespace, length(g.prefix)) = g.prefix END
                                AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
                           )
                  )
             ORDER BY p.created_at DESC
             LIMIT $6
            "#;

const GET_SQL: &str = r#"
            SELECT p.id, p.kind, p.namespace, p.keep_id, p.rationale, p.produced_by, p.posted_by,
                   p.similarity, p.state, p.reason, p.decided_at, p.created_at
              FROM cleanup_proposal p
             WHERE p.tenant_id = $1 AND p.id = $2
              AND EXISTS (
                    SELECT 1 FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
                     WHERE CASE WHEN g.exact THEN p.namespace = g.prefix
                                ELSE left(p.namespace, length(g.prefix)) = g.prefix END
                  )
              AND NOT EXISTS (
                    SELECT 1
                      FROM cleanup_proposal_member cm
                      JOIN memory m ON m.id = cm.memory_id
                     WHERE cm.proposal_id = p.id
                       AND NOT EXISTS (
                             SELECT 1
                               FROM unnest($3::text[], $4::bool[], $5::text[]) AS g(prefix, exact, max)
                              WHERE CASE WHEN g.exact THEN m.namespace = g.prefix
                                         ELSE left(m.namespace, length(g.prefix)) = g.prefix END
                                AND sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)
                           )
                  )
            "#;

/// A grant as three parallel arrays: the text to compare, whether the comparison is exact, and the
/// ceiling that pattern carries. `namespaces::matches` rewritten as something Postgres can compare:
/// `*` is the empty prefix, a trailing star is a prefix match on what precedes it, anything else is
/// the whole name. Trimmed and lowercased for the reason `matches` does it.
///
/// The memory adapter has its own copy for the timeline walk. Shared with the ingest adapter,
/// which applies the same grant to the proposal queue and the emission lookup.
pub(crate) fn grant_arrays(grants: &[NamespaceGrant]) -> (Vec<String>, Vec<bool>, Vec<String>) {
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

/// Live dated rows carrying one tag, oldest first by valid time.
///
/// `ORDER BY occurred_at` and never `created_at`. A July fact approved in August is older than an
/// August fact approved in July, and ordering by the transaction clock would propose the newer fact
/// as the one that was ended.
///
/// **Newest first, then reversed by the caller.** The limit has to bite at the old end. Taking the
/// oldest N would drop the newest facts, and the newest fact on a single-valued subject is the one
/// that holds today: it would never be proposed as anything's successor, and the subject's real
/// current value would be the one row the pass could not see. The caller reverses into
/// oldest-first order and reports that the window truncated.
///
/// Both grant axes, the same shape every other candidate query uses. The pass runs as the whole
/// tenant and an HTTP caller runs as itself; neither spelling skips the check.
const TAGGED_DATED_SQL: &str = concat!(
    r#"
    WITH granted AS (
        SELECT prefix, exact, sensitivity_rank(max) AS max_rank
          FROM unnest($2::text[], $3::bool[], $4::text[]) AS g(prefix, exact, max)
    )
    SELECT m.id, m.namespace, m.sensitivity, m.content, m.created_at, m.access_count,
           m.occurred_at
      FROM memory m
     WHERE m.tenant_id = $1
       AND "#,
    live!(),
    r#"
       AND m.occurred_at IS NOT NULL
       AND $5 = ANY(m.tags)
       AND sensitivity_rank(m.sensitivity) <= sensitivity_rank($6)
       AND EXISTS (
             SELECT 1 FROM granted g
              WHERE CASE WHEN g.exact
                         THEN m.namespace = g.prefix
                         ELSE left(m.namespace, length(g.prefix)) = g.prefix
                    END
                AND sensitivity_rank(m.sensitivity) <= g.max_rank
           )
     ORDER BY m.occurred_at DESC, m.id DESC
     LIMIT $7
"#
);

fn candidate_from(row: &sqlx::postgres::PgRow, prefix: &str) -> Result<Candidate> {
    let id: Uuid = row.try_get(format!("{prefix}id").as_str()).map_err(map_err)?;
    let sensitivity: String =
        row.try_get(format!("{prefix}sensitivity").as_str()).map_err(map_err)?;
    Ok(Candidate {
        id: id.to_string(),
        namespace: row.try_get(format!("{prefix}namespace").as_str()).map_err(map_err)?,
        sensitivity: parse_sensitivity(&sensitivity)?,
        content: row.try_get(format!("{prefix}content").as_str()).map_err(map_err)?,
        created_at: row.try_get(format!("{prefix}created_at").as_str()).map_err(map_err)?,
        access_count: row.try_get(format!("{prefix}access_count").as_str()).map_err(map_err)?,
        // Tolerated rather than required. Every other candidate query predates valid time and does
        // not select this column; only supersession needs it, and making it mandatory would mean
        // editing five statements that have no use for it.
        occurred_at: row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(
                format!("{prefix}occurred_at").as_str(),
            )
            .unwrap_or(None),
    })
}

fn parse_sensitivity(s: &str) -> Result<Sensitivity> {
    match s {
        "open" => Ok(Sensitivity::Open),
        "private" => Ok(Sensitivity::Private),
        "sealed" => Ok(Sensitivity::Sealed),
        other => Err(DomainError::internal(format!("unknown sensitivity {other:?} in the store"))),
    }
}

fn map_err(e: sqlx::Error) -> DomainError {
    DomainError::internal(format!("cleanup query failed: {e}"))
}

#[async_trait]
impl CleanupRepository for PgCleanupRepository {
    /// Live dated rows carrying one tag, oldest first by valid time.
    async fn tagged_dated(
        &self,
        tenant: &str,
        q: &CandidateQuery,
        tag: &str,
        limit: i64,
    ) -> Result<Vec<Candidate>> {
        let (prefixes, exact, maxima) = grant_arrays(&q.grant);
        let rows = sqlx::query(TAGGED_DATED_SQL)
            .bind(tenant)
            .bind(&prefixes)
            .bind(&exact)
            .bind(&maxima)
            .bind(tag)
            .bind(q.max_sensitivity.as_str())
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(|r| candidate_from(r, "")).collect()
    }

    /// Declare a subject's arity, replacing any earlier declaration for the tag.
    async fn declare_arity(
        &self,
        tenant: &str,
        tag: &str,
        arity: Arity,
        note: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO subject_cardinality (tenant_id, tag, arity, note)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id, tag)
             DO UPDATE SET arity = EXCLUDED.arity, note = EXCLUDED.note, created_at = now()",
        )
        .bind(tenant)
        .bind(tag)
        .bind(arity.as_str())
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn arities(&self, tenant: &str) -> Result<Vec<Cardinality>> {
        let rows = sqlx::query(
            "SELECT tag, arity, note, created_at FROM subject_cardinality
              WHERE tenant_id = $1 ORDER BY tag",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Cardinality {
                tag: r.get("tag"),
                // An unrecognised value reads as `many`, which is the silent side: it proposes
                // nothing. A row the store cannot parse must not become a licence to retire facts.
                arity: Arity::parse(r.get::<&str, _>("arity")).unwrap_or(Arity::Many),
                note: r.get("note"),
                created_at: r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            })
            .collect())
    }

    async fn forget_arity(&self, tenant: &str, tag: &str) -> Result<bool> {
        let n = sqlx::query("DELETE FROM subject_cardinality WHERE tenant_id = $1 AND tag = $2")
            .bind(tenant)
            .bind(tag)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n == 1)
    }
    async fn exact_duplicates(
        &self,
        tenant: &str,
        q: &CandidateQuery,
    ) -> Result<Vec<Vec<Candidate>>> {
        let (ns, exact) = ns_bounds(q.namespace.as_deref());
        let (g_prefix, g_exact, g_max) = grant_arrays(&q.grant);
        // Normalised the way the fingerprint is: case folded, whitespace collapsed, trimmed. Two
        // rows that differ only in spacing are the same fact typed twice, and a byte comparison
        // would call them distinct and leave the pair to the cosine band, where they arrive with a
        // judgement attached that they do not need.
        let rows = sqlx::query(EXACT_DUPLICATES_SQL)
            .bind(tenant)
            .bind(q.max_sensitivity.as_str())
            .bind(&ns)
            .bind(exact)
            .bind(&g_prefix)
            .bind(&g_exact)
            .bind(&g_max)
            .bind(q.since)
            .bind(q.limit)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;

        let mut groups: Vec<Vec<Candidate>> = Vec::new();
        let mut current_norm: Option<String> = None;
        for row in &rows {
            let norm: String = row.try_get("norm").map_err(map_err)?;
            let cand = candidate_from(row, "")?;
            if current_norm.as_deref() == Some(norm.as_str()) {
                groups.last_mut().expect("a group exists when the norm repeats").push(cand);
            } else {
                current_norm = Some(norm);
                groups.push(vec![cand]);
            }
        }
        // A group cut in half by the LIMIT is a group this run should not act on: proposing to
        // retire two of three identical rows leaves the third behind and the queue says the work
        // is done. Dropped, and the next run picks it up whole.
        groups.retain(|g| g.len() > 1);
        Ok(groups)
    }

    async fn similar_pairs(
        &self,
        tenant: &str,
        q: &CandidateQuery,
        min_similarity: f64,
    ) -> Result<Vec<CandidatePair>> {
        let (ns, exact) = ns_bounds(q.namespace.as_deref());
        let (g_prefix, g_exact, g_max) = grant_arrays(&q.grant);
        // `<=>` is cosine distance, so similarity is 1 minus it. The join is anchored on `a` and
        // open on `b`: a is what changed, b is the whole store. a.id < b.id keeps one row per pair
        // rather than both directions, and the ORDER BY on created_at decides which is the older.
        //
        // No index hint and no HNSW. See the module comment.
        let rows = sqlx::query(SIMILAR_PAIRS_SQL)
            .bind(tenant)
            .bind(q.max_sensitivity.as_str())
            .bind(&ns)
            .bind(exact)
            .bind(&g_prefix)
            .bind(&g_exact)
            .bind(&g_max)
            .bind(q.since)
            .bind(min_similarity)
            .bind(q.limit)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;

        let mut pairs = Vec::with_capacity(rows.len());
        for row in &rows {
            let a = candidate_from(row, "a_")?;
            let b = candidate_from(row, "b_")?;
            let similarity: f64 = row.try_get("similarity").map_err(map_err)?;
            let (older, newer) = if a.created_at <= b.created_at { (a, b) } else { (b, a) };
            pairs.push(CandidatePair { older, newer, similarity });
        }
        Ok(pairs)
    }

    async fn unread(&self, tenant: &str, q: &CandidateQuery, days: i64) -> Result<Vec<Candidate>> {
        let (ns, exact) = ns_bounds(q.namespace.as_deref());
        let (g_prefix, g_exact, g_max) = grant_arrays(&q.grant);
        // No `since` here. Staleness is a property of a row growing older without being read, so
        // the rows that qualify are exactly the ones the window would exclude.
        let rows = sqlx::query(UNREAD_SQL)
            .bind(tenant)
            .bind(q.max_sensitivity.as_str())
            .bind(&ns)
            .bind(exact)
            .bind(&g_prefix)
            .bind(&g_exact)
            .bind(&g_max)
            .bind(i32::try_from(days).unwrap_or(i32::MAX))
            .bind(q.limit)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;
        rows.iter().map(|r| candidate_from(r, "")).collect()
    }

    async fn newest_in_scope(
        &self,
        tenant: &str,
        q: &CandidateQuery,
    ) -> Result<Option<DateTime<Utc>>> {
        let (ns, exact) = ns_bounds(q.namespace.as_deref());
        let (g_prefix, g_exact, g_max) = grant_arrays(&q.grant);
        let newest: Option<DateTime<Utc>> = sqlx::query_scalar(NEWEST_SQL)
            .bind(tenant)
            .bind(q.max_sensitivity.as_str())
            .bind(&ns)
            .bind(exact)
            .bind(&g_prefix)
            .bind(&g_exact)
            .bind(&g_max)
            .fetch_one(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(newest)
    }

    async fn queue(&self, tenant: &str, p: NewProposal) -> Result<(QueueOutcome, String)> {
        let ids: Vec<String> = p.members.iter().map(|m| m.memory_id.clone()).collect();
        let key = cluster_key(p.kind, &ids);

        let mut tx = self.pool.begin().await.map_err(map_err)?;
        // ON CONFLICT DO NOTHING over (tenant_id, cluster_key). An hourly pass finds the same
        // cluster every hour until the owner acts, and every state counts as known, rejected
        // included: re-proposing what he already refused is how a queue stops being read.
        let inserted: Option<Uuid> = sqlx::query_scalar(
            r#"
            INSERT INTO cleanup_proposal
                   (id, tenant_id, kind, namespace, keep_id, rationale, produced_by, similarity,
                    cluster_key, posted_by)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (tenant_id, cluster_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(tenant)
        .bind(p.kind.as_str())
        .bind(&p.namespace)
        .bind(p.keep_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()))
        .bind(&p.rationale)
        .bind(&p.produced_by)
        .bind(p.similarity)
        .bind(&key)
        .bind(p.posted_by.as_deref())
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_err)?;

        let Some(id) = inserted else {
            tx.rollback().await.map_err(map_err)?;
            let existing: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM cleanup_proposal WHERE tenant_id = $1 AND cluster_key = $2",
            )
            .bind(tenant)
            .bind(&key)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;
            return Ok((
                QueueOutcome::AlreadyKnown,
                existing.map(|u| u.to_string()).unwrap_or_default(),
            ));
        };

        for m in &p.members {
            let member_id = Uuid::parse_str(&m.memory_id).map_err(|_| {
                DomainError::validation(format!("{} is not a memory id", m.memory_id))
            })?;
            sqlx::query(
                r#"
                INSERT INTO cleanup_proposal_member
                       (proposal_id, memory_id, disposition, seen_content)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (proposal_id, memory_id) DO NOTHING
                "#,
            )
            .bind(id)
            .bind(member_id)
            .bind(m.disposition.as_str())
            .bind(&m.seen_content)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        }
        tx.commit().await.map_err(map_err)?;
        Ok((QueueOutcome::Queued, id.to_string()))
    }

    async fn list(
        &self,
        tenant: &str,
        state: Option<&str>,
        limit: i64,
        grant: &[NamespaceGrant],
    ) -> Result<Vec<Proposal>> {
        let (g_prefix, g_exact, g_max) = grant_arrays(grant);
        let rows = sqlx::query(LIST_SQL)
            .bind(tenant)
            .bind(state)
            .bind(&g_prefix)
            .bind(&g_exact)
            .bind(&g_max)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(map_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut p = proposal_from(row)?;
            p.members = self.members_of(&p.id).await?;
            out.push(p);
        }
        Ok(out)
    }

    async fn get(
        &self,
        tenant: &str,
        id: &str,
        grant: &[NamespaceGrant],
    ) -> Result<Option<Proposal>> {
        let Ok(uuid) = Uuid::parse_str(id) else { return Ok(None) };
        let (g_prefix, g_exact, g_max) = grant_arrays(grant);
        let row = sqlx::query(GET_SQL)
            .bind(tenant)
            .bind(uuid)
            .bind(&g_prefix)
            .bind(&g_exact)
            .bind(&g_max)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;

        match row {
            None => Ok(None),
            Some(row) => {
                let mut p = proposal_from(&row)?;
                p.members = self.members_of(&p.id).await?;
                Ok(Some(p))
            }
        }
    }

    async fn decide(
        &self,
        tenant: &str,
        id: &str,
        state: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let Ok(uuid) = Uuid::parse_str(id) else { return Ok(false) };
        // `state = 'proposed'` in the predicate rather than checked first. Two callers deciding the
        // same row race otherwise, and the loser silently overwrites the winner's decision.
        let done = sqlx::query(
            r#"
            UPDATE cleanup_proposal
               SET state = $3, reason = $4, decided_at = now()
             WHERE tenant_id = $1 AND id = $2 AND state = 'proposed'
            "#,
        )
        .bind(tenant)
        .bind(uuid)
        .bind(state)
        .bind(reason)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(done.rows_affected() == 1)
    }

    /// The way back out of `rejected`, and the only one short of deleting the row in psql.
    ///
    /// `decided_at` stays where the rejection put it, so the row still carries the fact that
    /// somebody decided it once. The note goes: a finding sitting in the waiting list under a
    /// sentence explaining why it was refused reads as one nobody has answered.
    async fn unreject(&self, tenant: &str, id: &str) -> Result<bool> {
        let Ok(uuid) = Uuid::parse_str(id) else { return Ok(false) };
        // `state = 'rejected'` in the predicate for the reason `decide` puts `proposed` in its own.
        // Two callers racing cannot both win, and a second undo is not a second decision.
        let done = sqlx::query(
            r#"
            UPDATE cleanup_proposal
               SET state = 'proposed', reason = NULL
             WHERE tenant_id = $1 AND id = $2 AND state = 'rejected'
            "#,
        )
        .bind(tenant)
        .bind(uuid)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(done.rows_affected() == 1)
    }

    async fn close_answered(&self, tenant: &str) -> Result<Vec<String>> {
        // Three ways the store answers a finding by itself: a member is gone, a member was retired
        // by something else, or a member's text is not what the pass read. Apply refuses all three,
        // so leaving them queued costs the owner a read and tells him nothing.
        let ids: Vec<Uuid> = sqlx::query_scalar(
            r#"
            UPDATE cleanup_proposal p
               SET state = 'obsolete',
                   reason = 'the store answered this on its own',
                   decided_at = now()
             WHERE p.tenant_id = $1
               AND p.state = 'proposed'
               AND EXISTS (
                     SELECT 1
                       FROM cleanup_proposal_member cm
                       LEFT JOIN memory m ON m.id = cm.memory_id
                      WHERE cm.proposal_id = p.id
                        AND (m.id IS NULL
                             OR m.superseded_by IS NOT NULL
                             OR m.content IS DISTINCT FROM cm.seen_content)
                   )
            RETURNING p.id
            "#,
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(ids.into_iter().map(|u| u.to_string()).collect())
    }

    async fn valid_times(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<(String, Option<DateTime<Utc>>)>> {
        let uuids: Vec<Uuid> = ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect();
        let rows =
            sqlx::query("SELECT id, occurred_at FROM memory WHERE tenant_id = $1 AND id = ANY($2)")
                .bind(tenant)
                .bind(&uuids)
                .fetch_all(&self.pool)
                .await
                .map_err(map_err)?;
        let mut found: std::collections::HashMap<String, Option<DateTime<Utc>>> =
            std::collections::HashMap::new();
        for row in &rows {
            let id: Uuid = row.try_get("id").map_err(map_err)?;
            found.insert(id.to_string(), row.try_get("occurred_at").map_err(map_err)?);
        }
        // In the order asked for, and every id present. A caller reconciling an ordering cannot
        // work from a list that silently dropped the row it was asking about.
        Ok(ids.iter().map(|id| (id.clone(), found.get(id).copied().flatten())).collect())
    }

    async fn watermark(
        &self,
        tenant: &str,
        scope: &str,
        cadence: &str,
    ) -> Result<Option<Watermark>> {
        let row = sqlx::query(
            r#"
            SELECT last_run_at, through FROM cleanup_watermark
             WHERE tenant_id = $1 AND scope = $2 AND cadence = $3
            "#,
        )
        .bind(tenant)
        .bind(scope)
        .bind(cadence)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)?;
        match row {
            None => Ok(None),
            Some(row) => Ok(Some(Watermark {
                last_run_at: row.try_get("last_run_at").map_err(map_err)?,
                through: row.try_get("through").map_err(map_err)?,
            })),
        }
    }

    async fn advance(
        &self,
        tenant: &str,
        scope: &str,
        cadence: &str,
        through: DateTime<Utc>,
    ) -> Result<()> {
        // GREATEST on `through` so a re-run over an older window cannot walk the mark backwards and
        // re-propose everything between.
        sqlx::query(
            r#"
            INSERT INTO cleanup_watermark (tenant_id, scope, cadence, last_run_at, through)
            VALUES ($1, $2, $3, now(), $4)
            ON CONFLICT (tenant_id, scope, cadence) DO UPDATE
               SET last_run_at = now(),
                   through = GREATEST(cleanup_watermark.through, EXCLUDED.through)
            "#,
        )
        .bind(tenant)
        .bind(scope)
        .bind(cadence)
        .bind(through)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }
}

impl PgCleanupRepository {
    async fn members_of(&self, proposal_id: &str) -> Result<Vec<Member>> {
        let Ok(uuid) = Uuid::parse_str(proposal_id) else { return Ok(Vec::new()) };
        let rows = sqlx::query(
            r#"
            SELECT cm.memory_id, cm.disposition, cm.seen_content,
                   m.content AS current_content, m.superseded_by, m.namespace, m.sensitivity,
                   m.created_at, m.occurred_at, m.access_count, m.source_client
              FROM cleanup_proposal_member cm
              LEFT JOIN memory m ON m.id = cm.memory_id
             WHERE cm.proposal_id = $1
             ORDER BY cm.disposition DESC, cm.memory_id
            "#,
        )
        .bind(uuid)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let memory_id: Uuid = row.try_get("memory_id").map_err(map_err)?;
            let disposition: String = row.try_get("disposition").map_err(map_err)?;
            let superseded_by: Option<Uuid> = row.try_get("superseded_by").map_err(map_err)?;
            let sensitivity: Option<String> = row.try_get("sensitivity").map_err(map_err)?;
            out.push(Member {
                memory_id: memory_id.to_string(),
                disposition: Disposition::parse(&disposition).ok_or_else(|| {
                    DomainError::internal(format!("unknown disposition {disposition:?}"))
                })?,
                seen_content: row.try_get("seen_content").map_err(map_err)?,
                current_content: row.try_get("current_content").map_err(map_err)?,
                superseded_by: superseded_by.map(|u| u.to_string()),
                namespace: row.try_get("namespace").map_err(map_err)?,
                sensitivity: sensitivity.as_deref().map(parse_sensitivity).transpose()?,
                created_at: row.try_get("created_at").map_err(map_err)?,
                occurred_at: row.try_get("occurred_at").map_err(map_err)?,
                access_count: row.try_get("access_count").map_err(map_err)?,
                source_client: row.try_get("source_client").map_err(map_err)?,
            });
        }
        Ok(out)
    }
}

fn proposal_from(row: &sqlx::postgres::PgRow) -> Result<Proposal> {
    let id: Uuid = row.try_get("id").map_err(map_err)?;
    let keep_id: Option<Uuid> = row.try_get("keep_id").map_err(map_err)?;
    let kind: String = row.try_get("kind").map_err(map_err)?;
    Ok(Proposal {
        id: id.to_string(),
        kind: CleanupKind::parse(&kind)
            .ok_or_else(|| DomainError::internal(format!("unknown cleanup kind {kind:?}")))?,
        namespace: row.try_get("namespace").map_err(map_err)?,
        keep_id: keep_id.map(|u| u.to_string()),
        rationale: row.try_get("rationale").map_err(map_err)?,
        produced_by: row.try_get("produced_by").map_err(map_err)?,
        posted_by: row.try_get("posted_by").map_err(map_err)?,
        similarity: row.try_get("similarity").map_err(map_err)?,
        state: row.try_get("state").map_err(map_err)?,
        reason: row.try_get("reason").map_err(map_err)?,
        decided_at: row.try_get("decided_at").map_err(map_err)?,
        created_at: row.try_get("created_at").map_err(map_err)?,
        members: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_namespace_matches_only_itself() {
        assert_eq!(ns_bounds(Some("project:lumberroom")), ("project:lumberroom".to_string(), true));
    }

    #[test]
    fn a_trailing_star_becomes_a_prefix() {
        assert_eq!(ns_bounds(Some("project:*")), ("project:".to_string(), false));
    }

    #[test]
    fn no_namespace_means_no_scope_narrowing_and_the_grant_still_applies() {
        // The empty pattern is what the SQL reads as "no scope filter". It is not "every
        // namespace": the grant predicate runs beside it in every query, and a test below holds
        // that. A prefix match on "" would also match everything, but only by accident, and the
        // predicate says so explicitly.
        assert_eq!(ns_bounds(None), (String::new(), false));
    }

    #[test]
    fn a_grant_becomes_three_arrays_in_the_same_order() {
        let grants = vec![
            NamespaceGrant::new("*", Sensitivity::Sealed),
            NamespaceGrant::new("project:*", Sensitivity::Open),
            NamespaceGrant::new(" User:Me ", Sensitivity::Private),
        ];
        let (prefix, exact, max) = grant_arrays(&grants);
        assert_eq!(prefix, vec!["", "project:", "user:me"]);
        assert_eq!(exact, vec![false, false, true]);
        assert_eq!(max, vec!["sealed", "open", "private"]);
    }

    #[test]
    fn an_empty_grant_becomes_empty_arrays_that_admit_nothing() {
        // `EXISTS` over an empty `unnest` is false, so a caller granted nothing reads nothing.
        // The arrays have to be empty rather than absent for that to hold.
        let (prefix, exact, max) = grant_arrays(&[]);
        assert!(prefix.is_empty() && exact.is_empty() && max.is_empty());
    }

    #[test]
    fn a_glob_is_trimmed_and_folded_the_way_a_grant_is() {
        assert_eq!(
            ns_bounds(Some("  Project:Lumberroom  ")),
            ("project:lumberroom".to_string(), true)
        );
    }

    #[test]
    fn every_candidate_query_carries_the_same_scope_and_grant_predicates() {
        // Four copies of the filter that decides what a caller may see. One edited and three not
        // is the failure this catches, and it is silent: the pass keeps working and starts
        // reading rows it should not.
        for (name, sql) in [
            ("exact_duplicates", EXACT_DUPLICATES_SQL),
            ("similar_pairs", SIMILAR_PAIRS_SQL),
            ("unread", UNREAD_SQL),
            ("newest_in_scope", NEWEST_SQL),
        ] {
            assert!(sql.contains(IN_SCOPE_PREDICATE), "{name} lost the namespace scope predicate");
            assert!(
                sql.contains(GRANT_PREDICATE),
                "{name} lost the grant predicate, which is what keeps a caller's run inside its \
                 own namespaces"
            );
            assert!(
                sql.contains(live!()),
                "{name} lost the live test, so it would read retired or expired rows"
            );
            assert!(sql.contains("m.content IS NOT NULL"), "{name} would read sealed rows");
            assert!(
                sql.contains("WHEN 'open' THEN m.sensitivity = 'open'"),
                "{name} lost the sensitivity ceiling, which is what keeps a private row away from \
                 a model"
            );
        }
    }

    #[test]
    fn the_similar_pairs_query_never_reaches_for_the_index() {
        // Filtered HNSW returned zero rows against 40,000 here once. A pass that answers "nothing
        // to clean up" because an index truncated is the same failure wearing a different hat.
        assert!(
            !SIMILAR_PAIRS_SQL.contains("ORDER BY a.embedding <=>"),
            "an ORDER BY on the operator is what makes the planner choose HNSW"
        );
        assert!(SIMILAR_PAIRS_SQL.contains("ORDER BY similarity DESC"));
    }

    #[test]
    fn the_pair_join_takes_each_pair_once() {
        // Without a.id < b.id every pair arrives twice, in both directions, and the cluster key
        // hides the second while the limit silently halves the useful result.
        assert!(SIMILAR_PAIRS_SQL.contains("a.id < b.id"));
    }

    #[test]
    fn the_pair_join_stays_inside_one_namespace() {
        // A pair across two namespaces produced a proposal whose `namespace` named one of them,
        // and the queue's visibility check read only that field.
        assert!(SIMILAR_PAIRS_SQL.contains("b.namespace = a.namespace"));
    }

    #[test]
    fn both_queue_reads_refuse_a_proposal_with_a_member_outside_the_grant() {
        // One string for the rule, so the two reads cannot drift. The member check has to be a
        // NOT EXISTS over the members at their stored level rather than a test on the proposal's
        // own namespace, which the poster chose.
        for (name, sql) in [("list", LIST_SQL), ("get", GET_SQL)] {
            assert!(
                sql.contains(PROPOSAL_VISIBLE_PREDICATE.trim()),
                "{name} lost the visibility rule"
            );
        }
        assert!(PROPOSAL_VISIBLE_PREDICATE.contains("JOIN memory m ON m.id = cm.memory_id"));
        assert!(PROPOSAL_VISIBLE_PREDICATE
            .contains("sensitivity_rank(m.sensitivity) <= sensitivity_rank(g.max)"));
    }

    #[test]
    fn the_similar_pairs_window_leaves_one_side_open() {
        // The trap: filtering both sides makes the pass blind to a new row restating an old fact,
        // which is the most common duplicate there is.
        assert!(
            SIMILAR_PAIRS_SQL.contains("a.created_at >= $8 OR b.created_at >= $8"),
            "the window has to admit a pair where only one side is new"
        );
    }

    /// The cleanup pass reads live rows, so it answers both clocks the way every other live
    /// reader does. Decision 0017 carries the reasoning.
    #[test]
    fn the_unread_scan_skips_a_row_whose_period_has_closed() {
        assert!(UNREAD_SQL.contains("m.superseded_by IS NULL"));
        assert!(UNREAD_SQL.contains("m.occurred_until IS NULL OR m.occurred_until > now()"));
    }

    #[test]
    fn staleness_does_not_take_the_window() {
        // A stale row qualifies by being old and unread, so a window over recent rows excludes
        // exactly the rows this query is looking for.
        assert!(!UNREAD_SQL.contains("::timestamptz"));
        assert!(UNREAD_SQL.contains("m.access_count = 0"));
    }
}
