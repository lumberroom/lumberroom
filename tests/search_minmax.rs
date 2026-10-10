//! The min-max blend and the debug score columns, against a real Postgres.
//!
//! The unit tests in `adapters/postgres/memory.rs` pin the statement text and a Rust mirror of the
//! rescale. This file runs the statement itself over rows whose cosines are known by construction,
//! so the arithmetic Postgres does is the arithmetic under test.
//!
//! Each test builds its own database, as `search_touch_hot.rs` does, and stays off the shared
//! `lumberroom_rust_test`. Skipped when `DATABASE_URL` is unset or unreachable. A count of zero
//! here is not a green run.

use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{Fusion, SearchConfig, DEFAULT_RRF_K};
use lumberroom_server::domain::policy::NamespaceCeiling;
use lumberroom_server::domain::routing::{Scale, Thresholds};
use lumberroom_server::domain::types::{SearchHit, Sensitivity};
use lumberroom_server::ports::{MemoryRepository, SearchQuery, Weights};
use sqlx::{AssertSqlSafe, PgPool};

const TENANT: &str = "minmax";
const DIMS: usize = 768;

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

/// A unit vector at exactly `cosine` from the query, which is the first axis.
fn at_cosine(cosine: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS];
    v[0] = cosine;
    v[1] = (1.0 - cosine * cosine).sqrt();
    v
}

fn query_vector() -> Vec<f32> {
    at_cosine(1.0)
}

/// One open row. `cosine` None leaves the vector arm unable to see it.
async fn insert(pool: &PgPool, namespace: &str, content: &str, cosine: Option<f32>) -> String {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, embedding, source_client)
         VALUES ($1, $2, $3, $4, $5, 'minmax-test')",
    )
    .bind(id)
    .bind(TENANT)
    .bind(namespace)
    .bind(content)
    .bind(cosine.map(|c| pgvector::Vector::from(at_cosine(c))))
    .execute(pool)
    .await
    .expect("insert");
    id.to_string()
}

fn config(fusion: Fusion, debug_scores: bool) -> SearchConfig {
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
        debug_scores,
    }
}

async fn search(pool: &PgPool, cfg: &SearchConfig, namespace: &str) -> Vec<SearchHit> {
    postgres::PgMemoryRepository::new(pool.clone())
        .with_search(cfg)
        .search(SearchQuery {
            tenant_id: TENANT.into(),
            primary: vec![NamespaceCeiling {
                namespace: namespace.into(),
                max: Sensitivity::Sealed,
            }],
            secondary: vec![],
            embedding: query_vector(),
            text: "kestrel".into(),
            limit: 10,
            weights: Weights { vector: 1.0, lexical: 0.35, secondary_penalty: 0.85, usage: 0.05 },
            include_superseded: false,
            as_of: None,
            tags: vec![],
        })
        .await
        .expect("search")
}

fn norm_of(hits: &[SearchHit], id: &str) -> f64 {
    let hit = hits.iter().find(|h| h.memory.id == id).unwrap_or_else(|| panic!("{id} missing"));
    hit.scores.expect("scores under debug").cosine_norm.expect("a min-max hit carries its norm")
}

/// Cosines 0.9, 0.8 and 0.7 rescale to 1, 0.5 and 0, and a row the lexical arm alone found scores
/// 0 without pulling the minimum down. The fused score is the linear blend over the rescaled
/// cosine, term for term, with no use boost on rows nobody has read yet.
#[tokio::test]
async fn the_minmax_blend_rescales_the_pool_postgres_scores() {
    const DB: &str = "minmax_rescale_probe";
    let Some((pool, admin)) = fresh(DB).await else { return };
    let ns = "project:alpha";
    let top = insert(&pool, ns, "the harbour office opens at nine", Some(0.9)).await;
    let mid = insert(&pool, ns, "the ferry leaves from pier four", Some(0.8)).await;
    let low = insert(&pool, ns, "the kestrel host moved to port 5433", Some(0.7)).await;
    let lexical_only = insert(&pool, ns, "kestrel kestrel runbook", None).await;

    let hits = search(&pool, &config(Fusion::LinearMinmax, true), ns).await;
    assert!((norm_of(&hits, &top) - 1.0).abs() < 1e-6, "{hits:?}");
    assert!((norm_of(&hits, &mid) - 0.5).abs() < 1e-3, "{hits:?}");
    assert!(norm_of(&hits, &low).abs() < 1e-6, "{hits:?}");
    assert_eq!(norm_of(&hits, &lexical_only), 0.0, "{hits:?}");

    for hit in &hits {
        let s = hit.scores.unwrap();
        let blend = s.cosine_norm.unwrap() * 1.0 + s.keyword * 0.35;
        assert!((s.fused - blend).abs() < 1e-9, "{} fused {} vs {blend}", hit.memory.id, s.fused);
    }
    let low_hit = hits.iter().find(|h| h.memory.id == low).unwrap();
    assert!(low_hit.scores.unwrap().keyword > 0.0, "the lexical arm scored the kestrel row");
    assert_eq!(hits[0].memory.id, top, "the best cosine leads when the keyword term is small");
    drop_db(pool, admin, DB).await;
}

/// One candidate leaves max equal to min. The row takes the top of the range rather than a NULL
/// that would sort it by created_at.
#[tokio::test]
async fn a_single_candidate_takes_the_top_of_the_range() {
    const DB: &str = "minmax_single_probe";
    let Some((pool, admin)) = fresh(DB).await else { return };
    let only = insert(&pool, "project:solo", "the only row here", Some(0.42)).await;
    let hits = search(&pool, &config(Fusion::LinearMinmax, true), "project:solo").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(norm_of(&hits, &only), 1.0);
    assert!(hits[0].score.is_finite());
    drop_db(pool, admin, DB).await;
}

/// The debug flag changes what a hit carries and nothing about which hits come back or in what
/// order, under every blend. Off, no hit carries scores.
#[tokio::test]
async fn debug_scores_leave_the_ranking_alone() {
    const DB: &str = "minmax_debug_probe";
    let Some((pool, admin)) = fresh(DB).await else { return };
    let ns = "project:alpha";
    for (i, cosine) in [0.91f32, 0.88, 0.86, 0.83, 0.8, 0.77].into_iter().enumerate() {
        let text = if i % 2 == 0 { "kestrel notes" } else { "harbour notes" };
        insert(&pool, ns, &format!("{text} {i}"), Some(cosine)).await;
    }
    insert(&pool, ns, "kestrel only", None).await;

    for fusion in [Fusion::Linear, Fusion::Rrf, Fusion::LinearMinmax] {
        let off = search(&pool, &config(fusion, false), ns).await;
        let on = search(&pool, &config(fusion, true), ns).await;
        let ids = |h: &[SearchHit]| h.iter().map(|h| h.memory.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&off), ids(&on), "{fusion:?}");
        let scores = |h: &[SearchHit]| h.iter().map(|h| h.score).collect::<Vec<_>>();
        assert_eq!(scores(&off), scores(&on), "{fusion:?}");
        assert!(off.iter().all(|h| h.scores.is_none()), "{fusion:?} leaked scores while off");
        assert!(on.iter().all(|h| h.scores.is_some()), "{fusion:?} dropped scores while on");
        for h in &on {
            let s = h.scores.unwrap();
            assert_eq!(s.cosine_norm.is_some(), fusion == Fusion::LinearMinmax, "{fusion:?}");
            assert_eq!(s.vector_rank.is_some(), fusion == Fusion::Rrf && s.cosine > 0.0);
        }
    }
    drop_db(pool, admin, DB).await;
}
