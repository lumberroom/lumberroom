//! The Postgres adapter. The only module in the service that contains SQL.

/// What "the store holds this now" means, in one place, for every reader that asks it.
///
/// Two clauses, and the second is the one decision 0017 added. `superseded_by IS NULL` is
/// transaction time: no later row replaced this one. The period test is valid time: nobody closed
/// the fact's validity without a replacement, which is what `review::expire` writes.
///
/// A conjunct rather than a rewrite, which is what keeps the `memory_live` partial index from
/// migration 005. The index predicate is the first clause and this predicate implies it, so the
/// planner can still prove the index applies and the period test becomes a filter above the scan.
/// The trap `memory.rs` records at `($n OR superseded_by IS NULL)` is the opposite shape: weaker
/// than the index predicate, so unprovable.
///
/// Defined here so the readers in `memory` and in `cleanup` take the same text. A statement whose
/// alias is not `m` spells the two clauses out and names this macro in a comment.
macro_rules! live {
    () => {
        "m.superseded_by IS NULL AND (m.occurred_until IS NULL OR m.occurred_until > now())"
    };
}

mod alias;
mod cleanup;
mod ingest;
mod memory;
mod oauth;
mod registry;
mod sealed;
mod tool_calls;

pub use alias::PgAliasRepository;
/// The one glob-to-SQL translation. Any statement that filters on a caller's grants binds these
/// three arrays, so a second translation cannot drift from the first.
pub(crate) use cleanup::grant_arrays;
pub use cleanup::PgCleanupRepository;
pub use ingest::PgIngestRepository;
pub use memory::PgMemoryRepository;
pub use oauth::PgOauthStore;
pub use registry::PgRegistryRepository;
pub use sealed::PgSealedRepository;
pub use tool_calls::PgToolCallRepository;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::Duration;

use crate::domain::errors::{DomainError, Result};
use crate::ports::{
    CleanupRepository, IngestRepository, MemoryRepository, OauthStore, RegistryRepository,
    SealedRepository, ToolCallRepository,
};

#[derive(Clone)]
pub struct Repositories {
    pub memories: Arc<dyn MemoryRepository>,
    pub registry: Arc<dyn RegistryRepository>,
    pub tool_calls: Arc<dyn ToolCallRepository>,
    pub sealed: Arc<dyn SealedRepository>,
    pub oauth: Arc<dyn OauthStore>,
    /// Held beside the rest rather than inside `services::Repos`: ingestion is an operator surface
    /// with no tool behind it, and the services that use it take the port as an argument.
    pub ingest: Arc<dyn IngestRepository>,
    /// Same reasoning as `ingest`, and the same shape: a queue an operator reads, no tool, and the
    /// service takes the port as an argument.
    pub cleanup: Arc<dyn CleanupRepository>,
}

pub async fn connect(database_url: &str) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        // A pathological query must not hold a connection until the client gives up.
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET statement_timeout = '30s'").execute(conn).await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .map_err(|e| DomainError::unavailable("cannot reach the database").with_source(e))
}

pub fn repositories(pool: &PgPool, search: &crate::config::SearchConfig) -> Repositories {
    Repositories {
        memories: Arc::new(PgMemoryRepository::new(pool.clone()).with_search(search)),
        registry: Arc::new(PgRegistryRepository::new(pool.clone())),
        tool_calls: Arc::new(PgToolCallRepository::new(pool.clone())),
        sealed: Arc::new(PgSealedRepository::new(pool.clone())),
        oauth: Arc::new(PgOauthStore::new(pool.clone())),
        ingest: Arc::new(PgIngestRepository::new(pool.clone())),
        cleanup: Arc::new(PgCleanupRepository::new(pool.clone())),
    }
}

/// Migrate on a connection the pool will never see again.
///
/// sqlx takes a session-level advisory lock at the top of `Migrator::run`, and there are seven early
/// returns between that and the unlock: `Dirty`, `VersionMismatch`, the missing-migration check and
/// four `?` propagations. A failure therefore leaves the lock held.
///
/// Run on a pooled connection, that connection then returns to the pool still holding it, and the
/// next attempt takes a different one and blocks on `pg_advisory_lock` forever instead of reporting
/// the error that caused the first failure. Two processes starting against a database whose
/// migrations fail will hang rather than say why.
///
/// Measured on sqlx 0.9 against Postgres 16: one advisory lock still held after a failed migrate
/// returned its connection to the pool.
///
/// `detach` takes the connection out of the pool for good, so closing it releases the lock whatever
/// happened. `DISCARD ALL` on release would also release it and would break sqlx's prepared
/// statement cache with `26000 prepared statement does not exist`, so detaching is both cheaper and
/// local to the one place that needs it.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    use sqlx::Connection;

    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| {
            DomainError::internal("could not acquire a connection to migrate").with_source(e)
        })?
        .detach();

    let result = sqlx::migrate!("./migrations").run(&mut conn).await;

    // Before the `?`, always. This is the line that stops a failed migration wedging the next one.
    let _ = conn.close().await;

    result.map_err(|e| DomainError::internal("migration failed").with_source(e))?;
    Ok(())
}

