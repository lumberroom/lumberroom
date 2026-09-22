//! Acceptance suite for the review queue, Phase 8 T5. Against a real Postgres, skipped when none
//! is reachable, in the shape `tests/integration.rs` and `tests/cleanup.rs` use.
//!
//! Every test seeds through `write::run` and calls `review_queue::{queue, decide}` directly,
//! except the three that name the HTTP route: those go through a bound server the way
//! `tests/cleanup.rs` does, because the capability gate and the route wiring live in the router.
//!
//! This file will not compile until the wiring pass (plan W1) lands the HTTP routes on top of the
//! T0 lock. Implemented against that lock; not run.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use lumberroom_server::adapters::auth;
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::errors::{Kind, Result as DomainResult};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Memory, Principal, Sensitivity};
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::review_queue::{
    self, Decision, ProposalDecided, ProposalField, ProposalItem, ProposalSource, QueueQuery,
    Source, Verdict,
};
use lumberroom_server::services::{review, write, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";

/// Every test here truncates the shared test database, so they serialise themselves rather than
/// relying on `--test-threads=1` being remembered, the same reason `integration.rs` and
/// `cleanup.rs` each carry their own copy of this mutex.
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

struct Harness {
    ctx: Ctx,
    pool: PgPool,
    /// A live server on a loopback port, for the three tests that name the HTTP route. Everything
    /// else calls `review_queue` directly against `ctx`.
    base: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

impl Harness {
    async fn get(&self, path: &str) -> (u16, String) {
        let res = reqwest::Client::new()
            .get(format!("{}{path}", self.base))
            .bearer_auth("m".repeat(32))
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        (status, res.text().await.unwrap())
    }
}

/// Returns None when no database is reachable, so the suite skips rather than fails on a machine
/// without one. `tune` gets the loaded config before anything is built from it, the way
/// `integration.rs`'s `setup_with` hands it over, so `CONFLICT_SCAN_MAX` and the like are set on
/// the struct rather than through the process environment.
async fn setup(tune: impl FnOnce(&mut Config)) -> Option<Harness> {
    let guard = SERIAL.lock().await;
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let base_url = admin_url.rsplit_once('/')?.0.to_string();
    let admin = step!("connecting to the admin database", PgPool::connect(&admin_url).await);

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
    std::env::set_var(TEST_KEK_VAR, TEST_KEK_HEX);

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
    tune(&mut cfg);

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

    let memories = Arc::new(postgres::PgMemoryRepository::new(pool.clone()));
    let repos = Repos {
        memories: memories.clone(),
        registry: Arc::new(postgres::PgRegistryRepository::new(pool.clone())),
        tool_calls: Arc::new(postgres::PgToolCallRepository::new(pool.clone())),
        sealed: Some(Arc::new(postgres::PgSealedRepository::new(pool.clone()))),
        ciphertext: Some(memories),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
    };
    let cfg = Arc::new(cfg);
    let ctx = Ctx {
        cfg: cfg.clone(),
        repos: repos.clone(),
        embedder: Arc::new(HashEmbedder::new(768)),
        keys: Some(keys.clone()),
        kek_verified,
        principal: owner_like("mac"),
        invocation: Invocation::Cli,
        session_id: Some("test-session".into()),
    };
    lumberroom_server::services::bootstrap::clear_cache();

    let oauth: Arc<dyn OauthStore> = Arc::new(postgres::PgOauthStore::new(pool.clone()));
    let state = Arc::new(AppState {
        cleanup: Arc::new(postgres::PgCleanupRepository::new(pool.clone())),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        cfg: cfg.clone(),
        repos: repos.clone(),
        oauth: Arc::clone(&oauth),
        ingest: Arc::new(postgres::PgIngestRepository::new(pool.clone())),
        embedder: Arc::clone(&ctx.embedder),
        degraded_embedder: false,
        keys: ctx.keys.clone(),
        kek_verified: ctx.kek_verified,
        // The engine ships no proposal source. `source_proposal_on_an_engine_answers_400_source_not_filled`
        // is the test that leans on this being empty.
        proposals: Vec::new(),
    });
    let authenticator = auth::create(&ctx.cfg, Some(oauth)).ok()?;
    let app = lumberroom_server::http::router(state, authenticator);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr: SocketAddr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });

    Some(Harness { ctx, pool, base: format!("http://{addr}"), _serial: guard, _db: db_lock })
}

