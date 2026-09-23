//! The cleanup pass, against a real database. Skipped when none is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom cargo test --test cleanup
//!
//! Six properties, and every one of them is a thing the unit tests cannot reach because it is a
//! claim about what SQL returns rather than about what Rust computes.
//!
//! The window admits a pair where only one side is new. Filtering both sides is the obvious reading
//! of "read what changed" and it makes the pass blind to a new row restating an old fact.
//!
//! A second run over the same window queues nothing. This is the whole argument for an hourly
//! cadence being safe to run hourly.
//!
//! A private row never reaches the list handed to a model. The filter is in the query, so this is
//! the only place it can be checked.
//!
//! Applying retires through supersession, so the retired text is still readable afterwards.
//!
//! A member edited since the pass read it makes apply refuse rather than adapt.
//!
//! A finding the owner resolved by hand closes itself instead of sitting in the queue forever.
//!
//! A client granted ingestion and one namespace runs the pass, reads the queue and posts findings
//! inside that namespace and nowhere else. The grant is a term of the candidate queries and of the
//! queue reads, so this is the other place the SQL has to be checked.

use std::net::SocketAddr;
use std::sync::Arc;

use chrono::Utc;
use lumberroom_server::adapters::auth;
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres::{self, PgCleanupRepository};
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::cleanup::{Arity, CandidateQuery, CleanupRepository};
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::{
    bootstrap, cleanup, review, search, supersession, write, Ctx, Repos,
};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";

/// Two tokens, and the difference between them is the whole of the capability test: the owner's
/// own client reaches everything and still may not ingest, and the ingesting client is the one the
/// owner named.
const OWNER_TOKEN: &str = "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
const INGEST_TOKEN: &str = "iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii";
/// Ingestion plus `project:*` at `open` and nothing else. The shape the console's Ingest bot preset
/// produces once its read list is edited down, and the credential every grant test here holds.
const NARROW_TOKEN: &str = "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn";

/// A connection string with an inline password. The tripwire fires on it as
/// `connection_string_password`, and it is the shape a transcript actually carries.

/// Every test here truncates the shared test database, so they serialise themselves rather than
/// relying on `--test-threads=1` being remembered. Cargo runs one test binary at a time, so this
/// mutex and the ones in `integration.rs` and `console.rs` do not have to know about each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A setup step that is allowed to be missing, with the reason printed rather than swallowed.
///
/// A laptop with no Postgres and a store whose migrations no longer match this binary both skip,
/// and without the printed reason they skip with the same sentence. The suite skips rather than
/// fails, which makes that sentence the only thing standing between a broken run and a run
/// somebody reads as a pass.
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
    repo: Arc<dyn CleanupRepository>,
    pool: PgPool,
    /// A live server on a loopback port, because the capability gate lives in the router and a
    /// service-level test cannot see it.
    base: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    /// Held for the whole test. The mutex above serialises this binary's own threads; this is what
    /// keeps the other five binaries out of the same database.
    _db: common::DbGuard,
}

