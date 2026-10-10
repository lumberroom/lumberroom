//! `lumberroom-server embeddings` against a real Postgres (decision 0027). Each test runs the verbs
//! through `command::embeddings::execute` with the Postgres repository and drives a command-mode
//! `Sweep` by hand between them, so a written intent meets the pass that applies it. Skipped when no
//! database is reachable, in the shape `tests/thresholds_per_model.rs` uses.
//!
//! The two fakes carry production ids and answer deterministic unit vectors whose first component
//! names the model (bge +0.6, Gemma -0.6). The clock is real time plus an offset a test can move.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use lumberroom_server::adapters::embedding::create_spec;
use lumberroom_server::adapters::postgres::{self, PgEmbeddingMigrationRepository};
use lumberroom_server::command::embeddings::{execute, CommandEnv, Outcome, ProbeFn};
use lumberroom_server::config::{self, Config, EmbedProvider, EmbedderSpec, RemoteEmbedConfig};
use lumberroom_server::domain::embedding_command::configured_from_intent;
use lumberroom_server::domain::embedding_migration::{
    Configured, ControlMode, FlipOutcome, FlipRequest, FlipScope, Intent, IntentChange, Phase,
    Published, UnitState, Verb,
};
use lumberroom_server::domain::embedding_slot::VectorSlot;
use lumberroom_server::domain::errors::{DomainError, Result as DomainResult};
use lumberroom_server::domain::similarity::{Registry, SimilarityThresholds};
use lumberroom_server::ports::{ChangeOutcome, Embedder, EmbeddingMigrationRepository};
use lumberroom_server::services::embedders::EmbedderSet;
use lumberroom_server::services::embedding_migration::{Knobs, SingleUnit, Steer, Sweep};
use lumberroom_server::services::row_opener::NoOpener;
use sqlx::{Connection, PgPool, Row};
use uuid::Uuid;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const DIM: usize = 768;
const BGE: &str = "openai:BAAI/bge-base-en-v1.5";
const GEMMA: &str = "openai:google/embeddinggemma-2";
const WITHDRAWN: &str = "openai:some/withdrawn-model";
const UNIT: &str = "me";
const NS: &str = "project:embedding-command";

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Seconds the command's clock runs ahead of real time.
static CLOCK_OFFSET_SECS: AtomicI64 = AtomicI64::new(0);

fn clock() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::seconds(CLOCK_OFFSET_SECS.load(Ordering::SeqCst))
}

/// As `tests/embedding_migration.rs`: other binaries truncate `memory` and leave these alone.
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

/// Refuses a request that carries a text it was told to refuse, as llama-server refuses an input
/// past its window, and counts the texts it embeds apart from the sweep's liveness probe.
struct Fake {
    id: &'static str,
    refuse: Mutex<HashSet<String>>,
    documents: AtomicUsize,
}

impl Fake {
    fn new(id: &'static str) -> Arc<Self> {
        Arc::new(Fake { id, refuse: Mutex::new(HashSet::new()), documents: AtomicUsize::new(0) })
    }
    fn documents(&self) -> usize {
        self.documents.load(Ordering::SeqCst)
    }
    fn refuse(&self, text: &str) {
        self.refuse.lock().unwrap().insert(text.to_string());
    }
    fn accept_all(&self) {
        self.refuse.lock().unwrap().clear();
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
        if texts.iter().any(|t| self.refuse.lock().unwrap().contains(t)) {
            return Err(DomainError::unavailable(format!("{} refused the input", self.id)));
        }
        let counted = texts.iter().filter(|t| !t.starts_with("lumberroom embedding")).count();
        self.documents.fetch_add(counted, Ordering::SeqCst);
        Ok(texts.iter().map(|t| vector(marker(self.id), t)).collect())
    }
    async fn embed_query(&self, text: &str) -> DomainResult<Vec<f32>> {
        Ok(vector(marker(self.id), text))
    }
}

/// The start probe's embedder when the target answers.
fn answering(_spec: &EmbedderSpec) -> Option<Arc<dyn Embedder>> {
    Some(Fake::new(GEMMA) as Arc<dyn Embedder>)
}