macro_rules! ctx_or_skip {
    () => {
        match setup(|_| {}).await {
            Some(h) => h,
            None => {
                eprintln!("skipping: no database reachable");
                return;
            }
        }
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

/// A second client with an explicit ceiling per namespace, the way `integration.rs`'s
/// `restricted_at` is built. Namespace alone is not a grant under the two-axis model, so most
/// grant tests here need to set the second axis.
fn restricted_at(
    ctx: &Ctx,
    read: &[(&str, Sensitivity)],
    write: &[(&str, Sensitivity)],
) -> Ctx {
    let grants = |spec: &[(&str, Sensitivity)]| -> Vec<NamespaceGrant> {
        spec.iter().map(|(ns, max)| NamespaceGrant::new((*ns).to_string(), *max)).collect()
    };
    let mut c = ctx.clone();
    c.principal = Principal {
        client: "narrow".into(),
        token_id: "narrow-test".into(),
        mode: "token",
        scopes: vec![],
        read: grants(read),
        write: grants(write),
        registry_write: false,
        sealed_capable: false,
        may_delete: false,
        may_ingest: false,
        may_read_history: false,
    };
    c
}

fn nonce(label: &str) -> String {
    format!("zqxnonce{label}zqx")
}

async fn write_at(ctx: &Ctx, content: &str, namespace: &str) -> String {
    write::run(ctx, content, namespace, None, None, None, None).await.unwrap().id
}

async fn write_dated(
    ctx: &Ctx,
    content: &str,
    namespace: &str,
    occurred_at: DateTime<Utc>,
) -> String {
    write::run(ctx, content, namespace, None, None, None, Some(occurred_at)).await.unwrap().id
}

async fn set_created_at(pool: &PgPool, id: &str, at: DateTime<Utc>) {
    sqlx::query("UPDATE memory SET created_at = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
}

/// Sets `occurred_at` straight in the row, bypassing `write::run`'s near-now fence: the fixture
/// needs a same-day value and the fence refuses that on an explicit write.
async fn set_occurred_at(pool: &PgPool, id: &str, at: DateTime<Utc>) {
    sqlx::query("UPDATE memory SET occurred_at = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
}

async fn set_last_confirmed_at(pool: &PgPool, id: &str, at: DateTime<Utc>) {
    sqlx::query("UPDATE memory SET last_confirmed_at = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
}

/// Never accessed, older than any `--days` window a test uses, so `stale` picks it up.
async fn make_stale(pool: &PgPool, id: &str) {
    sqlx::query(
        "UPDATE memory SET created_at = now() - interval '400 days', last_accessed_at = NULL,
                            last_confirmed_at = NULL
         WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

/// A near-duplicate pair: two writes over the same namespace, close enough in wording that the
/// hash embedder scores them as neighbours but not identical enough for `write::run`'s own
/// near-duplicate collapse (banded at `DEDUPE_THRESHOLD`, 0.97 by default) to fold the second into
/// the first. `older` is backdated an hour so ordering is never a race against the clock.
async fn conflict_pair(ctx: &Ctx, pool: &PgPool, namespace: &str, tag: &str) -> (String, String) {
    let older = write_at(
        ctx,
        &format!("the {tag} rota starts on Monday and runs through Friday {}", nonce(tag)),
        namespace,
    )
    .await;
    set_created_at(pool, &older, Utc::now() - Duration::hours(1)).await;
    let newer = write_at(
        ctx,
        &format!("the {tag} rota starts on Tuesday and runs through Friday {}", nonce(tag)),
        namespace,
    )
    .await;
    (older, newer)
}

/// A private row that will never open again: written and sealed properly, then its ciphertext is
/// corrupted in place. `envelope::open` fails the GCM tag and `services::decrypt` drops it, which
/// is the one way to get an unopenable row past migration 008's content-representation check.
async fn break_ciphertext(pool: &PgPool, id: &str) {
    sqlx::query("UPDATE memory SET content_ct = '\\xdeadbeef'::bytea WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

/// A row inserted straight into the table at `open`, bypassing `write::run`'s refusal of empty
/// content. `write::run` never produces this shape; the empty-content test needs it anyway,
/// because `opened` has to come from `decrypt`'s returned ids and never from the text.
async fn put_empty_open(ctx: &Ctx, pool: &PgPool, namespace: &str) -> String {
    let id = uuid::Uuid::new_v4();
    let vectors = ctx.embedder.embed_documents(vec![String::new()]).await.unwrap();
    let embedding = pgvector::Vector::from(vectors[0].clone());
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, embedding, source_client,
                             embedding_model, sensitivity)
         VALUES ($1, $2, $3, '', $4, 'test', 'hash', 'open')",
    )
    .bind(id)
    .bind(&ctx.cfg.tenant_id)
    .bind(namespace)
    .bind(embedding)
    .execute(pool)
    .await
    .unwrap();
    id.to_string()
}

async fn live(pool: &PgPool, id: &str) -> (Option<String>, bool) {
    let row: (Option<uuid::Uuid>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT superseded_by, occurred_until FROM memory WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).unwrap())
    .fetch_one(pool)
    .await
    .unwrap();
    let is_live = row.0.is_none() && row.1.is_none_or(|u| u > Utc::now());
    (row.0.map(|u| u.to_string()), is_live)
}

/// A test double for `ProposalSource`. Origin "canned". The five tests over it exercise the
/// contract §7.2 states; none is evidence that a shipped source runs, because the engine ships
/// none.
struct CannedProposals {
    items: Mutex<Vec<(ProposalItem, Vec<Memory>)>>,
    last_decide: Mutex<Option<(String, Verdict)>>,
}

impl CannedProposals {
    fn new(items: Vec<(ProposalItem, Vec<Memory>)>) -> Self {
        Self { items: Mutex::new(items), last_decide: Mutex::new(None) }
    }
}

#[async_trait]
impl ProposalSource for CannedProposals {
    fn origin(&self) -> &'static str {
        "canned"
    }

    async fn pending(
        &self,
        _ctx: &Ctx,
        limit: i64,
        offset: i64,
    ) -> DomainResult<Vec<(ProposalItem, Vec<Memory>)>> {
        let items = self.items.lock().unwrap();
        let start = (offset as usize).min(items.len());
        let end = start.saturating_add((limit as usize).saturating_add(1)).min(items.len());
        Ok(items[start..end].to_vec())
    }

    async fn decide(
        &self,
        _ctx: &Ctx,
        id: &str,
        verdict: Verdict,
    ) -> DomainResult<ProposalDecided> {
        *self.last_decide.lock().unwrap() = Some((id.to_string(), verdict));
        Ok(ProposalDecided { state: "done".into(), written: None, superseded: vec![] })
    }
}

fn one_source(s: CannedProposals) -> Vec<Arc<dyn ProposalSource>> {
    vec![Arc::new(s)]
}

fn no_sources() -> Vec<Arc<dyn ProposalSource>> {
    Vec::new()
}

// ---------------------------------------------------------------------------------------------
// The ledger.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_kept_pair_leaves_the_queue_and_undismiss_brings_it_back() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "global", "keep1").await;
    let key = format!("conflict:{older}:{newer}");

    let before = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Conflict]),
        limit: None,
        offset: None,
        days: None,
        min_similarity: Some(0.0),
    })
    .await
    .unwrap();
    assert!(
        before.items.iter().any(|i| i.key == key || i.key == format!("conflict:{newer}:{older}")),
        "the seeded pair has to show up before it is dismissed"
    );

    let decided =
        review_queue::decide(&h.ctx, &no_sources(), Decision {
            key: key.clone(),
            verdict: Verdict::KeepBoth,
            keep: None,
            id: None,
            content: None,
            tags: None,
            occurred_at: None,
            reason: None,
        })
        .await
        .unwrap();
    assert_eq!(decided.verdict, Verdict::KeepBoth);

    let after = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Conflict]),
        limit: None,
        offset: None,
        days: None,
        min_similarity: Some(0.0),
    })
    .await
    .unwrap();
    assert!(
        !after.items.iter().any(|i| i.key == key),
        "a kept pair must leave the conflicts list: {:?}",
        after.items.iter().map(|i| &i.key).collect::<Vec<_>>()
    );

    let brought_back = review_queue::undismiss(&h.ctx, &older, &newer).await.unwrap();
    assert!(brought_back, "undismiss on a pair that is in the ledger answers true");

    let restored = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Conflict]),
        limit: None,
        offset: None,
        days: None,
        min_similarity: Some(0.0),
    })
    .await
    .unwrap();
    assert!(
        restored.items.iter().any(|i| i.key == key || i.key == format!("conflict:{newer}:{older}")),
        "undismiss brings the pair back"
    );
}