impl Harness {
    async fn post(&self, path: &str, token: &str, body: serde_json::Value) -> (u16, String) {
        let res = reqwest::Client::new()
            .post(format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        (status, res.text().await.unwrap())
    }

    async fn get(&self, path: &str, token: &str) -> (u16, String) {
        let res = reqwest::Client::new()
            .get(format!("{}{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        (status, res.text().await.unwrap())
    }
}

/// Returns None when no database is reachable, so the suite skips rather than fails on a machine
/// without one.
async fn setup() -> Option<Harness> {
    let guard = SERIAL.lock().await;
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let base = admin_url.rsplit_once('/')?.0.to_string();
    let admin = step!("connecting to the admin database", PgPool::connect(&admin_url).await);

    let exists: Result<Option<i32>, _> =
        sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(TEST_DB)
            .fetch_optional(&admin)
            .await;
    let exists = step!("looking for the test database", exists);
    if exists.is_none() {
        // DDL cannot take a bind parameter, so this is the one statement here that has to be built
        // as a string. Audited: TEST_DB is a compile-time constant with no external input.
        let created = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {TEST_DB}")))
            .execute(&admin)
            .await;
        step!("creating the test database", created);
    }
    admin.close().await;

    let url = format!("{base}/{TEST_DB}");
    std::env::set_var("DATABASE_URL", &url);
    std::env::set_var(
        "AUTH_TOKENS",
        format!(
            r#"[{{"client":"mac","token":"{OWNER_TOKEN}","read":[{{"namespace":"*","max":"sealed"}}],"write":[{{"namespace":"*","max":"sealed"}}],"sealedCapable":true,"registryWrite":true}},
                {{"client":"ingester","token":"{INGEST_TOKEN}","read":[{{"namespace":"*","max":"sealed"}}],"write":[{{"namespace":"*","max":"sealed"}}],"sealedCapable":true,"registryWrite":true,"mayIngest":true}},
                {{"client":"narrow","token":"{NARROW_TOKEN}","read":[{{"namespace":"project:*","max":"open"}}],"write":[],"mayIngest":true}}]"#
        ),
    );
    std::env::set_var("EMBED_PROVIDER", "hash");
    std::env::set_var(TEST_KEK_VAR, TEST_KEK_HEX);

    // Before the truncate below, and before anything reads. Every other binary
    // targeting this database waits here.
    let db_lock = common::lock_database(&url).await?;
    let pool = step!("connecting to the test database", postgres::connect(&url).await);
    step!("migrating the test database", postgres::migrate(&pool).await);
    let truncated = sqlx::query(
        "TRUNCATE memory, registry, registry_history, entity_alias, sealed_item, tool_calls,
                  registry_alias, kek_state,
                  oauth_client, oauth_code, oauth_token, oauth_refresh,
                  ingest_proposal, ingest_proposal_source, ingest_watermark, ingest_run,
                  cleanup_proposal, cleanup_proposal_member, cleanup_watermark, subject_cardinality,
                  recall_emission
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

    let memories = Arc::new(postgres::PgMemoryRepository::new(pool.clone()));
    let ctx = Ctx {
        cfg: Arc::new(cfg),
        repos: Repos {
            aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
            memories: memories.clone(),
            registry: Arc::new(postgres::PgRegistryRepository::new(pool.clone())),
            tool_calls: Arc::new(postgres::PgToolCallRepository::new(pool.clone())),
            sealed: Some(Arc::new(postgres::PgSealedRepository::new(pool.clone()))),
            ciphertext: Some(memories),
            oauth: None,
        },
        embedder: Arc::new(HashEmbedder::new(768)),
        keys: Some(keys),
        kek_verified,
        principal: owner_like("mac"),
        invocation: Invocation::Cli,
        // Set, and deliberately never used by anything in this file. The emission check is
        // tenant-wide on content hash: a check keyed on this would fire never.
        session_id: Some("test-session".into()),
    };
    bootstrap::clear_cache();

    let repo: Arc<dyn CleanupRepository> = Arc::new(PgCleanupRepository::new(pool.clone()));

    let oauth: Arc<dyn OauthStore> = Arc::new(postgres::PgOauthStore::new(pool.clone()));
    let state = Arc::new(AppState {
        cleanup: Arc::clone(&repo),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        cfg: Arc::clone(&ctx.cfg),
        repos: ctx.repos.clone(),
        oauth: Arc::clone(&oauth),
        ingest: Arc::new(postgres::PgIngestRepository::new(pool.clone())),
        embedder: Arc::clone(&ctx.embedder),
        degraded_embedder: false,
        keys: ctx.keys.clone(),
        kek_verified: ctx.kek_verified,
        proposals: Vec::new(),
    });
    let authenticator = auth::create(&ctx.cfg, Some(oauth)).ok()?;
    let app = lumberroom_server::http::router(state, authenticator);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr: SocketAddr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });

    Some(Harness { ctx, repo, pool, base: format!("http://{addr}"), _serial: guard, _db: db_lock })
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

/// The same, classified private, which is what the model-visibility test needs.
async fn put_private(h: &Harness, namespace: &str, content: &str) -> String {
    let id = put_raw(h, namespace, content).await;
    sqlx::query("UPDATE memory SET sensitivity = 'private' WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&id).unwrap())
        .execute(&h.pool)
        .await
        .unwrap();
    id
}

/// A row inserted straight into the table, bypassing `write::run`.
///
/// This is not a shortcut. `write::run` collapses a near-identical write into the row it matches,
/// so a duplicate cannot be created through it at all, and every duplicate in the owner's real
/// store got there some other way: written before that check existed, or restored from a dump, or
/// put there by a test harness. Those are the rows this pass is for, and this is how a test makes
/// one.
async fn put_raw(h: &Harness, namespace: &str, content: &str) -> String {
    let id = uuid::Uuid::new_v4();
    let vectors = h.ctx.embedder.embed_documents(vec![content.to_string()]).await.unwrap();
    let embedding = pgvector::Vector::from(vectors[0].clone());
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, embedding, source_client,
                             embedding_model, sensitivity)
         VALUES ($1, $2, $3, $4, $5, 'test', 'hash', 'open')",
    )
    .bind(id)
    .bind(&h.ctx.cfg.tenant_id)
    .bind(namespace)
    .bind(content)
    .bind(embedding)
    .execute(&h.pool)
    .await
    .unwrap();
    id.to_string()
}

/// Set a row's valid time, which is the clock supersession validates on.
async fn set_occurred(h: &Harness, id: &str, at: &str) {
    sqlx::query("UPDATE memory SET occurred_at = $2::timestamptz WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .bind(at)
        .execute(&h.pool)
        .await
        .unwrap();
}

/// Back-date a row, so a test can put one side of a pair outside the window without waiting an hour.
async fn backdate(h: &Harness, id: &str, days: i64) {
    sqlx::query(
        "UPDATE memory SET created_at = now() - make_interval(days => $2::int) WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(id).unwrap())
    .bind(i32::try_from(days).unwrap())
    .execute(&h.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn two_identical_rows_become_one_proposal_and_the_oldest_survives() {
    let h = harness_or_skip!();
    let first = put_raw(&h, "user:me", "the deploy runbook lives in DEPLOY.md").await;
    // Case and spacing differ, which is what makes this the pass's job rather than find_exact's:
    // that one compares bytes.
    let second = put_raw(&h, "user:me", "The deploy runbook lives in DEPLOY.md  ").await;
    assert_ne!(first, second);

    let (report, _) =
        cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None)
            .await
            .unwrap();
    assert_eq!(report.exact_groups, 1, "case and spacing should not make two rows distinct");
    // One finding, not two. Identical text also sits at a cosine of 1.0, so the same pair would
    // otherwise arrive again as a paraphrase under a different cluster key.
    assert_eq!(report.queued, 1, "the same pair was queued twice: {report:?}");

    let rows = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    let p = rows
        .iter()
        .find(|p| p.kind == lumberroom_server::domain::cleanup::CleanupKind::Exact)
        .unwrap();
    assert_eq!(p.keep_id.as_deref(), Some(first.as_str()), "the oldest carries the reads");
}

#[tokio::test]
async fn a_second_run_over_the_same_window_queues_nothing() {
    // The whole argument for an hourly cadence. Without it the queue grows by one row per hour per
    // cluster and stops being readable by lunchtime.
    let h = harness_or_skip!();
    put_raw(&h, "user:me", "the builder image carries g++").await;
    put_raw(&h, "user:me", "the builder image carries g++").await;

    let (first, _) = cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None)
        .await
        .unwrap();
    assert!(first.queued >= 1);
    let (second, _) =
        cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None)
            .await
            .unwrap();
    assert_eq!(second.queued, 0, "the same cluster was queued twice");
    assert!(second.already_known >= 1, "and it was not counted as known either");
}

#[tokio::test]
async fn the_exact_query_still_groups_an_old_row_once_the_watermark_has_passed_it() {
    // The anchor admits a whole group when one member is new. Case and spacing normalise away, so
    // these two are one group even though nothing compares their bytes.
    let h = harness_or_skip!();
    let old = put_raw(&h, "user:me", "the acceptance gates run against a live server").await;
    backdate(&h, &old, 30).await;
    put_raw(&h, "user:me", "an unrelated fact that moves the watermark").await;

    let (first, _) = cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None)
        .await
        .unwrap();
    let mark = first.through.expect("a run that found nothing still has to advance its watermark");
    let old_created: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT created_at FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&old).unwrap())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert!(old_created < mark, "the fixture did not put the old row outside the window");

    let restated = put_raw(&h, "user:me", "the  ACCEPTANCE gates run  against a live server").await;
    let (report, _) =
        cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None)
            .await
            .unwrap();

    let rows = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    let found = rows.iter().any(|p| {
        p.members.iter().any(|m| m.memory_id == old)
            && p.members.iter().any(|m| m.memory_id == restated)
    });
    assert!(found, "the old row fell out of its own group. report: {report:?}");
}