/// The real remote embedder for the spec, so a probe against an address nobody listens on fails
/// the way production's would.
fn remote(spec: &EmbedderSpec) -> Option<Arc<dyn Embedder>> {
    Some(create_spec(spec).expect("a remote embedder builds without the network"))
}

// ── harness ───────────────────────────────────────────────────────────────────────────────────

struct Harness {
    pool: PgPool,
    url: String,
    cfg: Arc<Config>,
    repo: Arc<PgEmbeddingMigrationRepository>,
    bge: Arc<Fake>,
    gemma: Arc<Fake>,
    set: Arc<EmbedderSet>,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

impl Drop for Harness {
    /// Before the fields drop, so the suite lock is still held.
    fn drop(&mut self) {
        CLOCK_OFFSET_SECS.store(0, Ordering::SeqCst);
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

/// Gemma's block as `EMBED_*` and bge's as `EMBED_PREVIOUS_*`, both remote, at an address nobody
/// listens on. Only `remote` ever builds from them.
fn spec(model: &str) -> EmbedderSpec {
    EmbedderSpec {
        provider: EmbedProvider::Openai,
        model: model.to_string(),
        dim: DIM,
        cache_dir: "/nonexistent".into(),
        remote: RemoteEmbedConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            timeout_secs: 2,
            ..Default::default()
        },
    }
}

async fn setup() -> Option<Harness> {
    let serial = SERIAL.lock().await;
    CLOCK_OFFSET_SECS.store(0, Ordering::SeqCst);
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
    let current = spec("google/embeddinggemma-2");
    cfg.embed.provider = current.provider;
    cfg.embed.model = current.model;
    cfg.embed.dim = DIM;
    cfg.embed.remote = current.remote;
    cfg.embed.thresholds = vec![];
    cfg.embed.previous = Some(spec("BAAI/bge-base-en-v1.5"));
    cfg.embed.previous_thresholds = vec![];
    cfg.embed.migrate.control = ControlMode::Command;
    cfg.embed.migrate.secs = 30;
    cfg.embed.migrate.rollback_days = 7;
    cfg.embed.disk.floor_mb = 0;

    let bge = Fake::new(BGE);
    let gemma = Fake::new(GEMMA);
    let registry = Registry::engine();
    let thresholds = [BGE, GEMMA]
        .iter()
        .map(|id| (id.to_string(), Arc::new(registry.resolve(id, &[], &[]))))
        .collect::<std::collections::HashMap<String, Arc<SimilarityThresholds>>>();
    let built: Vec<Arc<dyn Embedder>> =
        vec![gemma.clone() as Arc<dyn Embedder>, bge.clone() as Arc<dyn Embedder>];
    let resting = Configured {
        current: BGE.into(),
        previous: None,
        retire: None,
        flip: FlipScope::None,
        rollback_days: 7,
        guessed_acting: BTreeMap::new(),
        generation: Some(0),
    };
    Some(Harness {
        repo: Arc::new(PgEmbeddingMigrationRepository::new(pool.clone())),
        pool,
        url,
        cfg: Arc::new(cfg),
        bge,
        gemma,
        set: Arc::new(EmbedderSet::new(built, resting, thresholds)),
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

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn text(n: u128) -> String {
    format!("fact number {n} about the switch command")
}

fn me() -> SingleUnit {
    SingleUnit(UNIT.to_string())
}

impl Harness {
    fn env_with(&self, cfg: Arc<Config>, probe: Arc<ProbeFn>) -> CommandEnv {
        CommandEnv {
            cfg,
            repo: self.repo.clone(),
            registry: Arc::new(Registry::engine()),
            probe,
            disk: None,
            now: clock,
            poll: Duration::from_millis(10),
        }
    }

    async fn exec_with(&self, args: &[&str], probe: Arc<ProbeFn>) -> Outcome {
        let env = self.env_with(Arc::clone(&self.cfg), probe);
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        execute(&args, &env).await.unwrap()
    }

    async fn exec(&self, args: &[&str]) -> Outcome {
        self.exec_with(args, Arc::new(answering)).await
    }

    /// The server's sweep in command mode, sharing the request path's embedder set. One per test:
    /// it remembers refusals between passes as the server's does.
    fn sweep(&self) -> Sweep {
        Sweep::new(
            self.repo.clone(),
            Arc::clone(&self.set),
            Arc::new(NoOpener),
            Knobs {
                fill_chars: 4000,
                fill_duty: 100,
                floor_bytes: 0,
                fill_budget: Duration::from_secs(60),
            },
            Steer::Command {
                blocks: vec![GEMMA.into(), BGE.into()],
                rollback_days: 7,
                guessed_acting: BTreeMap::new(),
            },
            false,
            None,
        )
    }

    async fn state(&self) -> UnitState {
        self.repo.state(UNIT).await.unwrap().expect("the unit has a state row")
    }

    async fn intent(&self) -> Intent {
        self.repo.control().await.unwrap().0
    }

    async fn published(&self) -> Published {
        self.repo.control().await.unwrap().1
    }

    /// `n` open rows holding bge in slot A, seeded as the boot seeds them.
    async fn on_bge(&self, n: u128) {
        for i in 1..=n {
            insert(&self.pool, i, Some(BGE), None).await;
        }
        self.repo.seed().await.unwrap();
    }

    /// Started, filled and flipped onto Gemma through the command, with a pass after each verb.
    async fn on_gemma(&self, sweep: &Sweep, n: u128) {
        self.on_bge(n).await;
        sweep.pass(&me()).await;
        expect_written(&self.exec(&["start", "--no-wait"]).await);
        sweep.pass(&me()).await;
        expect_written(&self.exec(&["flip", "--no-wait"]).await);
        sweep.pass(&me()).await;
        assert_eq!(self.state().await.active_slot, VectorSlot::B, "the flip did not land");
    }
}

fn expect_written(out: &Outcome) {
    assert_eq!(out.code, 0, "{}", out.output);
    assert!(out.output.contains("written"), "{}", out.output);
}

async fn insert(pool: &PgPool, n: u128, a: Option<&str>, b: Option<&str>) -> Uuid {
    let content = text(n);
    let v =
        |model: Option<&str>| model.map(|m| pgvector::Vector::from(vector(marker(m), &content)));
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, source_client,
                             embedding, embedding_model, embedding_b, embedding_b_model)
         VALUES ($1, $2, $3, $4, 'test', $5, $6, $7, $8)",
    )
    .bind(id(n))
    .bind(UNIT)
    .bind(NS)
    .bind(&content)
    .bind(v(a))
    .bind(a)
    .bind(v(b))
    .bind(b)
    .execute(pool)
    .await
    .unwrap();
    id(n)
}

/// The model each slot of the row names, (a, b).
async fn models(pool: &PgPool, row: Uuid) -> (Option<String>, Option<String>) {
    let r = sqlx::query(
        "SELECT CASE WHEN embedding IS NULL THEN NULL ELSE embedding_model END AS a,
                CASE WHEN embedding_b IS NULL THEN NULL ELSE embedding_b_model END AS b
           FROM memory WHERE id = $1",
    )
    .bind(row)
    .fetch_one(pool)
    .await
    .unwrap();
    (r.get("a"), r.get("b"))
}

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

/// Moves the flip eight days back, past the seven-day rollback window.
async fn age_the_flip(pool: &PgPool) {
    sqlx::query("UPDATE embedding_state SET flipped_at = now() - interval '8 days'")
        .execute(pool)
        .await
        .unwrap();
}

fn phase(sweep: &Sweep) -> Phase {
    sweep.status.read().unwrap().units.iter().find(|u| u.unit == UNIT).unwrap().phase
}

// ── start ─────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn start_writes_one_generation_and_the_next_pass_applies_it() {
    let h = harness_or_skip!();
    h.on_bge(3).await;
    let sweep = h.sweep();
    sweep.pass(&me()).await;
    assert_eq!(phase(&sweep), Phase::Steady);

    let out = h.exec(&["start", "--no-wait"]).await;
    assert_eq!(out.code, 0, "{}", out.output);
    assert!(out.output.contains("generation 1 written"), "{}", out.output);
    let intent = h.intent().await;
    assert_eq!((intent.generation, intent.target.as_deref()), (1, Some(GEMMA)));
    assert_eq!((intent.verb, intent.flip), (Some(Verb::Start), false));
    assert_eq!(h.published().await.applied_generation, 0);

    sweep.pass(&me()).await;
    let published = h.published().await;
    assert_eq!(published.applied_generation, 1);
    assert_eq!(published.server_models, vec![GEMMA.to_string(), BGE.to_string()]);
    assert_eq!(h.state().await.model_b.as_deref(), Some(GEMMA));
    for n in 1..=3 {
        assert_eq!(models(&h.pool, id(n)).await, (Some(BGE.into()), Some(GEMMA.into())));
    }
    assert_eq!(phase(&sweep), Phase::Held);

    let again = h.exec(&["start", "--no-wait"]).await;
    assert_eq!(again.code, 0, "{}", again.output);
    assert!(again.output.contains("already"), "{}", again.output);
    assert_eq!(h.intent().await.generation, 1);
}

#[tokio::test]
async fn a_waiting_start_returns_once_a_pass_applies_it() {
    let h = harness_or_skip!();
    h.on_bge(2).await;
    let sweep = h.sweep();
    sweep.pass(&me()).await;
    let units = me();
    let server = async {
        while h.repo.generation().await.unwrap() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sweep.pass(&units).await;
    };
    let (out, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(h.exec(&["start"]), server)
    })
    .await
    .expect("the command never saw the pass");
    assert_eq!(out.code, 0, "{}", out.output);
    assert!(out.output.contains("applied by the pass"), "{}", out.output);
    assert!(out.output.contains("unit me: held"), "{}", out.output);
}

#[tokio::test]
async fn two_starts_write_one_generation() {
    let h = harness_or_skip!();
    h.on_bge(2).await;
    h.sweep().pass(&me()).await;
    let (one, two) = tokio::join!(h.exec(&["start", "--no-wait"]), h.exec(&["start", "--no-wait"]));
    assert_eq!((one.code, two.code), (0, 0), "{}\n{}", one.output, two.output);
    let written = [&one, &two].iter().filter(|o| o.output.contains("generation 1 written")).count();
    let noop = [&one, &two].iter().filter(|o| o.output.contains("already")).count();
    assert_eq!((written, noop), (1, 1), "{}\n{}", one.output, two.output);
    assert_eq!(h.intent().await.generation, 1);
}

#[tokio::test]
async fn start_is_refused_when_the_probe_fails() {
    let h = harness_or_skip!();
    h.on_bge(2).await;
    h.sweep().pass(&me()).await;
    let out = h.exec_with(&["start", "--no-wait"], Arc::new(remote)).await;
    assert_eq!(out.code, 1, "{}", out.output);
    assert!(
        out.output.contains(&format!("the probe embedding to {GEMMA} failed")),
        "{}",
        out.output
    );
    assert_eq!(h.intent().await.generation, 0);
}

// ── flip ──────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_flip_during_the_fill_waits_for_it() {
    let h = harness_or_skip!();
    h.on_bge(3).await;
    let sweep = h.sweep();
    sweep.pass(&me()).await;
    expect_written(&h.exec(&["start", "--no-wait"]).await);
    h.gemma.refuse(&text(2));
    expect_written(&h.exec(&["flip", "--no-wait"]).await);
    assert!(h.intent().await.flip);