/// A conflict pair where the two rows carry different sensitivities, so a grant can be built to
/// straddle them: readable on both, writable on only one.
async fn conflict_pair_at(
    ctx: &Ctx,
    pool: &PgPool,
    namespace: &str,
    tag: &str,
    older_sensitivity: &str,
    newer_sensitivity: &str,
) -> (String, String) {
    let older = write::run(
        ctx,
        &format!("the {tag} rota starts on Monday and runs through Friday {}", nonce(tag)),
        namespace,
        None,
        None,
        Some(older_sensitivity),
        None,
    )
    .await
    .unwrap()
    .id;
    set_created_at(pool, &older, Utc::now() - Duration::hours(1)).await;
    let newer = write::run(
        ctx,
        &format!("the {tag} rota starts on Tuesday and runs through Friday {}", nonce(tag)),
        namespace,
        None,
        None,
        Some(newer_sensitivity),
        None,
    )
    .await
    .unwrap()
    .id;
    (older, newer)
}

#[tokio::test]
async fn keep_both_needs_the_write_grant_on_both_rows() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);

    // Read covers both rows (open and private); write covers open alone, so exactly one row of
    // each pair is writable. `writable_row` runs on both ids in the key's order, so the check has
    // to fail whichever half lands first: try it with the writable row named first, then with it
    // named second.
    let half_writable = restricted_at(
        &h.ctx,
        &[("global", Sensitivity::Private)],
        &[("global", Sensitivity::Open)],
    );

    let (open_first, private_first) =
        conflict_pair_at(&h.ctx, &h.pool, "global", "keep2a", "open", "private").await;
    let err = review_queue::decide(&half_writable, &no_sources(), Decision {
        key: format!("conflict:{open_first}:{private_first}"),
        verdict: Verdict::KeepBoth,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap_err();
    assert_eq!(
        err.kind,
        Kind::NotFound,
        "the writable-first pair still has one unwritable row: refuse it"
    );

    let (private_second, open_second) =
        conflict_pair_at(&h.ctx, &h.pool, "global", "keep2b", "private", "open").await;
    let err = review_queue::decide(&half_writable, &no_sources(), Decision {
        key: format!("conflict:{private_second}:{open_second}"),
        verdict: Verdict::KeepBoth,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap_err();
    assert_eq!(
        err.kind,
        Kind::NotFound,
        "the writable-second pair still has one unwritable row: refuse it"
    );
}

#[tokio::test]
async fn keep_both_records_the_client_and_the_token_fingerprint() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "global", "keep3").await;

    review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{older}:{newer}"),
        verdict: Verdict::KeepBoth,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();

    let listing = review_queue::dismissed(&h.ctx, None).await.unwrap();
    let entry = listing
        .iter()
        .find(|d| {
            (d.lo_id == older.to_lowercase() || d.lo_id == newer.to_lowercase())
                && (d.hi_id == older.to_lowercase() || d.hi_id == newer.to_lowercase())
        })
        .expect("the pair just kept should be in the ledger");
    assert_eq!(entry.dismissed_by, h.ctx.principal.client);
    assert_eq!(entry.dismissed_token, h.ctx.principal.token_id);
}

#[tokio::test]
async fn undismiss_answers_false_for_a_pair_the_caller_may_not_change() {
    let h = ctx_or_skip!();
    let a = uuid::Uuid::new_v4().to_string();
    let b = uuid::Uuid::new_v4().to_string();
    let answer = review_queue::undismiss(&h.ctx, &a, &b).await.unwrap();
    assert!(!answer, "nothing was ever dismissed for this pair");
}

#[tokio::test]
async fn undismiss_answers_false_for_a_narrow_grant_that_cannot_read_the_pair() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "project:vault", "undismiss-narrow").await;
    review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{older}:{newer}"),
        verdict: Verdict::KeepBoth,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();

    let narrow = restricted_at(&h.ctx, &[("global", Sensitivity::Open)], &[("global", Sensitivity::Open)]);
    let answer = review_queue::undismiss(&narrow, &older, &newer).await.unwrap();
    assert!(!answer, "the pair is real and dismissed, but this grant cannot read either row");
}

