//! An embedding-model migration against a real Postgres (decision 0027): the repository adapter
//! statement by statement, the migration's tables and triggers, the write path's two vectors, and
//! the sweep end to end. Skipped when no database is reachable, in the shape
//! `tests/thresholds_per_model.rs` uses.
//!
//! Two fakes stand in for the models and carry production ids. Each answers a deterministic unit
//! vector whose first component is its marker (bge +0.6, Gemma -0.6), so a stored vector names the
//! model that made it. The rest of the vector comes from a hash of the text: one text under one
//! model always lands on the same vector, two texts sit near cosine 0.36, and one text under the
//! two models sits at 0.28. A search that read the wrong slot scores its own text far below 1.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lumberroom_server::adapters::postgres::{self, PgEmbeddingMigrationRepository};
use lumberroom_server::config::{self, Config};
use lumberroom_server::domain::embedding_migration::{
    Blocked, Configured, FlipOutcome, FlipRequest, FlipScope, Intent, IntentChange, Phase,
    Published, UnitState, UnitStatus, Verb,
};
use lumberroom_server::domain::embedding_phase::boot_check;
use lumberroom_server::domain::embedding_slot::VectorSlot;
use lumberroom_server::domain::errors::{DomainError, Result as DomainResult};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::similarity::{Registry, SimilarityThresholds};
use lumberroom_server::domain::types::{Invocation, Principal, Sensitivity};
use lumberroom_server::ports::{ChangeOutcome, Embedder, EmbeddingMigrationRepository, FreeSpace};
use lumberroom_server::services::embedders::EmbedderSet;
use lumberroom_server::services::embedding_migration::{Knobs, SingleUnit, Steer, Sweep};
use lumberroom_server::services::row_opener::{NoOpener, RowOpener};
use lumberroom_server::services::{search, write, Ctx, Repos};
use sqlx::{Connection, PgPool, Row};
use uuid::Uuid;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const DIM: usize = 768;
const BGE: &str = "openai:BAAI/bge-base-en-v1.5";
const GEMMA: &str = "openai:google/embeddinggemma-2";
const THIRD: &str = "openai:some/third-model";
const UNIT: &str = "me";
const NS: &str = "project:embedding-migration";

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The migration tables back to the state `migrate` leaves. Other binaries truncate `memory` and
/// leave these alone, and a state row left active on slot B would move their conflict scans onto
/// `embedding_b`.
const RESET: &str = "\
DELETE FROM embedding_state;
INSERT INTO embedding_control (singleton) VALUES (true) ON CONFLICT DO NOTHING;
UPDATE embedding_control
   SET generation = 0, target = NULL, flip = false, retire = NULL, verb = NULL,
       requested_at = NULL, applied_generation = 0, server_models = '{}',
       server_status = NULL, server_seen_at = NULL;";

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

// ── the fakes ─────────────────────────────────────────────────────────────────────────────────

fn fnv(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A unit vector: `marker` in component 0, then 0.64 and 0.48 at two positions the text picks.
fn vector(marker: f32, text: &str) -> Vec<f32> {
    let h = fnv(text);
    let i = 1 + (h % 767) as usize;
    let mut j = 1 + ((h >> 20) % 767) as usize;
    if j == i {
        j = 1 + (i % 767);
    }
    let mut v = vec![0.0f32; DIM];
    v[0] = marker;
    v[i] = 0.64;
    v[j] = 0.48;
    v
}

fn marker(model: &str) -> f32 {
    match model {
        BGE => 0.6,
        GEMMA => -0.6,
        _ => 0.0,
    }
}

/// Counts the texts it embeds as documents, leaving out the sweep's liveness probe, so a test can
/// say how many rows a pass sent.
struct Fake {
    id: &'static str,
    down: AtomicBool,
    documents: AtomicUsize,
    queries: AtomicUsize,
}

impl Fake {
    fn new(id: &'static str) -> Arc<Self> {
        Arc::new(Fake {
            id,
            down: AtomicBool::new(false),
            documents: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
        })
    }
    fn documents(&self) -> usize {
        self.documents.load(Ordering::SeqCst)
    }
    fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }
    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

#[async_trait]
impl Embedder for Fake {
    fn id(&self) -> String {
        self.id.to_string()
    }
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_documents(&self, texts: Vec<String>) -> DomainResult<Vec<Vec<f32>>> {
        if self.down.load(Ordering::SeqCst) {
            return Err(DomainError::unavailable(format!("{} is unreachable", self.id)));
        }
        let counted = texts.iter().filter(|t| !t.starts_with("lumberroom embedding")).count();
        self.documents.fetch_add(counted, Ordering::SeqCst);
        Ok(texts.iter().map(|t| vector(marker(self.id), t)).collect())
    }
    async fn embed_query(&self, text: &str) -> DomainResult<Vec<f32>> {
        if self.down.load(Ordering::SeqCst) {
            return Err(DomainError::unavailable(format!("{} is unreachable", self.id)));
        }
        self.queries.fetch_add(1, Ordering::SeqCst);
        Ok(vector(marker(self.id), text))
    }
}

/// Answers plenty for its first `healthy` reads and nothing after, so a test chooses the read at
/// which the sweep finds itself under the floor.
struct Disk {
    healthy: AtomicUsize,
}

impl FreeSpace for Disk {
    fn free_bytes(&self) -> DomainResult<u64> {
        let left = self.healthy.load(Ordering::SeqCst);
        if left == 0 {
            return Ok(0);
        }
        self.healthy.store(left - 1, Ordering::SeqCst);
        Ok(u64::MAX)
    }
}

/// Opens any private row it is asked for, and counts the asks.
#[derive(Default)]
struct AnyOpener(AtomicUsize);

#[async_trait]
impl RowOpener for AnyOpener {
    async fn open(&self, _unit: &str, ids: &[Uuid]) -> DomainResult<HashMap<Uuid, String>> {
        self.0.fetch_add(ids.len(), Ordering::SeqCst);
        Ok(ids.iter().map(|id| (*id, format!("opened private text {id}"))).collect())
    }
}

// ── harness ───────────────────────────────────────────────────────────────────────────────────

struct Harness {
    pool: PgPool,
    url: String,
    cfg: Arc<Config>,
    repo: Arc<PgEmbeddingMigrationRepository>,
    bge: Arc<Fake>,
    gemma: Arc<Fake>,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

impl Drop for Harness {
    /// Runs before the fields drop, so the suite lock is still held. A thread of its own, because
    /// Drop cannot await and the test's runtime may be the one unwinding.
    fn drop(&mut self) {
        let url = self.url.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
            rt.block_on(async {
                let mut conn = sqlx::PgConnection::connect(&url).await.ok()?;
                sqlx::raw_sql(RESET).execute(&mut conn).await.ok()
            })
        })
        .join();
    }
}