    // Two refusals stay under the three that would block the unit.
    for _ in 0..2 {
        sweep.pass(&me()).await;
        assert_eq!(h.state().await.active_slot, VectorSlot::A, "flipped over a pending row");
        assert_eq!(models(&h.pool, id(2)).await.1, None);
    }
    h.gemma.accept_all();
    for _ in 0..2 {
        sweep.pass(&me()).await;
    }
    assert_eq!(models(&h.pool, id(2)).await.1.as_deref(), Some(GEMMA));
    assert_eq!(h.state().await.active_slot, VectorSlot::B, "the flip never landed");
    assert_eq!(phase(&sweep), Phase::Flipped);
}

/// The cancel holds the control row while the pass's flip queues on it. The flip then reads the
/// cancel's generation, so it must not move the pointer.
#[tokio::test]
async fn a_cancel_committed_before_the_flip_makes_the_flip_stale() {
    let h = harness_or_skip!();
    for n in 1..=2 {
        insert(&h.pool, n, Some(BGE), Some(GEMMA)).await;
    }
    h.repo.seed().await.unwrap();
    h.repo.name_slot(UNIT, VectorSlot::B, GEMMA).await.unwrap();
    let state = h.state().await;
    assert_eq!(state.model_b.as_deref(), Some(GEMMA));

    let mut holder = h.pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *holder).await.unwrap();
    sqlx::query(
        "UPDATE embedding_control
            SET generation = generation + 1, target = NULL, flip = false, retire = $1,
                verb = 'rollback', requested_at = now()
          WHERE singleton",
    )
    .bind(GEMMA)
    .execute(&mut *holder)
    .await
    .unwrap();

    let repo = h.repo.clone();
    let request = FlipRequest {
        expect: state,
        target_slot: VectorSlot::B,
        target_model: GEMMA.into(),
        expect_generation: Some(0),
    };
    let flip = tokio::spawn(async move { repo.flip(&request).await });
    wait_for_lock_wait(&h.pool, "FROM embedding_control WHERE singleton FOR SHARE").await;
    sqlx::query("COMMIT").execute(&mut *holder).await.unwrap();

    let out = flip.await.unwrap().unwrap();
    assert!(matches!(out, FlipOutcome::Stale | FlipOutcome::LockTimeout), "{out:?}");
    assert_eq!(h.state().await.active_slot, VectorSlot::A);
}

