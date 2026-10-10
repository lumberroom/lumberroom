//! Each embedding model reads its own cosine thresholds (decision 0029). These tests write through
//! `write::run` and read through `review_queue::queue` against a real Postgres, under two fake
//! embedders that carry the production ids of bge-base-en-v1.5 and EmbeddingGemma 2. Skipped when
//! no database is reachable, in the shape `tests/review_queue.rs` uses.
//!
//! Every fake maps known texts to fixed unit vectors, so a test sets the cosine between two texts
//! exactly. A pair at cosine c is `e0` and `c*e0 + sqrt(1 - c^2)*e1`. The store rounds similarities
//! to four places, and every cosine here sits at least 0.004 from the nearest threshold it is tested
//! against, so f32 error cannot move a pair across a band.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::domain::errors::{DomainError, Result as DomainResult};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::similarity::{self, Source as Basis};
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::ports::Embedder;
use lumberroom_server::services::review_queue::{self, QueueQuery, Source};
use lumberroom_server::services::{conflicts, write, Ctx, Repos};

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const DIM: usize = 768;
const BGE_ID: &str = "Xenova/bge-base-en-v1.5@q8";
const GEMMA_ID: &str = "openai:google/embeddinggemma-2";

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

macro_rules! step {
    ($what:expr, $result:expr) => {
        match $result {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping: {} failed: {e:?}", $what);
                return None;
            }
        }
    };
}

/// An embedder whose vectors the test chose. It refuses a text it was not given, so a write path
/// that embeds something unexpected fails loudly instead of landing on an arbitrary vector.
struct FixedEmbedder {
    id: &'static str,
    vectors: HashMap<String, Vec<f32>>,
}

impl FixedEmbedder {
    /// `first` gets `e0`; `second` sits at exactly `cosine` from it.
    fn pair(id: &'static str, first: &str, second: &str, cosine: f32) -> Self {
        let mut a = vec![0.0f32; DIM];
        a[0] = 1.0;
        let mut b = vec![0.0f32; DIM];
        b[0] = cosine;
        b[1] = (1.0 - cosine * cosine).sqrt();
        Self { id, vectors: HashMap::from([(first.to_string(), a), (second.to_string(), b)]) }
    }

    fn vector(&self, text: &str) -> DomainResult<Vec<f32>> {
        self.vectors.get(text).cloned().ok_or_else(|| {
            DomainError::internal(format!("the {} fake has no vector for {text:?}", self.id))
        })
    }
}

#[async_trait]
impl Embedder for FixedEmbedder {
    fn id(&self) -> String {
        self.id.to_string()
    }

    fn dim(&self) -> usize {
        DIM
    }

    async fn embed_documents(&self, texts: Vec<String>) -> DomainResult<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.vector(t)).collect()
    }

    async fn embed_query(&self, text: &str) -> DomainResult<Vec<f32>> {
        self.vector(text)
    }
}