async fn setup() -> Option<Harness> {
    let serial = SERIAL.lock().await;
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let base_url = admin_url.rsplit_once('/')?.0.to_string();
    let admin = step!("connecting to the admin database", PgPool::connect(&admin_url).await);
    let exists: Result<Option<i32>, _> =
        sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(TEST_DB)
            .fetch_optional(&admin)
            .await;
    if step!("looking for the test database", exists).is_none() {
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

    let db = common::lock_database(&url).await?;
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
    step!("resetting the migration tables", sqlx::raw_sql(RESET).execute(&pool).await);

    let mut cfg: Config = step!("loading the config", config::load());
    cfg.tenant_id = UNIT.to_string();
    cfg.quality.dedupe_threshold = None;
    cfg.quality.conflict_threshold = None;
    cfg.bootstrap.dedup_cosine = None;
    cfg.search.route_max_top = None;
    cfg.search.route_max_spread = None;
    cfg.embed.thresholds = vec![];
    cfg.embed.migrate.shadow_timeout_ms = 2_000;
    lumberroom_server::services::bootstrap::clear_cache();

    Some(Harness {
        repo: Arc::new(PgEmbeddingMigrationRepository::new(pool.clone())),
        pool,
        url,
        cfg: Arc::new(cfg),
        bge: Fake::new(BGE),
        gemma: Fake::new(GEMMA),
        _serial: serial,
        _db: db,
    })
}

macro_rules! harness_or_skip {
    () => {
        match setup().await {
            Some(h) => h,
            None => {
                eprintln!("skipping: no database reachable");
                return;
            }
        }
    };
}

fn view(current: &str, previous: Option<&str>, flip: FlipScope) -> Configured {
    Configured {
        current: current.to_string(),
        previous: previous.map(str::to_string),
        retire: None,
        flip,
        rollback_days: 7,
        guessed_acting: BTreeMap::new(),
        generation: None,
    }
}

fn retiring(retire: &str) -> Configured {
    Configured { retire: Some(retire.to_string()), ..view(GEMMA, None, FlipScope::All) }
}

fn thresholds() -> HashMap<String, Arc<SimilarityThresholds>> {
    let registry = Registry::engine();
    [BGE, GEMMA]
        .iter()
        .map(|id| (id.to_string(), Arc::new(registry.resolve(id, &[], &[]))))
        .collect()
}

fn knobs(floor_bytes: u64) -> Knobs {
    Knobs { fill_chars: 4000, fill_duty: 100, floor_bytes, fill_budget: Duration::from_secs(60) }
}

fn owner() -> Principal {
    Principal {
        client: "mac".into(),
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

impl Harness {
    fn set(&self, configured: Configured) -> Arc<EmbedderSet> {
        let built: Vec<Arc<dyn Embedder>> =
            vec![self.gemma.clone() as Arc<dyn Embedder>, self.bge.clone() as Arc<dyn Embedder>];
        Arc::new(EmbedderSet::new(built, configured, thresholds()))
    }

    fn sweep(&self, set: &Arc<EmbedderSet>, configured: Configured) -> Sweep {
        self.sweep_with(set, configured, Arc::new(NoOpener), false, None)
    }

    fn sweep_with(
        &self,
        set: &Arc<EmbedderSet>,
        configured: Configured,
        opener: Arc<dyn RowOpener>,
        kek_verified: bool,
        disk: Option<Arc<dyn FreeSpace>>,
    ) -> Sweep {
        let floor = if disk.is_some() { 1_000 } else { 0 };
        Sweep::new(
            self.repo.clone(),
            Arc::clone(set),
            opener,
            knobs(floor),
            Steer::Env(configured),
            kek_verified,
            disk,
        )
    }

    /// A request context whose embedders are `set`, so the sweep and the request path share one
    /// state map as they do in the server.
    fn ctx(&self, set: &Arc<EmbedderSet>) -> Ctx {
        let memories = Arc::new(postgres::PgMemoryRepository::new(self.pool.clone()));
        Ctx {
            cfg: Arc::clone(&self.cfg),
            repos: Repos {
                memories: memories.clone(),
                registry: Arc::new(postgres::PgRegistryRepository::new(self.pool.clone())),
                tool_calls: Arc::new(postgres::PgToolCallRepository::new(self.pool.clone())),
                sealed: Some(Arc::new(postgres::PgSealedRepository::new(self.pool.clone()))),
                ciphertext: Some(memories),
                oauth: None,
                aliases: Arc::new(postgres::PgAliasRepository::new(self.pool.clone())),
            },
            embedders: Arc::clone(set),
            keys: None,
            kek_verified: false,
            principal: owner(),
            invocation: Invocation::Cli,
            session_id: Some("test-session".into()),
        }
    }

    async fn state(&self) -> UnitState {
        self.repo.state(UNIT).await.unwrap().expect("the unit has a state row")
    }

    async fn load_states(&self, set: &EmbedderSet) {
        set.set_states(self.repo.states().await.unwrap());
    }
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn text(n: u128) -> String {
    format!("fact number {n} about the embedding migration")
}

/// One row. `content` None makes it private, with a placeholder ciphertext; each named slot gets
/// that model's vector of the row's text.
async fn insert(
    pool: &PgPool,
    tenant: &str,
    n: u128,
    content: Option<&str>,
    sensitivity: &str,
    a: Option<&str>,
    b: Option<&str>,
) -> Uuid {
    let basis = content.map_or_else(|| format!("private row {n}"), str::to_string);
    let v = |model: Option<&str>| model.map(|m| pgvector::Vector::from(vector(marker(m), &basis)));
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, source_client, sensitivity,
                             embedding, embedding_model, embedding_b, embedding_b_model,
                             content_ct, content_nonce, dek_wrapped, dek_nonce, enc_alg)
         VALUES ($1, $2, $3, $4, 'test', $5, $6, $7, $8, $9,
                 CASE WHEN $4::text IS NULL THEN '\\x01'::bytea END,
                 CASE WHEN $4::text IS NULL THEN '\\x02'::bytea END,
                 CASE WHEN $4::text IS NULL THEN '\\x03'::bytea END,
                 CASE WHEN $4::text IS NULL THEN '\\x04'::bytea END,
                 CASE WHEN $4::text IS NULL THEN 'test' END)",
    )
    .bind(id(n))
    .bind(tenant)
    .bind(NS)
    .bind(content)
    .bind(sensitivity)
    .bind(v(a))
    .bind(a)
    .bind(v(b))
    .bind(b)
    .execute(pool)
    .await
    .unwrap();
    id(n)
}

async fn open_row(pool: &PgPool, n: u128, a: Option<&str>, b: Option<&str>) -> Uuid {
    insert(pool, UNIT, n, Some(&text(n)), "open", a, b).await
}

async fn put_state(
    pool: &PgPool,
    active: VectorSlot,
    a: Option<&str>,
    b: Option<&str>,
    flipped_days_ago: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO embedding_state (tenant_id, active_slot, model_a, model_b, flipped_at)
         VALUES ($1, $2, $3, $4, now() - make_interval(days => $5::int))
         ON CONFLICT (tenant_id) DO UPDATE
           SET active_slot = EXCLUDED.active_slot, model_a = EXCLUDED.model_a,
               model_b = EXCLUDED.model_b, flipped_at = EXCLUDED.flipped_at",
    )
    .bind(UNIT)
    .bind(active.as_str())
    .bind(a)
    .bind(b)
    .bind(flipped_days_ago.map(|d| d as i32))
    .execute(pool)
    .await
    .unwrap();
}

/// `n` open rows holding bge in slot A and Gemma in slot B, and a state row that names both with
/// slot A active.
async fn filled_unit(pool: &PgPool, n: u128) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for i in 1..=n {
        ids.push(open_row(pool, i, Some(BGE), Some(GEMMA)).await);
    }
    put_state(pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    ids
}

#[derive(Debug, PartialEq)]
struct Slots {
    a: Option<(String, f32)>,
    b: Option<(String, f32)>,
}

/// Each slot's model and the first component of its vector. A vector with no model reads as an
/// empty model name, which no assertion here expects.
async fn slots(pool: &PgPool, row: Uuid) -> Slots {
    let r = sqlx::query(
        "SELECT embedding, embedding_model, embedding_b, embedding_b_model FROM memory WHERE id = $1",
    )
    .bind(row)
    .fetch_one(pool)
    .await
    .unwrap();
    let read = |v: &str, m: &str| -> Option<(String, f32)> {
        let vector: Option<pgvector::Vector> = r.try_get(v).unwrap();
        let model: Option<String> = r.try_get(m).unwrap();
        vector.map(|v| (model.unwrap_or_default(), v.as_slice()[0]))
    };
    Slots { a: read("embedding", "embedding_model"), b: read("embedding_b", "embedding_b_model") }
}

fn from(model: &str) -> Option<(String, f32)> {
    Some((model.to_string(), marker(model)))
}

/// `xmin` of every row of the unit. Any UPDATE of a row, even one that writes the same values,
/// gives it a new one.
async fn xmins(pool: &PgPool) -> BTreeMap<Uuid, String> {
    sqlx::query("SELECT id, xmin::text AS x FROM memory WHERE tenant_id = $1")
        .bind(UNIT)
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get("id"), r.get("x")))
        .collect()
}

