//! Recall monitoring.
//!
//! HNSW is approximate and no published figure covers recall at this scale, on this hardware,
//! with this model. At tens of thousands of rows an exact scan is cheap, so ground truth is
//! always available and the honest answer is to measure rather than to trust someone else's
//! benchmark. A memory system that silently fails to return a fact it holds is worse than a slow
//! one, which is why this exists.
//!
//! Read the limits before quoting a figure from here. `nearest_ids` filters on nothing but the
//! tenant, so this measures the unfiltered index while every search a client runs filters by
//! namespace, sensitivity and supersession, which is where HNSW truncates. Probes are stored rows
//! queried with their own text, so a figure describes self-retrieval and not the retrieval a
//! question gets. `docs/research/recall-monitor.md` carries the measured numbers, the corpus they
//! came from, and the shape of the failure this does not cover.

use serde::Serialize;

use super::Ctx;
use crate::adapters::auth::filter_readable;
use crate::domain::errors::Result;

#[derive(Debug, Serialize)]
pub struct RecallReport {
    pub sampled: usize,
    pub k: i64,
    /// Mean fraction of the exact top-k that the indexed search also returned.
    pub recall_at_k: f64,
    /// Queries where the true nearest neighbour was missed entirely. The number that matters.
    pub top_one_misses: usize,
    pub worst: Vec<WorstCase>,
    pub index_ms: u128,
    pub exact_ms: u128,
    /// `exact_ms / index_ms`, and the field to read before `recall_at_k`.
    ///
    /// Both arms issue the same statement and only the second forbids an index scan, so nothing
    /// makes the planner use HNSW in the first. When it declines, the two arms run the same
    /// sequential scan, recall comes out at 1.0, and the report is a self-comparison rather than a
    /// measurement. That has now happened twice: once through `SET LOCAL` outside a transaction,
    /// once through the planner. This ratio is the only tell.
    ///
    /// Measured at 40,001 rows: `k=10` runs land between 9 and 16. A `k=1` run lands at 1.00,
    /// because the planner costs a sequential scan below an HNSW scan for a single row and takes
    /// it, and `pg_stat_user_indexes` confirms the index went untouched for every probe. Below
    /// roughly 2, the run measured nothing. `None` when the indexed arm finished inside the clock's
    /// resolution, which a small store does.
    pub exact_speedup: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct WorstCase {
    pub query: String,
    pub recall: f64,
}

/// Uses stored content as its own query on purpose: a memory should always retrieve itself, so a
/// miss is unambiguous rather than a matter of judgement.
///
/// The probes are the caller's to read, and that is a policy boundary rather than a nicety.
/// `WorstCase::query` publishes the opening characters of what was sampled, and `/admin/recall`
/// takes a bearer token and asks for no scope beyond it, so a tenant-wide sample handed a client
/// granted `global` the plaintext of open rows in every namespace its grant excludes. The report is
/// narrower for a narrow grant, and says so through `sampled`.
pub async fn measure(ctx: &Ctx, sample: i64, k: i64) -> Result<RecallReport> {
    // Discovery first, because a glob grant resolves against namespaces that exist. The counts that
    // come back carry no ceiling and are dropped; only the names go on to the grant.
    let mut existing: Vec<String> =
        ctx.repos.memories.namespace_counts(&ctx.cfg.tenant_id).await?.into_keys().collect();
    existing.sort();
    let readable = filter_readable(&ctx.principal, &existing);
    let probes = ctx.repos.memories.sample_content(&ctx.cfg.tenant_id, &readable, sample).await?;
    if probes.is_empty() {
        return Ok(RecallReport {
            sampled: 0,
            k,
            recall_at_k: 1.0,
            top_one_misses: 0,
            worst: vec![],
            index_ms: 0,
            exact_ms: 0,
            exact_speedup: None,
        });
    }

    let u = ctx.embedders.for_unit(ctx.tenant())?;
    let vectors = u.embedder.embed_documents(probes.clone()).await?;
    let mut scores: Vec<WorstCase> = Vec::with_capacity(probes.len());
    let mut top_one_misses = 0usize;
    let mut index_ms = 0u128;
    let mut exact_ms = 0u128;

    // Both `nearest_ids` arms stay tenant-wide. The index is what is being measured and a
    // per-grant candidate set would measure a different index than the one a search hits. They
    // return ids, ids never reach the report, and the two arms have to see the same rows or the
    // comparison means nothing.
    for (probe, vector) in probes.iter().zip(vectors.iter()) {
        let t = std::time::Instant::now();
        let indexed =
            ctx.repos.memories.nearest_ids(&ctx.cfg.tenant_id, vector, k, false, u.slot).await?;
        index_ms += t.elapsed().as_millis();

        let t = std::time::Instant::now();
        let exact =
            ctx.repos.memories.nearest_ids(&ctx.cfg.tenant_id, vector, k, true, u.slot).await?;
        exact_ms += t.elapsed().as_millis();

        let got: std::collections::HashSet<&String> = indexed.iter().collect();
        let hits = exact.iter().filter(|id| got.contains(id)).count();
        let recall = if exact.is_empty() { 1.0 } else { hits as f64 / exact.len() as f64 };
        if let Some(first) = exact.first() {
            if !got.contains(first) {
                top_one_misses += 1;
            }
        }
        scores.push(WorstCase { query: probe.chars().take(60).collect(), recall });
    }

    let mean = scores.iter().map(|s| s.recall).sum::<f64>() / scores.len() as f64;
    scores.sort_by(|a, b| a.recall.partial_cmp(&b.recall).unwrap_or(std::cmp::Ordering::Equal));

    Ok(RecallReport {
        sampled: scores.len(),
        k,
        recall_at_k: (mean * 10_000.0).round() / 10_000.0,
        top_one_misses,
        worst: scores.into_iter().take(5).collect(),
        index_ms,
        exact_ms,
        exact_speedup: (index_ms > 0)
            .then(|| ((exact_ms as f64 / index_ms as f64) * 100.0).round() / 100.0),
    })
}