#[tokio::test]
async fn the_similarity_window_admits_a_pair_where_only_one_side_is_new() {
    // The property on its own, at the repository, with the threshold set to zero so neither the
    // band nor the embedder can decide the outcome. Only the window is under test.
    //
    // Checked by mutation: turning the OR in SIMILAR_PAIRS_SQL into an AND fails this and nothing
    // else. An earlier version of this test went through cleanup::run and passed against both,
    // because the exact query was quietly finding the pair.
    let h = harness_or_skip!();
    let old = put_raw(&h, "user:me", "the release image is built from Dockerfile").await;
    backdate(&h, &old, 30).await;
    let fresh = put_raw(&h, "user:me", "an unrelated newer fact").await;

    let q = CandidateQuery {
        namespace: Some("user:me".into()),
        max_sensitivity: lumberroom_server::domain::types::Sensitivity::Sealed,
        grant: NamespaceGrant::everything(),
        since: Some(Utc::now() - chrono::Duration::days(1)),
        limit: 100,
    };
    let pairs = h.repo.similar_pairs(&h.ctx.cfg.tenant_id, &q, 0.0).await.unwrap();
    let covered = pairs.iter().any(|p| {
        let ids = [p.older.id.as_str(), p.newer.id.as_str()];
        ids.contains(&old.as_str()) && ids.contains(&fresh.as_str())
    });
    assert!(
        covered,
        "the window dropped a pair whose older side predates it, which is the most common \
         duplicate there is. pairs: {:?}",
        pairs.iter().map(|p| (&p.older.id, &p.newer.id)).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn applying_retires_through_supersession_so_the_old_text_is_still_readable() {
    let h = harness_or_skip!();
    let first = put_raw(&h, "user:me", "the CLI is dependency-free JavaScript").await;
    let second = put_raw(&h, "user:me", "The CLI is dependency-free JavaScript").await;

    cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None).await.unwrap();
    let rows = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    let p = rows
        .iter()
        .find(|p| p.kind == lumberroom_server::domain::cleanup::CleanupKind::Exact)
        .unwrap();

    let applied = cleanup::apply(&h.ctx, h.repo.as_ref(), &p.id).await.unwrap();
    assert_eq!(applied.retired, vec![second.clone()]);
    assert!(applied.deleted.is_empty(), "an exact duplicate must retire, never delete");

    // The retired row is still there, pointing at its replacement.
    let superseded_by: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT superseded_by FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&second).unwrap())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(superseded_by.map(|u| u.to_string()), Some(first.clone()));

    // And the survivor still answers.
    let hits = search::run(
        &h.ctx,
        "dependency-free JavaScript",
        Some(vec!["user:me".into()]),
        Some(10),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(hits.hits.iter().any(|hit| hit.id == first), "the surviving row stopped answering");
}