async fn pair(pool: &PgPool, older: Uuid, newer: Uuid) {
    sqlx::query(
        "INSERT INTO memory_conflict (tenant_id, older_id, newer_id, similarity)
         VALUES ($1, $2, $3, 0.95)",
    )
    .bind(UNIT)
    .bind(older)
    .bind(newer)
    .execute(pool)
    .await
    .unwrap();
    for row in [older, newer] {
        sqlx::query(
            "INSERT INTO memory_conflict_scan (memory_id, tenant_id, floor) VALUES ($1, $2, 0.9)",
        )
        .bind(row)
        .bind(UNIT)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// (pairs, scan marks) on record for the unit.
async fn conflict_rows(pool: &PgPool) -> (i64, i64) {
    let pairs: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_conflict WHERE tenant_id = $1")
        .bind(UNIT)
        .fetch_one(pool)
        .await
        .unwrap();
    let scans: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_conflict_scan WHERE tenant_id = $1")
            .bind(UNIT)
            .fetch_one(pool)
            .await
            .unwrap();
    (pairs, scans)
}

fn unit_status(sweep: &Sweep) -> UnitStatus {
    sweep.status.read().unwrap().units.iter().find(|u| u.unit == UNIT).cloned().unwrap()
}

fn me() -> SingleUnit {
    SingleUnit(UNIT.to_string())
}

fn flip_to(expect: UnitState, slot: VectorSlot, model: &str, generation: Option<i64>) -> FlipRequest {
    FlipRequest {
        expect,
        target_slot: slot,
        target_model: model.to_string(),
        expect_generation: generation,
    }
}

fn start(target: &str) -> IntentChange {
    IntentChange { target: Some(target.to_string()), flip: false, retire: None, verb: Verb::Start }
}

/// Polls until a backend waits on a lock while running a statement that contains `fragment`, so a
/// test commits its blocking transaction only once the other side is queued behind it.
async fn wait_for_lock_wait(pool: &PgPool, fragment: &str) {
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND position($1 in query) > 0",
        )
        .bind(fragment)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no backend queued on a lock running {fragment:?}");
}

// ── the migration ─────────────────────────────────────────────────────────────────────────────

/// A database of its own, so the row `migrate` inserts is the one asserted on and not one a
/// test's reset wrote.
#[tokio::test]
async fn the_migration_leaves_one_resting_control_row_and_guards_the_state_row() {
    let h = harness_or_skip!();
    let probe = "lumberroom_rust_test_embedding_control";
    let base = h.url.rsplit_once('/').unwrap().0.to_string();
    // Audited: `probe` is a constant with no external input.
    let drop_probe = format!("DROP DATABASE IF EXISTS {probe} WITH (FORCE)");
    sqlx::raw_sql(sqlx::AssertSqlSafe(drop_probe.clone())).execute(&h.pool).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {probe}")))
        .execute(&h.pool)
        .await
        .unwrap();

    let fresh = postgres::connect(&format!("{base}/{probe}")).await.unwrap();
    postgres::migrate(&fresh).await.unwrap();
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM embedding_control").fetch_one(&fresh).await.unwrap();
    assert_eq!(rows, 1, "migrate leaves exactly one control row");
    let repo = PgEmbeddingMigrationRepository::new(fresh.clone());
    let (intent, published) = repo.control().await.unwrap();
    assert_eq!(intent, Intent::default());
    assert_eq!(published, Published::default());

    // The singleton CHECK keeps a second row out, and the state row's two CHECKs hold.
    assert!(sqlx::query("INSERT INTO embedding_control (singleton) VALUES (false)")
        .execute(&fresh)
        .await
        .is_err());
    let state = |active: &'static str, a: Option<&'static str>, b: Option<&'static str>| {
        sqlx::query(
            "INSERT INTO embedding_state (tenant_id, active_slot, model_a, model_b)
             VALUES ('me', $1, $2, $3)",
        )
        .bind(active)
        .bind(a)
        .bind(b)
    };
    assert!(state("a", None, Some(GEMMA)).execute(&fresh).await.is_err(), "active slot unnamed");
    assert!(state("a", Some(BGE), Some(BGE)).execute(&fresh).await.is_err(), "one model twice");
    assert!(state("c", Some(BGE), None).execute(&fresh).await.is_err(), "unknown slot");
    state("b", None, Some(GEMMA)).execute(&fresh).await.unwrap();

    fresh.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(drop_probe)).execute(&h.pool).await.unwrap();
}

// ── the adapter ───────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn seed_names_each_slot_from_its_majority_and_never_overwrites_a_state() {
    let h = harness_or_skip!();
    for n in 1..=3 {
        open_row(&h.pool, n, Some(BGE), Some(GEMMA)).await;
    }
    open_row(&h.pool, 4, Some(THIRD), None).await;
    insert(&h.pool, "acme", 5, Some("acme fact"), "open", Some(BGE), None).await;
    insert(&h.pool, "beta", 6, Some("beta fact"), "open", None, Some(GEMMA)).await;
    insert(&h.pool, "gamma", 7, Some("gamma fact"), "open", None, None).await;

    let mut created = h.repo.seed().await.unwrap();
    created.sort_by(|x, y| x.unit.cmp(&y.unit));
    let shape: Vec<(&str, VectorSlot, Option<&str>, Option<&str>)> = created
        .iter()
        .map(|s| (s.unit.as_str(), s.active_slot, s.model_a.as_deref(), s.model_b.as_deref()))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("acme", VectorSlot::A, Some(BGE), None),
            ("beta", VectorSlot::B, None, Some(GEMMA)),
            ("me", VectorSlot::A, Some(BGE), Some(GEMMA)),
        ],
        "a unit with no vector gets no state row"
    );
    assert!(h.repo.seed().await.unwrap().is_empty(), "a second seed creates nothing");

    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(0)).await;
    assert!(h.repo.seed().await.unwrap().is_empty());
    assert_eq!(h.state().await.active_slot, VectorSlot::B, "seeding overwrote a state");
}

#[tokio::test]
async fn name_slot_names_only_an_empty_inactive_slot() {
    let h = harness_or_skip!();
    assert!(!h.repo.name_slot(UNIT, VectorSlot::B, GEMMA).await.unwrap(), "no state row");

    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    assert!(!h.repo.name_slot(UNIT, VectorSlot::A, GEMMA).await.unwrap(), "the active slot");
    assert!(!h.repo.name_slot(UNIT, VectorSlot::B, BGE).await.unwrap(), "the other slot's model");
    assert!(h.repo.name_slot(UNIT, VectorSlot::B, GEMMA).await.unwrap());
    assert!(!h.repo.name_slot(UNIT, VectorSlot::B, THIRD).await.unwrap(), "already named");
    let s = h.state().await;
    assert_eq!((s.model_a.as_deref(), s.model_b.as_deref()), (Some(BGE), Some(GEMMA)));
}

#[tokio::test]
async fn counts_split_the_unit_rows_by_what_each_slot_lacks() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    open_row(&h.pool, 1, Some(BGE), Some(GEMMA)).await; // complete
    open_row(&h.pool, 2, Some(BGE), None).await; // pending
    open_row(&h.pool, 3, None, None).await; // pending, active hole
    open_row(&h.pool, 4, Some(THIRD), None).await; // pending, foreign
    insert(&h.pool, UNIT, 5, Some("a sealed fact"), "sealed", None, None).await; // never counted
    insert(&h.pool, UNIT, 6, None, "private", Some(BGE), None).await; // pending
    insert(&h.pool, UNIT, 7, None, "private", None, None).await; // without vector
    open_row(&h.pool, 8, Some(BGE), Some(THIRD)).await; // pending, retire pending
    insert(&h.pool, "acme", 9, Some("another unit"), "open", None, None).await;

    let state = h.state().await;
    let c = h.repo.counts(UNIT, &state, Some(GEMMA), Some(THIRD)).await.unwrap();
    assert_eq!(c.eligible, 6, "{c:?}");
    assert_eq!(c.other_pending, 5, "{c:?}");
    assert_eq!(c.active_holes, 1, "{c:?}");
    assert_eq!(c.foreign_model, 1, "{c:?}");
    assert_eq!(c.without_vector, 1, "{c:?}");
    assert_eq!(c.retire_pending, 1, "{c:?}");
    assert_eq!(c.failed, 0);

    let quiet = h.repo.counts(UNIT, &state, None, None).await.unwrap();
    assert_eq!((quiet.other_pending, quiet.retire_pending), (0, 0), "{quiet:?}");
}

