//! Write-time conflict pairs, plan E1-T4: the triggers, the sweep and the scan against a real
//! Postgres, skipped when none is reachable, in the shape `tests/review_queue.rs` uses.
//!
//! Every event test ends in `assert_parity`: one sweep, then the stored-pair read must return the
//! rows the self-join returns today, bit for bit and in the same order. The stored read is spec 4.9
//! and stays a test constant until E2 moves it into the adapter.
//!
//! Rows that need an exact similarity go in through SQL with hand-built vectors. Rows that test the
//! write path go through `write::run`, whose hash embedder scores the fixture wording at about 0.91,
//! above the 0.90 conflict floor and below the 0.97 band where `write::run` folds a near-duplicate
//! into the row it already has.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::adapters::postgres::conflict_wake;
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal, Sensitivity};
use lumberroom_server::ports::{MemoryRepository, RestoreRow};
use lumberroom_server::services::conflicts::{self, Wakes};
use lumberroom_server::services::{forget, review, write, Ctx, Repos};
use sqlx::postgres::PgListener;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

mod common;

/// A database of its own rather than the suite's shared `lumberroom_rust_test`. Two assertions here
/// read database-wide state, the `memory_conflict` channel and `pg_stat_database.deadlocks`, and a
/// branch that applied a later migration to the shared database makes every test here skip.
const TEST_DB: &str = "lumberroom_rust_test_conflicts";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";

/// `CONFLICT_THRESHOLD`'s default. Every sweep here passes it explicitly so a test never depends on
/// what the environment set.
const FLOOR: f64 = 0.90;
const DIM: usize = 768;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Today's `CONFLICTS_SQL` (`src/adapters/postgres/memory.rs`, before E2), kept here as the oracle.
/// E2 replaces the adapter's body with the stored read; this copy must not follow it.
const SELF_JOIN_SQL: &str = "SELECT a.id AS older_id, b.id AS newer_id,
                    (1 - (a.embedding <=> b.embedding))::float8 AS similarity
               FROM memory a
               JOIN memory b
                 ON b.tenant_id = a.tenant_id
                AND b.namespace = a.namespace
                AND (a.created_at, a.id) < (b.created_at, b.id)
              WHERE a.tenant_id = $1
                AND a.superseded_by IS NULL
                AND (a.occurred_until IS NULL OR a.occurred_until > now())
                AND b.superseded_by IS NULL
                AND (b.occurred_until IS NULL OR b.occurred_until > now())
                AND a.embedding IS NOT NULL AND b.embedding IS NOT NULL
                AND 1 - (a.embedding <=> b.embedding) >= $2
                AND EXISTS (
                      SELECT 1
                        FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
                       WHERE CASE WHEN g.exact THEN a.namespace = g.prefix
                                  ELSE left(a.namespace, length(g.prefix)) = g.prefix END
                         AND sensitivity_rank(g.max) >= sensitivity_rank(a.sensitivity)
                    )
                AND EXISTS (
                      SELECT 1
                        FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
                       WHERE CASE WHEN g.exact THEN b.namespace = g.prefix
                                  ELSE left(b.namespace, length(g.prefix)) = g.prefix END
                         AND sensitivity_rank(g.max) >= sensitivity_rank(b.sensitivity)
                    )
                AND NOT EXISTS (
                      SELECT 1 FROM memory_pair_dismissed d
                       WHERE d.tenant_id = a.tenant_id
                         AND d.lo_id = least(a.id, b.id)
                         AND d.hi_id = greatest(a.id, b.id)
                    )
              ORDER BY similarity DESC, a.created_at, a.id, b.id
              LIMIT $3 OFFSET $4";

/// Spec 4.9, the read E2 ships, cut to the three columns parity compares.
const STORED_SQL: &str = "SELECT a.id AS older_id, b.id AS newer_id, c.similarity
  FROM memory_conflict c
  JOIN memory a ON a.tenant_id = c.tenant_id AND a.id = c.older_id
  JOIN memory b ON b.tenant_id = c.tenant_id AND b.id = c.newer_id
 WHERE c.tenant_id = $1
   AND c.similarity >= $2
   AND a.namespace = b.namespace
   AND a.superseded_by IS NULL
   AND (a.occurred_until IS NULL OR a.occurred_until > now())
   AND b.superseded_by IS NULL
   AND (b.occurred_until IS NULL OR b.occurred_until > now())
   AND EXISTS (
         SELECT 1
           FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
          WHERE CASE WHEN g.exact THEN a.namespace = g.prefix
                     ELSE left(a.namespace, length(g.prefix)) = g.prefix END
            AND sensitivity_rank(g.max) >= sensitivity_rank(a.sensitivity)
       )
   AND EXISTS (
         SELECT 1
           FROM unnest($5::text[], $6::bool[], $7::text[]) AS g(prefix, exact, max)
          WHERE CASE WHEN g.exact THEN b.namespace = g.prefix
                     ELSE left(b.namespace, length(g.prefix)) = g.prefix END
            AND sensitivity_rank(g.max) >= sensitivity_rank(b.sensitivity)
       )
   AND NOT EXISTS (
         SELECT 1 FROM memory_pair_dismissed d
          WHERE d.tenant_id = c.tenant_id
            AND d.lo_id = least(a.id, b.id)
            AND d.hi_id = greatest(a.id, b.id)
       )
 ORDER BY c.similarity DESC, a.created_at, a.id, b.id
 LIMIT $3 OFFSET $4";