#[tokio::test]
async fn a_member_edited_since_the_pass_read_it_makes_apply_refuse() {
    let h = harness_or_skip!();
    put_raw(&h, "user:me", "migrations are forward-only").await;
    let second = put_raw(&h, "user:me", "Migrations are forward-only").await;

    cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None).await.unwrap();
    let rows = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    let p = rows
        .iter()
        .find(|p| p.kind == lumberroom_server::domain::cleanup::CleanupKind::Exact)
        .unwrap();

    sqlx::query("UPDATE memory SET content = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&second).unwrap())
        .bind("migrations are forward-only, and sqlx embeds them at compile time")
        .execute(&h.pool)
        .await
        .unwrap();

    let err = cleanup::apply(&h.ctx, h.repo.as_ref(), &p.id).await.unwrap_err();
    assert_eq!(err.kind, lumberroom_server::domain::errors::Kind::Conflict);
    assert!(
        err.client_message().contains("changed since"),
        "the refusal should say the store moved: {}",
        err.client_message()
    );
}

#[tokio::test]
async fn a_finding_the_owner_answered_by_hand_closes_itself() {
    let h = harness_or_skip!();
    let first = put_raw(&h, "user:me", "the eval fixture lives under ~/.config/lumberroom").await;
    let second = put_raw(&h, "user:me", "The eval fixture lives under ~/.config/lumberroom").await;

    cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None).await.unwrap();
    let rows = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    let p = rows
        .iter()
        .find(|p| p.kind == lumberroom_server::domain::cleanup::CleanupKind::Exact)
        .unwrap();

    // The owner resolves it himself, which is what he does with a contradiction.
    review::supersede(&h.ctx, &second, &first).await.unwrap();

    let closed = h.repo.close_answered(&h.ctx.cfg.tenant_id).await.unwrap();
    assert!(closed.contains(&p.id), "the proposal stayed queued after the store answered it");

    let still_proposed =
        cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    assert!(!still_proposed.iter().any(|q| q.id == p.id));
}

#[tokio::test]
async fn a_private_row_never_reaches_the_list_handed_to_a_model() {
    // The filter is a predicate inside the query, so this is the only place it can be checked.
    let h = harness_or_skip!();
    put_private(&h, "personal:finance", "the household budget review is the first Sunday").await;
    put_private(&h, "personal:finance", "The household budget review is the first Sunday").await;

    let (report, for_model) =
        cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None)
            .await
            .unwrap();
    assert!(
        for_model.is_empty(),
        "a private row was handed to the model path: {for_model:?} (report {report:?})"
    );

    // And the deterministic pass still sees them, because nothing it reads leaves the machine.
    let q = CandidateQuery {
        namespace: Some("personal:finance".into()),
        max_sensitivity: lumberroom_server::domain::types::Sensitivity::Sealed,
        grant: NamespaceGrant::everything(),
        since: None,
        limit: 100,
    };
    let groups = h.repo.exact_duplicates(&h.ctx.cfg.tenant_id, &q).await.unwrap();
    assert_eq!(groups.len(), 1, "the deterministic pass should see private duplicates");
}

#[tokio::test]
async fn a_client_without_may_ingest_cannot_run_the_pass_or_read_the_queue() {
    // The capability gate lives in the router, so a service-level test cannot see it. The owner's
    // own credential reaches every namespace and still carries no mayIngest.
    let h = harness_or_skip!();
    let (status, body) = h.post("/admin/cleanup/run", OWNER_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, 403, "the owner's read-everything token ran the pass: {body}");
    assert!(body.contains("may_ingest"), "the refusal should name the grant to edit: {body}");

    let (status, _) = h.get("/admin/cleanup/proposals", OWNER_TOKEN).await;
    assert_eq!(status, 403);

    let (status, body) = h.post("/admin/cleanup/run", INGEST_TOKEN, serde_json::json!({})).await;
    assert_eq!(status, 200, "the granted client was refused: {body}");
}