#[tokio::test]
async fn next_batch_pages_pending_rows_in_id_order_and_skips_sealed_and_filled() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    open_row(&h.pool, 1, Some(BGE), None).await;
    open_row(&h.pool, 2, Some(BGE), Some(GEMMA)).await;
    insert(&h.pool, UNIT, 3, Some("sealed"), "sealed", Some(BGE), None).await;
    insert(&h.pool, UNIT, 4, None, "private", Some(BGE), None).await;
    open_row(&h.pool, 5, Some(BGE), Some(THIRD)).await;

    let page = h.repo.next_batch(UNIT, VectorSlot::B, GEMMA, None, 2).await.unwrap();
    let ids: Vec<Uuid> = page.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![id(1), id(4)]);
    assert_eq!(page[0].content.as_deref(), Some(text(1).as_str()));
    assert_eq!(page[0].chars, text(1).chars().count() as i64);
    assert_eq!(page[1].sensitivity, Sensitivity::Private);
    assert_eq!(page[1].content, None, "a private row comes back for the opener");
    assert!(page[1].chars > 0, "a private row sizes its request from the ciphertext");

    let rest = h.repo.next_batch(UNIT, VectorSlot::B, GEMMA, Some(id(4)), 10).await.unwrap();
    assert_eq!(rest.iter().map(|r| r.id).collect::<Vec<_>>(), vec![id(5)]);
    assert!(h.repo.next_batch(UNIT, VectorSlot::B, GEMMA, Some(id(5)), 10).await.unwrap().is_empty());

    let active = h.repo.next_batch(UNIT, VectorSlot::A, BGE, None, 10).await.unwrap();
    assert!(active.is_empty(), "every eligible row holds bge in slot A: {active:?}");
}

/// The `steady` fill reads holes, so a vector under another model's id is never re-embedded.
#[tokio::test]
async fn next_holes_returns_only_rows_with_no_vector_in_the_slot() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    open_row(&h.pool, 1, None, None).await;
    open_row(&h.pool, 2, Some(THIRD), None).await;
    insert(&h.pool, UNIT, 3, Some("sealed"), "sealed", None, None).await;
    insert(&h.pool, UNIT, 4, None, "private", None, Some(GEMMA)).await;
    insert(&h.pool, UNIT, 5, None, "private", None, None).await;
    open_row(&h.pool, 6, Some(BGE), None).await;
    insert(&h.pool, "acme", 7, Some("another unit"), "open", None, None).await;

    let page = h.repo.next_holes(UNIT, VectorSlot::A, None, 1).await.unwrap();
    assert_eq!(page.iter().map(|r| r.id).collect::<Vec<_>>(), vec![id(1)]);
    let rest = h.repo.next_holes(UNIT, VectorSlot::A, Some(id(1)), 10).await.unwrap();
    assert_eq!(rest.iter().map(|r| r.id).collect::<Vec<_>>(), vec![id(4)]);
    assert_eq!(rest[0].content, None, "a private row comes back for the opener");
    let b = h.repo.next_holes(UNIT, VectorSlot::B, None, 10).await.unwrap();
    assert_eq!(b.iter().map(|r| r.id).collect::<Vec<_>>(), vec![id(1), id(2), id(6)]);
}

#[tokio::test]
async fn a_store_lands_only_on_an_eligible_pending_row_of_the_named_model() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    let open = open_row(&h.pool, 1, Some(BGE), None).await;
    let sealed = insert(&h.pool, UNIT, 2, Some("sealed"), "sealed", Some(BGE), None).await;
    let full = open_row(&h.pool, 3, Some(BGE), Some(GEMMA)).await;
    let other = open_row(&h.pool, 4, Some(BGE), None).await;
    let g = |row: Uuid| vector(marker(GEMMA), &format!("stored {row}"));

    assert!(h.repo.store(UNIT, open, VectorSlot::B, GEMMA, g(open)).await.unwrap());
    assert_eq!(slots(&h.pool, open).await.b, from(GEMMA));
    assert!(!h.repo.store(UNIT, open, VectorSlot::B, GEMMA, g(open)).await.unwrap(), "filled");
    assert!(!h.repo.store(UNIT, sealed, VectorSlot::B, GEMMA, g(sealed)).await.unwrap());
    assert!(!h.repo.store(UNIT, full, VectorSlot::B, GEMMA, g(full)).await.unwrap());
    assert!(!h.repo.store("acme", other, VectorSlot::B, GEMMA, g(other)).await.unwrap());

    // Review M1: a pass that read state before a retire or a rename stores nothing, so no slot
    // ever holds a model its state row does not name.
    assert!(!h.repo.store(UNIT, other, VectorSlot::B, THIRD, g(other)).await.unwrap());
    assert!(!h.repo.store(UNIT, other, VectorSlot::A, GEMMA, g(other)).await.unwrap());
    assert_eq!(slots(&h.pool, other).await, Slots { a: from(BGE), b: None });
    assert_eq!(slots(&h.pool, sealed).await.b, None);
}

#[tokio::test]
async fn a_store_after_revocation_lands_nothing() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    let row = insert(&h.pool, UNIT, 1, None, "private", Some(BGE), None).await;
    let page = h.repo.next_batch(UNIT, VectorSlot::B, GEMMA, None, 10).await.unwrap();
    assert_eq!(page.iter().map(|r| r.id).collect::<Vec<_>>(), vec![row]);

    // The revocation commits between the read and the store: no content, no vectors, ciphertext
    // kept.
    sqlx::query(
        "UPDATE memory SET embedding = NULL, embedding_model = NULL,
                           embedding_b = NULL, embedding_b_model = NULL
          WHERE id = $1",
    )
    .bind(row)
    .execute(&h.pool)
    .await
    .unwrap();
    let landed = h
        .repo
        .store(UNIT, row, VectorSlot::B, GEMMA, vector(marker(GEMMA), "opened text"))
        .await
        .unwrap();
    assert!(!landed);
    assert_eq!(slots(&h.pool, row).await, Slots { a: None, b: None });
}

#[tokio::test]
async fn the_flip_moves_the_pointer_and_rewrites_no_memory_row() {
    let h = harness_or_skip!();
    let ids = filled_unit(&h.pool, 3).await;
    pair(&h.pool, ids[0], ids[1]).await;
    let before = xmins(&h.pool).await;

    let out = h.repo.flip(&flip_to(h.state().await, VectorSlot::B, GEMMA, None)).await.unwrap();
    assert!(matches!(out, FlipOutcome::Flipped { .. }), "{out:?}");
    let s = h.state().await;
    assert_eq!(s.active_slot, VectorSlot::B);
    assert_eq!((s.model_a.as_deref(), s.model_b.as_deref()), (Some(BGE), Some(GEMMA)));
    assert!(s.flipped_at.is_some());
    assert_eq!(xmins(&h.pool).await, before, "the flip wrote a memory row");
    // The flip resets conflict pairs: they were scored on the other model.
    assert_eq!(conflict_rows(&h.pool).await, (0, 0));
}

#[tokio::test]
async fn the_flip_refuses_a_changed_state_row_as_stale() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 2).await;
    let read = h.state().await;
    sqlx::query("UPDATE embedding_state SET model_a = $1 WHERE tenant_id = $2")
        .bind(THIRD)
        .bind(UNIT)
        .execute(&h.pool)
        .await
        .unwrap();
    let out = h.repo.flip(&flip_to(read, VectorSlot::B, GEMMA, None)).await.unwrap();
    assert_eq!(out, FlipOutcome::Stale);
    assert_eq!(h.state().await.active_slot, VectorSlot::A);
}

#[tokio::test]
async fn the_flip_refuses_a_moved_generation_as_stale() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 2).await;
    let read = h.state().await;
    let out = h.repo.flip(&flip_to(read.clone(), VectorSlot::B, GEMMA, Some(5))).await.unwrap();
    assert_eq!(out, FlipOutcome::Stale, "the control row holds generation 0");
    assert_eq!(h.state().await.active_slot, VectorSlot::A);

    let out = h.repo.flip(&flip_to(read, VectorSlot::B, GEMMA, Some(0))).await.unwrap();
    assert!(matches!(out, FlipOutcome::Flipped { .. }), "{out:?}");
}