/// The pattern test 15 runs in SQL. `\y` and never `\b`: a Postgres regex reads `\b` as a backspace,
/// and with it the pattern matched no function at all.
const INSERTS_PAIRS_RE: &str = r"INSERT\s+INTO\s+(public\.)?memory_conflict\y";

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

struct Harness {
    ctx: Ctx,
    pool: PgPool,
    url: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

impl Harness {
    fn tenant(&self) -> &str {
        &self.ctx.cfg.tenant_id
    }

    fn repo(&self) -> Arc<dyn MemoryRepository> {
        Arc::clone(&self.ctx.repos.memories)
    }
}

async fn setup() -> Option<Harness> {
    let guard = SERIAL.lock().await;
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
    std::env::set_var(TEST_KEK_VAR, TEST_KEK_HEX);

    let db_lock = common::lock_database(&url).await?;
    let pool = step!("connecting to the test database", postgres::connect(&url).await);
    step!("migrating the test database", postgres::migrate(&pool).await);
    // CASCADE reaches memory_conflict and memory_conflict_scan through their foreign keys.
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

    let cfg: Config = step!("loading the config", config::load());
    let keys: Arc<dyn KeyProvider> = Arc::new(EnvKeyProvider::new(TEST_KEK_VAR, TEST_KEK_ID));
    let kek = step!("reading the test key", keys.kek().await);
    let check = postgres::verify_kek(
        &pool,
        &cfg.tenant_id,
        TEST_KEK_ID,
        &lumberroom_server::crypto::kek::fingerprint(&kek),
        keys.provider(),
    )
    .await;
    let check = step!("verifying the test key", check);
    let kek_verified = !matches!(check, postgres::KekCheck::Mismatch { .. });

    let ctx = ctx_on(&pool, Arc::new(cfg), keys, kek_verified);
    lumberroom_server::services::bootstrap::clear_cache();
    Some(Harness { ctx, pool, url, _serial: guard, _db: db_lock })
}

fn ctx_on(pool: &PgPool, cfg: Arc<Config>, keys: Arc<dyn KeyProvider>, kek_verified: bool) -> Ctx {
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
    Ctx {
        cfg,
        repos,
        embedder: Arc::new(HashEmbedder::new(DIM)),
        keys: Some(keys),
        kek_verified,
        principal: Principal {
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
        },
        invocation: Invocation::Cli,
        session_id: Some("test-session".into()),
    }
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

/// Aborts spawned sweepers and listeners when a test ends, panics included. A sweeper left running
/// would scan the next test's rows in the middle of its setup.
struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Vectors and rows
// ---------------------------------------------------------------------------------------------

fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= n);
    v
}

/// A vector built from a few named axes, so a test can set a similarity exactly: `axis(&[(0, 1.0)])`
/// against `axis(&[(0, 0.95), (1, 0.3122)])` is 0.95.
fn axis(parts: &[(usize, f32)]) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    for (i, x) in parts {
        v[*i] = *x;
    }
    normalise(v)
}

/// A unit vector from a seed. Two of these sit near cosine 0 in 768 dimensions, so rows built this
/// way pair with nothing.
fn noise(seed: u64) -> Vec<f32> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let v = (0..DIM)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32
        })
        .collect();
    normalise(v)
}

/// `base` nudged by a little noise. Two of these from one base score about 0.96.
fn near(base: &[f32], seed: u64) -> Vec<f32> {
    let n = noise(seed);
    normalise(base.iter().zip(&n).map(|(b, e)| b + 0.2 * e).collect())
}

/// One live open row, inserted the way a direct `psql` insert would be: the wake trigger fires and
/// nothing else runs. `age_secs` backdates `created_at` so "oldest" never races the clock.
async fn insert_row<'e, E>(exec: E, tenant: &str, ns: &str, emb: &[f32], age_secs: i64) -> Uuid
where
    E: sqlx::PgExecutor<'e>,
{
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, embedding, source_client,
                             embedding_model, sensitivity, created_at)
         VALUES ($1, $2, $3, $4, $5, 'test', 'hash', 'open',
                 now() - make_interval(secs => $6::float8))",
    )
    .bind(id)
    .bind(tenant)
    .bind(ns)
    .bind(format!("fixture row {id}"))
    .bind(pgvector::Vector::from(emb.to_vec()))
    .bind(age_secs as f64)
    .execute(exec)
    .await
    .unwrap();
    id
}

/// Two wordings the hash embedder scores at about 0.91: a conflict, never a duplicate.
fn rota_pair(tag: &str) -> (String, String) {
    let a = format!(
        "the {tag} rota starts on Monday at nine and runs through Friday at five for the whole \
         platform team zq{tag}zq"
    );
    let b = a.replace("Monday", "Tuesday").replace("nine", "ten");
    (a, b)
}

async fn similarity_of(ctx: &Ctx, a: &str, b: &str) -> f64 {
    let v = ctx.embedder.embed_documents(vec![a.to_string(), b.to_string()]).await.unwrap();
    v[0].iter().zip(&v[1]).map(|(x, y)| (*x as f64) * (*y as f64)).sum()
}