/// What the boot check found in `kek_state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KekCheck {
    /// Nothing was recorded, and this key's id and fingerprint now are. First boot with encryption
    /// configured, and the only state in which an unrecognised key is not a problem.
    Recorded,
    /// The recorded fingerprint is this key's. Encrypted rows already written can be read.
    Matches,
    /// A different key. Every row already sealed was sealed under the recorded one, so encrypted
    /// writes have to stay refused: writing under this key would produce rows nobody can read and
    /// leave the store holding two families of ciphertext with one label.
    Mismatch { recorded_kek_id: String },
}

/// Compare the live KEK against the one this store was built with, and record it on first sight.
///
/// Step 4 of the Phase 3 migration order is the one that can strand data, which is why it is a
/// stored row rather than a note in a runbook: the fingerprint is an HMAC of a fixed label under the
/// key, so it identifies the key without being derived from it in a way that helps an attacker.
///
/// The comparison is a plain equality. The fingerprint is computed locally and compared against a
/// local row with no attacker-supplied input on either side, so there is no secret-dependent timing
/// to hide here, unlike the presented-token comparison in `adapters::auth::token`.
///
/// SQL for the KEK state lives in this module because all SQL lives in this module. The composition
/// root calls this at boot and sets `services::Ctx::kek_verified` from the answer.
pub async fn verify_kek(
    pool: &PgPool,
    tenant: &str,
    kek_id: &str,
    fingerprint: &str,
    provider: &str,
) -> Result<KekCheck> {
    let recorded = sqlx::query("SELECT kek_id, fingerprint FROM kek_state WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_optional(pool)
        .await?;

    let Some(row) = recorded else {
        // Boot is single-process, so this is not a race in practice. ON CONFLICT DO NOTHING keeps
        // it from being one anyway, and a losing insert falls through to the next boot's compare.
        sqlx::query(
            "INSERT INTO kek_state (tenant_id, kek_id, fingerprint, provider)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id) DO NOTHING",
        )
        .bind(tenant)
        .bind(kek_id)
        .bind(fingerprint)
        .bind(provider)
        .execute(pool)
        .await?;
        return Ok(KekCheck::Recorded);
    };

    let stored_fingerprint: String = row.get("fingerprint");
    if stored_fingerprint != fingerprint {
        return Ok(KekCheck::Mismatch { recorded_kek_id: row.get("kek_id") });
    }

    // Same key, so the id and provider are labels for it and may have been renamed in config
    // between boots. verified_at moves, which is what makes "when did this last work" answerable.
    sqlx::query(
        "UPDATE kek_state SET kek_id = $2, provider = $3, verified_at = now()
          WHERE tenant_id = $1",
    )
    .bind(tenant)
    .bind(kek_id)
    .bind(provider)
    .execute(pool)
    .await?;
    Ok(KekCheck::Matches)
}

/// The operator-editable classification table, read once at boot.
///
/// Read here rather than through `RegistryRepository`: this is policy, not registry data, and a
/// request-path port is the wrong home for a question asked once per process. It sits beside
/// `verify_kek` for the same reason that one does. The SQL stays in this module because all SQL
/// stays in this module.
///
/// Returned unsorted and unresolved. `SensitivityDefaults::new` orders longest-pattern-first, and
/// `config::resolve_sensitivity_defaults` decides whether these rows win at all.
pub async fn sensitivity_defaults(
    pool: &PgPool,
    tenant: &str,
) -> Result<Vec<(String, crate::domain::types::Sensitivity)>> {
    use crate::domain::types::Sensitivity;

    let rows =
        sqlx::query("SELECT pattern, sensitivity FROM sensitivity_default WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_all(pool)
            .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let pattern: String = row.get("pattern");
        let level: String = row.get("sensitivity");
        match Sensitivity::parse(&level) {
            Some(level) => out.push((pattern, level)),
            // A CHECK constraint on the column forbids this, so reaching it means the constraint was
            // dropped. Skip the row and say so rather than refusing to boot: one unreadable rule
            // must not lock the operator out of the store it classifies.
            None => tracing::warn!(
                pattern = %pattern,
                level = %level,
                "sensitivity_default row has an unknown level; ignoring it"
            ),
        }
    }
    Ok(out)
}

/// Re-apply the recall settings, and say so when they had gone missing.
///
/// `20260819000003_hnsw_recall.sql` sets `hnsw.iterative_scan` and `hnsw.ef_search` with
/// `ALTER DATABASE`, which stores them in `pg_db_role_setting`, a cluster catalog. A
/// single-database `pg_dump` does not carry that catalog, so a restored database comes back with
/// the migration recorded as applied and the settings gone.
///
/// What that costs is in the migration's own comment: with `iterative_scan` off, a query asking for
/// ten rows against a namespace holding 0.5% of the table returned ZERO, having pulled forty
/// candidates and filtered all forty away. No error, no warning. For a memory system, silently
/// answering "nothing is known" about a fact that is present is the worst failure available.
///
/// So this re-applies rather than only checking. `ALTER DATABASE ... SET` is idempotent and cheap,
/// and a restore heals itself on the next boot. Reading `pg_db_role_setting` rather than `SHOW`
/// is deliberate: pgvector registers its GUCs when the library loads, so `SHOW hnsw.iterative_scan`
/// raises `unrecognized configuration parameter` in a session that has not touched a vector yet,
/// which looks exactly like the setting being absent.
///
/// A managed Postgres may refuse `ALTER DATABASE` to the application role. That is a warning with
/// the statement to run by hand, not a refusal to start: the server still answers, and an operator
/// who cannot yet fix it is better off serving with a warning than not serving at all.
pub async fn ensure_recall_settings(pool: &PgPool) -> Result<()> {
    const WANTED: &[(&str, &str)] =
        &[("hnsw.iterative_scan", "strict_order"), ("hnsw.ef_search", "100")];

    let present: Vec<String> = sqlx::query_scalar(
        "SELECT coalesce(unnest, '') FROM (
           SELECT unnest(s.setconfig) FROM pg_db_role_setting s
             JOIN pg_database d ON d.oid = s.setdatabase
            WHERE d.datname = current_database() AND s.setrole = 0
         ) t",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for (name, value) in WANTED {
        let wanted = format!("{name}={value}");
        if present.iter().any(|p| p == &wanted) {
            continue;
        }

        tracing::warn!(
            setting = %name,
            "recall setting missing from this database, re-applying. A restored database loses \
             these because pg_dump of one database does not carry pg_db_role_setting, and without \
             hnsw.iterative_scan a filtered search can return nothing at all rather than fewer rows"
        );

        // The name and value are compile-time constants from WANTED, and the database name comes
        // from the server. format! is the only way to write DDL that names a database, and there is
        // no caller-supplied text anywhere in this statement.
        let stmt = format!(
            "ALTER DATABASE {} SET {name} = '{value}'",
            quote_ident(&current_database(pool).await?)
        );
        if let Err(e) = sqlx::raw_sql(sqlx::AssertSqlSafe(stmt.clone())).execute(pool).await {
            tracing::error!(
                setting = %name,
                error = %e,
                "could not re-apply the recall setting. Run this as a role that owns the database, \
                 then restart: {stmt}"
            );
        }
    }
    Ok(())
}