#[tokio::test]
async fn the_flip_refuses_an_incomplete_slot_and_writes_nothing() {
    let h = harness_or_skip!();
    let ids = filled_unit(&h.pool, 2).await;
    open_row(&h.pool, 3, Some(BGE), None).await;
    pair(&h.pool, ids[0], ids[1]).await;

    let out = h.repo.flip(&flip_to(h.state().await, VectorSlot::B, GEMMA, None)).await.unwrap();
    assert_eq!(out, FlipOutcome::Incomplete(1));
    let s = h.state().await;
    assert_eq!((s.active_slot, s.flipped_at), (VectorSlot::A, None));
    assert_eq!(conflict_rows(&h.pool).await, (1, 2), "a refused flip cleared the pairs");
}

#[tokio::test]
async fn a_retire_batch_clears_the_inactive_slot_in_batches_and_only_the_named_model() {
    let h = harness_or_skip!();
    for n in 1..=5 {
        open_row(&h.pool, n, Some(BGE), Some(GEMMA)).await;
    }
    let third = open_row(&h.pool, 6, Some(THIRD), Some(GEMMA)).await;
    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(30)).await;

    let mut batches = Vec::new();
    loop {
        let n = h.repo.retire_batch(UNIT, VectorSlot::A, BGE, 2).await.unwrap();
        batches.push(n);
        if n == 0 {
            break;
        }
    }
    assert_eq!(batches, vec![2, 2, 1, 0]);
    for n in 1..=5 {
        assert_eq!(slots(&h.pool, id(n)).await, Slots { a: None, b: from(GEMMA) }, "row {n}");
    }
    assert_eq!(slots(&h.pool, third).await.a.map(|(m, _)| m).as_deref(), Some(THIRD));
    assert!(h.repo.clear_slot(UNIT, VectorSlot::A, BGE).await.unwrap());
    assert_eq!(h.state().await.model_a, None);
}

#[tokio::test]
async fn a_retire_batch_never_clears_the_slot_the_unit_reads() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 3).await;
    let before = xmins(&h.pool).await;
    assert_eq!(h.repo.retire_batch(UNIT, VectorSlot::A, BGE, 100).await.unwrap(), 0);
    assert_eq!(xmins(&h.pool).await, before);
    assert!(!h.repo.clear_slot(UNIT, VectorSlot::A, BGE).await.unwrap());
    assert_eq!(h.state().await.model_a.as_deref(), Some(BGE));
}

#[tokio::test]
async fn clear_slot_waits_until_no_row_carries_the_model() {
    let h = harness_or_skip!();
    open_row(&h.pool, 1, Some(BGE), Some(GEMMA)).await;
    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(30)).await;
    assert!(!h.repo.clear_slot(UNIT, VectorSlot::A, BGE).await.unwrap(), "a row still holds bge");
    assert!(!h.repo.clear_slot(UNIT, VectorSlot::B, GEMMA).await.unwrap(), "the active slot");
    assert_eq!(h.repo.retire_batch(UNIT, VectorSlot::A, BGE, 10).await.unwrap(), 1);
    assert!(!h.repo.clear_slot(UNIT, VectorSlot::A, THIRD).await.unwrap(), "another model");
    assert!(h.repo.clear_slot(UNIT, VectorSlot::A, BGE).await.unwrap());
    assert_eq!(h.state().await.model_a, None);
}

#[tokio::test]
async fn change_intent_writes_the_next_generation_and_recreates_a_missing_row() {
    let h = harness_or_skip!();
    let out = h
        .repo
        .change_intent(&|i: &Intent, _s: &[UnitState]| {
            assert_eq!(i.generation, 0);
            Ok(Some(start(GEMMA)))
        })
        .await
        .unwrap();
    let ChangeOutcome::Written(written) = out else { panic!("{out:?}") };
    assert_eq!(written.generation, 1);
    assert_eq!(written.target.as_deref(), Some(GEMMA));
    assert_eq!(written.verb, Some(Verb::Start));
    assert!(written.requested_at.is_some());
    assert_eq!(h.repo.control().await.unwrap().0, written);
    assert_eq!(h.repo.generation().await.unwrap(), 1);

    sqlx::query("DELETE FROM embedding_control").execute(&h.pool).await.unwrap();
    assert_eq!(h.repo.control().await.unwrap(), (Intent::default(), Published::default()));
    assert_eq!(h.repo.generation().await.unwrap(), 0);
    let out =
        h.repo.change_intent(&|_: &Intent, _: &[UnitState]| Ok(Some(start(GEMMA)))).await.unwrap();
    assert!(matches!(out, ChangeOutcome::Written(ref i) if i.generation == 1), "{out:?}");
}

#[tokio::test]
async fn a_noop_or_a_refused_change_rolls_back() {
    let h = harness_or_skip!();
    let out = h.repo.change_intent(&|_: &Intent, _: &[UnitState]| Ok(None)).await.unwrap();
    assert_eq!(out, ChangeOutcome::NoOp(Intent::default()));
    let out = h
        .repo
        .change_intent(&|_: &Intent, _: &[UnitState]| Err("not now".to_string()))
        .await
        .unwrap();
    assert_eq!(out, ChangeOutcome::Refused("not now".into()));
    assert_eq!(h.repo.generation().await.unwrap(), 0);

    // With the row gone, the insert that recreates it belongs to the transaction too.
    sqlx::query("DELETE FROM embedding_control").execute(&h.pool).await.unwrap();
    h.repo.change_intent(&|_: &Intent, _: &[UnitState]| Ok(None)).await.unwrap();
    h.repo.change_intent(&|_: &Intent, _: &[UnitState]| Err("no".into())).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM embedding_control")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a no-op or a refusal committed the recreated row");
}

/// Review M7: a command that queued behind another's lock decides on the states that commit left.
#[tokio::test]
async fn change_intent_decides_on_states_read_after_the_row_lock() {
    let h = harness_or_skip!();
    let mut holder = h.pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *holder).await.unwrap();
    sqlx::query("SELECT generation FROM embedding_control WHERE singleton FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();

    let repo = h.repo.clone();
    let decided = tokio::spawn(async move {
        let seen: Mutex<Vec<UnitState>> = Mutex::new(Vec::new());
        repo.change_intent(&|_: &Intent, states: &[UnitState]| {
            *seen.lock().unwrap() = states.to_vec();
            Ok(None)
        })
        .await
        .unwrap();
        seen.into_inner().unwrap()
    });
    wait_for_lock_wait(&h.pool, "FROM embedding_control WHERE singleton FOR UPDATE").await;
    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    sqlx::query("COMMIT").execute(&mut *holder).await.unwrap();

    let seen = tokio::time::timeout(Duration::from_secs(10), decided).await.unwrap().unwrap();
    assert_eq!(seen.iter().map(|s| s.unit.as_str()).collect::<Vec<_>>(), vec![UNIT]);
}

#[tokio::test]
async fn publish_round_trips_through_the_control_row() {
    let h = harness_or_skip!();
    let status = serde_json::json!({ "summary": { "rollback": "instant" }, "units": [] });
    h.repo
        .publish(&Published {
            applied_generation: 3,
            server_models: vec![GEMMA.into(), BGE.into()],
            status: Some(status.clone()),
            seen_at: None,
        })
        .await
        .unwrap();
    let (intent, published) = h.repo.control().await.unwrap();
    assert_eq!(intent, Intent::default(), "publish touched the intent");
    assert_eq!(published.applied_generation, 3);
    assert_eq!(published.server_models, vec![GEMMA.to_string(), BGE.to_string()]);
    assert_eq!(published.status, Some(status));
    assert!(published.seen_at.is_some(), "the store stamps the time");
}

// ── triggers ──────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_fill_of_the_inactive_slot_b_keeps_conflict_pairs() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    let one = open_row(&h.pool, 1, Some(BGE), None).await;
    let two = open_row(&h.pool, 2, Some(BGE), None).await;
    pair(&h.pool, one, two).await;
    assert!(h.repo.store(UNIT, one, VectorSlot::B, GEMMA, vector(marker(GEMMA), "x")).await.unwrap());
    assert_eq!(conflict_rows(&h.pool).await, (1, 2), "a fill of the inactive slot cleared pairs");

    // The control: the slot the unit reads still clears them.
    sqlx::query("UPDATE memory SET embedding = $1 WHERE id = $2")
        .bind(pgvector::Vector::from(vector(marker(BGE), "rewritten")))
        .bind(two)
        .execute(&h.pool)
        .await
        .unwrap();
    assert_eq!(conflict_rows(&h.pool).await, (0, 1));
}

