//! Integration suite. Talks to a real Postgres with pgvector, in its own database, using the hash
//! embedder so nothing downloads. Skipped when no database is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom cargo test --test integration

use std::collections::HashMap;
use std::sync::Arc;

use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::{NamespaceGrant, SensitivityDefaults};
use lumberroom_server::domain::types::{Invocation, Principal, Sensitivity, ToolCall};
use lumberroom_server::ports::registry::RegistryUpsert;
use lumberroom_server::ports::RegistryWrite;
use lumberroom_server::services::{
    bootstrap, currency, export, forget, graph, recall, registry, review, search, write, Ctx, Repos,
};
use sqlx::{PgPool, Row};

mod common;

const TEST_DB: &str = "lumberroom_rust_test";

/// The KEK every test in this file encrypts under. Fixed rather than generated, so a failure that
/// leaves rows behind is still readable by the next run.
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";

/// Cargo runs tests in parallel and each of these truncates the shared test database, so they
/// serialise themselves rather than depending on `--test-threads=1` being remembered. The guard
/// is held for the lifetime of the test.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A setup step that is allowed to be missing, with the reason printed rather than swallowed.
///
/// Every fallible step below used to end in `.ok()?`, so a laptop with no Postgres and a container
/// whose migrations no longer match the store both skipped with the same sentence. The suite
/// already skips rather than fails, which makes the printed reason the only thing standing between
/// a broken run and a run somebody reads as a pass.
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

/// Returns None when no database is reachable, so the suite skips rather than fails on a machine
/// without one.
async fn setup() -> Option<(Ctx, PgPool, tokio::sync::MutexGuard<'static, ()>, common::DbGuard)> {
    setup_with(|_| {}).await
}

/// `setup`, with the loaded config handed to the caller before anything is built from it.
///
/// Tuning happens on the struct rather than through the process environment. `config::load` reads
/// environment variables, and a test that mutated one would be changing global state that the next
/// test in the same process inherits, which is a flake nobody can reproduce.
async fn setup_with(
    tune: impl FnOnce(&mut Config),
) -> Option<(Ctx, PgPool, tokio::sync::MutexGuard<'static, ()>, common::DbGuard)> {
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
        // DDL cannot take a bind parameter, so this is the one statement in the codebase that
        // must be built as a string. sqlx refuses that by default and requires the audit to be
        // written down as `AssertSqlSafe`, which is a word that shows up in review. Audited:
        // TEST_DB is a compile-time constant with no external input.
        let created = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {TEST_DB}")))
            .execute(&admin)
            .await;
        step!("creating the test database", created);
    }
    admin.close().await;

    let url = format!("{base}/{TEST_DB}");
    std::env::set_var("DATABASE_URL", &url);
    std::env::set_var("AUTH_TOKENS", format!("mac:{}", "m".repeat(32)));
    std::env::set_var("EMBED_PROVIDER", "hash");
    std::env::set_var(TEST_KEK_VAR, TEST_KEK_HEX);

    // Before the truncate below, and before anything reads. Every other binary
    // targeting this database waits here.
    let db_lock = common::lock_database(&url).await?;
    let pool = step!("connecting to the test database", postgres::connect(&url).await);
    step!("migrating the test database", postgres::migrate(&pool).await);
    // `kek_state` goes with the rows it describes. Every encrypted row is truncated here, so a
    // fingerprint recorded by an earlier run describes nothing and would only report this suite's
    // own key as a rotation.
    let truncated = sqlx::query(
        "TRUNCATE memory, registry, registry_history, entity_alias, sealed_item, tool_calls,
                  registry_alias, kek_state,
                  cleanup_proposal, cleanup_proposal_member, cleanup_watermark, subject_cardinality,
                  oauth_client, oauth_code, oauth_token, oauth_refresh
         RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await;
    step!("truncating the test database", truncated);

    let mut cfg: Config = step!("loading the config", config::load());
    tune(&mut cfg);

    // Encryption is wired in every test, not only the ones that exercise it. The store reads
    // `kek_state.kek_id` on the private write path, so a Ctx that carried a provider without the
    // boot check having run would refuse private writes with a message that points nowhere near
    // the missing step.
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

    // Composed the way main.rs composes it: one concrete memory repository handed up as both the
    // port the services read through and the ciphertext reader they decrypt through. A test that
    // built these separately would not exercise the seam the private read path depends on.
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
        },
        embedder: Arc::new(HashEmbedder::new(768)),
        keys: Some(keys),
        kek_verified,
        principal: owner_like("mac"),
        invocation: Invocation::Cli,
        session_id: Some("test-session".into()),
    };
    bootstrap::clear_cache();
    Some((ctx, pool, guard, db_lock))
}

macro_rules! ctx_or_skip {
    () => {
        match setup().await {
            // Both guards are bound so they live as long as the test body. The mutex serialises
            // this binary's threads; the DbGuard holds the advisory lock the other five wait on.
            Some((ctx, pool, guard, db)) => (ctx, pool, (guard, db)),
            None => {
                eprintln!("skipping: no database reachable");
                return;
            }
        }
    };
    ($tune:expr) => {
        match setup_with($tune).await {
            Some((ctx, pool, guard, db)) => (ctx, pool, (guard, db)),
            None => {
                eprintln!("skipping: no database reachable");
                return;
            }
        }
    };
}

fn restricted(ctx: &Ctx, read: &[&str], write: &[&str]) -> Ctx {
    // Open ceilings, which is what a bare glob in AUTH_TOKENS resolves to: a Phase 1 grant must
    // never silently gain access to private content when the sensitivity axis lands.
    let at_open = |names: &[&str]| -> Vec<(String, Sensitivity)> {
        names.iter().map(|n| ((*n).to_string(), Sensitivity::Open)).collect()
    };
    restricted_at(ctx, &at_open(read), &at_open(write))
}

/// A second client with an explicit ceiling per namespace. Namespace alone is not a grant, which is
/// the whole point of the two-axis model, so most policy tests need to set the second axis.
fn restricted_at(
    ctx: &Ctx,
    read: &[(String, Sensitivity)],
    write: &[(String, Sensitivity)],
) -> Ctx {
    let mut c = ctx.clone();
    let grants = |spec: &[(String, Sensitivity)]| -> Vec<NamespaceGrant> {
        spec.iter().map(|(ns, max)| NamespaceGrant::new(ns.clone(), *max)).collect()
    };
    c.principal = Principal {
        client: "browser".into(),
        token_id: "test".into(),
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

/// `(namespace, ceiling)` pairs, spelled once so the grant tables in the tests read as tables.
fn at(spec: &[(&str, Sensitivity)]) -> Vec<(String, Sensitivity)> {
    spec.iter().map(|(ns, max)| ((*ns).to_string(), *max)).collect()
}

/// Everything a client could hold, so a test can vary one flag against a known baseline.
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

/// A distinctive string that cannot appear by accident, so "the nonce is absent" means the content
/// is absent rather than that the test looked in the wrong field.
fn nonce(label: &str) -> String {
    format!("zqxnonce{label}zqx")
}

/// The registry port takes one struct now, so the tests spell the fields once here rather than
/// three times inline. `review_after` stays None: the per-kind interval is the service's business
/// and these tests exercise the store.
fn registry_write(
    namespace: &str,
    value: &str,
    provenance: &lumberroom_server::domain::types::Provenance,
) -> RegistryWrite {
    registry_write_at(namespace, "mcp-endpoint", value, Sensitivity::Open, provenance)
}

fn registry_write_at(
    namespace: &str,
    key: &str,
    value: &str,
    sensitivity: Sensitivity,
    provenance: &lumberroom_server::domain::types::Provenance,
) -> RegistryWrite {
    RegistryWrite {
        tenant_id: "me".into(),
        namespace: namespace.into(),
        kind: "host".into(),
        key: key.into(),
        value: serde_json::json!(value),
        provenance: provenance.clone(),
        sensitivity,
        // The helpers write as the owner, whose ceiling reaches everything.
        replace_ceiling: Sensitivity::Sealed,
        review_after: None,
    }
}

fn provenance() -> lumberroom_server::domain::types::Provenance {
    lumberroom_server::domain::types::Provenance {
        source_client: "mac".into(),
        conv_id: None,
        confidence: 1.0,
        user_confirmed: true,
        valid_from: "2026-08-19".into(),
    }
}

#[tokio::test]
async fn schema_and_dimension_guard() {
    let (_ctx, pool, _serial) = ctx_or_skip!();
    assert_eq!(postgres::assert_embedding_dim(&pool, 768).await.unwrap(), 768);
    // A mismatch must fail loudly rather than producing a confusing error on every write.
    assert!(postgres::assert_embedding_dim(&pool, 1536).await.is_err());
}

#[tokio::test]
async fn writes_embeds_and_attributes_the_source_client() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let res = write::run(
        &ctx,
        "Dana deploys lumberroom on an Oracle Ampere A1 instance",
        "global",
        Some(vec!["Infra".into(), "infra".into(), " deploy ".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!res.deduplicated);

    let stored = ctx
        .repos
        .memories
        .find_by_id(&ctx.cfg.tenant_id, res.id.parse().unwrap())
        .await
        .unwrap()
        .expect("row should exist");
    assert_eq!(stored.source_client, "mac");
    assert_eq!(stored.tags, vec!["infra", "deploy"]);
    assert!(stored.embedding_model.unwrap().contains("hash"));
}

#[tokio::test]
async fn collapses_an_exact_duplicate_and_keeps_it_per_namespace() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let first =
        write::run(&ctx, "A fact stated twice", "user:me", None, None, None, None).await.unwrap();
    let second = write::run(&ctx, "  A fact stated twice  ", "user:me", None, None, None, None)
        .await
        .unwrap();
    assert_eq!(first.id, second.id);
    assert!(second.deduplicated);

    // The same sentence in another namespace is a different fact.
    let other =
        write::run(&ctx, "A fact stated twice", "global", None, None, None, None).await.unwrap();
    assert_ne!(other.id, first.id);
}

#[tokio::test]
async fn records_supersedes_without_acting_on_it() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let old =
        write::run(&ctx, "The old port was 8080", "global", None, None, None, None).await.unwrap();
    let new = write::run(&ctx, "The port is 8787", "global", None, Some(&old.id), None, None)
        .await
        .unwrap();

    let target: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT supersedes FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&new.id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(target.unwrap().to_string(), old.id);

    // The target is retired, not deleted: history stays queryable, which is what makes the
    // decision log a side effect rather than a feature to build.
    let retired: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT superseded_by FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&old.id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retired.expect("the old row is retired").to_string(), new.id);
}

#[tokio::test]
async fn rejects_validation_failures() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    assert!(write::run(&ctx, "x", "nope", None, None, None, None).await.is_err());
    assert!(write::run(&ctx, "   ", "global", None, None, None, None).await.is_err());
    assert!(write::run(
        &ctx,
        "x",
        "global",
        None,
        Some("00000000-0000-0000-0000-000000000000"),
        None,
        None
    )
    .await
    .is_err());
}

#[tokio::test]
async fn enforces_grants_on_read_and_write() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    write::run(
        &ctx,
        "Warden uses Django with Celery for scheduled jobs",
        "project:warden",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let limited = restricted(&ctx, &["global"], &["global"]);
    assert!(
        write::run(&limited, "nope", "user:me", None, None, None, None).await.is_err(),
        "a write outside the grant must fail loudly"
    );

    let res =
        search::run(&limited, "what does warden use", None, None, None, None, None).await.unwrap();
    assert!(
        res.hits.iter().all(|h| h.namespace != "project:warden"),
        "a namespace the client cannot read must not appear"
    );
}

#[tokio::test]
async fn refuses_to_supersede_a_row_the_client_cannot_write() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let mine =
        write::run(&ctx, "only the operator may retire this", "user:me", None, None, None, None)
            .await
            .unwrap();

    let limited = restricted(&ctx, &["*"], &["global"]);
    let err = write::run(
        &limited,
        "browser tries to retire it",
        "global",
        None,
        Some(&mine.id),
        None,
        None,
    )
    .await
    .unwrap_err();
    let msg = err.client_message().to_string();
    assert!(msg.contains("does not exist or is not writable"));
    // Naming the namespace would tell the client that a namespace it cannot write exists.
    assert!(!msg.contains("user:me"));
}

#[tokio::test]
async fn search_reaches_other_projects_and_promotes_the_active_one() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    write::run(
        &ctx,
        "Warden uses Django with Celery for scheduled jobs",
        "project:warden",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let without =
        search::run(&ctx, "what does warden use for scheduled jobs", None, None, None, None, None)
            .await
            .unwrap();
    assert!(without.also_searched.contains(&"project:warden".to_string()));
    let hit_without = without.hits.iter().find(|h| h.content.contains("Celery")).expect("found");

    let with = search::run(
        &ctx,
        "what does warden use for scheduled jobs",
        None,
        None,
        Some("warden"),
        None,
        None,
    )
    .await
    .unwrap();
    let hit_with = with.hits.iter().find(|h| h.content.contains("Celery")).expect("found");
    assert!(hit_with.score > hit_without.score, "passing project must promote it");
    assert!(hit_with.primary);
}