async fn current_database(pool: &PgPool) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT current_database()").fetch_one(pool).await?)
}

/// Double any embedded quote, the way `quote_ident` does server side.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The embedding column width is fixed in SQL and the embedder is configurable, so a mismatch
/// would produce a confusing error on every write. Fail loudly at boot instead.
pub async fn assert_embedding_dim(pool: &PgPool, expected: usize) -> Result<usize> {
    let row = sqlx::query(
        "SELECT format_type(a.atttypid, a.atttypmod) AS type
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
          WHERE c.relname = 'memory' AND a.attname = 'embedding' AND a.attnum > 0",
    )
    .fetch_optional(pool)
    .await?;

    let ty: String = row
        .ok_or_else(|| {
            DomainError::internal("memory.embedding column not found. Did migrations run?")
        })?
        .get("type");

    let actual: usize =
        ty.trim_start_matches("vector(").trim_end_matches(')').parse().map_err(|_| {
            DomainError::internal(format!("cannot read embedding dimension from {ty:?}"))
        })?;

    if actual != expected {
        return Err(DomainError::internal(format!(
            "embedding dimension mismatch: memory.embedding is {ty} but EMBED_DIM={expected}. \
             Change EMBED_DIM back, or migrate the column."
        )));
    }
    Ok(actual)
}