#[tokio::test]
async fn a_deleted_row_takes_its_dismissals_with_it() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "global", "keep4").await;
    review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{older}:{newer}"),
        verdict: Verdict::KeepBoth,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();

    lumberroom_server::services::forget::by_id(&h.ctx, &older, None, false).await.unwrap();

    let still_there = review_queue::undismiss(&h.ctx, &older, &newer).await.unwrap();
    assert!(!still_there, "the cascade in migration 025 should have taken the ledger row with it");
}

// ---------------------------------------------------------------------------------------------
// Grants and paging.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_narrow_grant_sees_a_full_page_of_its_own_pairs_and_no_count_of_the_rest() {
    let h = ctx_or_skip!();
    for i in 0..6 {
        let id =
            write_at(&h.ctx, &format!("vault fact {i} {}", nonce("narrow")), "project:vault").await;
        make_stale(&h.pool, &id).await;
    }
    for i in 0..3 {
        let id =
            write_at(&h.ctx, &format!("global fact {i} {}", nonce("narrow")), "global").await;
        make_stale(&h.pool, &id).await;
    }

    let narrow = restricted_at(&h.ctx, &[("global", Sensitivity::Open)], &[("global", Sensitivity::Open)]);
    let q = review_queue::queue(&narrow, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: Some(3),
        offset: None,
        days: Some(0),
        min_similarity: None,
    })
    .await
    .unwrap();
    assert_eq!(q.items.len(), 3, "3 asked for and 3 readable rows exist");
    for item in &q.items {
        assert_eq!(item.namespace, "global", "a row outside the grant reached the caller");
    }
    assert!(!q.has_more, "exactly 3 readable rows exist, so there is no further page");
}

/// The conflict twin of the stale test above. Three tied pairs sit in namespaces the narrow grant
/// cannot read, written earlier than the one pair it can; identical wording ties every similarity
/// so `ORDER BY similarity DESC, a.created_at, ...` puts the unreadable pairs first. That makes the
/// grant filter load-bearing: drop both `EXISTS` blocks from `CONFLICTS_SQL` and the unreadable
/// pairs fill the `limit + 1` window before the readable one is ever fetched, so this page comes
/// back empty instead of holding its one pair.
#[tokio::test]
async fn a_narrow_grant_sees_a_full_page_of_its_own_conflict_pairs_and_none_of_the_rest() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);

    for (i, ns) in ["project:secret0", "project:secret1", "project:secret2"].iter().enumerate() {
        let older = write_at(&h.ctx, "the rota tie aa bb cc", ns).await;
        set_created_at(&h.pool, &older, Utc::now() - Duration::hours(6 - i as i64)).await;
        write_at(&h.ctx, "the rota tie aa bb dd", ns).await;
    }
    let older = write_at(&h.ctx, "the rota tie aa bb cc", "global").await;
    set_created_at(&h.pool, &older, Utc::now() - Duration::hours(1)).await;
    let newer = write_at(&h.ctx, "the rota tie aa bb dd", "global").await;
    let key = format!("conflict:{older}:{newer}");

    let narrow = restricted_at(&h.ctx, &[("global", Sensitivity::Open)], &[("global", Sensitivity::Open)]);
    let q = review_queue::queue(&narrow, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Conflict]),
        limit: Some(1),
        offset: None,
        days: None,
        min_similarity: Some(0.0),
    })
    .await
    .unwrap();
    assert_eq!(
        q.items.iter().map(|i| i.key.clone()).collect::<Vec<_>>(),
        vec![key],
        "the one pair this grant can read, and nothing from the three it cannot"
    );
    assert!(!q.has_more, "exactly one readable pair exists, so there is no further page");
}

#[tokio::test]
async fn the_envelope_carries_no_tenant_wide_count() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let vault_id = write_at(&h.ctx, &format!("a fact the narrow grant cannot reach {}", nonce("t")), "project:vault").await;
    make_stale(&h.pool, &vault_id).await;
    let global_id = write_at(&h.ctx, &format!("a fact the narrow grant may read {}", nonce("t")), "global").await;
    make_stale(&h.pool, &global_id).await;

    // A dismissed pair in the namespace the narrow grant cannot read: the owner sees it in the
    // ledger count, the narrow grant must not, so `dismissed` has to be filtered by the same read
    // grant as the listing rather than counted once over the whole tenant.
    let (vault_older, vault_newer) = conflict_pair(&h.ctx, &h.pool, "project:vault", "envelope").await;
    review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{vault_older}:{vault_newer}"),
        verdict: Verdict::KeepBoth,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();

    let narrow = restricted_at(&h.ctx, &[("global", Sensitivity::Open)], &[("global", Sensitivity::Open)]);
    let q = review_queue::queue(&narrow, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: Some(10),
        offset: None,
        days: Some(0),
        min_similarity: None,
    })
    .await
    .unwrap();
    assert_eq!(
        q.dismissed, 0,
        "the dismissed pair sits in a namespace this grant cannot read, so its count is 0, not 1"
    );

    let owner_q = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: Some(10),
        offset: None,
        days: Some(0),
        min_similarity: None,
    })
    .await
    .unwrap();
    assert_eq!(owner_q.dismissed, 1, "the owner's own grant does cover that namespace");

    assert_eq!(q.items.len(), 1, "only the readable row appears");
}