// ── rollback ──────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rollback_before_a_flip_cancels_and_deletes_the_partial_vectors() {
    let h = harness_or_skip!();
    h.on_bge(3).await;
    let sweep = h.sweep();
    sweep.pass(&me()).await;
    expect_written(&h.exec(&["start", "--no-wait"]).await);
    sweep.pass(&me()).await;
    assert_eq!(h.gemma.documents(), 3);

    let out = h.exec(&["rollback", "--no-wait"]).await;
    expect_written(&out);
    assert!(out.output.contains("cancels the start"), "{}", out.output);
    let intent = h.intent().await;
    assert_eq!((intent.target, intent.retire.as_deref()), (None, Some(GEMMA)));

    sweep.pass(&me()).await;
    for n in 1..=3 {
        assert_eq!(models(&h.pool, id(n)).await, (Some(BGE.into()), None), "row {n}");
    }
    let s = h.state().await;
    assert_eq!((s.active_slot, s.model_b), (VectorSlot::A, None));
    assert_eq!((h.gemma.documents(), h.bge.documents()), (3, 0));
    assert_eq!(phase(&sweep), Phase::Steady);
}

#[tokio::test]
async fn rollback_after_a_flip_flips_back_with_no_embedding_call() {
    let h = harness_or_skip!();
    let sweep = h.sweep();
    h.on_gemma(&sweep, 3).await;
    let documents = (h.bge.documents(), h.gemma.documents());
    let before = xmins(&h.pool).await;

    let out = h.exec(&["rollback", "--no-wait"]).await;
    expect_written(&out);
    assert!(out.output.contains("the rollback is instant"), "{}", out.output);
    sweep.pass(&me()).await;

    assert_eq!(h.state().await.active_slot, VectorSlot::A);
    assert_eq!((h.bge.documents(), h.gemma.documents()), documents, "the rollback embedded");
    assert_eq!(xmins(&h.pool).await, before, "the rollback wrote a memory row");
    assert_eq!(h.set.for_unit(UNIT).unwrap().embedder.id(), BGE);
}