async fn write_at(ctx: &Ctx, content: &str, ns: &str) -> Uuid {
    let out = write::run(ctx, content, ns, None, None, None, None).await.unwrap();
    Uuid::parse_str(&out.id).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

fn full_grant() -> (Vec<String>, Vec<bool>, Vec<String>) {
    (vec![String::new()], vec![false], vec!["sealed".to_string()])
}

async fn pairs_by(
    pool: &PgPool,
    sql: &'static str,
    tenant: &str,
    floor: f64,
) -> Vec<(Uuid, Uuid, f64)> {
    let (prefix, exact, max) = full_grant();
    sqlx::query_as(sql)
        .bind(tenant)
        .bind(floor)
        .bind(1_000_000i64)
        .bind(0i64)
        .bind(&prefix)
        .bind(&exact)
        .bind(&max)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// One sweep, then the stored read must equal the self-join row for row, similarity included to
/// the bit. A sweep that leaves rows pending would make the comparison prove nothing, so that fails
/// first.
async fn assert_parity(h: &Harness, tenant: &str, floor: f64) {
    let report =
        conflicts::sweep(h.repo().as_ref(), tenant, floor, Duration::from_secs(10)).await.unwrap();
    assert_eq!(report.pending, 0, "the parity sweep left rows pending: {report:?}");
    let stored = pairs_by(&h.pool, STORED_SQL, tenant, floor).await;
    let joined = pairs_by(&h.pool, SELF_JOIN_SQL, tenant, floor).await;
    assert_eq!(stored, joined, "stored pairs differ from the self-join at floor {floor}");
}

async fn pair_stored(pool: &PgPool, a: Uuid, b: Uuid) -> Option<f64> {
    sqlx::query_scalar(
        "SELECT similarity FROM memory_conflict
          WHERE (older_id = $1 AND newer_id = $2) OR (older_id = $2 AND newer_id = $1)",
    )
    .bind(a)
    .bind(b)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn pairs_naming(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM memory_conflict WHERE older_id = $1 OR newer_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn marked(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM memory_conflict_scan WHERE memory_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
        == 1
}

async fn listener(url: &str) -> PgListener {
    let mut l = PgListener::connect(url).await.unwrap();
    l.listen(conflict_wake::CONFLICT_WAKE_CHANNEL).await.unwrap();
    l
}

/// The next notification's payload, or None when nothing arrives inside `wait`.
async fn next_wake(l: &mut PgListener, wait: Duration) -> Option<String> {
    match tokio::time::timeout(wait, l.recv()).await {
        Ok(n) => Some(n.unwrap().payload().to_string()),
        Err(_) => None,
    }
}

async fn wait_for_pair(pool: &PgPool, a: Uuid, b: Uuid, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if pair_stored(pool, a, b).await.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error().and_then(|d| d.code()).map(|c| c.to_string())
}

// ---------------------------------------------------------------------------------------------
// 1. Two writes at once
// ---------------------------------------------------------------------------------------------

/// Spec section 6: each scan starts after its own row committed, so of two rows written at the
/// same moment at least one scan sees the other. A scan inside the inserting transaction loses that
/// and drops the pair in some rounds, which is why there are twenty.
#[tokio::test]
async fn two_concurrent_writes_in_one_namespace_both_find_their_pair() {
    let h = harness_or_skip!();
    let mut rounds = Vec::new();
    for round in 0..20 {
        let (a, b) = rota_pair(&format!("r{round}"));
        let sim = similarity_of(&h.ctx, &a, &b).await;
        assert!((FLOOR..0.97).contains(&sim), "fixture wording scored {sim}");
        // A namespace per round: the wording repeats across rounds and would pair across them.
        let ns = format!("project:round-{round}");
        let (x, y) = tokio::join!(write_at(&h.ctx, &a, &ns), write_at(&h.ctx, &b, &ns));
        assert_ne!(x, y, "round {round} folded the second write into the first");
        rounds.push((x, y));
    }

    let report = conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(report.pending, 0, "{report:?}");
    for (round, (x, y)) in rounds.iter().enumerate() {
        assert!(pair_stored(&h.pool, *x, *y).await.is_some(), "round {round} lost its pair");
        assert!(marked(&h.pool, *x).await && marked(&h.pool, *y).await, "round {round} unmarked");
    }
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 2. The wake fires at commit only
// ---------------------------------------------------------------------------------------------

/// Postgres holds a notification until commit, drops it on rollback and folds duplicates inside a
/// transaction. The sweeper relies on all three: it never hears of a row before it can see it, and
/// a bulk insert costs one wake.
#[tokio::test]
async fn an_insert_notifies_its_tenant_at_commit_and_not_on_rollback() {
    let h = harness_or_skip!();
    let tenant = "tenant-bulk-wake";
    let mut l = listener(&h.url).await;

    let mut tx = h.pool.begin().await.unwrap();
    for i in 0..300u64 {
        insert_row(&mut *tx, tenant, "global", &noise(1000 + i), 0).await;
    }
    assert_eq!(next_wake(&mut l, Duration::from_millis(300)).await, None, "a wake before commit");
    tx.commit().await.unwrap();
    assert_eq!(next_wake(&mut l, Duration::from_secs(2)).await.as_deref(), Some(tenant));
    assert_eq!(next_wake(&mut l, Duration::from_millis(500)).await, None, "more than one wake");

    let mut tx = h.pool.begin().await.unwrap();
    insert_row(&mut *tx, tenant, "global", &noise(5000), 0).await;
    tx.rollback().await.unwrap();
    assert_eq!(next_wake(&mut l, Duration::from_millis(500)).await, None, "a rollback woke");

    assert_parity(&h, tenant, FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 3. Archive restore
// ---------------------------------------------------------------------------------------------

fn restore_row(tenant: &str, ns: &str, emb: Vec<f32>, age: chrono::Duration) -> RestoreRow {
    let id = Uuid::new_v4();
    RestoreRow {
        tenant_id: tenant.to_string(),
        id,
        namespace: ns.to_string(),
        content: format!("restored row {id}"),
        embedding: emb,
        tags: vec![],
        source_client: "test".into(),
        embedding_model: "hash".into(),
        sensitivity: Sensitivity::Open,
        sealed: None,
        supersedes: None,
        superseded_by: None,
        superseded_at: None,
        occurred_at: None,
        occurred_until: None,
        access_count: 0,
        last_accessed_at: None,
        last_confirmed_at: None,
        created_at: chrono::Utc::now() - age,
    }
}

/// Archives carry rows and no pairs, so every restored row starts pending and the sweeper rebuilds
/// its pairs.
#[tokio::test]
async fn a_restored_row_is_pending_until_the_sweeper_scans_it() {
    let h = harness_or_skip!();
    let base = noise(7);
    let a = restore_row(h.tenant(), "global", near(&base, 1), chrono::Duration::hours(2));
    let b = restore_row(h.tenant(), "global", near(&base, 2), chrono::Duration::hours(1));
    let (ida, idb) = (a.id, b.id);
    h.repo().restore_row(a).await.unwrap();
    h.repo().restore_row(b).await.unwrap();

    let everything = NamespaceGrant::everything();
    let pending = h.repo().conflicts_pending(h.tenant(), FLOOR, &everything).await.unwrap();
    assert_eq!(pending, 2);

    let report = conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(report.pending, 0);
    assert_eq!(h.repo().conflicts_pending(h.tenant(), FLOOR, &everything).await.unwrap(), 0);
    assert!(pair_stored(&h.pool, ida, idb).await.is_some());
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 4. Revival
// ---------------------------------------------------------------------------------------------

/// A row written while A was expired never paired with A, because the scan skips retired rows.
/// Reviving A has to drop A's mark and wake the sweeper, or the pair never appears.
#[tokio::test]
async fn a_revived_row_wakes_and_pairs_with_rows_written_while_it_was_retired() {
    let h = harness_or_skip!();
    let ns = "project:revive";
    let (text_a, text_b) = rota_pair("revive");
    let a = write_at(&h.ctx, &text_a, ns).await;
    conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10)).await.unwrap();
    assert!(marked(&h.pool, a).await);

    let expired = review::expire(&h.ctx, &a.to_string()).await.unwrap();
    let b = write_at(&h.ctx, &text_b, ns).await;
    conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10)).await.unwrap();
    assert!(pair_stored(&h.pool, a, b).await.is_none(), "a retired row paired");

    let mut l = listener(&h.url).await;
    assert!(review::unexpire(&h.ctx, &a.to_string(), expired.until).await.unwrap());
    assert_eq!(next_wake(&mut l, Duration::from_secs(2)).await.as_deref(), Some(h.tenant()));
    assert!(!marked(&h.pool, a).await, "the revived row kept its mark");
    let everything = NamespaceGrant::everything();
    assert_eq!(h.repo().conflicts_pending(h.tenant(), FLOOR, &everything).await.unwrap(), 1);

    conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10)).await.unwrap();
    assert!(pair_stored(&h.pool, a, b).await.is_some(), "the revived row never paired");
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 5. Budget
// ---------------------------------------------------------------------------------------------

/// The marks are the progress record, so a sweep cut short by its budget loses nothing and the next
/// one starts at the oldest unmarked row.
#[tokio::test]
async fn a_sweep_stops_on_its_budget_and_resumes() {
    let h = harness_or_skip!();
    for i in 0..300u64 {
        insert_row(&h.pool, h.tenant(), "global", &noise(20_000 + i), 0).await;
    }
    let first = conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_millis(1))
        .await
        .unwrap();
    assert!(first.scanned >= conflicts::SWEEP_BATCH, "{first:?}");
    assert!(first.pending > 0, "a 1 ms budget swept everything: {first:?}");
    assert_eq!(first.scanned + first.pending, 300, "{first:?}");

    let second = conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(second.pending, 0, "{second:?}");
    assert_eq!(second.scanned, first.pending, "the second sweep rescanned marked rows");
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 6. Threshold lowered
// ---------------------------------------------------------------------------------------------

/// A mark above the floor counts as missing. Without that rule a lowered `CONFLICT_THRESHOLD` would
/// never find the pairs between the new floor and the old one.
#[tokio::test]
async fn lowering_the_threshold_turns_every_row_pending() {
    let h = harness_or_skip!();
    let a = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 1.0)]), 30).await;
    let b = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 0.95), (1, 0.3122)]), 20).await;
    let c = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 0.87), (2, 0.493)]), 10).await;
    conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10)).await.unwrap();
    assert!(pair_stored(&h.pool, a, b).await.is_some());
    assert!(pair_stored(&h.pool, a, c).await.is_none());

    let everything = NamespaceGrant::everything();
    assert_eq!(h.repo().conflicts_pending(h.tenant(), 0.85, &everything).await.unwrap(), 3);
    assert_eq!(h.repo().conflicts_pending(h.tenant(), FLOOR, &everything).await.unwrap(), 0);

    assert_parity(&h, h.tenant(), 0.85).await;
    let low = pair_stored(&h.pool, a, c).await.expect("the 0.87 pair after the rescan");
    assert!((0.85..FLOOR).contains(&low), "{low}");
    assert!(pair_stored(&h.pool, b, c).await.is_none(), "0.83 is under both floors");
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 7. Vector changed
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_changed_vector_drops_the_row_s_pairs_and_wakes() {
    let h = harness_or_skip!();
    let a = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 1.0)]), 30).await;
    let b = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 0.97), (1, 0.2431)]), 20).await;
    let c = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 0.97), (2, 0.2431)]), 10).await;
    conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10)).await.unwrap();
    assert!(pair_stored(&h.pool, a, b).await.is_some());
    assert!(pair_stored(&h.pool, a, c).await.is_some());
    assert!(pair_stored(&h.pool, b, c).await.is_some());

    let mut l = listener(&h.url).await;
    sqlx::query("UPDATE memory SET embedding = $2 WHERE id = $1")
        .bind(a)
        .bind(pgvector::Vector::from(axis(&[(5, 1.0)])))
        .execute(&h.pool)
        .await
        .unwrap();

    assert_eq!(pairs_naming(&h.pool, a).await, 0, "pairs on the old vector survived");
    assert!(!marked(&h.pool, a).await, "the re-embedded row kept its mark");
    assert!(pair_stored(&h.pool, b, c).await.is_some(), "a pair without the row was dropped");
    assert!(marked(&h.pool, b).await && marked(&h.pool, c).await);
    assert_eq!(next_wake(&mut l, Duration::from_secs(2)).await.as_deref(), Some(h.tenant()));
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 8. Forget
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn forgetting_a_row_cascades_its_pairs_and_mark() {
    let h = harness_or_skip!();
    let (text_a, text_b) = rota_pair("forget");
    let a = write_at(&h.ctx, &text_a, "global").await;
    let b = write_at(&h.ctx, &text_b, "global").await;
    conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10)).await.unwrap();
    assert!(pair_stored(&h.pool, a, b).await.is_some());

    forget::by_id(&h.ctx, &a.to_string(), None, false).await.unwrap();
    assert_eq!(pairs_naming(&h.pool, a).await, 0);
    assert!(!marked(&h.pool, a).await);
    assert!(marked(&h.pool, b).await, "the surviving row lost its mark");
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 12. The exclusive key waits out a scan in flight
// ---------------------------------------------------------------------------------------------