#[tokio::test]
async fn two_pairs_at_one_similarity_page_once_each() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    // Identical wording in two namespaces: the self-join runs per namespace, so both pairs get the
    // exact same embedding and therefore the exact same rounded similarity, a genuine tie rather
    // than one contrived by rounding.
    let older_a = write_at(&h.ctx, "the rota tie aa bb cc", "project:tiea").await;
    set_created_at(&h.pool, &older_a, Utc::now() - Duration::hours(2)).await;
    let _newer_a = write_at(&h.ctx, "the rota tie aa bb dd", "project:tiea").await;
    let older_b = write_at(&h.ctx, "the rota tie aa bb cc", "project:tieb").await;
    set_created_at(&h.pool, &older_b, Utc::now() - Duration::hours(2)).await;
    let _newer_b = write_at(&h.ctx, "the rota tie aa bb dd", "project:tieb").await;

    let query = |offset: i64| QueueQuery {
        sources: Some(vec![Source::Conflict]),
        limit: Some(1),
        offset: Some(offset),
        days: None,
        min_similarity: Some(0.0),
    };
    let page0 = review_queue::queue(&h.ctx, &no_sources(), query(0)).await.unwrap();
    let page1 = review_queue::queue(&h.ctx, &no_sources(), query(1)).await.unwrap();

    let seen: Vec<String> = page0
        .items
        .iter()
        .chain(page1.items.iter())
        .filter(|i| {
            let key = &i.key;
            key.contains(&older_a) || key.contains(&older_b)
        })
        .map(|i| i.key.clone())
        .collect();
    assert_eq!(seen.len(), 2, "two tied pairs, one per offset page, no repeat and no gap: {seen:?}");
    assert_ne!(seen[0], seen[1], "the same key must not appear on both pages");
}

#[tokio::test]
async fn an_offset_past_the_ceiling_is_refused_rather_than_clamped() {
    let h = ctx_or_skip!();
    let err = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: None,
        offset: Some(review_queue::MAX_OFFSET + 1),
        days: None,
        min_similarity: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.code(), Some(review_queue::codes::PAGE_TOO_DEEP));
}

#[tokio::test]
async fn a_namespace_over_the_scan_ceiling_refuses_conflicts_and_still_answers_stale() {
    let h = ctx_or_skip!(|c: &mut Config| {
        c.quality.conflict_scan_max = 100;
    });
    let stale_id = write_at(&h.ctx, &format!("a stale fact {}", nonce("ceiling")), "global").await;
    make_stale(&h.pool, &stale_id).await;
    for i in 0..101 {
        write_at(&h.ctx, &format!("filler row {i} {}", nonce("ceiling")), "global").await;
    }

    let q = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Conflict, Source::Stale]),
        limit: None,
        offset: None,
        days: Some(0),
        min_similarity: None,
    })
    .await
    .unwrap();
    assert_eq!(
        q.refused.get("conflict"),
        Some(&review_queue::codes::NAMESPACE_TOO_LARGE),
        "the namespace crossed conflict_scan_max"
    );
    assert!(q.items.iter().any(|i| i.source == Source::Stale), "stale still answers");
}

// ---------------------------------------------------------------------------------------------
// Decide: verdicts, merges, supersession, delete.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_row_whose_content_is_empty_still_takes_every_verdict() {
    let h = ctx_or_skip!();
    let id = put_empty_open(&h.ctx, &h.pool, "global").await;
    make_stale(&h.pool, &id).await;

    let q = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: None,
        offset: None,
        days: Some(0),
        min_similarity: None,
    })
    .await
    .unwrap();
    let item = q.items.iter().find(|i| i.rows.iter().any(|r| r.id == id)).unwrap();
    let row = item.rows.iter().find(|r| r.id == id).unwrap();
    assert!(row.opened, "an open row is never in decrypt's failed list, whatever its text is");
    assert_eq!(
        item.verdicts,
        vec![Verdict::Confirm, Verdict::Merge, Verdict::Delete],
        "an owner-writable stale row with empty content still takes confirm, merge and delete"
    );
}

#[tokio::test]
async fn a_merge_writes_once_and_retires_both_sources_into_it() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let old_date = Utc::now() - Duration::days(10);
    let new_date = Utc::now() - Duration::days(3);
    let older = write_dated(&h.ctx, "the pair merges old", "global", old_date).await;
    set_created_at(&h.pool, &older, Utc::now() - Duration::hours(1)).await;
    let newer = write_dated(&h.ctx, "the pair merges new", "global", new_date).await;

    let decided = review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{older}:{newer}"),
        verdict: Verdict::Merge,
        keep: None,
        id: None,
        content: Some("the merged fact, in the caller's own words".into()),
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();

    let written = decided.written.expect("a merge writes exactly one new row");
    assert_eq!(decided.superseded.len(), 2, "both sources retire into the written row");
    assert!(decided.unfinished.is_empty());

    let row = h
        .ctx
        .repos
        .memories
        .find_by_id(h.ctx.tenant(), uuid::Uuid::parse_str(&written).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.occurred_at.map(|d| d.date_naive()),
        Some(new_date.date_naive()),
        "the merge carries the newest source's occurred_at"
    );
    let (_, older_live) = live(&h.pool, &older).await;
    let (_, newer_live) = live(&h.pool, &newer).await;
    assert!(!older_live && !newer_live, "both sources retired");
}