#[tokio::test]
async fn rollback_after_retire_began_refills_before_flipping_back() {
    let h = harness_or_skip!();
    let sweep = h.sweep();
    h.on_gemma(&sweep, 3).await;
    age_the_flip(&h.pool).await;
    sweep.pass(&me()).await;

    expect_written(&h.exec(&["retire", "--no-wait"]).await);
    assert_eq!(h.intent().await.retire.as_deref(), Some(BGE));
    sweep.pass(&me()).await;
    assert_eq!(h.state().await.model_a, None, "retire left slot A named");
    for n in 1..=3 {
        assert_eq!(models(&h.pool, id(n)).await, (None, Some(GEMMA.into())), "row {n}");
    }
    assert_eq!(h.bge.documents(), 0);

    let out = h.exec(&["rollback", "--no-wait"]).await;
    expect_written(&out);
    assert!(out.output.contains("retire has deleted"), "{}", out.output);
    sweep.pass(&me()).await;
    assert_eq!(h.bge.documents(), 3, "the refill embeds every row once");
    assert_eq!(h.state().await.active_slot, VectorSlot::B, "flipped back before the refill");
    for n in 1..=3 {
        assert_eq!(models(&h.pool, id(n)).await.0.as_deref(), Some(BGE), "row {n}");
    }
    sweep.pass(&me()).await;
    assert_eq!(h.state().await.active_slot, VectorSlot::A);
    assert_eq!(h.bge.documents(), 3);
}