#[tokio::test]
async fn the_scheduled_pass_needs_no_principal_and_writes_proposals() {
    // The claim the in-server timer rests on: `run` takes a tenant, so a background task calls it
    // with no caller, no request and no invented identity. A synthetic principal holding `*` at
    // sealed would satisfy the old signature and then be reachable from anywhere.
    let h = harness_or_skip!();
    put_raw(&h, "user:me", "the scratch server refuses port 8787").await;
    put_raw(&h, "user:me", "The scratch server refuses port 8787  ").await;

    let (report, _) =
        cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None)
            .await
            .expect("a pass with no principal should run");
    assert_eq!(report.queued, 1, "the scheduled pass wrote nothing: {report:?}");
}

#[tokio::test]
async fn a_survivor_that_became_true_first_is_swapped_before_the_proposal_is_queued() {
    // The bug this exists for, found by applying a correct proposal against the owner's real store:
    // the pass picks its survivor by created_at or by which wording reads better, and supersession
    // validates on valid time. When those disagree the queue holds a finding nobody can act on.
    let h = harness_or_skip!();
    let older_fact = put_raw(&h, "user:me", "the console renders a supersession chain").await;
    let newer_fact = put_raw(&h, "user:me", "The console renders a supersession chain  ").await;
    // The row written second became true FIRST, which is what a backfill from transcript
    // timestamps produces and what migration 015 warns its values are.
    set_occurred(&h, &older_fact, "2026-08-14T12:55:45Z").await;
    set_occurred(&h, &newer_fact, "2026-08-14T12:54:08Z").await;

    cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None).await.unwrap();
    let rows = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    let p = rows
        .iter()
        .find(|p| p.kind == lumberroom_server::domain::cleanup::CleanupKind::Exact)
        .unwrap();

    assert_eq!(
        p.keep_id.as_deref(),
        Some(older_fact.as_str()),
        "the survivor should be the row that became true later, whatever order they were written in"
    );

    // And the proposal actually applies, which is the property the swap exists to give.
    cleanup::apply(&h.ctx, h.repo.as_ref(), &p.id)
        .await
        .expect("a reconciled proposal must be appliable");
}

#[tokio::test]
async fn a_cluster_with_no_valid_time_is_left_alone() {
    // The guard fires on two known dates in the wrong order and nothing else. Most of the store
    // carries no occurred_at, and inventing an ordering for it would be worse than leaving it.
    let h = harness_or_skip!();
    let first = put_raw(&h, "user:me", "the recall monitor compares two scans").await;
    put_raw(&h, "user:me", "The recall monitor compares two scans  ").await;

    cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None).await.unwrap();
    let rows = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    let p = rows
        .iter()
        .find(|p| p.kind == lumberroom_server::domain::cleanup::CleanupKind::Exact)
        .unwrap();
    assert_eq!(p.keep_id.as_deref(), Some(first.as_str()), "the oldest should still survive");
}

// -- the grant, over HTTP ------------------------------------------------------------------------