#[tokio::test]
async fn a_merge_of_two_same_day_rows_writes_without_an_occurred_at() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let now = Utc::now();
    let older = write_at(&h.ctx, "same day merge old", "global").await;
    set_created_at(&h.pool, &older, Utc::now() - Duration::hours(1)).await;
    set_occurred_at(&h.pool, &older, now).await;
    let newer = write_at(&h.ctx, "same day merge new", "global").await;
    set_occurred_at(&h.pool, &newer, now).await;

    let decided = review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{older}:{newer}"),
        verdict: Verdict::Merge,
        keep: None,
        id: None,
        content: Some("merged, no occurred_at should stick".into()),
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();
    let written = decided.written.unwrap();
    let row = h
        .ctx
        .repos
        .memories
        .find_by_id(h.ctx.tenant(), uuid::Uuid::parse_str(&written).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.occurred_at.is_none(),
        "same-day sources both fail the near-now fence, so the default takes none"
    );
}

#[tokio::test]
async fn a_merge_whose_second_retirement_fails_reports_the_leftover_in_unfinished() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let older = write_at(&h.ctx, "leftover merge old", "global").await;
    set_created_at(&h.pool, &older, Utc::now() - Duration::hours(1)).await;
    let newer = write_at(&h.ctx, "leftover merge new", "global").await;

    // Expire the older row between seeding and deciding. `write::validate_supersedes` refuses a
    // supersede target whose period has already closed, which is what leaves it unfinished.
    review::expire(&h.ctx, &older).await.unwrap();

    let decided = review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{older}:{newer}"),
        verdict: Verdict::Merge,
        keep: None,
        id: None,
        content: Some("the write lands even though one retirement will not".into()),
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();
    assert!(decided.written.is_some(), "the write itself still lands");
    assert!(
        decided.unfinished.iter().any(|id| id == &older),
        "the expired row is reported rather than silently dropped: {:?}",
        decided.unfinished
    );
}

#[tokio::test]
async fn a_verdict_the_source_does_not_take_is_refused_before_any_row_changes() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "global", "badverdict").await;

    let err = review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{older}:{newer}"),
        verdict: Verdict::Confirm,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.code(), Some(review_queue::codes::VERDICT_NOT_FOR_SOURCE));

    let (older_super, older_live) = live(&h.pool, &older).await;
    let (newer_super, newer_live) = live(&h.pool, &newer).await;
    assert!(older_super.is_none() && newer_super.is_none() && older_live && newer_live);
}

#[tokio::test]
async fn delete_through_the_queue_still_needs_may_delete() {
    let h = ctx_or_skip!();
    let id = write_at(&h.ctx, &format!("delete through the queue {}", nonce("del")), "global").await;
    make_stale(&h.pool, &id).await;

    let mut no_delete = h.ctx.clone();
    no_delete.principal.may_delete = false;

    let err = review_queue::decide(&no_delete, &no_sources(), Decision {
        key: format!("stale:{id}"),
        verdict: Verdict::Delete,
        keep: None,
        id: Some(id.clone()),
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.kind.http_status(), 403);
    let (_, still_live) = live(&h.pool, &id).await;
    assert!(still_live);
}

#[tokio::test]
async fn a_supersede_default_keeps_the_newer_row_whatever_order_the_key_spelled() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "global", "spelled").await;

    // Spelled with the newer id first: `parse_key`'s job is to reorder from the stored rows, not
    // from how the caller wrote the key.
    review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("conflict:{newer}:{older}"),
        verdict: Verdict::Supersede,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();

    let (older_super, _) = live(&h.pool, &older).await;
    let (_, newer_live) = live(&h.pool, &newer).await;
    assert_eq!(older_super.as_deref(), Some(newer.as_str()), "older retires into newer by default");
    assert!(newer_live, "the newer row is the one left standing");
}

// ---------------------------------------------------------------------------------------------
// The HTTP routes.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_queue_route_answers_the_envelope_with_sources_and_defaults_from_config() {
    let h = ctx_or_skip!();
    let (status, body) = h.get("/admin/review/queue").await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["sources"]["conflict"], true);
    assert_eq!(v["sources"]["stale"], true);
    assert_eq!(v["sources"]["proposal"], serde_json::json!([]));
    assert_eq!(v["stale_days"], h.ctx.cfg.quality.stale_days);
    assert!((v["min_similarity"].as_f64().unwrap() - h.ctx.cfg.quality.conflict_threshold).abs() < 1e-9);
    assert_eq!(v["limit"], review_queue::DEFAULT_LIMIT);
    assert_eq!(v["offset"], 0);
}