// ── retire ────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn retire_is_refused_inside_the_window() {
    let h = harness_or_skip!();
    let sweep = h.sweep();
    h.on_gemma(&sweep, 2).await;
    let generation = h.intent().await.generation;
    let out = h.exec(&["retire", "--no-wait"]).await;
    assert_eq!(out.code, 1, "{}", out.output);
    assert!(out.output.contains("rollback window closes"), "{}", out.output);
    assert_eq!(h.intent().await.generation, generation);
}

/// A row the active model refuses three times counts as failed, and retire would delete the only
/// vector it has.
#[tokio::test]
async fn retire_is_refused_while_a_unit_has_a_failed_row() {
    let h = harness_or_skip!();
    let sweep = h.sweep();
    h.on_gemma(&sweep, 2).await;
    age_the_flip(&h.pool).await;
    insert(&h.pool, 99, Some(BGE), None).await;
    h.gemma.refuse(&text(99));
    for _ in 0..3 {
        sweep.pass(&me()).await;
    }

    let generation = h.intent().await.generation;
    let out = h.exec(&["retire", "--no-wait"]).await;
    assert_eq!(out.code, 1, "{}", out.output);
    assert!(out.output.contains("has 1 failed rows"), "{}", out.output);
    assert_eq!(h.intent().await.generation, generation);
    assert_eq!(models(&h.pool, id(99)).await.0.as_deref(), Some(BGE), "the fallback vector went");
}

/// An active hole does not refuse the command. The sweep fills it before the first retire batch,
/// and until then deletes nothing.
#[tokio::test]
async fn retire_waits_for_an_active_hole_before_it_deletes() {
    let h = harness_or_skip!();
    let sweep = h.sweep();
    h.on_gemma(&sweep, 2).await;
    age_the_flip(&h.pool).await;
    insert(&h.pool, 99, Some(BGE), None).await;
    h.gemma.refuse(&text(99));
    sweep.pass(&me()).await;

    expect_written(&h.exec(&["retire", "--no-wait"]).await);
    sweep.pass(&me()).await;
    for n in [1, 2, 99] {
        assert_eq!(models(&h.pool, id(n)).await.0.as_deref(), Some(BGE), "row {n} lost bge");
    }
    assert_eq!(h.state().await.model_a.as_deref(), Some(BGE));

    h.gemma.accept_all();
    sweep.pass(&me()).await;
    assert_eq!(models(&h.pool, id(99)).await, (None, Some(GEMMA.into())));
    for n in [1, 2] {
        assert_eq!(models(&h.pool, id(n)).await, (None, Some(GEMMA.into())), "row {n}");
    }
    assert_eq!(h.state().await.model_a, None);
}

// ── drift, status, boot, env mode ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_command_against_a_drifted_config_is_refused() {
    let h = harness_or_skip!();
    h.on_bge(2).await;
    // A server that booted on bge alone and passed a moment ago.
    h.repo
        .publish(&Published {
            applied_generation: 0,
            server_models: vec![BGE.into()],
            status: None,
            seen_at: None,
        })
        .await
        .unwrap();
    for verb in ["start", "flip", "rollback", "retire"] {
        let out = h.exec(&[verb, "--no-wait"]).await;
        assert_eq!(out.code, 1, "{verb}: {}", out.output);
        assert!(out.output.contains("refused: this command computes"), "{verb}: {}", out.output);
    }
    assert_eq!(h.intent().await.generation, 0);
}