#[tokio::test]
async fn an_explicit_namespace_list_is_honoured_exactly() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    write::run(&ctx, "Warden uses Celery", "project:warden", None, None, None, None).await.unwrap();
    let res = search::run(
        &ctx,
        "what does warden use",
        Some(vec!["user:me".into()]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(res.also_searched.is_empty(), "no widening when namespaces are explicit");
    assert!(res.hits.iter().all(|h| h.namespace == "user:me"));
}

#[tokio::test]
async fn treats_sql_payloads_as_content_not_as_sql() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let payload = "'; DROP TABLE memory; --";
    let written = write::run(
        &ctx,
        payload,
        "global",
        Some(vec!["x'; DROP TABLE registry; --".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let back = ctx
        .repos
        .memories
        .find_by_id(&ctx.cfg.tenant_id, written.id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(back.content, payload);

    assert!(search::run(&ctx, "' OR 1=1; DROP TABLE memory; --", None, None, None, None, None)
        .await
        .is_ok());
    let got = registry::get(&ctx, "host", "') ; DROP TABLE memory; --", None, None).await.unwrap();
    assert!(!got.found);

    let tables: Vec<String> =
        sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")
            .fetch_all(&pool)
            .await
            .unwrap();
    for t in ["memory", "registry", "tool_calls"] {
        assert!(tables.iter().any(|x| x == t), "{t} must still exist");
    }
}

#[tokio::test]
async fn registry_is_exact_and_prefers_a_project_override() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let provenance = lumberroom_server::domain::types::Provenance {
        source_client: "mac".into(),
        conv_id: None,
        confidence: 1.0,
        user_confirmed: true,
        valid_from: "2026-08-19".into(),
    };
    ctx.repos
        .registry
        .upsert(registry_write("global", "https://lumberroom.example.com/mcp", &provenance))
        .await
        .unwrap();

    let found = registry::get(&ctx, "host", "mcp-endpoint", None, None).await.unwrap();
    assert!(found.found);
    assert_eq!(found.namespace.as_deref(), Some("global"));

    // No fuzziness.
    assert!(!registry::get(&ctx, "host", "mcp-endpoints", None, None).await.unwrap().found);

    ctx.repos
        .registry
        .upsert(registry_write("project:warden", "https://warden.internal/mcp", &provenance))
        .await
        .unwrap();
    let override_hit =
        registry::get(&ctx, "host", "mcp-endpoint", None, Some("warden")).await.unwrap();
    assert_eq!(override_hit.namespace.as_deref(), Some("project:warden"));

    // The upsert bumps the version rather than inserting a second row.
    let written = ctx
        .repos
        .registry
        .upsert(registry_write("global", "https://changed.example.com/mcp", &provenance))
        .await
        .unwrap();
    assert!(matches!(written, RegistryUpsert::Written { version: 2, .. }), "{written:?}");
}

#[tokio::test]
async fn digest_covers_readable_namespaces_and_caches() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    write::run(
        &ctx,
        "Dana prefers TypeScript for server work",
        "user:me",
        Some(vec!["preference".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "Warden uses Celery for scheduled jobs",
        "project:warden",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    bootstrap::clear_cache();
    let d = bootstrap::run(&ctx, Some("/tmp/warden")).await.unwrap();
    assert!(!d.cached);
    assert_eq!(d.project.as_deref(), Some("project:warden"));
    assert!(d.text.contains("## Memory digest"));
    assert!(d.project_context.iter().any(|f| f.content.contains("Celery")));

    let again = bootstrap::run(&ctx, Some("/tmp/warden")).await.unwrap();
    assert!(again.cached, "the second call should be served from cache");

    // A write must invalidate, or a fact written now appears only after the TTL.
    write::run(&ctx, "A fact written moments ago", "user:me", None, None, None, None)
        .await
        .unwrap();
    let fresh = bootstrap::run(&ctx, Some("/tmp/warden")).await.unwrap();
    assert!(!fresh.cached);
    assert!(fresh.text.contains("A fact written moments ago"));
}

#[tokio::test]
async fn digest_hides_namespaces_a_restricted_client_cannot_read() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    write::run(
        &ctx,
        "Warden uses Celery for scheduled jobs",
        "project:warden",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    bootstrap::clear_cache();

    // Every subquery must intersect the grant. The TypeScript build shipped a bug where the
    // profile and project subqueries skipped this filter, and this is the test that caught it.
    let limited = restricted(&ctx, &["global"], &["global"]);
    let d = bootstrap::run(&limited, Some("/tmp/warden")).await.unwrap();
    assert_eq!(d.namespaces, vec!["global"]);
    assert!(!d.text.contains("Celery"));
}

#[tokio::test]
async fn bootstrap_stays_inside_the_latency_budget() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    write::run(&ctx, "a fact to make the digest non-empty", "global", None, None, None, None)
        .await
        .unwrap();
    bootstrap::clear_cache();
    let started = std::time::Instant::now();
    bootstrap::run(&ctx, Some("/tmp/warden")).await.unwrap();
    assert!(started.elapsed().as_millis() < 200, "PRD §5 budget");
}

#[tokio::test]
async fn filtered_search_returns_the_full_limit_from_a_sparse_namespace() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    // With hnsw.iterative_scan off, a selective filter silently returns fewer rows than asked
    // for: the scan pulls a fixed candidate batch, the filter removes all of it, and the caller
    // is told nothing is known. Migration 003 sets strict_order. This fails if that is lost.
    for i in 0..40 {
        write::run(
            &ctx,
            &format!("bulk filler fact number {i} about unrelated matters"),
            "global",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    for i in 0..6 {
        write::run(
            &ctx,
            &format!("scarce namespace fact {i} concerning the rare project"),
            "project:scarce",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let res = search::run(
        &ctx,
        "what do we know about the rare project",
        Some(vec!["project:scarce".into()]),
        Some(5),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(res.hits.len(), 5);
    assert!(res.hits.iter().all(|h| h.namespace == "project:scarce"));
}

#[tokio::test]
async fn reports_honest_recall_against_an_exact_scan() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    for i in 0..12 {
        write::run(
            &ctx,
            &format!("recall probe fact {i} about infrastructure"),
            "global",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let report = recall::measure(&ctx, 10, 5).await.unwrap();
    assert!(report.sampled > 0);
    assert!(report.recall_at_k >= 0.9, "recall was {}", report.recall_at_k);
    assert_eq!(report.top_one_misses, 0);
}

/// The wire contract is snake_case throughout and the CLI depends on it. A rename on the domain
/// side once turned every latency into "-ms" with nothing failing, so the key set is pinned.
#[tokio::test]
async fn published_payloads_keep_their_field_names() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let written = write::run(
        &ctx,
        "a fact so recall has something to sample",
        "global",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // memory_write. `superseded` and `possible_conflicts` are skipped when there is nothing to
    // report, so this write, which retires nothing and has no near neighbour, publishes four keys.
    let json = serde_json::to_value(&written).unwrap();
    let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["deduplicated", "id", "namespace", "sensitivity"]);

    // memory_search. `superseded_by` is skipped unless the caller asked for history.
    let hits = search::run(&ctx, "what has something to sample", None, None, None, None, None)
        .await
        .unwrap();
    let json =
        serde_json::to_value(hits.hits.first().expect("the written row comes back")).unwrap();
    let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "content",
            "created_at",
            "id",
            "namespace",
            "primary",
            "score",
            "sensitivity",
            "similarity",
            "source_client",
            "tags",
        ]
    );

    // context_bootstrap. The digest is what every surface reads first, and the sealed inventory is
    // the Phase 3 addition a client has to be able to find.
    let digest = bootstrap::run(&ctx, None).await.unwrap();
    let json = serde_json::to_value(&digest).unwrap();
    let keys = json.as_object().unwrap();
    for field in ["text", "counts", "sealed_inventory"] {
        assert!(keys.contains_key(field), "digest lost {field}");
    }

    let report = recall::measure(&ctx, 2, 3).await.unwrap();
    let json = serde_json::to_value(&report).unwrap();
    let mut keys: Vec<&str> = json.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "exact_ms",
            // Added with the planner check. Present on every report, `null` when the indexed arm
            // finished inside the clock's resolution, which is what a small store does.
            "exact_speedup",
            "index_ms",
            "k",
            "recall_at_k",
            "sampled",
            "top_one_misses",
            "worst",
        ]
    );

    let stats = ctx.repos.tool_calls.stats(1).await.unwrap();
    let json = serde_json::to_value(&stats).unwrap();
    if let Some(first) = json.as_array().and_then(|a| a.first()) {
        let mut keys: Vec<&str> = first.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["calls", "client", "failures", "p50_ms", "p95_ms", "tool", "unprompted"]
        );
    }
}

#[tokio::test]
async fn separates_model_initiated_calls_from_hook_and_cli_calls() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    for (tool, succeeded, unprompted, latency) in [
        ("context_bootstrap", true, false, 12),
        ("memory_write", true, true, 40),
        ("memory_write", false, true, 5),
    ] {
        ctx.repos.tool_calls.record(ToolCall {
            client: "mac".into(),
            tool: tool.into(),
            succeeded,
            unprompted,
            latency_ms: latency,
            session_id: Some("test-session".into()),
            namespace: None,
        });
    }
    // record() is fire and forget by contract, so give the spawned inserts a moment.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let stats = ctx.repos.tool_calls.stats(1).await.unwrap();
    let by_tool: HashMap<&str, &lumberroom_server::ports::ToolCallStats> =
        stats.iter().map(|s| (s.tool.as_str(), s)).collect();
    let writes = by_tool.get("memory_write").expect("writes recorded");
    assert_eq!(writes.unprompted, 2);
    assert_eq!(writes.failures, 1);
    let boots = by_tool.get("context_bootstrap").expect("bootstrap recorded");
    assert_eq!(boots.unprompted, 0);
    assert!(boots.p50_ms.unwrap() > 0);
}

// ---------------------------------------------------------------------------------------------
// POLICY. The Phase 3 exit criterion, as tests rather than as a script.
//
// The leak path in a memory system is the convenience surface, not the obvious one. Every arm is
// asserted separately: a combined assertion passes while one arm leaks.
// ---------------------------------------------------------------------------------------------

/// The whole published digest as one string, so an assertion sweeps `text`, every fact list, the
/// inventory, the sealed inventory and both count maps at once. A per-field assertion misses the
/// field that was added after it was written.
fn digest_json(d: &lumberroom_server::services::bootstrap::Digest) -> String {
    serde_json::to_value(d).unwrap().to_string()
}

#[tokio::test]
async fn a_namespace_outside_the_grant_is_invisible_through_every_surface() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let secret = nonce("excluded");

    write::run(
        &ctx,
        &format!("the deploy key is {secret}"),
        "project:vault",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();
    write::run(&ctx, "an open fact anyone may read", "global", None, None, None, None)
        .await
        .unwrap();
    ctx.repos
        .registry
        .upsert(registry_write_at(
            "project:vault",
            "vault-host",
            &secret,
            Sensitivity::Private,
            &provenance(),
        ))
        .await
        .unwrap();

    let limited = restricted(&ctx, &["global"], &["global"]);

    // memory_search, asking for the namespace by name so nothing is narrowed by the default list.
    let res = search::run(
        &limited,
        "what is the deploy key",
        Some(vec!["project:vault".into()]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(res.hits.is_empty(), "a namespace outside the grant answers nothing");

    // context_bootstrap, including the inventory line. The Phase 1 bug was a digest subquery that
    // skipped the grant filter, so this asserts on the digest specifically rather than on search.
    bootstrap::clear_cache();
    let d = bootstrap::run(&limited, Some("vault")).await.unwrap();
    assert!(
        !digest_json(&d).contains(&secret),
        "the digest leaked a row from an excluded namespace"
    );
    assert!(
        !d.inventory.contains_key("project:vault"),
        "the inventory named an excluded namespace"
    );
    assert!(!d.counts.by_namespace.contains_key("project:vault"));
    assert_eq!(d.counts.memories, 1, "only the one readable row counts");
    assert!(d.registry.iter().all(|r| r.namespace != "project:vault"));

    // registry_get.
    let got = registry::get(&limited, "host", "vault-host", None, None).await.unwrap();
    assert!(!got.found);
    assert!(!got.searched.iter().any(|n| n == "project:vault"));

    // And a write into it is refused loudly rather than dropped.
    let err = write::run(&limited, "trying anyway", "project:vault", None, None, None, None)
        .await
        .unwrap_err();
    assert_eq!(err.kind.http_status(), 403);
}

#[tokio::test]
async fn a_ceiling_of_open_cannot_read_a_private_row_in_a_namespace_it_reaches() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let secret = nonce("ceiling");

    write::run(
        &ctx,
        &format!("the salary review lands at {secret}"),
        "project:hr",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "the hr project uses the standard onboarding checklist",
        "project:hr",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Namespace alone is not a grant. This client reaches project:hr and still may not read what is
    // stored above open in it, which is the two-axis model's whole point.
    let at_open = restricted_at(&ctx, &at(&[("project:hr", Sensitivity::Open)]), &[]);
    let res = search::run(
        &at_open,
        "salary review checklist",
        Some(vec!["project:hr".into()]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        res.hits.iter().all(|h| !h.content.contains(&secret)),
        "a row above the ceiling came back"
    );
    assert_eq!(res.hits.len(), 1, "the open row in the same namespace still answers");

    bootstrap::clear_cache();
    let d = bootstrap::run(&at_open, Some("hr")).await.unwrap();
    assert!(!digest_json(&d).contains(&secret));
    assert_eq!(d.counts.by_namespace.get("project:hr"), Some(&1), "only the open row is counted");

    // The same namespace at the higher ceiling does see it, so the refusal above is the ceiling
    // rather than something else being broken.
    let at_private = restricted_at(&ctx, &at(&[("project:hr", Sensitivity::Private)]), &[]);
    let res = search::run(
        &at_private,
        "salary review checklist",
        Some(vec!["project:hr".into()]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        res.hits.iter().any(|h| h.content.contains(&secret)),
        "the private row must round-trip at private"
    );
}

#[tokio::test]
async fn two_clients_with_different_ceilings_do_not_share_a_cached_digest() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let secret = nonce("cachekey");
    write::run(
        &ctx,
        &format!("the private note says {secret}"),
        "user:me",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();

    bootstrap::clear_cache();
    // Same client name on purpose: the cache key has to separate these on the ceiling, not on the
    // name. A key built from namespaces alone serves A's digest to B, which is a leak with no
    // attacker in it.
    let mut high = restricted_at(&ctx, &at(&[("user:me", Sensitivity::Private)]), &[]);
    high.principal.client = "same".into();
    let mut low = restricted_at(&ctx, &at(&[("user:me", Sensitivity::Open)]), &[]);
    low.principal.client = "same".into();

    let a = bootstrap::run(&high, None).await.unwrap();
    assert!(digest_json(&a).contains(&secret), "the private-ceiling client should see it");

    let b = bootstrap::run(&low, None).await.unwrap();
    assert!(!b.cached, "a different ceiling must not hit the cache entry built for another one");
    assert!(!digest_json(&b).contains(&secret));
}

#[tokio::test]
async fn a_private_row_reaches_every_digest_section_it_belongs_to_and_no_further() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let secret = nonce("bothsections");
    write::run(
        &ctx,
        &format!("the private note says {secret}"),
        "user:me",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();

    // A fresh row in the user namespace lands in profile and in recent at once. The digest decrypts
    // all three sections in one pass, so a decryptor that spent the ciphertext on the first copy
    // reported the second as data loss and the row vanished from both.
    bootstrap::clear_cache();
    let high = restricted_at(&ctx, &at(&[("user:me", Sensitivity::Private)]), &[]);
    let d = bootstrap::run(&high, None).await.unwrap();
    assert!(d.profile.iter().any(|f| f.content.contains(&secret)), "profile holds the private row");
    assert!(d.recent.iter().any(|f| f.content.contains(&secret)), "recent holds the same row");
    // The rendered text prints a fact once across sections by design, so the payload is what carries
    // the claim above.
    assert_eq!(d.text.matches(&secret).count(), 1, "the markdown prints it once");

    // Search reads the same row through the same decryptor.
    let hits =
        search::run(&high, "private note", Some(vec!["user:me".into()]), None, None, None, None)
            .await
            .unwrap();
    assert!(hits.hits.iter().any(|h| h.content.contains(&secret)), "search serves it at private");

    // The other direction, on the same row: an open ceiling reaches the namespace and still gets
    // nothing, through the digest and through search.
    bootstrap::clear_cache();
    let low = restricted_at(&ctx, &at(&[("user:me", Sensitivity::Open)]), &[]);
    let d = bootstrap::run(&low, None).await.unwrap();
    assert!(!digest_json(&d).contains(&secret), "an open ceiling must not see it in the digest");
    assert!(d.profile.is_empty() && d.recent.is_empty());
    let hits =
        search::run(&low, "private note", Some(vec!["user:me".into()]), None, None, None, None)
            .await
            .unwrap();
    assert!(
        hits.hits.iter().all(|h| !h.content.contains(&secret)),
        "search must not serve it at open"
    );
}

#[tokio::test]
async fn registry_get_applies_the_ceiling_per_namespace() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    ctx.repos
        .registry
        .upsert(registry_write_at(
            "user:me",
            "bank-account",
            "sort-code-40-11-22",
            Sensitivity::Private,
            &provenance(),
        ))
        .await
        .unwrap();

    let low = restricted_at(&ctx, &at(&[("user:me", Sensitivity::Open)]), &[]);
    assert!(!registry::get(&low, "host", "bank-account", None, None).await.unwrap().found);

    let high = restricted_at(&ctx, &at(&[("user:me", Sensitivity::Private)]), &[]);
    let got = registry::get(&high, "host", "bank-account", None, None).await.unwrap();
    assert!(got.found);
    assert_eq!(got.sensitivity, Some(Sensitivity::Private));
}

#[tokio::test]
async fn a_sealed_item_is_served_as_ciphertext_and_only_to_a_sealed_grant() {
    use base64::Engine as _;
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let plaintext = nonce("sealedblob");
    let b64 = base64::engine::general_purpose::STANDARD.encode(plaintext.as_bytes());

    let put = lumberroom_server::services::sealed::put(
        &ctx,
        "project:creds",
        "hmac-aws-key",
        &b64,
        "aes-256-gcm/client-v1",
    )
    .await
    .unwrap();
    assert_eq!(put.namespace, "project:creds");

    // A client holding the ceiling but not the capability. The bytes are the same for everyone,
    // because the server holds no key for them; `decryptable` is the honest label on them.
    let mut blind = restricted_at(&ctx, &at(&[("project:creds", Sensitivity::Sealed)]), &[]);
    blind.principal.sealed_capable = false;
    let got = lumberroom_server::services::sealed::get(
        &blind,
        "hmac-aws-key",
        Some(vec!["project:creds".into()]),
    )
    .await
    .unwrap();
    assert!(got.found);
    assert!(!got.decryptable, "a client that cannot decrypt is told so rather than left to guess");
    let item = got.item.expect("the item is served");
    assert_eq!(item.ciphertext, b64);
    assert!(!item.ciphertext.contains(&plaintext), "the stored blob must not be the plaintext");

    // A ceiling of open in the same namespace reaches nothing. The service refuses rather than
    // reporting `found: false`, because retrieval is by exact key and an empty namespace list means
    // there is no place to look rather than nothing stored there.
    //
    // 403 rather than 400: the namespace was named and the grant is what refused it. This used to
    // answer 400 "name the namespace to read a sealed item from", which sent the operator looking at
    // a request that was correct.
    let low = restricted_at(&ctx, &at(&[("project:creds", Sensitivity::Open)]), &[]);
    let err = lumberroom_server::services::sealed::get(
        &low,
        "hmac-aws-key",
        Some(vec!["project:creds".into()]),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind.http_status(), 403);
    assert!(!err.client_message().contains(&plaintext));
}

#[tokio::test]
async fn the_tripwire_refuses_a_credential_at_open_and_never_echoes_it() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    // A well-known example key id, so nothing real is written down here.
    let credential = "AKIAIOSFODNN7EXAMPLE";

    let err =
        write::run(&ctx, &format!("the aws key is {credential}"), "global", None, None, None, None)
            .await
            .unwrap_err();
    assert_eq!(err.kind.http_status(), 400);
    let msg = err.client_message().to_string();
    assert!(msg.contains("aws_access_key_id"), "the refusal names the rule: {msg}");
    assert!(
        !msg.contains(credential),
        "echoing the secret puts it in whatever transcript, log or bug report the error lands in next"
    );

    // The tripwire is a backstop for storing a credential in the clear, so the same content stored
    // above open is accepted.
    let ok = write::run(
        &ctx,
        &format!("the aws key is {credential}"),
        "global",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(ok.sensitivity, Sensitivity::Private);
}

/// Finding 1. `personal:finance` is one of the namespaces migration 004 and
/// `SensitivityDefaults::seeded()` both classify private, and namespace validation used to refuse
/// it, so the rule could never fire and no reachable namespace classified above open.
///
/// The second half is the trap the fix could have walked into: a shape that validates but is
/// invisible to the digest is worse than a shape that does not validate, because the write reports
/// success and the fact is never seen again.
#[tokio::test]
async fn a_personal_namespace_is_writable_lands_private_and_reaches_the_digest() {
    let (ctx, _pool, _serial) =
        ctx_or_skip!(|cfg: &mut Config| cfg.policy.defaults = SensitivityDefaults::seeded());
    let secret = nonce("personalfinance");

    // No explicit level: the namespace default is what classifies it. That is the product claim,
    // that nobody classifies anything in the normal case.
    let w = write::run(
        &ctx,
        &format!("the retainer is {secret}"),
        "personal:finance",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(w.namespace, "personal:finance");
    assert_eq!(w.sensitivity, Sensitivity::Private, "the seeded rule has to actually fire");

    bootstrap::clear_cache();
    let d = bootstrap::run(&ctx, None).await.unwrap();
    assert!(
        d.recent.iter().any(|f| f.content.contains(&secret)),
        "a private personal fact the owner may read has to reach the digest: {}",
        digest_json(&d)
    );
    assert!(
        d.inventory.contains_key("personal:finance"),
        "the namespace has to appear in the inventory the model is shown: {:?}",
        d.inventory
    );

    // And the ceiling still holds on it, the same as any other private row.
    bootstrap::clear_cache();
    let low = restricted_at(&ctx, &at(&[("personal:finance", Sensitivity::Open)]), &[]);
    let d = bootstrap::run(&low, None).await.unwrap();
    assert!(
        !digest_json(&d).contains(&secret),
        "an open ceiling reaches the namespace, not the row"
    );
}

/// Finding 1, the part that matters most. A `credentials:*` namespace now validates, so the question
/// is what a plaintext `memory_write` into one does. It must refuse, and the refusal must name the
/// route that works, because the alternative is a credential stored in the clear and stemmed into
/// the lexical index.
#[tokio::test]
async fn a_credentials_namespace_refuses_plaintext_and_points_at_lumberroom_seal() {
    let (ctx, _pool, _serial) =
        ctx_or_skip!(|cfg: &mut Config| cfg.policy.defaults = SensitivityDefaults::seeded());
    let secret = nonce("credplaintext");

    let err = write::run(
        &ctx,
        &format!("the token is {secret}"),
        "credentials:aws",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind.http_status(), 400, "a clear refusal, not an internal error");
    let msg = err.client_message().to_string();
    assert!(msg.contains("lumberroom seal"), "the refusal has to name the route that works: {msg}");
    assert!(msg.contains("credentials:aws"), "and the namespace it is talking about: {msg}");
    assert!(!msg.contains(&secret), "a refusal must not echo the credential back");

    // The refusal does not depend on the classification table. An operator who replaces
    // SENSITIVITY_DEFAULTS with rules of their own drops the `credentials:*` row with it, and this
    // call would otherwise store the credential at open.
    let mut wide = ctx.clone();
    let mut cfg = (*ctx.cfg).clone();
    cfg.policy.defaults = SensitivityDefaults::new(vec![("*".to_string(), Sensitivity::Open)]);
    wide.cfg = Arc::new(cfg);
    let err =
        write::run(&wide, "an unremarkable sentence", "credentials:aws", None, None, None, None)
            .await
            .unwrap_err();
    assert!(
        err.client_message().contains("lumberroom seal"),
        "the shape refuses even when no rule classifies it: {}",
        err.client_message()
    );

    // Nothing landed, by either route.
    let hits =
        search::run(&ctx, "token", Some(vec!["credentials:aws".into()]), None, None, None, None)
            .await
            .unwrap();
    assert!(hits.hits.is_empty(), "no plaintext row exists in a credentials namespace");
}

/// Finding 1's other invisibility trap, and the one a namespace test would not catch. A
/// `credentials:*` namespace holds sealed items and nothing else, so it never appears in the memory
/// table's namespace counts, which is where the digest builds its readable set from. Before this it
/// was unreachable and the gap was dead; now it validates, and the owner would have been told
/// nothing was stored while `lumberroom seal` was storing it.
#[tokio::test]
async fn a_namespace_holding_only_sealed_items_reaches_the_digest_at_a_sealed_ceiling() {
    use base64::Engine as _;
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let b64 = base64::engine::general_purpose::STANDARD.encode(nonce("credsealed").as_bytes());
    lumberroom_server::services::sealed::put(
        &ctx,
        "credentials:aws",
        "hmac-prod-key",
        &b64,
        "aes-256-gcm/client-v1",
    )
    .await
    .unwrap();

    bootstrap::clear_cache();
    let d = bootstrap::run(&ctx, None).await.unwrap();
    assert_eq!(
        d.sealed_inventory.get("credentials:aws"),
        Some(&1),
        "the owner has to be told the item exists: {:?}",
        d.sealed_inventory
    );
    assert!(
        d.text.contains("credentials:aws (1)"),
        "and told in the text the model reads: {}",
        d.text
    );
    assert!(
        !d.inventory.contains_key("credentials:aws"),
        "a sealed-only namespace is not a memory inventory entry, where a zero count would announce \
         it to a client whose ceiling stops at open"
    );

    // The other direction. A ceiling of open on the same namespace learns nothing at all, not even
    // that the namespace is there.
    bootstrap::clear_cache();
    let low = restricted_at(&ctx, &at(&[("credentials:aws", Sensitivity::Open)]), &[]);
    let d = bootstrap::run(&low, None).await.unwrap();
    assert!(
        !digest_json(&d).contains("credentials:aws"),
        "an open ceiling must not learn the namespace exists: {}",
        digest_json(&d)
    );
}

/// The inventory line, which is where a namespace name outlives the content the ceiling refused.
///
/// `namespace_counts` applies no ceiling, and the digest built its inventory by intersecting
/// filtered NAMES with those RAW counts. A client granted `*` at open was told
/// `personal:finance: 1`: the row refused, the namespace and the number handed over. Migration 004
/// classifies that namespace private, so this fired on a default install, and `scripts/policy-test.sh`
/// passed every time because it greps for the nonce and a name with a count beside it is not the
/// nonce.
///
/// The rendered text carries the same claim as the payload, because the text is what a model reads.
#[tokio::test]
async fn a_namespace_the_ceiling_refuses_is_absent_from_the_inventory_and_from_the_digest_text() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let secret = nonce("inventoryceiling");
    write::run(
        &ctx,
        &format!("the retainer is {secret}"),
        "personal:finance",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();

    // Named by the grant, refused by the ceiling. `*` at open is what a bare Phase 1 glob resolves
    // to, so this is the shipped default rather than a grant invented for the test.
    bootstrap::clear_cache();
    let at_open = restricted_at(&ctx, &at(&[("*", Sensitivity::Open)]), &[]);
    let d = bootstrap::run(&at_open, None).await.unwrap();

    let published = digest_json(&d);
    assert!(!published.contains(&secret), "the row itself leaked: {published}");
    assert!(
        !published.contains("personal:finance"),
        "the name is the disclosure, with or without the row: {published}"
    );
    assert!(!d.inventory.contains_key("personal:finance"), "inventory: {:?}", d.inventory);
    assert!(!d.counts.by_namespace.contains_key("personal:finance"));
    assert!(
        !d.text.contains("personal:finance"),
        "the markdown is what a model reads, and it names namespaces with their counts: {}",
        d.text
    );
    assert_eq!(d.counts.memories, 0, "there is nothing this client may read");
}

/// The other direction on the same row. Hiding the namespace from everyone would pass the test
/// above and break the digest, so the count has to survive for a ceiling that reaches the row.
#[tokio::test]
async fn a_ceiling_that_reaches_the_row_is_still_told_the_count() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let secret = nonce("inventoryreaches");
    write::run(
        &ctx,
        &format!("the retainer is {secret}"),
        "personal:finance",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();

    bootstrap::clear_cache();
    let at_private = restricted_at(&ctx, &at(&[("*", Sensitivity::Private)]), &[]);
    let d = bootstrap::run(&at_private, None).await.unwrap();

    assert_eq!(
        d.inventory.get("personal:finance"),
        Some(&1),
        "the inventory the model is shown has to hold it: {:?}",
        d.inventory
    );
    assert_eq!(d.counts.by_namespace.get("personal:finance"), Some(&1));
    assert!(d.recent.iter().any(|f| f.content.contains(&secret)), "and the row itself comes back");
    assert!(d.text.contains("personal:finance"), "named in the text as well: {}", d.text);
}

/// The same shape in the recall monitor, found by reading every caller of `namespace_counts`.
///
/// `sample_content` took no ceilings and `RecallReport::worst` publishes the opening characters of
/// every probe, so `/admin/recall`, which wants a bearer token and no scope beyond it, read the
/// plaintext of open rows out of namespaces the caller's grant excludes. Content, not a name: worse
/// than the inventory leak this run started from.
#[tokio::test]
async fn the_recall_report_never_quotes_content_the_caller_cannot_search_for() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let outside = nonce("recalloutside");
    let inside = nonce("recallinside");
    write::run(
        &ctx,
        &format!("the vault rota starts {outside}"),
        "project:vault",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        &format!("an open fact anyone may read {inside}"),
        "global",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let limited = restricted(&ctx, &["global"], &["global"]);
    let report = recall::measure(&limited, 25, 5).await.unwrap();
    let published = serde_json::to_value(&report).unwrap().to_string();
    assert!(
        !published.contains(&outside),
        "the monitor quoted a row from a namespace the grant excludes: {published}"
    );
    assert!(
        published.contains(&inside),
        "and it still measures what this client may read: {published}"
    );
    assert_eq!(report.sampled, 1, "one readable row, one probe: {published}");

    // The owner measures the whole store, so the narrowing above is the grant rather than a monitor
    // that stopped working.
    let full = recall::measure(&ctx, 25, 5).await.unwrap();
    assert_eq!(full.sampled, 2, "both rows are the owner's to sample");
}

/// The third instance of the same shape, in `memory_search`.
///
/// `also_searched` published the discovery set: `namespace_counts`, which applies no policy, run
/// through `filter_readable`, which applies the namespace axis and not the ceiling. A grant naming a
/// namespace at open therefore had that name handed back while the second axis refused every row
/// behind it. The field now names the namespaces that answered, taken from the hits the caller is
/// already holding, so a name reaches the response only once a both-axes filter put a row behind it.
#[tokio::test]
async fn a_namespace_the_ceiling_refuses_is_absent_from_also_searched() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let refused = nonce("alsosearchedceiling");
    let readable = nonce("alsosearchedopen");
    // Classified explicitly, because this harness loads the namespace defaults from the environment
    // and an unset SENSITIVITY_DEFAULTS leaves every namespace open. The level is what this test is
    // about, so it is stated rather than inferred.
    write::run(
        &ctx,
        &format!("the appointment is at nine {refused}"),
        "personal:health",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        &format!("Warden uses Django with Celery for scheduled jobs {readable}"),
        "project:warden",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // personal:health is NAMED by this grant at open and migration 004 classifies it private, so the
    // namespace axis admits the name and the ceiling refuses every row. project:warden is the
    // positive control: hiding every widened namespace would pass the leak assertion and break the
    // search.
    let narrow = restricted_at(
        &ctx,
        &at(&[
            ("user:me", Sensitivity::Open),
            ("global", Sensitivity::Open),
            ("personal:health", Sensitivity::Open),
            ("project:warden", Sensitivity::Open),
        ]),
        &[],
    );
    let res = search::run(
        &narrow,
        "what does warden use for scheduled jobs",
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let published = serde_json::to_value(&res).unwrap().to_string();

    assert!(!published.contains(&refused), "the row itself leaked: {published}");
    assert!(
        !res.also_searched.contains(&"personal:health".to_string()),
        "the name is the disclosure, with or without the row: {:?}",
        res.also_searched
    );
    assert!(
        !published.contains("personal:health"),
        "and it must not reach the response through any other field: {published}"
    );
    assert!(
        res.hits.iter().any(|h| h.content.contains(&readable)),
        "the widened search still answers: {published}"
    );
    assert!(
        res.also_searched.contains(&"project:warden".to_string()),
        "a namespace that produced a readable hit is still named: {:?}",
        res.also_searched
    );
}

/// `ExportResult::excluded` counted the rows the grant dropped, which is the size of everything this
/// caller may not see, published as one number per export.
///
/// `list_for_export` takes no grant, so the difference between the page it returns and the page that
/// survives `can_read` is exactly that. The count now covers only rows this caller may read that the
/// export ceiling left out, plus the private rows that would not open.
#[tokio::test]
async fn the_export_never_counts_the_rows_a_grant_excludes() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let outside = nonce("exportoutside");
    let inside = nonce("exportinside");
    write::run(
        &ctx,
        &format!("the vault rota starts {outside}"),
        "project:vault",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        &format!("an open fact anyone may read {inside}"),
        "global",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let limited = restricted(&ctx, &["global"], &["global"]);
    let mirrored = export::run(&limited, None, None).await.unwrap();
    let published = serde_json::to_value(&mirrored).unwrap().to_string();
    assert!(!published.contains(&outside), "the row itself leaked: {published}");
    assert_eq!(mirrored.memories, 1, "one readable row: {published}");
    assert_eq!(
        mirrored.excluded, 0,
        "the number of rows outside the grant is the size of what this client cannot see"
    );

    // The owner mirrors the whole store, so the narrowing above is the grant rather than an export
    // that stopped working.
    let full = export::run(&ctx, None, None).await.unwrap();
    assert_eq!(full.memories, 2);
    assert_eq!(full.excluded, 0);
}

/// Finding 2. The `sensitivity_default` table migration 004 seeds was never read: `config::load`
/// built the rule set from `SENSITIVITY_DEFAULTS` alone and an unset variable produced an EMPTY set,
/// which classifies every namespace open without saying so.
///
/// `project:vault` is the discriminator. The seeded table and `seeded()` hold identical rules, so a
/// test using only seeded rows cannot tell "read the table" from "fell back to seeded()"; a row that
/// exists in neither can only have come from the database.
#[tokio::test]
async fn the_sensitivity_default_table_classifies_a_write_when_the_environment_is_silent() {
    let (ctx, pool, _serial) = ctx_or_skip!();

    sqlx::query(
        "INSERT INTO sensitivity_default (tenant_id, pattern, sensitivity)
         VALUES ($1, 'project:vault', 'private')
         ON CONFLICT (tenant_id, pattern) DO UPDATE SET sensitivity = EXCLUDED.sensitivity",
    )
    .bind(&ctx.cfg.tenant_id)
    .execute(&pool)
    .await
    .unwrap();

    let mut cfg = (*ctx.cfg).clone();
    assert!(
        !cfg.policy.defaults_from_env,
        "this test only means something with SENSITIVITY_DEFAULTS unset"
    );
    assert_eq!(
        cfg.policy.defaults.for_namespace("project:vault"),
        Sensitivity::Open,
        "before resolution there is no rule, which is the bug: everything classifies open"
    );

    // Exactly what the composition root does at boot, through the same two calls.
    let table = postgres::sensitivity_defaults(&pool, &cfg.tenant_id).await.unwrap();
    let source = cfg.apply_sensitivity_defaults(table);
    assert_eq!(source, "sensitivity_default table");

    let mut booted = ctx.clone();
    booted.cfg = Arc::new(cfg);

    let secret = nonce("tablevsenv");
    let w = write::run(
        &booted,
        &format!("vault note {secret}"),
        "project:vault",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        w.sensitivity,
        Sensitivity::Private,
        "the row the table says is private has to land private"
    );

    // The row this test added is not part of the seeded set, and `setup` does not truncate this
    // table, so it goes back out rather than following the next run around.
    sqlx::query(
        "DELETE FROM sensitivity_default WHERE tenant_id = $1 AND pattern = 'project:vault'",
    )
    .bind(&booted.cfg.tenant_id)
    .execute(&pool)
    .await
    .unwrap();
}

/// Finding 4. A sealed read the sensitivity ceiling refused answered "name the namespace to read a
/// sealed item from" when the namespace had been named. The refusal was right and the explanation
/// cost the operator an hour looking at a request that was correct.
#[tokio::test]
async fn a_sealed_read_the_ceiling_refused_says_so_instead_of_asking_for_a_namespace() {
    let (ctx, _pool, _serial) = ctx_or_skip!();

    let named = restricted_at(&ctx, &at(&[("global", Sensitivity::Open)]), &[]);
    let err = lumberroom_server::services::sealed::get(
        &named,
        "hmac-anything",
        Some(vec!["global".into()]),
    )
    .await
    .unwrap_err();
    let msg = err.client_message().to_string();
    assert_eq!(err.kind.http_status(), 403, "the grant refused it, so it is not a bad request");
    assert!(msg.contains("sealed"), "the message names the ceiling that refused: {msg}");
    assert!(
        !msg.contains("name the namespace"),
        "the namespace WAS named; asking for it again is the bug: {msg}"
    );
    // An error that lists what the caller may reach is a way to map the grant by probing.
    assert!(!msg.contains("user:me"), "the refusal must not enumerate the grant: {msg}");

    // The original message is still right for the case it was written for: nothing named, and a
    // grant with no concrete namespace to look in.
    let globs = restricted_at(&ctx, &at(&[("*", Sensitivity::Sealed)]), &[]);
    let err =
        lumberroom_server::services::sealed::get(&globs, "hmac-anything", None).await.unwrap_err();
    assert_eq!(err.kind.http_status(), 400);
    assert!(
        err.client_message().contains("name the namespace"),
        "a glob is a pattern, not a place: {}",
        err.client_message()
    );
}

/// Not covered elsewhere: `envelope::open` is unit-tested against synthetic ciphertext, and
/// `find_by_id` is exercised above only for open rows. Nothing else round-trips a private write
/// through the real DB columns, `SealedReader::sealed_batch`, and the running `Ctx`'s own KEK, or
/// confirms a plain read never lets the content column carry a private row's text.
#[tokio::test]
async fn a_private_row_stores_ciphertext_and_reopens_to_the_original_through_the_real_kek() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let plaintext = "the private probe fact zqxprobezqx";
    let w =
        write::run(&ctx, plaintext, "user:me", None, None, Some("private"), None).await.unwrap();
    let id = uuid::Uuid::parse_str(&w.id).unwrap();

    let row: (Option<String>, Option<Vec<u8>>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT content, content_ct, enc_alg, kek_id FROM memory WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.0.is_none(), "a private row's content column must stay NULL");
    assert!(row.1.is_some(), "a private row must carry ciphertext");
    assert!(row.2.is_some(), "a private row must record its encryption algorithm");
    assert!(row.3.is_some(), "a private row must record which kek sealed it");

    let batch = lumberroom_server::services::SealedReader::sealed_batch(
        &*ctx.repos.ciphertext.clone().unwrap(),
        "me",
        &[id],
    )
    .await
    .unwrap();
    assert_eq!(batch.len(), 1, "sealed_batch must return exactly the row asked for");

    let kek = ctx.keys.clone().unwrap().kek().await.unwrap();
    let (rid, sealed, _kek_id) = &batch[0];
    let opened = lumberroom_server::crypto::envelope::open(&kek, *rid, sealed).unwrap();
    assert_eq!(opened, plaintext, "the stored ciphertext must reopen to what was written");

    let m = ctx.repos.memories.find_by_id("me", id).await.unwrap().unwrap();
    assert!(m.content.is_empty(), "a plain read must not expose private content");
    assert_eq!(m.sensitivity, Sensitivity::Private);
}

/// A rename splits one subject across two namespaces, and no wording of the question crosses that
/// line: the namespace filter runs before the ranking. This is the case aliases exist for.
///
/// Both searches name the namespace they want. With no list the service reads `user:me` and
/// `global` first and every other namespace at a penalty, so `project:ferrous` is already in the
/// scan before any alias exists and the negative half of this test would assert nothing. Naming the
/// namespace puts the expansion on trial by itself: `secondary` is then the aliased set and nothing
/// else.
#[tokio::test]
async fn an_alias_reaches_facts_filed_under_the_projects_old_name() {
    let (ctx, _pool, _serial) = ctx_or_skip!();

    write::run(
        &ctx,
        "Ferrous ships its frontend from apps/web",
        "project:ferrous",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(&ctx, "Cuprum bills annually in euros", "project:cuprum", None, None, None, None)
        .await
        .unwrap();

    let asked = || Some(vec!["project:cuprum".to_string()]);
    let before =
        search::run(&ctx, "how does cuprum ship its frontend", asked(), None, None, None, None)
            .await
            .unwrap();
    assert!(
        !before.hits.iter().any(|x| x.namespace == "project:ferrous"),
        "without an alias the old name's namespace is unreachable"
    );

    put_alias(&ctx, "project:cuprum", "ferrous", "cuprum").await;

    let after =
        search::run(&ctx, "how does cuprum ship its frontend", asked(), None, None, None, None)
            .await
            .unwrap();
    let reached: Vec<&str> = after.hits.iter().map(|x| x.namespace.as_str()).collect();
    assert!(
        reached.contains(&"project:ferrous"),
        "the alias has to pull the old name's namespace in: {reached:?}"
    );
    assert!(
        after.also_searched.iter().any(|n| n == "project:ferrous"),
        "and the answer has to say it looked there: {:?}",
        after.also_searched
    );
    assert!(
        after.hits.iter().filter(|x| x.namespace == "project:ferrous").all(|x| !x.primary),
        "an aliased namespace is secondary, so it keeps the cross-namespace penalty"
    );
}

/// The direction the first version of this feature could not do.
///
/// An alias row lives in whichever namespace the owner typed when he recorded it, and the group
/// lookup used to key on that namespace. So a group recorded under the new name resolved when you
/// searched the new name and not when you searched the old one. Observed on the owner's own store
/// before the fix: searching `project:lumen` returned 8 hits across two namespaces, searching
/// `project:warden` returned 4 and reported `also_searched: []`.
#[tokio::test]
async fn an_alias_resolves_from_the_old_name_as_well_as_the_new_one() {
    let (ctx, _pool, _serial) = ctx_or_skip!();

    write::run(
        &ctx,
        "Tinbox ships its frontend from apps/web",
        "project:tinbox",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(&ctx, "Zincbox bills annually in euros", "project:zincbox", None, None, None, None)
        .await
        .unwrap();

    // Recorded once, under the new name, which is what a person does when a project is renamed.
    put_alias(&ctx, "project:zincbox", "tinbox", "zincbox").await;

    // The old name, which is the side that used to find nothing.
    let from_old = search::run(
        &ctx,
        "how does the project bill",
        Some(vec!["project:tinbox".to_string()]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        from_old.also_searched.iter().any(|n| n == "project:zincbox"),
        "searching the old name has to reach the new one: {:?}",
        from_old.also_searched
    );
    assert!(
        from_old.hits.iter().any(|x| x.namespace == "project:zincbox"),
        "and return its rows: {:?}",
        from_old.hits.iter().map(|x| &x.namespace).collect::<Vec<_>>()
    );

    // And the direction that already worked still does.
    let from_new = search::run(
        &ctx,
        "how does the project ship its frontend",
        Some(vec!["project:zincbox".to_string()]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        from_new.also_searched.iter().any(|n| n == "project:tinbox"),
        "the canonical side regressed: {:?}",
        from_new.also_searched
    );
}

/// A rename inside one prefix must not reach across prefixes.
///
/// The group scope is the prefix, so `project:` matches every project namespace and nothing else.
/// `personal:tinbox` is a different subject from `project:tinbox` and stays one.
#[tokio::test]
async fn an_alias_group_does_not_cross_a_namespace_prefix() {
    let (ctx, _pool, _serial) = ctx_or_skip!();

    write::run(&ctx, "Leadbox runs its billing monthly", "project:leadbox", None, None, None, None)
        .await
        .unwrap();
    write::run(
        &ctx,
        "the leadbox in the hallway needs a new lock",
        "personal:leadbox",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    put_alias(&ctx, "project:goldbox", "leadbox", "goldbox").await;

    let personal = search::run(
        &ctx,
        "what needs a new lock",
        Some(vec!["personal:leadbox".to_string()]),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        !personal.also_searched.iter().any(|n| n.starts_with("project:")),
        "a project rename reached a personal namespace: {:?}",
        personal.also_searched
    );
}

/// The alias runs through the grant, never around it.
///
/// A rename is not a reason to read a namespace the caller was never given. This is the disclosure
/// the expansion could have introduced: the owner records one alias and every client searching the
/// new name starts reading a namespace its grant excludes, with the answer naming it.
#[tokio::test]
async fn an_alias_does_not_pull_in_a_namespace_the_caller_may_not_read() {
    let (ctx, _pool, _serial) = ctx_or_skip!();

    let secret = nonce("aliasgrant");
    write::run(
        &ctx,
        &format!("Ferrous keeps {secret} in apps/web"),
        "project:ferrous",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(&ctx, "Cuprum bills annually in euros", "project:cuprum", None, None, None, None)
        .await
        .unwrap();
    put_alias(&ctx, "project:cuprum", "ferrous", "cuprum").await;

    // The owner reads both, so the alias demonstrably works before the narrow grant is asked the
    // same question. Without this half, a broken expansion would pass the assertions below.
    let asked = || Some(vec!["project:cuprum".to_string()]);
    let owner =
        search::run(&ctx, "where does ferrous keep its frontend", asked(), None, None, None, None)
            .await
            .unwrap();
    assert!(
        owner.hits.iter().any(|x| x.namespace == "project:ferrous"),
        "the alias has to reach the old namespace for the owner"
    );

    let narrow = restricted(&ctx, &["project:cuprum"], &["project:cuprum"]);
    let answer = search::run(
        &narrow,
        "where does ferrous keep its frontend",
        asked(),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        !answer.hits.iter().any(|x| x.namespace == "project:ferrous"),
        "the alias reached a namespace this grant excludes"
    );
    assert!(
        !answer.hits.iter().any(|x| x.content.contains(&secret)),
        "and the content behind it came with the namespace"
    );
    assert!(
        !answer.also_searched.iter().any(|n| n == "project:ferrous"),
        "naming it in the answer tells the client the namespace exists: {:?}",
        answer.also_searched
    );
    assert_eq!(answer.namespaces, vec!["project:cuprum".to_string()]);
}

/// Warden, then Quill, then Lumen. The owner's own case, and the reason a group is a group rather
/// than a pair: a question naming any of the three has to read the facts written under all three.
///
/// The alias rows live in each namespace the question can be asked from. `group` is keyed on the
/// namespace it is asked in, so a group recorded only under the canonical name answers only there,
/// which is a rename that half works and the failure this asserts against.
#[tokio::test]
async fn a_group_of_three_names_resolves_from_any_of_them() {
    let (ctx, _pool, _serial) = ctx_or_skip!();

    let names = ["warden", "quill", "lumen"];
    for name in names {
        write::run(
            &ctx,
            &format!("{name} answers on the port it was given as {name}"),
            &format!("project:{name}"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        // Two rows per namespace: the other two names, both pointing at the newest one.
        for alias in names.iter().filter(|n| **n != "lumen") {
            put_alias(&ctx, &format!("project:{name}"), alias, "lumen").await;
        }
    }

    for asked in names {
        let namespace = format!("project:{asked}");
        let answer = search::run(
            &ctx,
            "which port was this project given",
            Some(vec![namespace.clone()]),
            Some(10),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let reached: std::collections::BTreeSet<&str> =
            answer.hits.iter().map(|x| x.namespace.as_str()).collect();
        for name in names {
            let other = format!("project:{name}");
            assert!(
                reached.contains(other.as_str()),
                "asking from {namespace} did not reach {other}: {reached:?}"
            );
        }
    }
}

/// One alias row, through the service the console and `/admin/alias` both call.
async fn put_alias(ctx: &Ctx, namespace: &str, alias: &str, canonical: &str) {
    lumberroom_server::services::alias::put(
        ctx,
        ctx.repos.aliases.as_ref(),
        namespace,
        alias,
        canonical,
        None,
        None,
        Some("manual"),
    )
    .await
    .unwrap();
}

// -- the memory timeline -------------------------------------------------------------------------

/// A chain of three versions of one fact, oldest first, each written into the namespace it is given.
///
/// The valid times are months back on purpose. `write::run` refuses an `occurred_at` inside the
/// near-now fence, so a chain dated today never reaches the timeline to be read.
async fn three_versions(ctx: &Ctx, namespaces: [&str; 3]) -> [uuid::Uuid; 3] {
    let day = |n: i64| Some(chrono::Utc::now() - chrono::Duration::days(n));
    let mut written: Vec<String> = Vec::new();
    for (i, (content, days)) in [
        ("the deploy box answers on port 8080", 300),
        ("the deploy box answers on port 8787", 200),
        ("the deploy box answers on port 9443", 100),
    ]
    .into_iter()
    .enumerate()
    {
        let previous = written.last().cloned();
        let outcome =
            write::run(ctx, content, namespaces[i], None, previous.as_deref(), None, day(days))
                .await
                .unwrap();
        written.push(outcome.id);
    }
    let id = |s: &String| uuid::Uuid::parse_str(s).unwrap();
    [id(&written[0]), id(&written[1]), id(&written[2])]
}

fn grants(spec: &[(&str, Sensitivity)]) -> Vec<NamespaceGrant> {
    spec.iter().map(|(ns, max)| NamespaceGrant::new(*ns, *max)).collect()
}

/// Three versions, read back as a sequence with the periods they held for.
///
/// This is the read a live search cannot answer. A search hides every version but the last, and an
/// as-of search returns the one slice that held at an instant; neither shows the order.
#[tokio::test]
async fn a_chain_of_three_reads_back_oldest_first_with_periods_that_tile() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let ids = three_versions(&ctx, ["project:ports"; 3]).await;

    for (anchor, from) in [(ids[2], "the newest"), (ids[0], "the oldest"), (ids[1], "the middle")] {
        let timeline = ctx
            .repos
            .memories
            .subject_history(ctx.tenant(), &ctx.principal.read, anchor)
            .await
            .unwrap();
        let order: Vec<&str> = timeline.versions.iter().map(|m| m.id.as_str()).collect();
        let expected: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        assert_eq!(order, expected, "anchored on {from} the walk returned {order:?}");
        assert_eq!(timeline.withheld, 0, "the owner reads all three");
        assert!(!timeline.depth_capped);
    }

    let timeline = ctx
        .repos
        .memories
        .subject_history(ctx.tenant(), &ctx.principal.read, ids[2])
        .await
        .unwrap();
    let periods: Vec<(
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = timeline.versions.iter().map(|m| (m.occurred_at, m.occurred_until)).collect();
    for (i, (start, end)) in periods.iter().enumerate() {
        assert!(start.is_some(), "version {i} lost the date it was written with");
        match periods.get(i + 1) {
            // Half-open, so one period's end is the next one's start and no instant belongs to two
            // versions. A gap here reads as "nothing was true then" and an overlap as "both were".
            Some((next_start, _)) => assert_eq!(
                end,
                next_start,
                "version {i} does not end where version {} begins",
                i + 1
            ),
            None => assert!(end.is_none(), "the live version still holds, so it has no end"),
        }
        if let Some((next_start, _)) = periods.get(i + 1) {
            assert!(start < next_start, "version {i} does not start before the one after it");
        }
    }
}

/// A version in a namespace the caller cannot read leaves a counted gap rather than a short answer.
///
/// Stopping the walk at the first version it cannot read is the failure this replaced: a caller
/// holding the current row would be handed two versions and no sign that a third sat between them,
/// which reports an incomplete history as a complete one.
#[tokio::test]
async fn a_version_the_grant_refuses_is_counted_rather_than_stopping_the_walk() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let ids = three_versions(&ctx, ["project:ports", "project:hidden", "project:ports"]).await;
    let narrow = grants(&[("project:ports", Sensitivity::Open)]);

    let timeline = ctx.repos.memories.subject_history(ctx.tenant(), &narrow, ids[2]).await.unwrap();
    let order: Vec<&str> = timeline.versions.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        order,
        vec![ids[0].to_string(), ids[2].to_string()],
        "the readable versions come back, in order"
    );
    assert_eq!(timeline.withheld, 1, "and the one that did not is counted");
    assert!(
        !timeline.versions.iter().any(|m| m.namespace == "project:hidden"),
        "the withheld version's namespace must not travel with the count"
    );

    // Anchored on the version this grant cannot read, the answer is empty and reports no gap.
    // A count on a row the caller cannot read at all would confirm the row exists.
    let blind = ctx.repos.memories.subject_history(ctx.tenant(), &narrow, ids[1]).await.unwrap();
    assert!(blind.versions.is_empty());
    assert_eq!(blind.withheld, 0, "an unreadable anchor reveals nothing, not even a number");
}

/// The capability, on the two paths a service-level caller can reach.
///
/// There is no `services::` function wrapping `subject_history`; the check sits in
/// `http::admin_memory_history`, which answers 403, and in `console::data::chain`, which hands back
/// the one row the reader asked for and no past. Both are asserted here, along with the as-of
/// search, which is the third way retired rows reach a caller.
#[tokio::test]
async fn a_client_without_the_history_capability_reads_no_past() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let ids = three_versions(&ctx, ["project:ports"; 3]).await;

    let mut blind = ctx.clone();
    blind.principal.may_read_history = false;

    let refused = search::run(
        &blind,
        "which port does the deploy box answer on",
        Some(vec!["project:ports".to_string()]),
        None,
        None,
        None,
        Some(chrono::Utc::now() - chrono::Duration::days(250)),
    )
    .await
    .unwrap_err();
    assert!(
        refused.client_message().contains("may not read facts that no longer hold"),
        "{}",
        refused.client_message()
    );

    let leaf =
        lumberroom_server::console::data::leaf(&blind, &ids[2].to_string()).await.unwrap().unwrap();
    assert_eq!(leaf.revisions.len(), 1, "a reader without the capability sees one version");
    assert_eq!(leaf.revisions[0].id, ids[2].to_string());

    // The same call with the capability, so the assertion above is about the capability rather than
    // about a chain that was never built.
    let full =
        lumberroom_server::console::data::leaf(&ctx, &ids[2].to_string()).await.unwrap().unwrap();
    assert_eq!(full.revisions.len(), 3);
}

// -- the registry archive ------------------------------------------------------------------------

/// Three writes to one key leave two versions, and neither of them is the value it holds now.
///
/// The archive fills on replacement, so what is here is what the key stopped holding. A history
/// read that included the live value would answer "what does this hold" twice and leave the caller
/// unable to tell the current value from a retired one.
#[tokio::test]
async fn three_upserts_leave_two_versions_newest_first_without_the_live_value() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    for port in ["8080", "8443", "8787"] {
        registry::set(
            &ctx,
            "global",
            "service",
            "services.lumberroom.port",
            &serde_json::json!(port),
            None,
            None,
        )
        .await
        .unwrap();
    }

    let archive =
        registry::history(&ctx, "service", "services.lumberroom.port", Some("global"), None, None)
            .await
            .unwrap();
    let values: Vec<&str> =
        archive.entries.iter().map(|v| v.value.as_str().unwrap_or_default()).collect();
    assert_eq!(values, vec!["8443", "8080"], "newest first, and the live value is not among them");
    assert_eq!(archive.namespace.as_deref(), Some("global"));
    assert_eq!(archive.key, "services.lumberroom.port");

    let versions: Vec<i32> = archive.entries.iter().map(|v| v.version).collect();
    assert_eq!(
        versions,
        vec![2, 1],
        "each row carries the version it was, not the one that replaced it"
    );
    assert!(
        archive.entries.windows(2).all(|w| w[0].replaced_at >= w[1].replaced_at),
        "the order is by when the value was replaced"
    );

    let live = registry::get(&ctx, "service", "services.lumberroom.port", Some("global"), None)
        .await
        .unwrap();
    assert_eq!(
        live.value.as_str(),
        Some("8787"),
        "the value the archive left out is the one `get` still answers with"
    );
    assert_eq!(live.version, Some(3));
}

#[tokio::test]
async fn a_history_limit_of_one_returns_the_newest_version_alone() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    for port in ["8080", "8443", "8787"] {
        registry::set(
            &ctx,
            "global",
            "service",
            "services.lumberroom.port",
            &serde_json::json!(port),
            None,
            None,
        )
        .await
        .unwrap();
    }

    let page = registry::history(
        &ctx,
        "service",
        "services.lumberroom.port",
        Some("global"),
        None,
        Some(1),
    )
    .await
    .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].value.as_str(),
        Some("8443"),
        "the newest retired value, not the oldest"
    );
}

/// A grant over what a key holds is not a grant over what it used to hold.
///
/// The refusal comes before the grant runs, so the client learns nothing from the shape of the
/// answer: not the namespaces that were searched, and not whether the key exists.
#[tokio::test]
async fn a_client_without_the_history_capability_is_refused_the_registry_archive() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    registry::set(
        &ctx,
        "global",
        "service",
        "services.lumberroom.port",
        &serde_json::json!("8080"),
        None,
        None,
    )
    .await
    .unwrap();
    registry::set(
        &ctx,
        "global",
        "service",
        "services.lumberroom.port",
        &serde_json::json!("8787"),
        None,
        None,
    )
    .await
    .unwrap();

    let blind = restricted(&ctx, &["global"], &[]);
    assert!(!blind.principal.may_read_history);
    let refused = registry::history(
        &blind,
        "service",
        "services.lumberroom.port",
        Some("global"),
        None,
        None,
    )
    .await
    .unwrap_err();
    let message = refused.client_message().to_string();
    assert!(
        message.contains("no longer holds"),
        "the refusal has to say what was refused: {message}"
    );
    assert!(!message.contains("global"), "and it must not name a namespace: {message}");
    assert!(!message.contains("services.lumberroom.port"), "or the key: {message}");

    // The same client still reads the value, which is what makes this a second axis rather than a
    // narrower grant.
    let live = registry::get(&blind, "service", "services.lumberroom.port", Some("global"), None)
        .await
        .unwrap();
    assert_eq!(live.value.as_str(), Some("8787"));
}

// -- data at rest --------------------------------------------------------------------------------
//
// Each test here pins one property of what the database holds, or of what a narrow grant can
// learn from it: the keyed emission digest, the proposal plaintext, the foreign keys a forget
// has to get past, the chain edits a delete may make, and the registry overwrite guard.

/// A second principal built from the owner's, with one capability flipped. The policy tests above
/// use `restricted_at` for grants; this is for the boolean axes.
fn with_principal(ctx: &Ctx, edit: impl FnOnce(&mut Principal)) -> Ctx {
    let mut c = ctx.clone();
    edit(&mut c.principal);
    c
}

fn narrow(ctx: &Ctx, namespace: &str, max: Sensitivity) -> Ctx {
    restricted_at(ctx, &at(&[(namespace, max)]), &at(&[(namespace, max)]))
}

async fn memory_links(pool: &PgPool, id: &str) -> (Option<String>, Option<String>) {
    let row = sqlx::query("SELECT supersedes, superseded_by FROM memory WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .fetch_one(pool)
        .await
        .unwrap();
    (
        row.get::<Option<uuid::Uuid>, _>("supersedes").map(|u| u.to_string()),
        row.get::<Option<uuid::Uuid>, _>("superseded_by").map(|u| u.to_string()),
    )
}

async fn occurred_until(pool: &PgPool, id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query("SELECT occurred_until FROM memory WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
        .get("occurred_until")
}

async fn memory_exists(pool: &PgPool, id: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM memory WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
        > 0
}

/// A queue row written straight into the table, the way the post path would leave it. The ingest
/// service is not under test here; the column contents after a decision are.
async fn raw_proposal(pool: &PgPool, content: &str, quote: Option<&str>) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ingest_proposal
             (id, tenant_id, fingerprint, content, namespace, speaker, quote, extractor)
         VALUES ($1, 'me', $2, $3, 'user:me', 'owner_typed', $4, 'test')",
    )
    .bind(id)
    .bind(format!("fp-{id}"))
    .bind(content)
    .bind(quote)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn proposal_state(
    pool: &PgPool,
    id: uuid::Uuid,
) -> (String, Option<String>, Option<uuid::Uuid>, Option<uuid::Uuid>) {
    let row = sqlx::query(
        "SELECT content, quote, memory_id, supersedes FROM ingest_proposal WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    (row.get("content"), row.get("quote"), row.get("memory_id"), row.get("supersedes"))
}

async fn link_proposal(pool: &PgPool, proposal: uuid::Uuid, memory: &str) {
    sqlx::query(
        "UPDATE ingest_proposal SET state = 'written', memory_id = $2, decided_at = now()
          WHERE id = $1",
    )
    .bind(proposal)
    .bind(uuid::Uuid::parse_str(memory).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn include_superseded_needs_the_history_capability_like_as_of_does() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let old = write::run(
        &ctx,
        "the vault password lives in the old keychain",
        "user:me",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "the vault password lives in 1Password",
        "user:me",
        None,
        Some(&old.id),
        None,
        None,
    )
    .await
    .unwrap();

    let blind = with_principal(&ctx, |p| p.may_read_history = false);
    let refused =
        search::run(&blind, "where the vault password lives", None, None, None, Some(true), None)
            .await
            .unwrap_err();
    assert_eq!(refused.kind.http_status(), 403);
    assert!(refused.client_message().contains("no longer hold"), "{}", refused.client_message());

    // The same client still searches live rows, and the retired one is not among them.
    let live = search::run(&blind, "where the vault password lives", None, None, None, None, None)
        .await
        .unwrap();
    assert!(!live.hits.iter().any(|h| h.id == old.id));

    // With the capability, the flag does what it says.
    let seen =
        search::run(&ctx, "where the vault password lives", None, None, None, Some(true), None)
            .await
            .unwrap();
    assert!(
        seen.hits.iter().any(|h| h.id == old.id),
        "the owner asked for history and did not get it"
    );
}

#[tokio::test]
async fn an_emission_is_a_keyed_digest_and_is_never_recorded_for_an_encrypted_row() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let open_text = format!("the open fact is {}", nonce("emitopen"));
    let private_text = format!("the private fact is {}", nonce("emitpriv"));
    let open = write::run(&ctx, &open_text, "user:me", None, None, None, None).await.unwrap();
    let private = write::run(&ctx, &private_text, "user:me", None, None, Some("private"), None)
        .await
        .unwrap();
    assert_eq!(private.sensitivity, Sensitivity::Private);

    let hits =
        search::run(&ctx, "the fact is", Some(vec!["user:me".into()]), Some(10), None, None, None)
            .await
            .unwrap();
    assert!(hits.hits.iter().any(|h| h.id == open.id));
    assert!(hits.hits.iter().any(|h| h.id == private.id), "the owner reads the private row");
    // record_emissions is fire and forget by contract.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let recorded: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT memory_id, content_sha256 FROM recall_emission WHERE tenant_id = 'me'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let open_id = uuid::Uuid::parse_str(&open.id).unwrap();
    let private_id = uuid::Uuid::parse_str(&private.id).unwrap();
    assert!(
        !recorded.iter().any(|(id, _)| *id == private_id),
        "an encrypted row must leave no digest of its plaintext behind"
    );
    let stored = recorded
        .iter()
        .find(|(id, _)| *id == open_id)
        .map(|(_, h)| h.clone())
        .expect("the open row was emitted");

    let keyed =
        lumberroom_server::crypto::Digester::from_provider(ctx.keys.as_ref()).await.unwrap();
    assert!(keyed.is_keyed());
    assert_eq!(stored, keyed.digest(&open_text), "the stored digest is the keyed one");
    assert_ne!(
        stored,
        lumberroom_server::crypto::Digester::unkeyed().digest(&open_text),
        "a plain hash of the text would let a dump holder verify a guess"
    );
}

#[tokio::test]
async fn a_proposal_loses_its_plaintext_once_its_memory_is_encrypted_or_forgotten() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let secret = nonce("proposal");
    let text = format!("the health note is {secret}");

    // Linked to a private row: cleared at the link.
    let sealed = raw_proposal(&pool, &text, Some(&text)).await;
    let private =
        write::run(&ctx, &text, "user:me", None, None, Some("private"), None).await.unwrap();
    link_proposal(&pool, sealed, &private.id).await;
    let (content, quote, memory_id, _) = proposal_state(&pool, sealed).await;
    assert_eq!(content, "", "the plaintext stayed in the queue after the row was sealed");
    assert_eq!(quote, None);
    assert_eq!(memory_id.map(|u| u.to_string()), Some(private.id.clone()));

    // Linked to an open row: kept, there is nothing to protect.
    let open_text = format!("the open note is {}", nonce("proposalopen"));
    let plain = raw_proposal(&pool, &open_text, None).await;
    let open = write::run(&ctx, &open_text, "user:me", None, None, None, None).await.unwrap();
    link_proposal(&pool, plain, &open.id).await;
    let (content, _, _, _) = proposal_state(&pool, plain).await;
    assert_eq!(content, open_text);

    // Forgotten: the link goes, and the text with it.
    let gone = forget::by_id(&ctx, &open.id, Some("test"), false).await.unwrap();
    assert_eq!(gone.count, 1);
    let (content, _, memory_id, _) = proposal_state(&pool, plain).await;
    assert_eq!(memory_id, None, "the proposal still pointed at a deleted row");
    assert_eq!(content, "", "a shred that leaves the sentence in the queue is not a shred");
}

#[tokio::test]
async fn forgetting_a_memory_the_queue_produced_or_targets_succeeds() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let text = format!("an ingested private fact {}", nonce("fk"));
    let produced =
        write::run(&ctx, &text, "user:me", None, None, Some("private"), None).await.unwrap();
    let proposal = raw_proposal(&pool, &text, None).await;
    link_proposal(&pool, proposal, &produced.id).await;

    let target =
        write::run(&ctx, "a fact a proposal wants to replace", "user:me", None, None, None, None)
            .await
            .unwrap();
    let pinned = raw_proposal(&pool, "the replacement", None).await;
    sqlx::query("UPDATE ingest_proposal SET supersedes = $2 WHERE id = $1")
        .bind(pinned)
        .bind(uuid::Uuid::parse_str(&target.id).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let preview = forget::by_id(&ctx, &produced.id, None, true).await.unwrap();
    assert!(preview.dry_run && preview.count == 1);
    let done = forget::by_id(&ctx, &produced.id, Some("test"), false).await.unwrap();
    assert_eq!(done.count, 1, "{}", done.text);
    assert!(!memory_exists(&pool, &produced.id).await, "the foreign key kept the row alive");

    let done = forget::by_id(&ctx, &target.id, Some("test"), false).await.unwrap();
    assert_eq!(done.count, 1, "{}", done.text);
    assert!(!memory_exists(&pool, &target.id).await);
    let (_, _, _, supersedes) = proposal_state(&pool, pinned).await;
    assert_eq!(supersedes, None, "a proposal cannot pin a memory against the owner's forget");
}

#[tokio::test]
async fn no_foreign_key_into_memory_is_left_to_block_a_delete() {
    let (_ctx, pool, _serial) = ctx_or_skip!();
    let keys: Vec<(String, String)> = sqlx::query_as(
        "SELECT conname::text, confdeltype::text FROM pg_constraint
          WHERE confrelid = 'memory'::regclass ORDER BY conname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!keys.is_empty());
    for (name, action) in &keys {
        match name.as_str() {
            // The delete path edits these itself under the caller's grant, and a constraint that
            // refuses is the signal that it missed one. Anything else pointing at memory has to
            // release the row on its own.
            "memory_supersedes_fkey" | "memory_superseded_by_fkey" => {
                assert_eq!(action, "a", "{name}")
            }
            _ => assert!(
                action == "n" || action == "c",
                "{name} is {action:?}: a NO ACTION key into memory makes forget fail with an \
                 internal error on the rows it references"
            ),
        }
    }
}

#[tokio::test]
async fn deleting_a_correction_does_not_revive_a_row_the_caller_cannot_reach() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let retired = write::run(
        &ctx,
        "the salary figure, the old one",
        "user:me",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();
    let correction = write::run(
        &ctx,
        "the salary figure was corrected",
        "user:me",
        None,
        Some(&retired.id),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(correction.sensitivity, Sensitivity::Open);
    assert_eq!(memory_links(&pool, &retired.id).await.1, Some(correction.id.clone()));

    let mut limited = narrow(&ctx, "user:me", Sensitivity::Open);
    limited.principal.may_delete = true;
    let refused = forget::by_id(&limited, &correction.id, None, false).await.unwrap_err();
    assert_eq!(refused.kind.http_status(), 404);
    assert!(!refused.client_message().contains("private"), "{}", refused.client_message());
    assert!(memory_exists(&pool, &correction.id).await, "the refusal has to leave the row");
    assert_eq!(
        memory_links(&pool, &retired.id).await.1,
        Some(correction.id.clone()),
        "the private fact the owner retired came back to life"
    );
    // The dry run says the same thing, so a preview never promises a delete that is then refused.
    assert!(forget::by_id(&limited, &correction.id, None, true).await.is_err());

    // The owner holds both grants and the revival is reported.
    let done = forget::by_id(&ctx, &correction.id, Some("test"), false).await.unwrap();
    assert_eq!(done.revived, vec![retired.id.clone()], "{}", done.text);
    assert!(done.text.contains("Revived 1 row"), "{}", done.text);
    assert_eq!(memory_links(&pool, &retired.id).await, (None, None));
}

#[tokio::test]
async fn the_date_review_proposes_one_day_reports_two_and_ignores_facts_that_name_none() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let one = write::run(
        &ctx,
        "the auditor signed the report on 11 February 2026 without qualification",
        "user:me",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let two = write::run(
        &ctx,
        "the filing went in on 2026-02-03 and the hearing followed on 9 April 2026",
        "user:me",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let none = write::run(
        &ctx,
        "the auditor prefers quarterly reviews",
        "user:me",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let found = review::date_candidates(&ctx, Some(200)).await.unwrap();
    let by_id = |id: &str| found.iter().find(|c| c.id == id);

    let single = by_id(&one.id).expect("a fact naming one day is a candidate");
    assert_eq!(single.proposed.as_deref(), Some("2026-02-11"));
    assert!(single.ambiguous.is_empty());

    let pair = by_id(&two.id).expect("a fact naming two days is still worth showing");
    assert_eq!(pair.proposed, None, "two dates means the owner picks, not the scan");
    assert_eq!(pair.ambiguous, vec!["2026-02-03", "2026-04-09"]);

    assert!(by_id(&none.id).is_none(), "a timeless fact is not a date candidate");

    // The review proposes and writes nothing.
    let still = review::date_candidates(&ctx, Some(200)).await.unwrap();
    assert!(by_id(&one.id).is_some() && still.iter().any(|c| c.id == one.id));
}

#[tokio::test]
async fn filling_a_date_takes_only_one_the_fact_itself_names_and_never_moves_an_existing_start() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let day = "2026-03-04T00:00:00Z".parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    let other = "2026-03-05T00:00:00Z".parse::<chrono::DateTime<chrono::Utc>>().unwrap();

    let row = write::run(
        &ctx,
        "the regulator cleared the filing on 4 March 2026",
        "user:me",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // A day the text does not name is refused, because nothing could check it against the row later.
    let refused = review::fill_date(&ctx, &row.id, other).await.unwrap_err();
    assert!(refused.client_message().contains("does not name"), "{}", refused.client_message());

    // The day the sentence states goes in.
    review::fill_date(&ctx, &row.id, day).await.unwrap();
    let after = review::fill_date(&ctx, &row.id, day).await.unwrap_err();
    assert!(
        after.client_message().contains("already carries"),
        "a start is filled once and never moved: {}",
        after.client_message()
    );

    // And it reaches the read it was missing from.
    let hits = search::run(
        &ctx,
        "regulator cleared the filing",
        None,
        Some(10),
        None,
        None,
        Some("2026-03-10T00:00:00Z".parse().unwrap()),
    )
    .await
    .unwrap();
    assert!(
        hits.hits.iter().any(|h| h.id == row.id),
        "the filled date should let an as-of read after it return the row"
    );
}

// ---- decision 0017: a fact can stop being true without being replaced ----

/// Does a live search for `label` return the row `id`?
///
/// The label is a nonce, so a hit is the row and a miss is the row being absent rather than the
/// query having drifted.
async fn live_search_finds(ctx: &Ctx, label: &str, id: &str) -> bool {
    search::run(ctx, label, None, Some(20), None, None, None)
        .await
        .unwrap()
        .hits
        .iter()
        .any(|h| h.id == id)
}

#[tokio::test]
async fn an_expired_row_leaves_search_and_the_digest_and_comes_back() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let label = nonce("expire");
    let row = write::run(
        &ctx,
        &format!("the sprint board {label} is where this week's work sits"),
        "global",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert!(live_search_finds(&ctx, &label, &row.id).await, "a fresh fact answers a live search");
    bootstrap::clear_cache();
    let before = bootstrap::run(&ctx, None).await.unwrap();
    assert!(digest_json(&before).contains(&label), "a live fact reaches the digest");

    let expired = review::expire(&ctx, &row.id).await.unwrap();
    assert_eq!(expired.id, row.id);

    assert!(
        !live_search_finds(&ctx, &label, &row.id).await,
        "a fact whose period has closed answers no live search"
    );
    let after = bootstrap::run(&ctx, None).await.unwrap();
    assert!(!digest_json(&after).contains(&label), "and it is out of the digest too");

    assert!(review::unexpire(&ctx, &row.id, expired.until).await.unwrap());
    assert!(
        live_search_finds(&ctx, &label, &row.id).await,
        "unexpire puts the fact back where it was"
    );
}

/// The whole reason this closes a period rather than deleting: the text, the history and every
/// as-of read survive the retirement.
#[tokio::test]
async fn an_expired_row_still_answers_as_of_a_time_inside_its_period() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let label = nonce("asof");
    let a_year_ago = chrono::Utc::now() - chrono::Duration::days(365);
    let row = write::run(
        &ctx,
        &format!("the {label} rate card held through the year"),
        "global",
        None,
        None,
        None,
        Some(a_year_ago),
    )
    .await
    .unwrap();

    review::expire(&ctx, &row.id).await.unwrap();

    let six_months_ago = chrono::Utc::now() - chrono::Duration::days(180);
    let then =
        search::run(&ctx, &label, None, Some(20), None, None, Some(six_months_ago)).await.unwrap();
    assert!(
        then.hits.iter().any(|h| h.id == row.id),
        "an instant inside the period still gets the fact"
    );
    assert!(!live_search_finds(&ctx, &label, &row.id).await, "and now does not");
}

#[tokio::test]
async fn expire_refuses_a_row_a_successor_already_retired() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let old = write::run(&ctx, "the port is 8080", "global", None, None, None, None).await.unwrap();
    let new = write::run(&ctx, "the port is 8787", "global", None, None, None, None).await.unwrap();
    review::supersede(&ctx, &old.id, &new.id).await.unwrap();

    let ended_by_the_supersession = occurred_until(&pool, &old.id).await;
    let err = review::expire(&ctx, &old.id).await.unwrap_err();
    // The refusal names the supersession rather than calling the row expired, because a stamped
    // row with a closed period was replaced and this path did not close it.
    assert!(err.client_message().contains("retired by a supersession"), "{}", err.client_message());
    assert_eq!(
        occurred_until(&pool, &old.id).await,
        ended_by_the_supersession,
        "the end the supersession wrote is not this path's to move"
    );
}

#[tokio::test]
async fn unexpire_refuses_an_instant_it_did_not_write() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let label = nonce("guard");
    let row = write::run(
        &ctx,
        &format!("the {label} standup runs at nine"),
        "global",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let expired = review::expire(&ctx, &row.id).await.unwrap();
    assert!(
        !review::unexpire(&ctx, &row.id, chrono::Utc::now()).await.unwrap(),
        "an instant nobody wrote reopens nothing"
    );
    assert_eq!(occurred_until(&pool, &row.id).await, Some(expired.until));

    assert!(review::unexpire(&ctx, &row.id, expired.until).await.unwrap());
    assert_eq!(occurred_until(&pool, &row.id).await, None);
}

#[tokio::test]
async fn expire_needs_the_write_grant_at_the_rows_own_level() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let row =
        write::run(&ctx, "a fact anyone may read", "global", None, None, None, None).await.unwrap();

    let reader = restricted(&ctx, &["global"], &[]);
    let err = review::expire(&reader, &row.id).await.unwrap_err();
    assert_eq!(err.kind.http_status(), 404);
    assert!(err.client_message().contains("not yours to change"), "{}", err.client_message());
}

/// The failure that made `find_exact` the first item on the review: the owner restates a fact the
/// store closed, the write collapses into the closed row, and the fact stays absent from every
/// live answer with nothing reporting it.
#[tokio::test]
async fn restating_an_expired_fact_writes_a_new_live_row_rather_than_collapsing_into_it() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let label = nonce("restate");
    let text = format!("the {label} standing order runs monthly");

    let first = write::run(&ctx, &text, "global", None, None, None, None).await.unwrap();
    let expired = review::expire(&ctx, &first.id).await.unwrap();

    let second = write::run(&ctx, &text, "global", None, None, None, None).await.unwrap();
    assert!(!second.deduplicated, "a closed fact restated is a fact again, not a duplicate");
    assert_ne!(second.id, first.id);
    assert_eq!(
        occurred_until(&pool, &first.id).await,
        Some(expired.until),
        "the old row stays closed"
    );
    assert_eq!(occurred_until(&pool, &second.id).await, None);
    assert!(live_search_finds(&ctx, &label, &second.id).await, "the restated fact answers now");
    assert!(!live_search_finds(&ctx, &label, &first.id).await);
}

/// A closed period takes no successor, which is what keeps `forget`'s revive honest: the revive
/// clears `occurred_until` on every row it brings back, and only a supersession may have written
/// one there.
#[tokio::test]
async fn an_expired_row_takes_no_successor_so_a_revive_cannot_wipe_its_end() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let old =
        write::run(&ctx, "the office is on the third floor", "global", None, None, None, None)
            .await
            .unwrap();
    let expired = review::expire(&ctx, &old.id).await.unwrap();
    let new =
        write::run(&ctx, "the office is on the ninth floor", "global", None, None, None, None)
            .await
            .unwrap();

    let refused = review::supersede(&ctx, &old.id, &new.id).await.unwrap_err();
    assert!(refused.client_message().contains("expired"), "{}", refused.client_message());

    // The write path takes the same target through the same check, so neither door is laxer.
    let refused_write = write::run(
        &ctx,
        "the office is on the tenth floor",
        "global",
        None,
        Some(&old.id),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        refused_write.client_message().contains("expired"),
        "{}",
        refused_write.client_message()
    );

    assert_eq!(memory_links(&pool, &old.id).await.1, None, "nothing retired it");
    assert_eq!(occurred_until(&pool, &old.id).await, Some(expired.until), "its end stands");
}

#[tokio::test]
async fn expiring_a_row_twice_names_the_instant_rather_than_blaming_a_race() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let row =
        write::run(&ctx, "the winter timetable is in force", "global", None, None, None, None)
            .await
            .unwrap();
    let expired = review::expire(&ctx, &row.id).await.unwrap();

    let again = review::expire(&ctx, &row.id).await.unwrap_err();
    assert_eq!(again.kind.http_status(), 400);
    assert!(again.client_message().contains("already expired at"), "{}", again.client_message());
    assert!(
        again.client_message().contains(&expired.until.to_rfc3339()),
        "the refusal names the instant, which is what unexpire is guarded on: {}",
        again.client_message()
    );
}

/// The retired page is the one list that says what left the live reads, so an expiry belongs on it
/// and has to be tellable from a supersession.
#[tokio::test]
async fn an_expired_row_reaches_the_retired_list_marked_as_expired() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let label = nonce("retiredlist");
    let row = write::run(
        &ctx,
        &format!("the {label} car park closes at seven"),
        "global",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    review::expire(&ctx, &row.id).await.unwrap();

    let readable = vec![lumberroom_server::domain::policy::NamespaceCeiling {
        namespace: "global".into(),
        max: Sensitivity::Sealed,
    }];
    let since = chrono::Utc::now() - chrono::Duration::days(7);
    let rows = ctx.repos.memories.retired_since(ctx.tenant(), &readable, since, 100).await.unwrap();

    let found = rows
        .iter()
        .find(|r| r.id.to_string() == row.id)
        .expect("the expired row is on the list of what left");
    assert!(found.expired, "and it is marked as expired rather than as replaced");
    assert!(found.successor_id.is_none());
}

/// A successor has to hold now. An expired row named as the replacement would retire a live fact
/// into one that no live read returns, which is the fact disappearing with a correction's paperwork
/// around it.
#[tokio::test]
async fn supersede_refuses_an_expired_row_as_the_replacement() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let old = write::run(&ctx, "the meeting is on Tuesday", "global", None, None, None, None)
        .await
        .unwrap();
    let new = write::run(&ctx, "the meeting is on Thursday", "global", None, None, None, None)
        .await
        .unwrap();
    review::expire(&ctx, &new.id).await.unwrap();

    let refused = review::supersede(&ctx, &old.id, &new.id).await.unwrap_err();
    assert!(refused.client_message().contains("does not hold now"), "{}", refused.client_message());
    assert_eq!(memory_links(&pool, &old.id).await.1, None, "the live row keeps holding");
}

/// The shape a restore leaves when it cannot relink a successor: `superseded_at` and
/// `occurred_until` set, the link NULL. It is a retirement whose successor is missing, and reading
/// the link alone filed it as expired everywhere, took away its replace form and refused a
/// supersession over it.
#[tokio::test]
async fn a_retirement_whose_successor_is_missing_reads_as_retired_rather_than_expired() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let orphan =
        write::run(&ctx, "the invoice runs through Stripe", "global", None, None, None, None)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE memory SET superseded_at = now(), occurred_until = now(), superseded_by = NULL
          WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(&orphan.id).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let readable = vec![lumberroom_server::domain::policy::NamespaceCeiling {
        namespace: "global".into(),
        max: Sensitivity::Sealed,
    }];
    let since = chrono::Utc::now() - chrono::Duration::days(7);
    let rows = ctx.repos.memories.retired_since(ctx.tenant(), &readable, since, 100).await.unwrap();
    let listed = rows
        .iter()
        .find(|r| r.id.to_string() == orphan.id)
        .expect("a retirement belongs on the list of what left");
    assert!(!listed.expired, "a stamped row with an end was superseded, not expired");
    assert!(listed.successor_id.is_none(), "and its successor is the part that went missing");

    // The replace path still works on it, which is the half that was lost.
    let replacement = write::run(
        &ctx,
        "the invoice runs through GoCardless",
        "global",
        None,
        Some(&orphan.id),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        memory_links(&pool, &orphan.id).await.1,
        Some(replacement.id),
        "the supersession landed and the link points at the new row"
    );
}

/// A fact whose period ends next week still holds this week, and both doors say so.
///
/// An archive restore binds `occurred_until`, `superseded_by` and `superseded_at` one by one, so a
/// live row with a future end reaches the store without an expiry ever running. The service check
/// let it through and the statement guard did not, which left the caller holding a supersession the
/// database never wrote.
#[tokio::test]
async fn a_future_end_takes_a_successor_at_both_doors() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let old = write::run(&ctx, "the loan runs at four percent", "global", None, None, None, None)
        .await
        .unwrap();
    let ends = chrono::Utc::now() + chrono::Duration::days(7);
    sqlx::query("UPDATE memory SET occurred_until = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&old.id).unwrap())
        .bind(ends)
        .execute(&pool)
        .await
        .unwrap();

    let new = write::run(&ctx, "the loan runs at five percent", "global", None, None, None, None)
        .await
        .unwrap();
    review::supersede(&ctx, &old.id, &new.id).await.unwrap();

    assert_eq!(
        memory_links(&pool, &old.id).await.1,
        Some(new.id.clone()),
        "the retirement landed rather than matching no row"
    );
    assert_eq!(
        memory_links(&pool, &new.id).await.0,
        Some(old.id.clone()),
        "and the mirror names the row it replaced"
    );
    // COALESCE keeps the end somebody stated. A supersession moves the link, never the date.
    assert_eq!(occurred_until(&pool, &old.id).await.map(|u| u.timestamp()), Some(ends.timestamp()));

    // The write path takes a target through the same check, so neither door is laxer.
    let third =
        write::run(&ctx, "the loan runs at six percent", "global", None, Some(&new.id), None, None)
            .await
            .unwrap();
    assert_eq!(memory_links(&pool, &new.id).await.1, Some(third.id));
}

/// A retire that moves no row leaves nothing behind, least of all a mirror claiming it happened.
///
/// The service refuses an expired target first, and the repository refuses it again inside the
/// transaction. The second refusal is the one under test: a caller reaching the port directly must
/// not end up with `supersedes` written on the new row while the old row stays live, which is two
/// rows where one claims to have replaced the other.
#[tokio::test]
async fn a_retire_that_moves_no_row_never_leaves_the_mirror_written() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let old =
        write::run(&ctx, "the depot opens at six", "global", None, None, None, None).await.unwrap();
    let new = write::run(&ctx, "the depot opens at seven", "global", None, None, None, None)
        .await
        .unwrap();
    // Expired: an end that has arrived, and no supersession stamp. Written by hand because an
    // archive restore writes it this way and `expire` is not the only path to the column.
    sqlx::query(
        "UPDATE memory SET occurred_until = now() - interval '1 day', superseded_at = NULL
          WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(&old.id).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let refused = review::supersede(&ctx, &old.id, &new.id).await.unwrap_err();
    assert!(refused.client_message().contains("expired"), "{}", refused.client_message());

    let from_the_port = ctx
        .repos
        .memories
        .supersede(
            ctx.tenant(),
            uuid::Uuid::parse_str(&old.id).unwrap(),
            uuid::Uuid::parse_str(&new.id).unwrap(),
        )
        .await
        .expect_err("the statement matched no row, so the transaction has nothing to commit");
    assert_eq!(from_the_port.kind.http_status(), 409);

    assert_eq!(memory_links(&pool, &new.id).await.0, None, "no mirror on the new row");
    assert_eq!(memory_links(&pool, &old.id).await.1, None, "and the old row keeps its own life");
}

// ---- decision 0014 part 4: the graph ----

/// The severing claim, which is the production-tier one. An edge whose far end the caller may not
/// read must not exist for that caller, because edge count and path length are facts about the store
/// that no content filter hides.
#[tokio::test]
async fn a_walk_never_crosses_a_node_the_caller_may_not_read() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let tag = vec!["quarterly-close".to_string()];

    // Three rows on one tag. The middle one is private, so a narrow client must not reach the third
    // through it, and must not learn that a third exists.
    let a = write::run(
        &ctx,
        "the close starts on the first working day",
        "user:me",
        Some(tag.clone()),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let secret = write::run(
        &ctx,
        "the close depends on the restricted ledger key",
        "user:me",
        Some(tag.clone()),
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();
    let c = write::run(
        &ctx,
        "the close is signed off by the auditor",
        "user:me",
        Some(tag),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    graph::rebuild(&ctx).await.unwrap();

    // The owner sees all three joined.
    let owner = graph::walk(&ctx, "the close", 60).await.unwrap();
    let owner_ids: Vec<&str> = owner.reached.iter().map(|r| r.id.as_str()).collect();
    assert!(owner_ids.contains(&secret.id.as_str()), "the owner reads private rows: {owner_ids:?}");

    // A client capped at open reaches the open rows and never the private one, by any path.
    let narrow = narrow(&ctx, "user:me", Sensitivity::Open);
    let seen = graph::walk(&narrow, "the close", 60).await.unwrap();
    let ids: Vec<&str> = seen.reached.iter().map(|r| r.id.as_str()).collect();
    assert!(!ids.contains(&secret.id.as_str()), "a private row was walked into: {ids:?}");

    // And no edge naming it comes back either. An edge is a fact about the store even when its
    // content is withheld.
    let touches_secret = seen
        .edges
        .iter()
        .any(|e| e.from_id.to_string() == secret.id || e.to_id.to_string() == secret.id);
    assert!(!touches_secret, "an edge named a row the caller cannot read");

    // The open rows are still joined to each other, so severing did not cost the answer.
    assert!(ids.contains(&a.id.as_str()) || ids.contains(&c.id.as_str()));
}

/// The router is what keeps a walk from being the default path. A question search answers must not
/// pay for one, and the verdict has to come back either way so the thresholds can be calibrated.
#[tokio::test]
async fn a_question_search_answers_does_not_pay_for_a_walk() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    write::run(
        &ctx,
        "the deployment runbook lives in docs/runbook.md",
        "user:me",
        Some(vec!["runbook".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "the deployment runbook was reviewed in June",
        "user:me",
        Some(vec!["runbook".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    graph::rebuild(&ctx).await.unwrap();

    // A question that names its subject plainly. Search stands clear of the field, so no walk.
    let routed = graph::routed(&ctx, "where does the deployment runbook live", 60).await.unwrap();
    let v = routed.verdict.clone().expect("a routed call always reports its verdict");
    assert!(!v.because.is_empty(), "a verdict has to say why");
    // Asserted, not guarded by an `if`. Wrapping this in a condition on the route makes the test
    // pass whenever the router changes its mind, which is exactly the regression it exists to catch.
    assert_eq!(
        v.route,
        lumberroom_server::domain::routing::Route::Search,
        "a question naming its subject plainly must not pay for a walk: {v:?}"
    );
    assert!(routed.edges.is_empty(), "the search route walked anyway: {routed:?}");

    // Forcing walks whatever the router said, which is what makes calibration possible.
    let forced = graph::walk(&ctx, "where does the deployment runbook live", 60).await.unwrap();
    assert!(forced.verdict.is_none(), "a forced walk asked nobody");
    assert!(forced.seeds > 0);
}

/// An empty question is not a walk and not an error.
#[tokio::test]
async fn an_empty_question_routes_nowhere() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let w = graph::routed(&ctx, "   ", 60).await.unwrap();
    assert_eq!(w.seeds, 0);
    assert!(w.edges.is_empty());
    assert!(w.verdict.is_none(), "nothing was asked, so nothing was decided");
}

/// A supersession edge reaches exactly what `memory_history` refuses. That gate has been opened by
/// a second spelling once already, which is why this checks the third.
#[tokio::test]
async fn a_walk_reaches_retired_rows_only_with_the_history_capability() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let old = write::run(
        &ctx,
        "the signing key lives in the old vault",
        "user:me",
        Some(vec!["signing".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "the signing key lives in the new vault",
        "user:me",
        Some(vec!["signing".into()]),
        Some(&old.id),
        None,
        None,
    )
    .await
    .unwrap();
    graph::rebuild(&ctx).await.unwrap();

    let blind = with_principal(&ctx, |p| p.may_read_history = false);
    let refused = graph::walk(&blind, "the signing key", 60).await.unwrap();
    let ids: Vec<&str> = refused.reached.iter().map(|r| r.id.as_str()).collect();
    assert!(
        !ids.contains(&old.id.as_str()),
        "a retired row was walked to without the history capability: {ids:?}"
    );
    assert!(
        !refused.edges.iter().any(|e| e.relation == "supersedes"),
        "a supersession edge is history by another name"
    );
}

/// Rebuilding is idempotent, which is what makes it safe to run on a schedule.
#[tokio::test]
async fn rebuilding_the_edge_table_twice_writes_nothing_the_second_time() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    for text in ["the racks are in row four", "the racks were audited in row four"] {
        write::run(&ctx, text, "user:me", Some(vec!["racks".into()]), None, None, None)
            .await
            .unwrap();
    }
    let first = graph::rebuild(&ctx).await.unwrap();
    let second = graph::rebuild(&ctx).await.unwrap();
    assert_eq!(first, second, "a second pass wrote new edges");
    assert!(first > 0, "two rows sharing a curated tag are an edge");
}

/// Rebuild has to mean rebuild. The idempotency test cannot catch this, because it rebuilds twice
/// over a store that never changed: an edge whose reason disappears has to disappear with it, or the
/// walk keeps crossing a join the store no longer believes in.
#[tokio::test]
async fn an_edge_whose_reason_is_gone_does_not_survive_a_rebuild() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let a = write::run(
        &ctx,
        "the failover drill runs on the first Tuesday",
        "user:me",
        Some(vec!["drill".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "the failover drill needs two operators",
        "user:me",
        Some(vec!["drill".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let with_tag = graph::rebuild(&ctx).await.unwrap();
    assert!(with_tag > 0, "two rows sharing a curated tag are an edge");

    // Take the tag off one row. The shared subject is gone, so the edge has no reason left.
    sqlx::query("UPDATE memory SET tags = ARRAY[]::text[] WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&a.id).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let after = graph::rebuild(&ctx).await.unwrap();
    assert!(
        after < with_tag,
        "the edge outlived the tag that justified it: {with_tag} then {after}"
    );
    let still: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_edge WHERE (src_id = $1 OR dst_id = $1) AND relation = 'shares_tag'",
    )
    .bind(uuid::Uuid::parse_str(&a.id).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(still, 0, "a shares_tag edge survived the tag being removed");
}

/// Deleting a row takes its edges with it. Two foreign keys with the default would reproduce the
/// shred failure 0013 was written about, and an edge outliving a shredded row leaks its shape.
#[tokio::test]
async fn deleting_a_row_removes_its_edges_rather_than_blocking_the_delete() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let a = write::run(
        &ctx,
        "the spare parts are in bin nine",
        "user:me",
        Some(vec!["bins".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "the spare parts were counted in bin nine",
        "user:me",
        Some(vec!["bins".into()]),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    graph::rebuild(&ctx).await.unwrap();

    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_edge").fetch_one(&pool).await.unwrap();
    assert!(before > 0);

    forget::by_id(&ctx, &a.id, Some("test"), false).await.expect("the delete must not be blocked");

    let touching: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_edge WHERE src_id = $1 OR dst_id = $1")
            .bind(uuid::Uuid::parse_str(&a.id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(touching, 0, "an edge outlived the row it named");
}

#[tokio::test]
async fn the_currency_measure_counts_closed_intervals_and_catches_a_double_answer() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let before = currency::run(&ctx, &[]).await.unwrap();

    // A dated pair: the successor starts after the predecessor, so supersession closes the period.
    let old = write::run(
        &ctx,
        "the office coffee machine is a moka pot",
        "user:me",
        None,
        None,
        None,
        Some("2026-01-05T00:00:00Z".parse().unwrap()),
    )
    .await
    .unwrap();
    let new = write::run(
        &ctx,
        "the office coffee machine is an espresso lever",
        "user:me",
        None,
        Some(&old.id),
        None,
        Some("2026-04-09T00:00:00Z".parse().unwrap()),
    )
    .await
    .unwrap();

    let after = currency::run(&ctx, &[]).await.unwrap();
    assert_eq!(after.coverage.pairs, before.coverage.pairs + 1);
    assert_eq!(after.coverage.closed, before.coverage.closed + 1, "a dated pair closes its period");
    assert_eq!(after.coverage.both_dated, before.coverage.both_dated + 1);

    // February sits inside the first fact's period, so the moka pot held and the lever did not.
    let case = currency::CurrencyCase {
        question: "what is the office coffee machine".into(),
        as_of: "2026-02-01T00:00:00Z".parse().unwrap(),
        expect_id: old.id.clone(),
        refuse_id: new.id.clone(),
    };
    let report = currency::run(&ctx, std::slice::from_ref(&case)).await.unwrap();
    assert_eq!(report.accuracy, Some(1.0), "{:?}", report.cases);
    assert_eq!(report.returned_both, 0);

    // The same question in May wants the other half, and asking for the wrong one has to fail
    // rather than pass on the strength of the row merely being present.
    let inverted = currency::CurrencyCase {
        question: "what is the office coffee machine".into(),
        as_of: "2026-05-01T00:00:00Z".parse().unwrap(),
        expect_id: old.id.clone(),
        refuse_id: new.id.clone(),
    };
    let wrong = currency::run(&ctx, &[inverted]).await.unwrap();
    assert_eq!(wrong.accuracy, Some(0.0), "the retired fact does not hold in May");

    // A case naming no expectation is refused, because it would score as a pass and measure nothing.
    let empty = currency::CurrencyCase {
        question: "anything".into(),
        as_of: chrono::Utc::now(),
        expect_id: String::new(),
        refuse_id: new.id.clone(),
    };
    assert!(currency::run(&ctx, &[empty]).await.is_err());
}

#[tokio::test]
async fn an_undated_pair_is_counted_as_open_which_is_what_the_measure_is_for() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let before = currency::run(&ctx, &[]).await.unwrap();

    // Both facts dated the same day: the period cannot close, so the pair is a link with no
    // interval. This is the row shape 0014 was written about.
    let day = "2026-06-11T00:00:00Z";
    let old = write::run(
        &ctx,
        "the standup is at 9 and moved on 11 June 2026",
        "user:me",
        None,
        None,
        None,
        Some(day.parse().unwrap()),
    )
    .await
    .unwrap();
    write::run(
        &ctx,
        "the standup is at 10 as of 11 June 2026",
        "user:me",
        None,
        Some(&old.id),
        None,
        Some(day.parse().unwrap()),
    )
    .await
    .unwrap();

    let after = currency::run(&ctx, &[]).await.unwrap();
    assert_eq!(after.coverage.pairs, before.coverage.pairs + 1);
    assert_eq!(after.coverage.closed, before.coverage.closed, "an empty period is not written");
    assert_eq!(
        after.coverage.dated_but_open,
        before.coverage.dated_but_open + 1,
        "the retired fact still reads as holding at every instant after its start"
    );
}

#[tokio::test]
async fn as_of_on_the_model_surface_still_answers_to_the_history_capability() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let instant = "2026-05-01T00:00:00Z".parse::<chrono::DateTime<chrono::Utc>>().unwrap();

    // The argument reaching a tool changes nothing about who may use it. This is the check that
    // would have caught the door being opened by a second spelling, which is how it went wrong once.
    let blind = with_principal(&ctx, |p| p.may_read_history = false);
    let refused = search::run(&blind, "anything", None, None, None, None, Some(instant))
        .await
        .expect_err("as_of without the capability has to refuse");
    assert_eq!(refused.kind.http_status(), 403, "{}", refused.client_message());

    // And it still refuses the pair, because the as-of statement applies no supersession filter of
    // its own and the flag would be ignored rather than honoured.
    let both = search::run(&ctx, "anything", None, None, None, Some(true), Some(instant))
        .await
        .expect_err("as_of beside include_superseded is a caller believing two things");
    assert!(both.client_message().contains("include_superseded"), "{}", both.client_message());
}

#[tokio::test]
async fn two_undated_facts_do_not_both_answer_an_instant_before_either_was_written() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    // Neither row carries a date, which is most of the store. Before the fallback landed, both
    // matched every instant and an as-of read handed back a fact and its replacement together.
    let old = write::run(&ctx, "the terminal theme is dark", "user:me", None, None, None, None)
        .await
        .unwrap();
    let new =
        write::run(&ctx, "the terminal theme is light", "user:me", None, Some(&old.id), None, None)
            .await
            .unwrap();

    let long_ago = "2020-01-01T00:00:00Z".parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    let before = search::run(&ctx, "terminal theme", None, Some(10), None, None, Some(long_ago))
        .await
        .unwrap();
    let ids: Vec<&str> = before.hits.iter().map(|h| h.id.as_str()).collect();
    assert!(
        !ids.contains(&old.id.as_str()) && !ids.contains(&new.id.as_str()),
        "the store cannot claim either fact held in 2020: {ids:?}"
    );

    // Now, the live head answers and the row it retired does not.
    let now =
        search::run(&ctx, "terminal theme", None, Some(10), None, None, Some(chrono::Utc::now()))
            .await
            .unwrap();
    let ids: Vec<&str> = now.hits.iter().map(|h| h.id.as_str()).collect();
    assert!(ids.contains(&new.id.as_str()), "the live fact has to answer now: {ids:?}");
    assert!(!ids.contains(&old.id.as_str()), "the retired fact answered too: {ids:?}");
}

#[tokio::test]
async fn a_revived_row_comes_back_to_the_as_of_read_as_well_as_the_live_one() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let retired =
        write::run(&ctx, "the deploy target is fly.io", "user:me", None, None, None, None)
            .await
            .unwrap();
    let correction = write::run(
        &ctx,
        "the deploy target is a hetzner box",
        "user:me",
        None,
        Some(&retired.id),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        occurred_until(&pool, &retired.id).await.is_some(),
        "supersession has to close the predecessor's period, or this test proves nothing"
    );

    forget::by_id(&ctx, &correction.id, Some("test"), false).await.unwrap();

    // Both reads, because the bug put them out of step: live search filters on `superseded_by`
    // alone and returned the row, every as-of read filters on `occurred_until` and did not.
    assert_eq!(occurred_until(&pool, &retired.id).await, None);
    let live = search::run(&ctx, "deploy target", None, Some(10), None, None, None).await.unwrap();
    assert!(live.hits.iter().any(|h| h.id == retired.id), "the revived row is missing from search");
    let as_of =
        search::run(&ctx, "deploy target", None, Some(10), None, None, Some(chrono::Utc::now()))
            .await
            .unwrap();
    assert!(
        as_of.hits.iter().any(|h| h.id == retired.id),
        "the revived row reads as ended, so as-of denies a fact live search returns"
    );
}

#[tokio::test]
async fn deleting_the_middle_of_a_chain_splices_the_ends_together() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let first =
        write::run(&ctx, "the port is 8080", "user:me", None, None, None, None).await.unwrap();
    let second = write::run(&ctx, "the port is 8443", "user:me", None, Some(&first.id), None, None)
        .await
        .unwrap();
    let third = write::run(&ctx, "the port is 8787", "user:me", None, Some(&second.id), None, None)
        .await
        .unwrap();

    let done = forget::by_id(&ctx, &second.id, Some("test"), false).await.unwrap();
    assert_eq!(done.count, 1);
    assert_eq!(done.spliced, vec![first.id.clone()], "{}", done.text);
    assert!(done.revived.is_empty(), "a row with a successor revives nothing");

    assert_eq!(
        memory_links(&pool, &first.id).await.1,
        Some(third.id.clone()),
        "the first row stays retired, behind the third"
    );
    assert_eq!(
        memory_links(&pool, &third.id).await.0,
        Some(first.id.clone()),
        "the third row now names the first as what it replaced"
    );

    // The first row is retired, so a live search does not serve it beside the third.
    let live =
        search::run(&ctx, "the port", Some(vec!["user:me".into()]), Some(10), None, None, None)
            .await
            .unwrap();
    assert!(live.hits.iter().any(|h| h.id == third.id));
    assert!(!live.hits.iter().any(|h| h.id == first.id), "two live versions of one fact");
}

#[tokio::test]
async fn a_write_only_grant_cannot_learn_whether_its_exact_sentence_is_stored() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let sentence = format!("the production database password is kept in {}", nonce("dedupe"));
    let existing =
        write::run(&ctx, &sentence, "project:secret", None, None, None, None).await.unwrap();

    let write_only = restricted(&ctx, &[], &["project:secret"]);
    let probe =
        write::run(&write_only, &sentence, "project:secret", None, None, None, None).await.unwrap();
    assert!(!probe.deduplicated, "deduplicated:true is a yes to an exact-content guess");
    assert_ne!(probe.id, existing.id, "and the id is the row's own");

    // The same sentence from a client that may read the namespace collapses, as before.
    let reader = restricted(&ctx, &["project:secret"], &["project:secret"]);
    let again =
        write::run(&reader, &sentence, "project:secret", None, None, None, None).await.unwrap();
    assert!(again.deduplicated);
}

#[tokio::test]
async fn a_private_write_is_refused_while_the_server_is_degraded_onto_the_hash_embedder() {
    use lumberroom_server::config::EmbedProvider;
    use lumberroom_server::ports::Embedder as _;
    // The harness runs the hash embedder under EMBED_PROVIDER=hash, which is the operator's own
    // choice and goes through. Tuning the config to `local` while the embedder stays the hash
    // sketch is the fallback window.
    let (ctx, _pool, _serial) =
        ctx_or_skip!(|cfg: &mut Config| cfg.embed.provider = EmbedProvider::Local);
    assert!(
        HashEmbedder::new(768).id().starts_with(write::HASH_EMBEDDER_ID_PREFIX),
        "the service keys on this prefix"
    );

    let refused = write::run(
        &ctx,
        "a private fact while degraded",
        "user:me",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.kind.http_status(), 503);
    assert!(
        refused.client_message().contains("fallback hash embedder"),
        "{}",
        refused.client_message()
    );

    // Open writes are unaffected: the sketch of an open row sits beside its plaintext anyway.
    write::run(&ctx, "an open fact while degraded", "user:me", None, None, None, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_open_ceiling_writer_cannot_replace_or_declassify_a_private_registry_row() {
    let (ctx, pool, _serial) = ctx_or_skip!();
    let secret = nonce("regslot");
    ctx.repos
        .registry
        .upsert(registry_write_at(
            "user:me",
            "credentials.aws.root",
            &secret,
            Sensitivity::Private,
            &provenance(),
        ))
        .await
        .unwrap();

    let mut bot = narrow(&ctx, "user:me", Sensitivity::Open);
    bot.principal.registry_write = true;
    let refused = registry::set(
        &bot,
        "user:me",
        "host",
        "credentials.aws.root",
        &serde_json::json!("poisoned"),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.kind.http_status(), 403);
    let message = refused.client_message().to_string();
    assert!(!message.contains("private"), "the level is the thing being hidden: {message}");
    assert!(!message.contains(&secret), "{message}");

    let row = sqlx::query(
        "SELECT value, sensitivity, version FROM registry WHERE key = 'credentials.aws.root'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<serde_json::Value, _>("value"), serde_json::json!(secret));
    assert_eq!(row.get::<&str, _>("sensitivity"), "private");
    assert_eq!(row.get::<i32, _>("version"), 1, "a refused write must not bump the version");

    // The owner's agents still read the owner's value.
    let got =
        registry::get(&ctx, "host", "credentials.aws.root", Some("user:me"), None).await.unwrap();
    assert_eq!(got.value, serde_json::json!(secret));

    // And the same bot writes an empty slot at open, which is all its grant was for.
    registry::set(
        &bot,
        "user:me",
        "host",
        "machines.laptop.os",
        &serde_json::json!("macOS"),
        None,
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn raising_a_registry_keys_level_takes_its_history_with_it() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let secret = nonce("reghist");
    registry::set(
        &ctx,
        "user:me",
        "credential-ref",
        "credentials.vault.root",
        &serde_json::json!("old-place"),
        None,
        None,
    )
    .await
    .unwrap();
    registry::set(
        &ctx,
        "user:me",
        "credential-ref",
        "credentials.vault.root",
        &serde_json::json!(secret),
        None,
        None,
    )
    .await
    .unwrap();
    // The same value, reclassified. Before this, the archive took an identical copy at open.
    registry::set(
        &ctx,
        "user:me",
        "credential-ref",
        "credentials.vault.root",
        &serde_json::json!(secret),
        Some("private"),
        None,
    )
    .await
    .unwrap();

    let mut open_reader = narrow(&ctx, "user:me", Sensitivity::Open);
    open_reader.principal.may_read_history = true;
    let seen = registry::history(
        &open_reader,
        "credential-ref",
        "credentials.vault.root",
        Some("user:me"),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        seen.entries.is_empty(),
        "an open ceiling read the private value out of the archive: {:?}",
        seen.entries.iter().map(|e| e.value.clone()).collect::<Vec<_>>()
    );

    let all = registry::history(
        &ctx,
        "credential-ref",
        "credentials.vault.root",
        Some("user:me"),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(all.entries.len(), 2);
    assert!(
        all.entries.iter().all(|e| e.sensitivity == Sensitivity::Private),
        "the raise lifts every earlier row"
    );

    // Lowering never lowers the archive.
    registry::set(
        &ctx,
        "user:me",
        "credential-ref",
        "credentials.vault.root",
        &serde_json::json!("public-place"),
        Some("open"),
        None,
    )
    .await
    .unwrap();
    let after = registry::history(
        &ctx,
        "credential-ref",
        "credentials.vault.root",
        Some("user:me"),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(after.entries.len(), 3);
    assert!(after.entries.iter().all(|e| e.sensitivity == Sensitivity::Private));
}

#[tokio::test]
async fn a_registry_value_at_open_goes_through_the_credential_tripwire() {
    let (ctx, _pool, _serial) = ctx_or_skip!();
    let token = "6f2a4c1e7b3d4f8a9c2e1d5b6a7c8e9f0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d";
    let refused = registry::set(
        &ctx,
        "user:me",
        "credential-ref",
        "credentials.lumberroom.token",
        &serde_json::json!({ "token": token }),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(refused.kind.http_status(), 400);
    let message = refused.client_message().to_string();
    assert!(message.contains("hex_credential"), "{message}");
    assert!(!message.contains(token), "{message}");

    // A reference to where it lives is what the registry is for.
    registry::set(
        &ctx,
        "user:me",
        "credential-ref",
        "credentials.lumberroom.token",
        &serde_json::json!({ "vault": "1Password", "item": "lumberroom client token" }),
        None,
        None,
    )
    .await
    .unwrap();
}

/// The upgrade guard reads `user_namespace_rows` because `namespace_counts` cannot see the two
/// shapes that matter most.
///
/// `namespace_counts` answers "which namespaces exist, for glob resolution", and it answers it from
/// live memory rows plus registry names. A namespace holding only retired rows never appears in it,
/// and a registry-only namespace appears with a count of zero. Both are stranded by the move to
/// `user:me`, and a guard that filtered that map on `n > 0` warned about neither.
#[tokio::test]
async fn the_stranded_namespace_query_sees_what_namespace_counts_misses() {
    let Some((ctx, pool, _guard, _db)) = setup().await else { return };

    let head = uuid::Uuid::new_v4();
    let retired = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, source_client)
         VALUES ($1, 'me', 'user:me', 'the live one', 'test')",
    )
    .bind(head)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, source_client, superseded_by)
         VALUES ($1, 'me', 'user:alice', 'retired under the old tenant', 'test', $2)",
    )
    .bind(retired)
    .bind(head)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO registry (tenant_id, namespace, kind, key, value, provenance)
         VALUES ('me', 'user:bob', 'service', 'db.host', '\"box\"'::jsonb, '{}'::jsonb)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let counts = ctx.repos.memories.namespace_counts("me").await.unwrap();
    assert!(!counts.contains_key("user:alice"), "the premise: retired-only is absent here");
    assert_eq!(counts.get("user:bob"), Some(&0), "the premise: registry-only counts zero here");

    let rows = ctx.repos.memories.user_namespace_rows("me").await.unwrap();
    let found: Vec<(String, String, i64)> =
        rows.iter().map(|r| (r.namespace.clone(), r.table.clone(), r.rows)).collect();
    assert!(
        found.contains(&("user:alice".to_string(), "memory".to_string(), 1)),
        "retired rows are still stranded rows: {found:?}"
    );
    assert!(
        found.contains(&("user:bob".to_string(), "registry".to_string(), 1)),
        "a registry-only namespace is stranded too: {found:?}"
    );
    assert!(
        found.iter().any(|(ns, t, _)| ns == "user:me" && t == "memory"),
        "user:me is reported and the caller is what skips it: {found:?}"
    );
}