#[tokio::test]
async fn a_fill_of_the_inactive_slot_a_keeps_conflict_pairs() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(0)).await;
    let one = open_row(&h.pool, 1, None, Some(GEMMA)).await;
    let two = open_row(&h.pool, 2, None, Some(GEMMA)).await;
    pair(&h.pool, one, two).await;
    assert!(h.repo.store(UNIT, one, VectorSlot::A, BGE, vector(marker(BGE), "x")).await.unwrap());
    assert_eq!(conflict_rows(&h.pool).await, (1, 2), "a fill of the inactive slot cleared pairs");

    sqlx::query("UPDATE memory SET embedding_b = $1 WHERE id = $2")
        .bind(pgvector::Vector::from(vector(marker(GEMMA), "rewritten")))
        .bind(two)
        .execute(&h.pool)
        .await
        .unwrap();
    assert_eq!(conflict_rows(&h.pool).await, (0, 1));
}

#[tokio::test]
async fn a_namespace_move_clears_pairs_whatever_the_slot() {
    let h = harness_or_skip!();
    let one = open_row(&h.pool, 1, Some(BGE), Some(GEMMA)).await;
    let two = open_row(&h.pool, 2, Some(BGE), Some(GEMMA)).await;
    for active in [VectorSlot::A, VectorSlot::B] {
        put_state(&h.pool, active, Some(BGE), Some(GEMMA), Some(0)).await;
        pair(&h.pool, one, two).await;
        sqlx::query("UPDATE memory SET namespace = $1 WHERE id = $2")
            .bind(format!("project:moved-{}", active.as_str()))
            .bind(one)
            .execute(&h.pool)
            .await
            .unwrap();
        assert_eq!(conflict_rows(&h.pool).await, (0, 1), "active {active:?}");
        sqlx::query("DELETE FROM memory_conflict_scan").execute(&h.pool).await.unwrap();
    }
}

// ── writes ────────────────────────────────────────────────────────────────────────────────────

const NOTE: &str = "Dana keeps the release notes in the team wiki";

#[tokio::test]
async fn a_write_lands_both_vectors_in_their_slots() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    h.load_states(&set).await;
    let out = write::run(&h.ctx(&set), NOTE, NS, None, None, None, None).await.unwrap();
    let row: Uuid = out.id.parse().unwrap();
    assert_eq!(slots(&h.pool, row).await, Slots { a: from(BGE), b: from(GEMMA) });
}

#[tokio::test]
async fn a_write_on_a_unit_active_on_b_lands_each_vector_in_its_slot() {
    let h = harness_or_skip!();
    // bge in slot B and active there, so the slot comes from the state row and not the model.
    put_state(&h.pool, VectorSlot::B, Some(GEMMA), Some(BGE), None).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    h.load_states(&set).await;
    let out = write::run(&h.ctx(&set), NOTE, NS, None, None, None, None).await.unwrap();
    let row: Uuid = out.id.parse().unwrap();
    assert_eq!(slots(&h.pool, row).await, Slots { a: from(GEMMA), b: from(BGE) });
}

#[tokio::test]
async fn a_unit_with_no_state_row_answers_the_previous_model() {
    let h = harness_or_skip!();
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    let u = set.for_unit(UNIT).unwrap();
    assert_eq!(u.slot, VectorSlot::A);
    assert_eq!(u.embedder.id(), BGE);
    assert_eq!(u.second.as_ref().map(|e| e.id()).as_deref(), Some(GEMMA));
    assert_eq!(u.thresholds.model, BGE, "the thresholds follow the model searches read");

    let out = write::run(&h.ctx(&set), NOTE, NS, None, None, None, None).await.unwrap();
    let row: Uuid = out.id.parse().unwrap();
    assert_eq!(slots(&h.pool, row).await, Slots { a: from(BGE), b: from(GEMMA) });
}

#[tokio::test]
async fn a_search_with_the_active_embedder_down_returns_the_embedder_error() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 2).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    h.load_states(&set).await;
    h.bge.set_down(true);
    let err = search::run(&h.ctx(&set), &text(1), None, Some(5), None, None, None)
        .await
        .expect_err("a search with its model down must fail, never answer from the other slot");
    assert!(err.log_message().contains("is unreachable"), "{}", err.log_message());
    assert_eq!(h.gemma.queries(), 0, "the search fell back to the other model");
}

// ── the sweep ─────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_sweep_names_the_other_slot_and_fills_it_to_held() {
    let h = harness_or_skip!();
    for n in 1..=5 {
        open_row(&h.pool, n, Some(BGE), None).await;
    }
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    let sweep = h.sweep(&set, view(GEMMA, Some(BGE), FlipScope::None));
    sweep.pass(&me()).await;

    let s = h.state().await;
    assert_eq!((s.active_slot, s.model_a.as_deref()), (VectorSlot::A, Some(BGE)), "seeded");
    assert_eq!(s.model_b.as_deref(), Some(GEMMA), "named before the fill");
    for n in 1..=5 {
        assert_eq!(slots(&h.pool, id(n)).await, Slots { a: from(BGE), b: from(GEMMA) }, "row {n}");
    }
    assert_eq!((h.gemma.documents(), h.bge.documents()), (5, 0));
    let st = unit_status(&sweep);
    assert_eq!(st.phase, Phase::Held);
    assert_eq!(st.counts.other_pending, 0);
    assert_eq!(set.for_unit(UNIT).unwrap().slot, VectorSlot::A, "held keeps serving slot A");
}

#[tokio::test]
async fn searches_read_the_active_slot_after_the_flip() {
    let h = harness_or_skip!();
    let ids = filled_unit(&h.pool, 3).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::All));
    h.load_states(&set).await;
    let ctx = h.ctx(&set);
    let similarity_of = |hits: &[search::Hit]| {
        hits.iter().find(|x| x.id == ids[0].to_string()).map(|x| x.similarity).unwrap_or(-1.0)
    };

    let before = search::run(&ctx, &text(1), None, Some(5), None, None, None).await.unwrap();
    assert!(similarity_of(&before.hits) > 0.99, "{:?}", similarity_of(&before.hits));
    assert_eq!((h.bge.queries(), h.gemma.queries()), (1, 0));

    h.sweep(&set, view(GEMMA, Some(BGE), FlipScope::All)).pass(&me()).await;
    assert_eq!(h.state().await.active_slot, VectorSlot::B);
    let after = search::run(&ctx, &text(1), None, Some(5), None, None, None).await.unwrap();
    assert!(similarity_of(&after.hits) > 0.99, "read the wrong slot: {}", similarity_of(&after.hits));
    assert_eq!((h.bge.queries(), h.gemma.queries()), (1, 1));
}

#[tokio::test]
async fn a_search_during_the_flip_reads_one_model() {
    let h = harness_or_skip!();
    let ids = filled_unit(&h.pool, 3).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::All));
    h.load_states(&set).await;
    let ctx = h.ctx(&set);
    let sweep = h.sweep(&set, view(GEMMA, Some(BGE), FlipScope::All));

    let searches = async {
        let mut scores = Vec::new();
        for _ in 0..20 {
            let r = search::run(&ctx, &text(1), None, Some(5), None, None, None).await.unwrap();
            let own = r.hits.iter().find(|x| x.id == ids[0].to_string()).map(|x| x.similarity);
            scores.push(own.unwrap_or(-1.0));
        }
        scores
    };
    let units = me();
    let (scores, _) = tokio::join!(searches, sweep.pass(&units));
    assert_eq!(h.state().await.active_slot, VectorSlot::B);
    assert!(scores.iter().all(|s| *s > 0.99), "a search mixed the two models: {scores:?}");
}