#[tokio::test]
async fn status_warns_when_no_pass_ran_recently() {
    let h = harness_or_skip!();
    h.on_bge(2).await;
    h.sweep().pass(&me()).await;
    let fresh = h.exec(&["status"]).await;
    assert_eq!(fresh.code, 0);
    assert!(!fresh.output.contains("no embedding pass ran"), "{}", fresh.output);
    assert!(fresh.output.contains("unit me: steady"), "{}", fresh.output);

    CLOCK_OFFSET_SECS.store(3600, Ordering::SeqCst);
    let stale = h.exec(&["status"]).await;
    assert_eq!(stale.code, 0);
    assert!(stale.output.contains("no embedding pass ran in the last 90 s"), "{}", stale.output);
}

#[tokio::test]
async fn boot_refuses_a_target_no_block_configures() {
    let h = harness_or_skip!();
    h.on_bge(2).await;
    let sweep = h.sweep();
    sweep.pass(&me()).await;
    let change = |_: &Intent, _: &[UnitState]| -> Result<Option<IntentChange>, String> {
        Ok(Some(IntentChange {
            target: Some(WITHDRAWN.into()),
            flip: false,
            retire: None,
            verb: Verb::Start,
        }))
    };
    let written = h.repo.change_intent(&change).await.unwrap();
    assert!(matches!(written, ChangeOutcome::Written(_)), "{written:?}");

    let intent = h.intent().await;
    let blocks = [GEMMA.to_string(), BGE.to_string()];
    let err = configured_from_intent(&intent, &blocks, Some(BGE), 7, &BTreeMap::new()).unwrap_err();
    assert!(err.contains(WITHDRAWN) && err.contains("embeddings rollback"), "{err}");

    // The server reports the refusal where the operator reads, and does nothing else.
    sweep.pass(&me()).await;
    let status = h.exec(&["status"]).await;
    assert!(
        status.output.contains("error: ") && status.output.contains(WITHDRAWN),
        "{}",
        status.output
    );
    assert_eq!(h.state().await.model_b, None, "the sweep acted on a target it cannot build");

    // The fix the message names: rollback moves the target off the withdrawn model.
    expect_written(&h.exec(&["rollback", "--no-wait"]).await);
    let intent = h.intent().await;
    assert!(configured_from_intent(&intent, &blocks, Some(BGE), 7, &BTreeMap::new()).is_ok());
}

/// The fork's mode: a full move from `.env` with a pass per step, and the control row exactly as
/// `migrate` left it.
#[tokio::test]
async fn env_mode_leaves_the_control_row_untouched() {
    let h = harness_or_skip!();
    h.on_bge(3).await;
    let configured = Configured {
        current: GEMMA.into(),
        previous: Some(BGE.into()),
        retire: None,
        flip: FlipScope::All,
        rollback_days: 7,
        guessed_acting: BTreeMap::new(),
        generation: None,
    };
    let sweep = Sweep::new(
        h.repo.clone(),
        Arc::clone(&h.set),
        Arc::new(NoOpener),
        Knobs {
            fill_chars: 4000,
            fill_duty: 100,
            floor_bytes: 0,
            fill_budget: Duration::from_secs(60),
        },
        Steer::Env(configured),
        false,
        None,
    );
    sweep.pass(&me()).await;
    sweep.pass(&me()).await;
    assert_eq!(h.state().await.active_slot, VectorSlot::B, "the env-mode move did not finish");

    let mut env_cfg = (*h.cfg).clone();
    env_cfg.embed.migrate.control = ControlMode::Env;
    let env = h.env_with(Arc::new(env_cfg), Arc::new(answering));
    for verb in ["status", "start", "flip", "rollback", "retire"] {
        let out = execute(&[verb.to_string()], &env).await.unwrap();
        assert_eq!(out.code, 2, "{verb}: {}", out.output);
    }

    let row = sqlx::query(
        "SELECT generation, applied_generation, server_seen_at IS NULL AS unseen,
                cardinality(server_models) AS models, server_status IS NULL AS no_status
           FROM embedding_control",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("generation"), 0);
    assert_eq!(row.get::<i64, _>("applied_generation"), 0);
    assert!(row.get::<bool, _>("unseen"));
    assert_eq!(row.get::<i32, _>("models"), 0);
    assert!(row.get::<bool, _>("no_status"));
}