#[tokio::test]
async fn the_old_conflicts_and_stale_routes_answer_the_same_json_they_did_before() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let id = write_at(&h.ctx, &format!("stale route shape {}", nonce("route")), "global").await;
    make_stale(&h.pool, &id).await;
    conflict_pair(&h.ctx, &h.pool, "global", "routeconf").await;

    let (status, body) = h.get("/admin/review/stale?days=0").await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("days").is_some() && v.get("rows").is_some(), "stale route keeps its own shape: {v}");
    let row = v["rows"].as_array().unwrap().first().expect("a stale row").clone();
    for key in ["id", "namespace", "sensitivity", "content", "created_at"] {
        assert!(row.get(key).is_some(), "stale row keeps {key}: {row}");
    }
    // The CHANGELOG records these as dropped when both routes moved onto the queue. Pinned here so
    // a later widening of the row is a decision rather than an accident.
    for key in ["tags", "source_client", "embedding_model", "superseded_by"] {
        assert!(row.get(key).is_none(), "stale row no longer carries {key}: {row}");
    }

    let (status, body) = h.get("/admin/review/conflicts?min_similarity=0").await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        v.get("min_similarity").is_some() && v.get("pairs").is_some(),
        "conflicts route keeps its own shape: {v}"
    );
    let pair = v["pairs"].as_array().unwrap().first().expect("a conflict pair").clone();
    for side in ["older", "newer"] {
        let s = pair[side].as_object().expect("a side object");
        assert_eq!(s.len(), 5, "{side} keeps its five fields: {s:?}");
        for key in ["id", "namespace", "content", "sensitivity", "created_at"] {
            assert!(s.contains_key(key), "{side} keeps {key}: {s:?}");
        }
    }
}

/// Both routes default to 25 rows, which they did before the queue existed. The queue's own
/// default is 50 and a client that reads these routes never asked for it.
#[tokio::test]
async fn the_old_routes_keep_their_own_page_default() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    // Each text differs by more than a number: two rows the dedupe threshold collapses would seed
    // one row and the page count would read as a paging bug.
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
        "sierra", "tango", "uniform", "victor", "whiskey", "xray", "yankee", "zulu",
    ];
    for w in words {
        let id = write_at(&h.ctx, &format!("{w} is a stale fixture {}", nonce(w)), "global").await;
        make_stale(&h.pool, &id).await;
    }

    let (status, body) = h.get("/admin/review/stale?days=0").await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["rows"].as_array().unwrap().len(), 25, "stale default page: {v}");
}

#[tokio::test]
async fn source_proposal_on_an_engine_answers_400_source_not_filled() {
    let h = ctx_or_skip!();
    let (status, body) = h.get("/admin/review/queue?source=proposal").await;
    assert_eq!(status, 400, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], review_queue::codes::SOURCE_NOT_FILLED);
}

// ---------------------------------------------------------------------------------------------
// The proposal seam, through the CannedProposals double.
// ---------------------------------------------------------------------------------------------

fn field(label: &str, value: &str) -> ProposalField {
    ProposalField { label: label.into(), value: value.into() }
}

#[tokio::test]
async fn a_canned_proposal_appears_with_its_members_fields_and_verdicts() {
    let h = ctx_or_skip!();
    let member_id = write_at(&h.ctx, &format!("a member row {}", nonce("canned1")), "global").await;
    let member = h
        .ctx
        .repos
        .memories
        .find_by_id(h.ctx.tenant(), uuid::Uuid::parse_str(&member_id).unwrap())
        .await
        .unwrap()
        .unwrap();

    let item = ProposalItem {
        id: "p1".into(),
        origin: "canned".into(),
        kind: "example".into(),
        proposed_content: Some("what apply would write".into()),
        fields: vec![field("why", "a test fixture offered it")],
        created_at: Utc::now().to_rfc3339(),
        verdicts: vec![Verdict::Apply, Verdict::Dismiss],
    };
    let sources = one_source(CannedProposals::new(vec![(item, vec![member])]));

    let q = review_queue::queue(&h.ctx, &sources, QueueQuery {
        sources: Some(vec![Source::Proposal]),
        limit: None,
        offset: None,
        days: None,
        min_similarity: None,
    })
    .await
    .unwrap();
    let found = q.items.iter().find(|i| i.source == Source::Proposal).expect("the canned item");
    let proposal = found.proposal.as_ref().unwrap();
    assert_eq!(proposal.proposed_content.as_deref(), Some("what apply would write"));
    assert_eq!(proposal.fields.len(), 1);
    assert_eq!(found.verdicts, vec![Verdict::Apply, Verdict::Dismiss]);
}

#[tokio::test]
async fn a_full_page_of_canned_proposals_reports_has_more() {
    let h = ctx_or_skip!();
    let mut member_rows = Vec::new();
    for i in 0..3 {
        let id = write_at(&h.ctx, &format!("proposal member {i} {}", nonce("page")), "global").await;
        member_rows.push(
            h.ctx
                .repos
                .memories
                .find_by_id(h.ctx.tenant(), uuid::Uuid::parse_str(&id).unwrap())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let items: Vec<_> = member_rows
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            (
                ProposalItem {
                    id: format!("p{i}"),
                    origin: "canned".into(),
                    kind: "example".into(),
                    proposed_content: None,
                    fields: vec![],
                    created_at: Utc::now().to_rfc3339(),
                    verdicts: vec![Verdict::Dismiss],
                },
                vec![m],
            )
        })
        .collect();
    let sources = one_source(CannedProposals::new(items));

    let q = review_queue::queue(&h.ctx, &sources, QueueQuery {
        sources: Some(vec![Source::Proposal]),
        limit: Some(2),
        offset: None,
        days: None,
        min_similarity: None,
    })
    .await
    .unwrap();
    assert_eq!(q.items.iter().filter(|i| i.source == Source::Proposal).count(), 2);
    assert!(q.has_more, "3 offered, 2 kept: the third is the only signal a further page exists");
}