/// A client with ingestion and one namespace runs the pass and gets that namespace's findings.
///
/// Before the grant went into the query, a run with no `namespace` read every namespace at sealed
/// and returned the text of every pair above the floor in `for_the_model`, whatever the caller was
/// granted. The route answered 500 for a band holding a private pair and 200 otherwise, which made
/// the floor itself readable by walking it.
#[tokio::test]
async fn a_narrow_client_running_the_pass_reads_nothing_outside_its_grant() {
    let h = harness_or_skip!();
    put_raw(&h, "user:me", "the owner's laptop is called kestrel").await;
    put_raw(&h, "user:me", "The owner's laptop is called kestrel  ").await;
    put_raw(&h, "user:me", "the owner's phone is on silent after ten").await;
    let mine_a =
        put_raw(&h, "project:lumberroom", "the gate script is scripts/deploy-check.sh").await;
    let mine_b =
        put_raw(&h, "project:lumberroom", "The gate script is scripts/deploy-check.sh").await;
    let mine_c = put_raw(&h, "project:lumberroom", "the builder image carries g++").await;

    // Floor at zero so every pair in reach lands in the model band, which is the band that leaves
    // the machine and the one a narrow caller must not be able to widen.
    let (status, body) = h
        .post(
            "/admin/cleanup/run",
            NARROW_TOKEN,
            serde_json::json!({ "cadence": "hourly", "min_similarity": 0.0 }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["report"]["exact_groups"], 1, "only the project pair is in reach: {body}");
    assert_eq!(v["report"]["queued"], 1, "{body}");
    let pairs = v["for_the_model"].as_array().unwrap();
    assert!(
        !pairs.is_empty(),
        "the floor at zero should put the unrelated project row in the band"
    );
    for pair in pairs {
        assert_eq!(pair["namespace"], "project:lumberroom", "a pair leaked past the grant: {body}");
        for id in [&pair["a_id"], &pair["b_id"]] {
            let id = id.as_str().unwrap();
            assert!(
                id == mine_a || id == mine_b || id == mine_c,
                "an id outside the grant was returned: {body}"
            );
        }
    }

    // The user:me pair was never queued, because the narrow run never saw it.
    let outside: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cleanup_proposal WHERE namespace = 'user:me'")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(outside, 0, "a narrow run queued a finding outside its grant");

    // And a scope outside the grant is refused rather than narrowed to nothing.
    let (status, body) = h
        .post("/admin/cleanup/run", NARROW_TOKEN, serde_json::json!({ "namespace": "user:me" }))
        .await;
    assert_eq!(status, 403, "{body}");
    let (status, body) =
        h.post("/admin/cleanup/run", NARROW_TOKEN, serde_json::json!({ "namespace": "*" })).await;
    assert_eq!(status, 403, "a prefix grant does not cover the whole store: {body}");
}

/// A private pair in the model band is counted and withheld. It used to fail the run with an
/// internal error, and the exact floor at which a scope flipped from 200 to 500 was the cosine of
/// its closest private pair.
#[tokio::test]
async fn a_private_pair_in_the_model_band_is_withheld_rather_than_failing_the_run() {
    let h = harness_or_skip!();
    put_private(&h, "personal:finance", "the mortgage renews in March").await;
    put_private(&h, "personal:finance", "the car loan ends in June").await;

    let (report, for_model) =
        cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, Some(0.0))
            .await
            .expect("a private pair in the band must not fail the pass");
    assert!(for_model.is_empty(), "a private row reached the model list: {for_model:?}");
    assert!(report.withheld_from_model >= 1, "the withheld pair was not counted: {report:?}");
}

/// The queue reads apply the grant in the query. A proposal with a member the caller cannot read
/// is absent from the list, and its id answers 404 from every route that takes one.
#[tokio::test]
async fn the_queue_shows_a_narrow_client_only_proposals_it_could_have_produced() {
    let h = harness_or_skip!();
    put_raw(&h, "user:me", "the backup runs at 02:00").await;
    put_raw(&h, "user:me", "The backup runs at 02:00  ").await;
    put_raw(&h, "project:lumberroom", "the console listens on 8787").await;
    put_raw(&h, "project:lumberroom", "The console listens on 8787  ").await;
    cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, None).await.unwrap();

    let everything = cleanup::list(&h.ctx, h.repo.as_ref(), Some("proposed"), 50).await.unwrap();
    assert_eq!(everything.len(), 2, "the fixture should have queued one finding per namespace");
    let theirs = everything.iter().find(|p| p.namespace == "user:me").unwrap();
    let mine = everything.iter().find(|p| p.namespace == "project:lumberroom").unwrap();

    let (status, body) = h.get("/admin/cleanup/proposals?state=proposed", NARROW_TOKEN).await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let ids: Vec<&str> =
        v["proposals"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![mine.id.as_str()], "{body}");

    let (status, _) = h.get(&format!("/admin/cleanup/proposals/{}", theirs.id), NARROW_TOKEN).await;
    assert_eq!(status, 404, "an id outside the grant must read as missing");
    let (status, _) = h.get(&format!("/admin/cleanup/proposals/{}", mine.id), NARROW_TOKEN).await;
    assert_eq!(status, 200);

    for action in ["reject", "unreject", "apply"] {
        let (status, body) = h
            .post(
                &format!("/admin/cleanup/proposals/{}/{action}", theirs.id),
                NARROW_TOKEN,
                serde_json::json!({}),
            )
            .await;
        assert_eq!(status, 404, "{action} reached a proposal outside the grant: {body}");
    }
    let state: String = sqlx::query_scalar("SELECT state FROM cleanup_proposal WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&theirs.id).unwrap())
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(state, "proposed", "a refused route still decided the proposal");
}

