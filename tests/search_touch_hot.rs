//! Two costs a read used to pay for nothing, pinned against a real Postgres.
//!
//! Issue 72: the touch after every search updated each returned row outside HOT, so one read wrote
//! a new entry into every index on `memory`, the HNSW graph included. Issue 75: at a lexical weight
//! of zero the full-text arm still ran its GIN scan and `ts_rank` for a score multiplied by zero.
//!
//! Each test builds its own database from scratch. A fresh database gives fresh heap pages, which is
//! what makes the HOT count exact rather than a ratio, and it keeps these tests away from the
//! shared `lumberroom_rust_test`, which the integration suite truncates.
//!
//! Skipped when `DATABASE_URL` is unset or unreachable. A count of zero here is not a green run.

use std::time::{Duration, Instant};

use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{Fusion, SearchConfig, DEFAULT_RRF_K};
use lumberroom_server::domain::policy::NamespaceCeiling;
use lumberroom_server::domain::routing::{Scale, Thresholds};
use lumberroom_server::domain::types::Sensitivity;
use lumberroom_server::ports::{MemoryRepository, SearchQuery, Weights};
use sqlx::{AssertSqlSafe, PgPool};

const TENANT: &str = "touch_hot";
const DIMS: usize = 768;

/// A migrated database of its own, dropped first if a crashed run left it behind.
async fn fresh(name: &str) -> Option<(PgPool, PgPool)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let base = url.rsplit_once('/')?.0.to_string();
    let admin = PgPool::connect(&url).await.ok()?;
    // DDL takes no bind parameter. Audited: every caller passes a compile-time literal.
    sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
        .execute(&admin)
        .await
        .ok()?;
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&admin).await.ok()?;
    let pool = postgres::connect(&format!("{base}/{name}")).await.ok()?;
    postgres::migrate(&pool).await.expect("migrations apply to an empty database");
    Some((pool, admin))
}

async fn drop_db(pool: PgPool, admin: PgPool, name: &str) {
    pool.close().await;
    let _ = sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
        .execute(&admin)
        .await;
}

/// One open row in `project:alpha`. `embedded` false leaves the vector arm unable to see it, so
/// the lexical arm is the only way it can reach a result.
async fn insert(pool: &PgPool, content: &str, embedded: bool) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let embedding = embedded.then(|| pgvector::Vector::from(vec![1.0f32; DIMS]));
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, embedding, source_client)
         VALUES ($1, $2, 'project:alpha', $3, $4, 'touch-hot-test')",
    )
    .bind(id)
    .bind(TENANT)
    .bind(content)
    .bind(embedding)
    .execute(pool)
    .await
    .expect("insert");
    id
}

fn search_config(fusion: Fusion) -> SearchConfig {
    SearchConfig {
        default_limit: 8,
        max_limit: 50,
        graph_route: Thresholds { scale: Scale::Cosine, max_top: 0.65, max_spread: 0.08 },
        vector_weight: 1.0,
        lexical_weight: 0.35,
        include_all_projects: true,
        other_project_penalty: 0.85,
        usage_weight: 0.05,
        fusion,
        rrf_k: DEFAULT_RRF_K,
        debug_scores: false,
    }
}

async fn search_ids(repo: &impl MemoryRepository, lexical: f64) -> Vec<String> {
    repo.search(SearchQuery {
        tenant_id: TENANT.into(),
        primary: vec![NamespaceCeiling {
            namespace: "project:alpha".into(),
            max: Sensitivity::Sealed,
        }],
        secondary: vec![],
        embedding: vec![1.0f32; DIMS],
        text: "kestrel".into(),
        limit: 10,
        weights: Weights { vector: 1.0, lexical, secondary_penalty: 0.85, usage: 0.05 },
        include_superseded: false,
        as_of: None,
        tags: vec![],
    })
    .await
    .expect("search")
    .into_iter()
    .map(|h| h.memory.id)
    .collect()
}

/// Weight zero turns the lexical arm off, under every blend. A row only that arm can find drops out
/// of the result at zero and comes back at the default weight, which proves the arm ran nothing
/// rather than scoring a row nobody would rank.
///
/// Fails if the `$10 > 0` guard leaves the lexical arm: the row with no embedding matches
/// "kestrel" and comes back with a score of zero.
#[tokio::test]
async fn a_zero_lexical_weight_skips_the_lexical_arm() {
    const DB: &str = "lexical_off_probe";
    let Some((pool, admin)) = fresh(DB).await else { return };
    let vector_hit = insert(&pool, "the harbour office opens at nine", true).await;
    let lexical_only = insert(&pool, "the kestrel host moved to port 5433", false).await;

    for fusion in [Fusion::Linear, Fusion::Rrf, Fusion::LinearMinmax] {
        let repo =
            postgres::PgMemoryRepository::new(pool.clone()).with_search(&search_config(fusion));

        let off = search_ids(&repo, 0.0).await;
        assert_eq!(off, vec![vector_hit.to_string()], "{fusion:?} at lexical weight 0");

        let on = search_ids(&repo, 0.35).await;
        assert!(
            on.contains(&lexical_only.to_string()),
            "{fusion:?} at lexical weight 0.35 lost the lexical arm: {on:?}"
        );
    }
    drop_db(pool, admin, DB).await;
}

/// No index on `memory` may read the columns the touch writes. Postgres counts a column named in
/// an index predicate as indexed, so one partial index on `last_accessed_at` was enough to send
/// every touch down the non-HOT path. The check reads the catalog, so an index the cloud fork adds
/// fails here too.
#[tokio::test]
async fn no_index_on_memory_reads_the_access_columns() {
    const DB: &str = "touch_hot_catalog_probe";
    let Some((pool, admin)) = fresh(DB).await else { return };
    let offending: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_indexdef(i.indexrelid)
           FROM pg_index i
          WHERE i.indrelid = 'memory'::regclass
            AND pg_get_indexdef(i.indexrelid) ~ '\\m(last_accessed_at|access_count)\\M'",
    )
    .fetch_all(&pool)
    .await
    .expect("catalog read");
    assert!(offending.is_empty(), "these indexes make every touch a non-HOT update: {offending:?}");
    drop_db(pool, admin, DB).await;
}

/// The repository's own touch, measured from `pg_stat_user_tables`.
///
/// The touch runs on a spawned task and a backend flushes its table counters about a second after
/// it commits, so the test polls rather than reading once. Fails if any index reads
/// `last_accessed_at` or `access_count`: the update count reaches three and the HOT count stays
/// at zero.
#[tokio::test]
async fn the_search_touch_is_a_hot_update() {
    const DB: &str = "touch_hot_probe";
    let Some((pool, admin)) = fresh(DB).await else { return };
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(insert(&pool, &format!("touched row {i}"), true).await);
    }
    let repo = postgres::PgMemoryRepository::new(pool.clone());
    repo.touch_accessed(TENANT, ids.clone());

    let deadline = Instant::now() + Duration::from_secs(20);
    let (updated, hot) = loop {
        let (updated, hot): (i64, i64) = sqlx::query_as(
            "SELECT n_tup_upd, n_tup_hot_upd FROM pg_stat_user_tables WHERE relname = 'memory'",
        )
        .fetch_one(&pool)
        .await
        .expect("stats read");
        if updated >= ids.len() as i64 || Instant::now() > deadline {
            break (updated, hot);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };

    assert_eq!(updated, ids.len() as i64, "the touch did not land within the deadline");
    assert_eq!(hot, updated, "{updated} touches, {hot} of them HOT");
    drop_db(pool, admin, DB).await;
}