#[tokio::test]
async fn a_canned_proposal_with_a_member_outside_the_grant_drops_whole() {
    let h = ctx_or_skip!();
    let unreadable_id =
        write_at(&h.ctx, &format!("outside the narrow grant {}", nonce("outside")), "project:vault").await;
    let member = h
        .ctx
        .repos
        .memories
        .find_by_id(h.ctx.tenant(), uuid::Uuid::parse_str(&unreadable_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    let item = ProposalItem {
        id: "p-outside".into(),
        origin: "canned".into(),
        kind: "example".into(),
        proposed_content: None,
        fields: vec![],
        created_at: Utc::now().to_rfc3339(),
        verdicts: vec![Verdict::Apply],
    };
    let sources = one_source(CannedProposals::new(vec![(item, vec![member])]));

    let narrow = restricted_at(&h.ctx, &[("global", Sensitivity::Open)], &[("global", Sensitivity::Open)]);
    let q = review_queue::queue(&narrow, &sources, QueueQuery {
        sources: Some(vec![Source::Proposal]),
        limit: None,
        offset: None,
        days: None,
        min_similarity: None,
    })
    .await
    .unwrap();
    assert!(
        !q.items.iter().any(|i| i.source == Source::Proposal),
        "a member outside the grant drops the whole proposal, not a partial view of it"
    );
}

#[tokio::test]
async fn a_canned_proposal_with_a_member_that_will_not_open_takes_no_verdict() {
    let h = ctx_or_skip!();
    let id = write::run(&h.ctx, "a private member that will be corrupted", "global", None, None, Some("private"), None)
        .await
        .unwrap()
        .id;
    break_ciphertext(&h.pool, &id).await;
    let member = h
        .ctx
        .repos
        .memories
        .find_by_id(h.ctx.tenant(), uuid::Uuid::parse_str(&id).unwrap())
        .await
        .unwrap()
        .unwrap();
    let item = ProposalItem {
        id: "p-broken".into(),
        origin: "canned".into(),
        kind: "example".into(),
        proposed_content: None,
        fields: vec![],
        created_at: Utc::now().to_rfc3339(),
        verdicts: vec![Verdict::Apply],
    };
    let sources = one_source(CannedProposals::new(vec![(item, vec![member])]));

    let q = review_queue::queue(&h.ctx, &sources, QueueQuery {
        sources: Some(vec![Source::Proposal]),
        limit: None,
        offset: None,
        days: None,
        min_similarity: None,
    })
    .await
    .unwrap();
    let found = q.items.iter().find(|i| i.source == Source::Proposal).expect("the item still appears");
    assert!(
        found.verdicts.is_empty(),
        "a member that will not decrypt empties the verdicts, it does not remove the item"
    );
    assert!(found.rows.iter().any(|r| !r.opened));
}

#[tokio::test]
async fn a_proposal_decision_reaches_the_source_named_by_the_key_and_an_unknown_origin_is_refused() {
    let h = ctx_or_skip!();
    let member_id = write_at(&h.ctx, &format!("canned decide member {}", nonce("decide")), "global").await;
    let member = h
        .ctx
        .repos
        .memories
        .find_by_id(h.ctx.tenant(), uuid::Uuid::parse_str(&member_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    let item = ProposalItem {
        id: "p-decide".into(),
        origin: "canned".into(),
        kind: "example".into(),
        proposed_content: None,
        fields: vec![],
        created_at: Utc::now().to_rfc3339(),
        verdicts: vec![Verdict::Apply],
    };
    let canned = Arc::new(CannedProposals::new(vec![(item, vec![member])]));
    let sources: Vec<Arc<dyn ProposalSource>> = vec![canned.clone()];

    review_queue::decide(&h.ctx, &sources, Decision {
        key: "proposal:canned:p-decide".into(),
        verdict: Verdict::Apply,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();
    assert_eq!(
        *canned.last_decide.lock().unwrap(),
        Some(("p-decide".to_string(), Verdict::Apply)),
        "the decision reached the source the key named"
    );

    let err = review_queue::decide(&h.ctx, &sources, Decision {
        key: "proposal:notcanned:whatever".into(),
        verdict: Verdict::Apply,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.code(), Some(review_queue::codes::UNKNOWN_ORIGIN));
}

// ---------------------------------------------------------------------------------------------
// Confirming a stale row.
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_confirmed_stale_row_leaves_the_list_for_one_window() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.stale_days = 30);
    let id = write_at(&h.ctx, &format!("confirm and it leaves the list {}", nonce("confirm")), "global").await;
    make_stale(&h.pool, &id).await;

    let before = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: None,
        offset: None,
        days: Some(30),
        min_similarity: None,
    })
    .await
    .unwrap();
    assert!(before.items.iter().any(|i| i.rows.iter().any(|r| r.id == id)));

    review_queue::decide(&h.ctx, &no_sources(), Decision {
        key: format!("stale:{id}"),
        verdict: Verdict::Confirm,
        keep: None,
        id: None,
        content: None,
        tags: None,
        occurred_at: None,
        reason: None,
    })
    .await
    .unwrap();

    let just_after = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: None,
        offset: None,
        days: Some(30),
        min_similarity: None,
    })
    .await
    .unwrap();
    assert!(
        !just_after.items.iter().any(|i| i.rows.iter().any(|r| r.id == id)),
        "confirming clears the row for one window"
    );

    // Backdate the confirmation past the window (floored at one day, so `--days 0` still matters).
    set_last_confirmed_at(&h.pool, &id, Utc::now() - Duration::days(31)).await;
    let after_window = review_queue::queue(&h.ctx, &no_sources(), QueueQuery {
        sources: Some(vec![Source::Stale]),
        limit: None,
        offset: None,
        days: Some(30),
        min_similarity: None,
    })
    .await
    .unwrap();
    assert!(
        after_window.items.iter().any(|i| i.rows.iter().any(|r| r.id == id)),
        "the row comes back once the window passes"
    );
}