/// A posted finding has to name rows the poster can read, in the namespace it claims, holding the
/// text it says it saw. Anything else is refused before the queue, and a missing row and a row
/// outside the grant are refused with the same answer.
#[tokio::test]
async fn a_posted_finding_cannot_name_a_row_the_poster_may_not_read() {
    let h = harness_or_skip!();
    let theirs = put_raw(&h, "user:me", "the owner's passport renews in 2031").await;
    let mine_old = put_raw(&h, "project:lumberroom", "the image is built from Dockerfile").await;
    let mine_new = put_raw(&h, "project:lumberroom", "The image is built from Dockerfile.").await;

    let stale = |id: &str, namespace: &str, seen: &str| {
        serde_json::json!({ "proposals": [{
            "kind": "stale",
            "namespace": namespace,
            "rationale": "nothing has read this in the 214 days since it was written.",
            "produced_by": "unread",
            "members": [{ "memory_id": id, "disposition": "retire", "seen_content": seen }],
        }] })
    };

    // The forged deletion from the report: a real id outside the grant, its exact text.
    let (outside, body) = h
        .post(
            "/admin/cleanup/proposals",
            NARROW_TOKEN,
            stale(&theirs, "user:me", "the owner's passport renews in 2031"),
        )
        .await;
    assert_eq!(outside, 404, "{body}");
    let (missing, _) = h
        .post(
            "/admin/cleanup/proposals",
            NARROW_TOKEN,
            stale(&uuid::Uuid::new_v4().to_string(), "user:me", "anything"),
        )
        .await;
    assert_eq!(missing, outside, "a missing row and a row outside the grant must answer alike");

    // Inside the grant, a stale finding is a deletion and the poster holds no may_delete.
    let (status, body) = h
        .post(
            "/admin/cleanup/proposals",
            NARROW_TOKEN,
            stale(&mine_old, "project:lumberroom", "the image is built from Dockerfile"),
        )
        .await;
    assert_eq!(status, 403, "{body}");

    let paraphrase = |namespace: &str, keep: &str, seen_new: &str| {
        serde_json::json!({ "proposals": [{
            "kind": "paraphrase",
            "namespace": namespace,
            "keep_id": keep,
            "rationale": "these two say the same thing.",
            "produced_by": "qwen/qwen3.7-flash",
            "similarity": 0.91,
            "members": [
                { "memory_id": mine_new, "disposition": "keep", "seen_content": seen_new },
                { "memory_id": mine_old, "disposition": "retire",
                  "seen_content": "the image is built from Dockerfile" },
            ],
        }] })
    };

    // The namespace the item claims has to be where the members live.
    let (status, body) = h
        .post(
            "/admin/cleanup/proposals",
            NARROW_TOKEN,
            paraphrase("project:other", &mine_new, "The image is built from Dockerfile."),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    // And the text the poster says it saw has to be the text the row holds.
    let (status, body) = h
        .post(
            "/admin/cleanup/proposals",
            NARROW_TOKEN,
            paraphrase("project:lumberroom", &mine_new, "something the row does not say"),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    // The survivor has to be one of the cluster's own keep members.
    let (status, body) = h
        .post(
            "/admin/cleanup/proposals",
            NARROW_TOKEN,
            paraphrase("project:lumberroom", &theirs, "The image is built from Dockerfile."),
        )
        .await;
    assert_eq!(status, 400, "{body}");

    // The honest shape queues, and the queue says who posted it.
    let (status, body) = h
        .post(
            "/admin/cleanup/proposals",
            NARROW_TOKEN,
            paraphrase("project:lumberroom", &mine_new, "The image is built from Dockerfile."),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["queued"], 1, "{body}");
    let id = v["ids"][0].as_str().unwrap();
    let shown = cleanup::get(&h.ctx, h.repo.as_ref(), id).await.unwrap().unwrap();
    assert_eq!(shown.posted_by.as_deref(), Some("narrow"));
    assert_eq!(shown.produced_by, "qwen/qwen3.7-flash");

    let forged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cleanup_proposal_member WHERE memory_id = $1")
            .bind(uuid::Uuid::parse_str(&theirs).unwrap())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(forged, 0, "a refused post still wrote a member row");
}

/// The in-process pass writes no poster, and a pair across two namespaces is not a finding.
#[tokio::test]
async fn the_scheduled_pass_stays_inside_one_namespace_and_names_no_poster() {
    let h = harness_or_skip!();
    put_raw(&h, "user:me", "the release tag is cut from main").await;
    put_raw(&h, "project:lumberroom", "the release tag is cut from main").await;

    // Two rows, one per namespace, identical text. With the floor at zero the only thing keeping
    // them apart is the join.
    let (report, for_model) =
        cleanup::run(&h.ctx.cfg.tenant_id, h.repo.as_ref(), None, "hourly", 500, Some(0.0))
            .await
            .unwrap();
    assert_eq!(report.exact_groups, 0, "exact groups are per namespace: {report:?}");
    assert_eq!(report.queued, 0, "a cross-namespace pair was queued: {report:?}");
    assert!(for_model.is_empty(), "a cross-namespace pair reached the model band: {for_model:?}");
    let straddling: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM cleanup_proposal p
           JOIN cleanup_proposal_member cm ON cm.proposal_id = p.id
           JOIN memory m ON m.id = cm.memory_id
          WHERE m.namespace <> p.namespace",
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(straddling, 0);

    let posters: Vec<Option<String>> = sqlx::query_scalar("SELECT posted_by FROM cleanup_proposal")
        .fetch_all(&h.pool)
        .await
        .unwrap();
    assert!(posters.iter().all(Option::is_none), "the in-process pass named a poster: {posters:?}");
}

// ---- decision 0014 part 3: supersession proposals from a declared subject ----

/// The declaration is the whole gate. An undeclared tag proposes nothing however dated its rows
/// are, because cardinality is not in the text and guessing it hides live facts.
#[tokio::test]
async fn an_undeclared_subject_proposes_nothing_however_dated_its_facts_are() {
    let h = harness_or_skip!();
    let day =
        |d: u32| -> chrono::DateTime<Utc> { format!("2026-0{}-01T00:00:00Z", d).parse().unwrap() };

    for (n, text) in [(1u32, "the retainer is 2k a month"), (3, "the retainer is 4k a month")] {
        write::run(
            &h.ctx,
            text,
            "user:me",
            Some(vec!["retainer".into()]),
            None,
            None,
            Some(day(n)),
        )
        .await
        .unwrap();
    }

    let quiet = supersession::run(&h.ctx, h.repo.as_ref()).await.unwrap();
    assert_eq!(quiet.tags_scanned, 0, "nothing is declared, so nothing is looked at");
    assert_eq!(quiet.queued, 0);

    // The preview answers before anything is declared, which is the point: the owner sees what a
    // declaration would end while it is still costless to change their mind.
    let p = supersession::preview(&h.ctx, h.repo.as_ref(), "retainer").await.unwrap();
    assert_eq!(p.dated_rows, 2);
    assert_eq!(p.would_end.len(), 1, "two facts in a row make one ending");
    assert_eq!(p.same_day_skipped, 0);

    // Declaring it many is a decision too, and it stays quiet.
    supersession::declare(&h.ctx, h.repo.as_ref(), "retainer", Arity::Many, None).await.unwrap();
    let still_quiet = supersession::run(&h.ctx, h.repo.as_ref()).await.unwrap();
    assert_eq!(still_quiet.queued, 0, "a list is not a sequence of replacements");

    // Declaring it single is what produces the pair, and it proposes rather than applies.
    supersession::declare(&h.ctx, h.repo.as_ref(), "retainer", Arity::Single, Some("one rate"))
        .await
        .unwrap();
    let report = supersession::run(&h.ctx, h.repo.as_ref()).await.unwrap();
    assert_eq!(report.tags_scanned, 1);
    assert_eq!(report.queued, 1, "{report:?}");

    // Nothing was retired. The queue holds a proposal and the store is untouched.
    let live = search::run(&h.ctx, "the retainer", None, Some(10), None, None, None).await.unwrap();
    assert_eq!(live.hits.len(), 2, "the pass proposes and never applies");

    // Running it again queues nothing, which is what makes an hourly cadence safe.
    let again = supersession::run(&h.ctx, h.repo.as_ref()).await.unwrap();
    assert_eq!(again.queued, 0);
    assert_eq!(again.already_known, 1);
}

/// A declaration is reversible, and the reversal is what makes declaring safe to try.
#[tokio::test]
async fn a_declaration_can_be_withdrawn() {
    let h = harness_or_skip!();
    supersession::declare(&h.ctx, h.repo.as_ref(), "rate", Arity::Single, None).await.unwrap();
    assert_eq!(supersession::declarations(&h.ctx, h.repo.as_ref()).await.unwrap().len(), 1);

    // Re-declaring replaces rather than appends: two contradictory declarations would leave the
    // pass to pick, which is the guess this whole design refuses.
    supersession::declare(&h.ctx, h.repo.as_ref(), "rate", Arity::Many, None).await.unwrap();
    let all = supersession::declarations(&h.ctx, h.repo.as_ref()).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].arity, Arity::Many);

    assert!(supersession::forget(&h.ctx, h.repo.as_ref(), "rate").await.unwrap());
    assert!(supersession::declarations(&h.ctx, h.repo.as_ref()).await.unwrap().is_empty());
    assert!(!supersession::forget(&h.ctx, h.repo.as_ref(), "rate").await.unwrap());
}

/// Same-day facts are the dump's own shape, and closing their period would write "never true".
#[tokio::test]
async fn facts_sharing_a_day_are_reported_and_never_proposed() {
    let h = harness_or_skip!();
    let day: chrono::DateTime<Utc> = "2026-04-04T00:00:00Z".parse().unwrap();
    for text in ["the plan is monthly", "the plan is yearly"] {
        write::run(&h.ctx, text, "user:me", Some(vec!["plan".into()]), None, None, Some(day))
            .await
            .unwrap();
    }
    supersession::declare(&h.ctx, h.repo.as_ref(), "plan", Arity::Single, None).await.unwrap();

    let report = supersession::run(&h.ctx, h.repo.as_ref()).await.unwrap();
    assert_eq!(report.queued, 0, "an empty interval is not an ending");
    assert_eq!(report.same_day_skipped, 1, "and the owner is told it was skipped");
}