#[tokio::test]
async fn rollback_flips_back_with_no_embedding_calls_and_no_row_writes() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 4).await;
    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(1)).await;
    let before = xmins(&h.pool).await;
    let set = h.set(view(BGE, Some(GEMMA), FlipScope::All));
    let sweep = h.sweep(&set, view(BGE, Some(GEMMA), FlipScope::All));
    sweep.pass(&me()).await;

    assert_eq!(h.state().await.active_slot, VectorSlot::A);
    assert_eq!((h.bge.documents(), h.gemma.documents()), (0, 0));
    assert_eq!(xmins(&h.pool).await, before);
    assert_eq!(unit_status(&sweep).phase, Phase::Flipped);
    assert_eq!(set.for_unit(UNIT).unwrap().embedder.id(), BGE);
}

#[tokio::test]
async fn retire_waits_for_the_rollback_window() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 3).await;
    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(1)).await;
    let set = h.set(retiring(BGE));
    let sweep = h.sweep(&set, retiring(BGE));
    sweep.pass(&me()).await;
    for n in 1..=3 {
        assert_eq!(slots(&h.pool, id(n)).await.a, from(BGE), "deleted inside the window");
    }
    let st = unit_status(&sweep);
    assert_eq!(st.phase, Phase::Retiring);
    assert!(st.retire_after.is_some_and(|at| at > chrono::Utc::now()), "{st:?}");

    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(8)).await;
    sweep.pass(&me()).await;
    for n in 1..=3 {
        assert_eq!(slots(&h.pool, id(n)).await, Slots { a: None, b: from(GEMMA) }, "row {n}");
    }
    assert_eq!(h.state().await.model_a, None);
    assert_eq!(unit_status(&sweep).phase, Phase::Steady);
    assert_eq!((h.bge.documents(), h.gemma.documents()), (0, 0));
}

#[tokio::test]
async fn a_unit_with_a_missed_second_vector_does_not_flip() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 2).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    h.load_states(&set).await;
    h.gemma.set_down(true);
    let out = write::run(&h.ctx(&set), NOTE, NS, None, None, None, None).await.unwrap();
    let row: Uuid = out.id.parse().unwrap();
    assert_eq!(slots(&h.pool, row).await, Slots { a: from(BGE), b: None }, "the miss still writes");

    let sweep = h.sweep(&set, view(GEMMA, Some(BGE), FlipScope::None));
    sweep.pass(&me()).await;
    let st = unit_status(&sweep);
    assert_eq!((st.phase, st.counts.other_pending), (Phase::Filling, 1), "{st:?}");
    let refused = h.repo.flip(&flip_to(h.state().await, VectorSlot::B, GEMMA, None)).await;
    assert_eq!(refused.unwrap(), FlipOutcome::Incomplete(1));

    h.gemma.set_down(false);
    sweep.pass(&me()).await;
    assert_eq!(slots(&h.pool, row).await.b, from(GEMMA));
    assert_eq!(unit_status(&sweep).phase, Phase::Held);
    let out = h.repo.flip(&flip_to(h.state().await, VectorSlot::B, GEMMA, None)).await.unwrap();
    assert!(matches!(out, FlipOutcome::Flipped { .. }), "{out:?}");
}

#[tokio::test]
async fn a_restarted_sweep_embeds_only_what_is_missing() {
    let h = harness_or_skip!();
    for n in 1000..1070 {
        open_row(&h.pool, n, Some(BGE), None).await;
    }
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));

    // Two healthy reads: the one that opens the pass and the gate before the first page. The gate
    // before the second page finds the disk full and stops the pass, standing in for a restart.
    let disk: Arc<dyn FreeSpace> = Arc::new(Disk { healthy: AtomicUsize::new(2) });
    let first = h.sweep_with(
        &set,
        view(GEMMA, Some(BGE), FlipScope::None),
        Arc::new(NoOpener),
        false,
        Some(disk),
    );
    first.pass(&me()).await;
    let after_first = h.gemma.documents();
    assert_eq!(after_first, 64, "one page of rows before the stop");

    let second = h.sweep(&set, view(GEMMA, Some(BGE), FlipScope::None));
    second.pass(&me()).await;
    assert_eq!(h.gemma.documents(), 70, "the new sweep embedded a row the old one had stored");

    // An id below every cursor position the last fill reached.
    let low = open_row(&h.pool, 1, Some(BGE), None).await;
    second.pass(&me()).await;
    assert_eq!(h.gemma.documents(), 71);
    assert_eq!(slots(&h.pool, low).await.b, from(GEMMA));
    assert_eq!(unit_status(&second).phase, Phase::Held);
}

#[tokio::test]
async fn a_steady_unit_reports_a_foreign_model_row_and_leaves_it() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    let foreign = open_row(&h.pool, 1, Some(THIRD), None).await;
    let hole = open_row(&h.pool, 2, None, None).await;
    let set = h.set(view(BGE, None, FlipScope::All));
    let sweep = h.sweep(&set, view(BGE, None, FlipScope::All));
    sweep.pass(&me()).await;
    sweep.pass(&me()).await;
    assert_eq!(slots(&h.pool, foreign).await.a, Some((THIRD.to_string(), 0.0)));
    assert_eq!(slots(&h.pool, hole).await.a, from(BGE));
    assert_eq!(h.bge.documents(), 1, "the steady fill re-embedded a row under another id");
    let st = unit_status(&sweep);
    assert_eq!((st.phase, st.counts.foreign_model), (Phase::Steady, 1), "{st:?}");
}

#[tokio::test]
async fn sealed_rows_are_never_embedded() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    let sealed = insert(&h.pool, UNIT, 1, Some("a sealed fact"), "sealed", None, None).await;
    let hole = open_row(&h.pool, 2, None, None).await;
    let set = h.set(view(BGE, None, FlipScope::All));
    h.sweep(&set, view(BGE, None, FlipScope::All)).pass(&me()).await;
    assert_eq!(slots(&h.pool, hole).await.a, from(BGE), "the control: an open hole fills");

    let set = h.set(view(GEMMA, Some(BGE), FlipScope::All));
    let sweep = h.sweep(&set, view(GEMMA, Some(BGE), FlipScope::All));
    sweep.pass(&me()).await;
    sweep.pass(&me()).await;
    assert_eq!(slots(&h.pool, sealed).await, Slots { a: None, b: None });
    assert_eq!((h.bge.documents(), h.gemma.documents()), (1, 1));
    assert_eq!(unit_status(&sweep).counts.eligible, 1);
}

#[tokio::test]
async fn a_private_row_without_vectors_stays_without_them() {
    let h = harness_or_skip!();
    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    let bare = insert(&h.pool, UNIT, 1, None, "private", None, None).await;
    let held = insert(&h.pool, UNIT, 2, None, "private", Some(BGE), None).await;
    let opener = Arc::new(AnyOpener::default());
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    let sweep = h.sweep_with(
        &set,
        view(GEMMA, Some(BGE), FlipScope::None),
        opener.clone(),
        true,
        None,
    );
    sweep.pass(&me()).await;

    assert_eq!(slots(&h.pool, bare).await, Slots { a: None, b: None });
    assert_eq!(slots(&h.pool, held).await.b, from(GEMMA), "the control: a private row with a vector fills");
    assert_eq!(opener.0.load(Ordering::SeqCst), 1, "the sweep opened a row it may not embed");
    assert_eq!(unit_status(&sweep).counts.without_vector, 1);
}

#[tokio::test]
async fn the_sweep_pauses_below_the_disk_floor_and_writes_continue() {
    let h = harness_or_skip!();
    for n in 1..=3 {
        open_row(&h.pool, n, Some(BGE), None).await;
    }
    put_state(&h.pool, VectorSlot::A, Some(BGE), Some(GEMMA), None).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    let disk: Arc<dyn FreeSpace> = Arc::new(Disk { healthy: AtomicUsize::new(0) });
    let sweep = h.sweep_with(
        &set,
        view(GEMMA, Some(BGE), FlipScope::None),
        Arc::new(NoOpener),
        false,
        Some(disk),
    );
    sweep.pass(&me()).await;
    assert_eq!(h.gemma.documents(), 0, "the sweep filled under the floor");
    let paused = sweep.status.read().unwrap().disk;
    assert!(paused.is_some_and(|d| d.paused && d.floor_bytes == 1_000), "{paused:?}");

    h.load_states(&set).await;
    let out = write::run(&h.ctx(&set), NOTE, NS, None, None, None, None).await.unwrap();
    let row: Uuid = out.id.parse().unwrap();
    assert_eq!(slots(&h.pool, row).await, Slots { a: from(BGE), b: from(GEMMA) });
}