struct Harness {
    ctx: Ctx,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

impl Harness {
    /// The harness context with `embedder` in place of the default, resolved through the same
    /// registry the boot path uses.
    fn under(&self, embedder: FixedEmbedder) -> Ctx {
        let mut ctx = self.ctx.clone();
        ctx.embedders = common::test_embedders(Arc::new(embedder), &ctx.cfg);
        ctx
    }
}

/// Clears every old single threshold variable before `tune` runs, so a value the shell exported
/// cannot stand in for the table these tests read.
async fn setup(tune: impl FnOnce(&mut Config)) -> Option<Harness> {
    let guard = SERIAL.lock().await;
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let base_url = admin_url.rsplit_once('/')?.0.to_string();
    let admin = step!("connecting to the admin database", sqlx::PgPool::connect(&admin_url).await);

    let exists: Result<Option<i32>, _> =
        sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(TEST_DB)
            .fetch_optional(&admin)
            .await;
    let exists = step!("looking for the test database", exists);
    if exists.is_none() {
        // Audited: TEST_DB is a compile-time constant with no external input.
        let created = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {TEST_DB}")))
            .execute(&admin)
            .await;
        step!("creating the test database", created);
    }
    admin.close().await;

    let url = format!("{base_url}/{TEST_DB}");
    std::env::set_var("DATABASE_URL", &url);
    std::env::set_var("AUTH_TOKENS", format!("mac:{}", "m".repeat(32)));
    std::env::set_var("EMBED_PROVIDER", "hash");

    let db_lock = common::lock_database(&url).await?;
    let pool = step!("connecting to the test database", postgres::connect(&url).await);
    step!("migrating the test database", postgres::migrate(&pool).await);
    let truncated = sqlx::query(
        "TRUNCATE memory, registry, registry_history, entity_alias, sealed_item, tool_calls,
                  registry_alias, kek_state, memory_pair_dismissed,
                  cleanup_proposal, cleanup_proposal_member, cleanup_watermark, subject_cardinality,
                  oauth_client, oauth_code, oauth_token, oauth_refresh
         RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await;
    step!("truncating the test database", truncated);

    let mut cfg: Config = step!("loading the config", config::load());
    cfg.quality.dedupe_threshold = None;
    cfg.quality.conflict_threshold = None;
    cfg.bootstrap.dedup_cosine = None;
    cfg.search.route_max_top = None;
    cfg.search.route_max_spread = None;
    cfg.embed.thresholds = vec![];
    tune(&mut cfg);

    let memories = Arc::new(postgres::PgMemoryRepository::new(pool.clone()));
    let repos = Repos {
        memories: memories.clone(),
        registry: Arc::new(postgres::PgRegistryRepository::new(pool.clone())),
        tool_calls: Arc::new(postgres::PgToolCallRepository::new(pool.clone())),
        sealed: Some(Arc::new(postgres::PgSealedRepository::new(pool.clone()))),
        ciphertext: Some(memories),
        oauth: None,
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
    };
    let cfg = Arc::new(cfg);
    // Every write here is open, so no key provider: a test that reached the encryption path would
    // fail on the refusal instead of passing on a key it never meant to use.
    let ctx = Ctx {
        cfg: Arc::clone(&cfg),
        repos,
        embedders: common::test_embedders(
            Arc::new(FixedEmbedder { id: BGE_ID, vectors: HashMap::new() }),
            &cfg,
        ),
        keys: None,
        kek_verified: false,
        principal: owner_like("mac"),
        invocation: Invocation::Cli,
        session_id: Some("test-session".into()),
    };
    lumberroom_server::services::bootstrap::clear_cache();

    Some(Harness { ctx, _serial: guard, _db: db_lock })
}

macro_rules! harness_or_skip {
    () => {
        harness_or_skip!(|_| {})
    };
    ($tune:expr) => {
        match setup($tune).await {
            Some(h) => h,
            None => {
                eprintln!("skipping: no database reachable");
                return;
            }
        }
    };
}

fn owner_like(client: &str) -> Principal {
    Principal {
        client: client.into(),
        token_id: "test".into(),
        mode: "token",
        scopes: vec![],
        read: NamespaceGrant::everything(),
        write: NamespaceGrant::everything(),
        registry_write: true,
        sealed_capable: true,
        may_delete: true,
        may_ingest: true,
        may_read_history: true,
    }
}

fn threshold(ctx: &Ctx, key: &str) -> f64 {
    ctx.embedders.for_unit(ctx.tenant()).unwrap().thresholds.get(key)
}

// Two texts with the same digits, identifiers and negations, so `collapse_block` passes them and
// only the cosine band decides. `write.rs`'s own unit tests pin this exact pair as collapsible.
const TABS_OVER: &str = "Dana prefers tabs over spaces";
const TABS_TO: &str = "Dana prefers tabs to spaces";

#[tokio::test]
async fn the_dedupe_band_reads_the_unit_model_value() {
    let h = harness_or_skip!();
    assert_eq!(write::collapse_block(TABS_OVER, TABS_TO), None, "the fixture must pass the guard");

    // 0.98 sits above bge's dedupe (0.97) and between Gemma's conflict (0.91) and dedupe (0.995).
    let bge = h.under(FixedEmbedder::pair(BGE_ID, TABS_OVER, TABS_TO, 0.98));
    assert_eq!(threshold(&bge, similarity::DEDUPE), 0.97);
    let first =
        write::run(&bge, TABS_OVER, "project:dedupe-bge", None, None, None, None).await.unwrap();
    let second =
        write::run(&bge, TABS_TO, "project:dedupe-bge", None, None, None, None).await.unwrap();
    assert!(second.deduplicated, "bge folds a pair at 0.98: {second:?}");
    assert_eq!(second.id, first.id);

    let gemma = h.under(FixedEmbedder::pair(GEMMA_ID, TABS_OVER, TABS_TO, 0.98));
    assert_eq!(threshold(&gemma, similarity::DEDUPE), 0.995);
    assert_eq!(threshold(&gemma, similarity::CONFLICT), 0.91);
    let first = write::run(&gemma, TABS_OVER, "project:dedupe-gemma", None, None, None, None)
        .await
        .unwrap();
    let second =
        write::run(&gemma, TABS_TO, "project:dedupe-gemma", None, None, None, None).await.unwrap();
    assert!(!second.deduplicated, "Gemma keeps a pair at 0.98 apart: {second:?}");
    assert_ne!(second.id, first.id);
    let conflicts: Vec<(&str, f64)> =
        second.possible_conflicts.iter().map(|c| (c.id.as_str(), c.similarity)).collect();
    assert_eq!(conflicts, vec![(first.id.as_str(), 0.98)]);
}

const INDENT_FOR: &str = "Dana indents with tabs for alignment";
const INDENT_IN: &str = "Dana indents with tabs in alignment";

#[tokio::test]
async fn a_correction_with_supersedes_skips_the_band_under_either_model() {
    let h = harness_or_skip!();
    assert_eq!(
        write::collapse_block(INDENT_FOR, INDENT_IN),
        None,
        "the fixture must pass the guard"
    );

    for (id, tag) in [(BGE_ID, "bge"), (GEMMA_ID, "gemma")] {
        // 0.999 clears both dedupe values (0.97, 0.995), so without `supersedes` the band folds
        // the pair. The control proves the fixture reaches the band before the real case skips it.
        let ctx = h.under(FixedEmbedder::pair(id, INDENT_FOR, INDENT_IN, 0.999));
        let control = format!("project:supersede-control-{tag}");
        let kept = write::run(&ctx, INDENT_FOR, &control, None, None, None, None).await.unwrap();
        let folded = write::run(&ctx, INDENT_IN, &control, None, None, None, None).await.unwrap();
        assert!(folded.deduplicated, "{tag}: the control pair at 0.999 must fold: {folded:?}");
        assert_eq!(folded.id, kept.id);

        let namespace = format!("project:supersede-{tag}");
        let old = write::run(&ctx, INDENT_FOR, &namespace, None, None, None, None).await.unwrap();
        let new =
            write::run(&ctx, INDENT_IN, &namespace, None, Some(&old.id), None, None).await.unwrap();
        assert!(!new.deduplicated, "{tag}: a correction lands as its own row: {new:?}");
        assert_ne!(new.id, old.id);
        assert_eq!(new.superseded.as_deref(), Some(old.id.as_str()), "{tag}");
    }
}

const STANDUP_DAILY: &str = "the team standup happens daily after lunch";
const STANDUP_EACH: &str = "the team standup happens each day after lunch";

#[tokio::test]
async fn the_review_queue_floor_reads_the_unit_conflict_value() {
    let h = harness_or_skip!();
    let namespace = "project:queue-floor";

    // Stored once, under bge, and swept at a floor below both models' conflict values, so the pair
    // sits in memory_conflict at 0.905 whichever model reads it. Only the queue's floor differs.
    let bge = h.under(FixedEmbedder::pair(BGE_ID, STANDUP_DAILY, STANDUP_EACH, 0.905));
    let older = write::run(&bge, STANDUP_DAILY, namespace, None, None, None, None).await.unwrap();
    let newer = write::run(&bge, STANDUP_EACH, namespace, None, None, None, None).await.unwrap();
    assert!(!newer.deduplicated && newer.id != older.id, "0.905 is below bge's dedupe: {newer:?}");
    let report = conflicts::sweep(
        bge.repos.memories.as_ref(),
        bge.tenant(),
        0.5,
        std::time::Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(report.pending, 0, "the sweep left rows unscanned: {report:?}");

    let conflict_only = || QueueQuery {
        sources: Some(vec![Source::Conflict]),
        limit: None,
        offset: None,
        days: None,
        min_similarity: None,
    };
    let listed = |q: &review_queue::Queue| -> Vec<f64> {
        q.items.iter().filter(|i| i.namespace == namespace).filter_map(|i| i.similarity).collect()
    };

    let under_bge = review_queue::queue(&bge, &[], conflict_only()).await.unwrap();
    assert_eq!(under_bge.min_similarity, 0.90);
    assert_eq!(listed(&under_bge), vec![0.905], "bge's floor 0.90 lists the pair");

    let gemma = h.under(FixedEmbedder::pair(GEMMA_ID, STANDUP_DAILY, STANDUP_EACH, 0.905));
    let under_gemma = review_queue::queue(&gemma, &[], conflict_only()).await.unwrap();
    assert_eq!(under_gemma.min_similarity, 0.91);
    assert!(listed(&under_gemma).is_empty(), "Gemma's floor 0.91 hides the pair");
}

#[tokio::test]
async fn an_override_moves_one_model_only() {
    let h = harness_or_skip!(|cfg: &mut Config| {
        cfg.embed.thresholds = similarity::parse_overrides("dedupe=0.99").unwrap();
    });

    // The override reaches bge's dedupe and nothing else bge resolves.
    let bge = h.under(FixedEmbedder::pair(BGE_ID, TABS_OVER, TABS_TO, 0.98));
    let t = bge.embedders.for_unit(bge.tenant()).unwrap().thresholds;
    assert_eq!(t.values[similarity::DEDUPE].value, 0.99);
    assert_eq!(t.values[similarity::DEDUPE].source, Basis::Override);
    assert_eq!(t.values[similarity::CONFLICT].value, 0.90);
    assert_eq!(t.values[similarity::CONFLICT].source, Basis::Shipped);

    // A pair bge folds at its table value (the dedupe test above) now lands as its own row.
    let first =
        write::run(&bge, TABS_OVER, "project:override", None, None, None, None).await.unwrap();
    let second =
        write::run(&bge, TABS_TO, "project:override", None, None, None, None).await.unwrap();
    assert!(!second.deduplicated, "dedupe=0.99 keeps a pair at 0.98 apart: {second:?}");
    assert_ne!(second.id, first.id);

    // EMBED_THRESHOLDS belongs to the block that configures bge. The registry applies an override
    // list to whatever id it is handed, so what keeps the override off Gemma is the set: it resolves
    // that block for bge's id and holds nothing for any other.
    let held: Vec<&str> = bge.embedders.all_thresholds().keys().map(String::as_str).collect();
    assert_eq!(held, vec![BGE_ID]);
    assert!(bge.embedders.thresholds_for(GEMMA_ID).is_err(), "bge's set holds no Gemma values");
}