/// A scan that started on A's old vector must commit before the re-embed clears A's pairs. Without
/// the exclusive advisory key in `memory_conflict_forget_row`, the DELETE runs first, sees no
/// committed pair, and the stale pair lands after it and stays.
#[tokio::test]
async fn a_vector_change_waits_for_a_scan_in_flight() {
    let h = harness_or_skip!();
    let a = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 1.0)]), 20).await;
    let b = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 0.95), (1, 0.3122)]), 10).await;

    let mut scan = PgConnection::connect(&h.url).await.unwrap();
    sqlx::query("BEGIN").execute(&mut scan).await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('memory_conflict:' || $1, 0))",
    )
    .bind(h.tenant())
    .execute(&mut scan)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_conflict (tenant_id, older_id, newer_id, similarity)
         VALUES ($1, $2, $3, 0.95)",
    )
    .bind(h.tenant())
    .bind(a)
    .bind(b)
    .execute(&mut scan)
    .await
    .unwrap();

    let pool = h.pool.clone();
    let mut update = tokio::spawn(async move {
        sqlx::query("UPDATE memory SET embedding = $2 WHERE id = $1")
            .bind(a)
            .bind(pgvector::Vector::from(axis(&[(5, 1.0)])))
            .execute(&pool)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut update).await.is_err(),
        "the re-embed did not wait for the scan in flight"
    );
    sqlx::query("COMMIT").execute(&mut scan).await.unwrap();
    update.await.unwrap().unwrap();

    assert_eq!(pairs_naming(&h.pool, a).await, 0, "a pair on the old vector survived");
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 13. Sweep and writes together
// ---------------------------------------------------------------------------------------------