#[tokio::test]
async fn a_flip_race_row_gets_its_active_vector_on_the_next_pass() {
    let h = harness_or_skip!();
    filled_unit(&h.pool, 2).await;
    put_state(&h.pool, VectorSlot::B, Some(BGE), Some(GEMMA), Some(0)).await;
    // A writer that resolved its slots before the flip committed lands bge alone in slot A.
    let late = open_row(&h.pool, 3, Some(BGE), None).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::All));
    let sweep = h.sweep(&set, view(GEMMA, Some(BGE), FlipScope::All));
    sweep.pass(&me()).await;
    assert_eq!(slots(&h.pool, late).await, Slots { a: from(BGE), b: from(GEMMA) });
    assert_eq!(h.gemma.documents(), 1);
    let st = unit_status(&sweep);
    assert_eq!((st.phase, st.counts.active_holes), (Phase::Flipped, 0), "{st:?}");
}

#[tokio::test]
async fn a_correcting_write_during_the_flip_succeeds() {
    let h = harness_or_skip!();
    let ids = filled_unit(&h.pool, 3).await;
    let set = h.set(view(GEMMA, Some(BGE), FlipScope::None));
    h.load_states(&set).await;
    let ctx = h.ctx(&set);
    let target = ids[1].to_string();
    let request = flip_to(h.state().await, VectorSlot::B, GEMMA, None);
    let (written, flipped) = tokio::join!(
        write::run(&ctx, "fact number 2, corrected", NS, None, Some(target.as_str()), None, None),
        h.repo.flip(&request),
    );
    let written = written.expect("the correcting write failed beside the flip");
    let flipped = flipped.expect("the flip failed beside the correcting write");
    assert_eq!(written.superseded.as_deref(), Some(target.as_str()));
    assert!(
        matches!(flipped, FlipOutcome::Flipped { .. } | FlipOutcome::Incomplete(_)),
        "{flipped:?}"
    );
}

#[tokio::test]
async fn the_status_reports_each_phase() {
    let h = harness_or_skip!();
    for n in 1..=3 {
        open_row(&h.pool, n, Some(BGE), None).await;
    }
    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    let set = h.set(view(BGE, None, FlipScope::All));
    let phase = |configured: Configured| {
        let sweep = h.sweep(&set, configured);
        async move {
            sweep.pass(&me()).await;
            unit_status(&sweep)
        }
    };

    assert_eq!(phase(view(BGE, None, FlipScope::All)).await.phase, Phase::Steady);
    h.gemma.set_down(true);
    let st = phase(view(GEMMA, Some(BGE), FlipScope::None)).await;
    assert_eq!((st.phase, st.counts.other_pending), (Phase::Filling, 3), "{st:?}");
    h.gemma.set_down(false);
    assert_eq!(phase(view(GEMMA, Some(BGE), FlipScope::None)).await.phase, Phase::Held);
    assert_eq!(phase(view(GEMMA, Some(BGE), FlipScope::All)).await.phase, Phase::Flipped);
    let st = phase(retiring(BGE)).await;
    assert_eq!(st.phase, Phase::Retiring);
    assert!(st.retire_after.is_some(), "{st:?}");

    sqlx::query("UPDATE embedding_state SET model_a = $1 WHERE tenant_id = $2")
        .bind(THIRD)
        .bind(UNIT)
        .execute(&h.pool)
        .await
        .unwrap();
    let st = phase(view(GEMMA, Some(BGE), FlipScope::All)).await;
    assert_eq!(st.phase, Phase::Blocked);
    assert_eq!(st.blocked, Some(Blocked::OtherHoldsThird(THIRD.into())));
}

// ── boot and tools ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn boot_refuses_a_changed_model_with_no_previous_block() {
    let h = harness_or_skip!();
    open_row(&h.pool, 1, Some(BGE), None).await;
    h.repo.seed().await.unwrap();
    let states = h.repo.states().await.unwrap();
    let err = boot_check(&states, &view(GEMMA, None, FlipScope::All)).unwrap_err();
    assert!(err.contains(&format!("{UNIT} (active on {BGE})")), "{err}");
    assert!(boot_check(&states, &view(GEMMA, Some(BGE), FlipScope::None)).is_ok());
}

#[tokio::test]
async fn verify_embedding_exits_non_zero_on_a_stuck_unit() {
    let h = harness_or_skip!();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_lumberroom-server"))
            .arg("verify-embedding")
            .env("DATABASE_URL", &h.url)
            .env("AUTH_TOKENS", format!("mac:{}", "m".repeat(32)))
            .env("EMBED_PROVIDER", "hash")
            .env("EMBED_DIM", "768")
            .env("EMBED_MIGRATION_CONTROL", "command")
            .env("EMBED_DISK_FLOOR_MB", "0")
            .env_remove("EMBED_PREVIOUS_PROVIDER")
            .env_remove("EMBED_FLIP")
            .env_remove("EMBED_RETIRE")
            .output()
            .unwrap()
    };
    put_state(&h.pool, VectorSlot::A, Some(BGE), None, None).await;
    let stuck = run();
    let stderr = String::from_utf8_lossy(&stuck.stderr);
    assert!(!stuck.status.success(), "a unit on {BGE} passed a hash-only config: {stderr}");
    assert!(stderr.contains(&format!("{UNIT} (active on {BGE})")), "{stderr}");

    put_state(&h.pool, VectorSlot::A, Some("hash-v1-768"), None, None).await;
    let ok = run();
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(ok.status.success(), "{stdout}{}", String::from_utf8_lossy(&ok.stderr));
    assert!(stdout.contains("verified: yes"), "{stdout}");
}

/// Integer components make every cosine here an exact ratio (3/5, 4/5, 24/25), so each score
/// equals its threshold's double and the script's mapping has no rounding to hide behind.
#[tokio::test]
async fn the_thresholds_script_maps_identical_slots_to_the_same_values() {
    let h = harness_or_skip!();
    let mut e0 = vec![0.0f32; DIM];
    e0[0] = 1.0;
    let mut r1 = vec![0.0f32; DIM];
    (r1[0], r1[1]) = (3.0, 4.0);
    let mut r2 = vec![0.0f32; DIM];
    (r2[0], r2[1]) = (4.0, 3.0);
    for (n, v) in [(1u128, e0), (2, r1), (3, r2)] {
        sqlx::query(
            "INSERT INTO memory (id, tenant_id, namespace, content, source_client,
                                 embedding, embedding_model, embedding_b, embedding_b_model)
             VALUES ($1, $2, $3, $4, 'test', $5, $6, $5, $7)",
        )
        .bind(id(n))
        .bind(UNIT)
        .bind(NS)
        .bind(text(n))
        .bind(pgvector::Vector::from(v))
        .bind(BGE)
        .bind(GEMMA)
        .execute(&h.pool)
        .await
        .unwrap();
    }

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/embedding-thresholds.sql");
    let script = std::fs::read_to_string(path).unwrap();
    // psql fills the variables and runs the backslash lines; this test does both by hand. The
    // backslash lines only default `old` and `new`, which each direction below sets itself.
    let body: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('\\'))
        .collect::<Vec<_>>()
        .join("\n");
    for (old, new) in [("embedding", "embedding_b"), ("embedding_b", "embedding")] {
        let sql = body
            .replace(":\"old\"", old)
            .replace(":\"new\"", new)
            .replace(":'tenant'", "'me'")
            .replace(":'thresholds'", "'dedupe=0.96,bootstrap_dedup=0.8,conflict=0.6'");
        assert!(!sql.contains(":'") && !sql.contains(":\""), "a psql variable is left: {sql}");
        let mut conn = h.pool.acquire().await.unwrap();
        // Audited: the script is a file in this repository, with literals this test chose.
        let rows = sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).fetch_all(&mut *conn).await.unwrap();
        let first: Vec<Option<String>> = rows.iter().map(|r| r.try_get(0).unwrap()).collect();
        assert_eq!(
            first.last().cloned().flatten().as_deref(),
            Some("bootstrap_dedup=0.800,conflict=0.600,dedupe=0.960"),
            "old {old}, new {new}: {first:?}"
        );
    }
}