async fn deadlocks(pool: &PgPool) -> i64 {
    sqlx::query("SELECT pg_stat_clear_snapshot()").execute(pool).await.unwrap();
    sqlx::query_scalar("SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The sweep swallows a failed record and logs it, so a deadlock inside it would not surface as an
/// error here. `pg_stat_database.deadlocks` counts it, and a backend flushes that counter when it
/// exits, so the writes and the sweep run on a pool of their own that closes before the read.
#[tokio::test]
async fn a_sweep_and_concurrent_writes_in_one_namespace_do_not_deadlock() {
    let h = harness_or_skip!();
    let ns = "project:busy";
    let template = "the deploy window for the billing service opens at nine and closes at eleven \
                    on every weekday morning after the standup ends {v} {v}";
    let texts: Vec<String> =
        (0..200).map(|i| template.replace("{v}", &format!("zqv{i}zq"))).collect();
    let vectors = h.ctx.embedder.embed_documents(texts).await.unwrap();
    for (i, v) in vectors.iter().enumerate() {
        insert_row(&h.pool, h.tenant(), ns, v, 1000 - i as i64).await;
    }

    let before = deadlocks(&h.pool).await;
    let side = postgres::connect(&h.url).await.unwrap();
    let ctx =
        ctx_on(&side, Arc::clone(&h.ctx.cfg), h.ctx.keys.clone().unwrap(), h.ctx.kek_verified);

    let mut tasks = tokio::task::JoinSet::new();
    let repo = Arc::clone(&ctx.repos.memories);
    let tenant = h.tenant().to_string();
    tasks.spawn(async move {
        conflicts::sweep(repo.as_ref(), &tenant, FLOOR, Duration::from_secs(10))
            .await
            .map(|_| ())
            .map_err(|e| format!("sweep: {e:?}"))
    });
    for j in 0..20 {
        let ctx = ctx.clone();
        let text = template.replace("{v}", &format!("zqw{j}zq"));
        tasks.spawn(async move {
            write::run(&ctx, &text, ns, None, None, None, None)
                .await
                .map(|_| ())
                .map_err(|e| format!("write {j}: {e:?}"))
        });
    }
    let mut errors = Vec::new();
    while let Some(r) = tasks.join_next().await {
        if let Err(e) = r.unwrap() {
            errors.push(e);
        }
    }
    drop(ctx);
    side.close().await;
    assert!(errors.is_empty(), "{errors:?}");

    // A backend reports its counters as it exits, a beat after the client closed it.
    let mut after = deadlocks(&h.pool).await;
    for _ in 0..20 {
        if after != before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        after = deadlocks(&h.pool).await;
    }
    assert_eq!(after, before, "Postgres detected a deadlock during the sweep");
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 14. lock_timeout breaks the cycle with a locked row
// ---------------------------------------------------------------------------------------------

/// Starts `memory_conflict_record(r)` on a connection of its own and returns once the scan is
/// waiting on a lock, so the caller knows the cycle is set before it revives anything.
async fn start_blocked_scan(
    h: &Harness,
    r: Uuid,
) -> tokio::task::JoinHandle<(Result<(), sqlx::Error>, Duration)> {
    let mut conn = PgConnection::connect(&h.url).await.unwrap();
    let pid: i32 =
        sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut conn).await.unwrap();
    let tenant = h.tenant().to_string();
    let scan = tokio::spawn(async move {
        let start = Instant::now();
        let res = sqlx::query("SELECT memory_conflict_record($1, $2, $3)")
            .bind(&tenant)
            .bind(r)
            .bind(FLOOR)
            .execute(&mut conn)
            .await
            .map(|_| ());
        (res, start.elapsed())
    });
    for _ in 0..100 {
        let waiting: Option<String> =
            sqlx::query_scalar("SELECT wait_event_type FROM pg_stat_activity WHERE pid = $1")
                .bind(pid)
                .fetch_optional(&h.pool)
                .await
                .unwrap()
                .flatten();
        if waiting.as_deref() == Some("Lock") {
            return scan;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the scan never waited on the locked row");
}

async fn assert_scan_timed_out(scan: tokio::task::JoinHandle<(Result<(), sqlx::Error>, Duration)>) {
    let (res, took) = scan.await.unwrap();
    let err = res.expect_err("the scan finished against a locked row");
    assert_eq!(sqlstate(&err).as_deref(), Some("55P03"), "{err:?}");
    assert!(took < Duration::from_secs(1), "the scan took {took:?}");
}

/// Two shapes reach the cycle in spec 4.4: the engine delete path, which locks the doomed row and
/// revives its predecessor, and any bare row lock followed by an unexpire. Without the scan's
/// `lock_timeout`, Postgres's detector waits a second and may kill the user's transaction.
#[tokio::test]
async fn a_revive_behind_a_locked_row_commits_while_a_scan_waits() {
    let h = harness_or_skip!();
    let base = axis(&[(0, 1.0)]);
    let similar = axis(&[(0, 0.95), (1, 0.3122)]);

    // Shape one: SELECT supersedes ... FOR UPDATE, then the predecessor revived, then the delete.
    let ns = "project:delete-path";
    let r = insert_row(&h.pool, h.tenant(), ns, &similar, 40).await;
    let d = insert_row(&h.pool, h.tenant(), ns, &base, 30).await;
    let p = insert_row(&h.pool, h.tenant(), ns, &axis(&[(9, 1.0)]), 50).await;
    sqlx::query(
        "UPDATE memory SET superseded_by = $2, superseded_at = now(), occurred_until = now()
          WHERE id = $1",
    )
    .bind(p)
    .bind(d)
    .execute(&h.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE memory SET supersedes = $2 WHERE id = $1")
        .bind(d)
        .bind(p)
        .execute(&h.pool)
        .await
        .unwrap();

    let mut t = h.pool.begin().await.unwrap();
    sqlx::query("SELECT supersedes FROM memory WHERE tenant_id = $1 AND id = $2 FOR UPDATE")
        .bind(h.tenant())
        .bind(d)
        .execute(&mut *t)
        .await
        .unwrap();
    let scan = start_blocked_scan(&h, r).await;
    sqlx::query(
        "UPDATE memory SET superseded_by = NULL, superseded_at = NULL, occurred_until = NULL
          WHERE tenant_id = $1 AND superseded_by = $2 AND id = ANY($3)",
    )
    .bind(h.tenant())
    .bind(d)
    .bind(vec![p])
    .execute(&mut *t)
    .await
    .expect("the revive was killed");
    sqlx::query("DELETE FROM memory WHERE tenant_id = $1 AND id = $2")
        .bind(h.tenant())
        .bind(d)
        .execute(&mut *t)
        .await
        .unwrap();
    t.commit().await.expect("the delete path did not commit");
    assert_scan_timed_out(scan).await;
    assert!(!marked(&h.pool, r).await, "a failed scan left a mark");
    assert_parity(&h, h.tenant(), FLOOR).await;
    assert!(marked(&h.pool, r).await);

    // Shape two: a bare FOR UPDATE, then an unexpire.
    let ns = "project:bare-lock";
    let r = insert_row(&h.pool, h.tenant(), ns, &similar, 40).await;
    let d = insert_row(&h.pool, h.tenant(), ns, &base, 30).await;
    let e = insert_row(&h.pool, h.tenant(), ns, &axis(&[(9, 1.0)]), 50).await;
    sqlx::query("UPDATE memory SET occurred_until = now() - interval '1 hour' WHERE id = $1")
        .bind(e)
        .execute(&h.pool)
        .await
        .unwrap();

    let mut t = h.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM memory WHERE id = $1 FOR UPDATE")
        .bind(d)
        .execute(&mut *t)
        .await
        .unwrap();
    let scan = start_blocked_scan(&h, r).await;
    sqlx::query("UPDATE memory SET occurred_until = NULL WHERE id = $1")
        .bind(e)
        .execute(&mut *t)
        .await
        .expect("the unexpire was killed");
    t.commit().await.expect("the unexpire did not commit");
    assert_scan_timed_out(scan).await;
    assert!(!marked(&h.pool, r).await, "a failed scan left a mark");
    assert_parity(&h, h.tenant(), FLOOR).await;
    assert!(marked(&h.pool, r).await);
    assert!(pair_stored(&h.pool, r, d).await.is_some());
}

// ---------------------------------------------------------------------------------------------
// 15. The catalog
// ---------------------------------------------------------------------------------------------

/// Spec section 6: a scan inside a trigger on `memory` runs inside the inserting transaction, where
/// two concurrent writes miss each other. This is the one edit that would break the design, and
/// only the catalog can see it.
#[tokio::test]
async fn no_trigger_on_memory_scans_or_writes_pairs() {
    let h = harness_or_skip!();

    // The pattern must catch the one function that does insert pairs, or the check below is blind.
    let self_check: Vec<String> =
        sqlx::query_scalar("SELECT proname::text FROM pg_proc WHERE prosrc ~* $1 ORDER BY 1")
            .bind(INSERTS_PAIRS_RE)
            .fetch_all(&h.pool)
            .await
            .unwrap();
    assert!(
        self_check.iter().any(|p| p == "memory_conflict_record"),
        "the pattern matched {self_check:?}, not memory_conflict_record"
    );

    let triggers: Vec<String> = sqlx::query_scalar(
        "SELECT t.tgname::text FROM pg_trigger t
          WHERE t.tgrelid = 'memory'::regclass AND NOT t.tgisinternal ORDER BY 1",
    )
    .fetch_all(&h.pool)
    .await
    .unwrap();
    for wanted in ["memory_conflict_moved", "memory_conflict_revived", "memory_conflict_wake"] {
        assert!(triggers.iter().any(|t| t == wanted), "{wanted} is missing: {triggers:?}");
    }

    let offenders: Vec<String> = sqlx::query_scalar(
        "SELECT t.tgname || ' -> ' || p.proname
           FROM pg_trigger t
           JOIN pg_proc p ON p.oid = t.tgfoid
          WHERE t.tgrelid = 'memory'::regclass
            AND NOT t.tgisinternal
            AND (strpos(p.prosrc, 'memory_conflict_record') > 0 OR p.prosrc ~* $1)
          ORDER BY 1",
    )
    .bind(INSERTS_PAIRS_RE)
    .fetch_all(&h.pool)
    .await
    .unwrap();
    assert!(offenders.is_empty(), "triggers on memory that scan or write pairs: {offenders:?}");
}

// ---------------------------------------------------------------------------------------------
// 16 and 18. The loop
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_wake_records_a_write_s_pair_without_a_tick() {
    let h = harness_or_skip!();
    let wakes = Arc::new(Wakes::default());
    let feed = Arc::clone(&wakes);
    let listening = conflict_wake::listen(&h.pool, move |t| feed.wake(t)).await.unwrap();
    let sweeper = tokio::spawn(conflicts::run_loop(
        h.repo(),
        h.tenant().to_string(),
        FLOOR,
        Duration::from_secs(10),
        Duration::from_secs(3600),
        wakes,
    ));
    let _tasks = AbortOnDrop(vec![listening, sweeper]);

    let (text_a, text_b) = rota_pair("wake");
    let a = write_at(&h.ctx, &text_a, "global").await;
    let b = write_at(&h.ctx, &text_b, "global").await;
    assert!(wait_for_pair(&h.pool, a, b, Duration::from_secs(2)).await, "no pair within 2 s");
}

/// No listener at all: the timer alone has to find the rows, which is what happens to every wake
/// behind a pooler in transaction mode.
#[tokio::test]
async fn a_lost_wake_is_swept_on_the_next_tick() {
    let h = harness_or_skip!();
    let sweeper = tokio::spawn(conflicts::run_loop(
        h.repo(),
        h.tenant().to_string(),
        FLOOR,
        Duration::from_secs(10),
        Duration::from_secs(1),
        Arc::new(Wakes::default()),
    ));
    let _tasks = AbortOnDrop(vec![sweeper]);

    let (text_a, text_b) = rota_pair("tick");
    let a = write_at(&h.ctx, &text_a, "global").await;
    let b = write_at(&h.ctx, &text_b, "global").await;
    assert!(wait_for_pair(&h.pool, a, b, Duration::from_secs(3)).await, "no pair within 3 s");
}

// ---------------------------------------------------------------------------------------------
// 17. Bulk insert
// ---------------------------------------------------------------------------------------------

/// The write pays no scan. A scan inside the insert would hold the tenant's shared advisory key and
/// a lock on `memory_conflict` until commit; uncommitted pairs are invisible from outside, so the
/// lock table is where it shows.
#[tokio::test]
async fn a_bulk_insert_runs_no_scan_inside_its_transaction() {
    let h = harness_or_skip!();
    let base = noise(31);
    let mut tx = h.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *tx).await.unwrap();
    let mut ids = Vec::new();
    for i in 0..300u64 {
        ids.push(insert_row(&mut *tx, h.tenant(), "global", &near(&base, 40_000 + i), 0).await);
    }

    let held: Vec<String> = sqlx::query_scalar(
        "SELECT locktype || ' ' || coalesce(relation::regclass::text, '') || ' ' || mode
           FROM pg_locks
          WHERE pid = $1
            AND (locktype = 'advisory'
                 OR relation IN ('memory_conflict'::regclass, 'memory_conflict_scan'::regclass))",
    )
    .bind(pid)
    .fetch_all(&h.pool)
    .await
    .unwrap();
    assert!(held.is_empty(), "the inserting transaction holds conflict locks: {held:?}");
    let pairs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_conflict WHERE older_id = ANY($1) OR newer_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    let marks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_conflict_scan WHERE memory_id = ANY($1)")
            .bind(&ids)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!((pairs, marks), (0, 0));

    tx.commit().await.unwrap();
    assert_parity(&h, h.tenant(), FLOOR).await;
}

// ---------------------------------------------------------------------------------------------
// 19. A row that always fails
// ---------------------------------------------------------------------------------------------

/// A failing row comes back in every batch. The stop on a batch that commits no scan is what ends
/// the sweep; without it the sweep retries the row until its budget runs out, every tick.
#[tokio::test]
async fn a_row_that_fails_every_scan_ends_the_sweep_early() {
    let h = harness_or_skip!();
    let r = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 0.95), (1, 0.3122)]), 100).await;
    let d = insert_row(&h.pool, h.tenant(), "global", &axis(&[(0, 1.0)]), 50).await;
    // D carries a mark as a fixture, so R is the only row whose scan needs D's row lock.
    sqlx::query(
        "INSERT INTO memory_conflict_scan (memory_id, tenant_id, floor) VALUES ($1, $2, $3)",
    )
    .bind(d)
    .bind(h.tenant())
    .bind(FLOOR)
    .execute(&h.pool)
    .await
    .unwrap();
    let mut others = Vec::new();
    for i in 0..10u64 {
        others.push(insert_row(&h.pool, h.tenant(), "global", &noise(60_000 + i), 10).await);
    }

    let mut t = h.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM memory WHERE id = $1 FOR UPDATE")
        .bind(d)
        .execute(&mut *t)
        .await
        .unwrap();

    let start = Instant::now();
    let report = conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10))
        .await
        .unwrap();
    let took = start.elapsed();
    assert!(took < Duration::from_secs(2), "the sweep ran {took:?}");
    assert_eq!(report.scanned, 10, "{report:?}");
    assert_eq!(report.pending, 1, "{report:?}");
    for id in &others {
        assert!(marked(&h.pool, *id).await);
    }
    assert!(!marked(&h.pool, r).await);

    t.commit().await.unwrap();
    let report = conflicts::sweep(h.repo().as_ref(), h.tenant(), FLOOR, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(report, conflicts::SweepReport { scanned: 1, pending: 0 });
    assert!(marked(&h.pool, r).await);
    assert!(pair_stored(&h.pool, r, d).await.is_some());
    assert_parity(&h, h.tenant(), FLOOR).await;
}
