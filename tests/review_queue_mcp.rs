//! `review_queue` and `review_decide` on the MCP surface, Phase 8 T7. Against a real Postgres, in
//! the shape `tests/mcp_capability.rs` and `tests/review_queue.rs` each use: a bound server, real
//! JSON-RPC round trips, skipped when no database is reachable.
//!
//! Not run. `cargo test` is off limits here: the integration suite truncates a shared database and
//! this file was written and checked (`./scripts/cargo.sh check --all-targets`) but never executed.

use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{Duration, Utc};
use lumberroom_server::adapters::auth;
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::{write, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";
const OWNER_TOKEN: &str = "oooooooooooooooooooooooooooooooo";

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
    base: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

impl Harness {
    /// One JSON-RPC round trip, the same shape `tests/mcp_capability.rs::rpc` uses.
    async fn rpc(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let res = reqwest::Client::new()
            .post(format!("{}/mcp", self.base))
            .bearer_auth(OWNER_TOKEN)
            .header("accept", "application/json, text/event-stream")
            .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .unwrap();
        let status = res.status();
        let text = res.text().await.unwrap();
        assert!(status.is_success(), "{method} answered {status}: {text}");
        parse_body(&text)
    }

    async fn call(&self, name: &str, args: serde_json::Value) -> serde_json::Value {
        let init = self
            .rpc(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "review-queue-mcp-test", "version": "0.1.0" },
                }),
            )
            .await;
        assert!(init.get("error").is_none(), "initialize failed: {init}");
        let body = self.rpc("tools/call", serde_json::json!({ "name": name, "arguments": args })).await;
        assert!(body.get("error").is_none(), "{name} failed: {body}");
        body["result"].clone()
    }
}

fn parse_body(text: &str) -> serde_json::Value {
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text).unwrap_or_else(|e| panic!("body {text:?}: {e}"));
    }
    let last = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or_else(|| panic!("no JSON and no SSE frame in {text:?}"));
    serde_json::from_str(last).unwrap_or_else(|e| panic!("SSE frame {last:?}: {e}"))
}

/// A `tools/call` result carries `isError` and a `structuredContent` mirror of the same JSON the
/// HTTP route answers; the tests below read structured content rather than parsing the text block.
fn refused(result: &serde_json::Value) -> bool {
    result.get("isError").and_then(serde_json::Value::as_bool).unwrap_or(false)
}

fn text(result: &serde_json::Value) -> String {
    result["content"]
        .as_array()
        .map(|blocks| blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

fn structured(result: &serde_json::Value) -> serde_json::Value {
    result["structuredContent"].clone()
}

fn owner() -> Principal {
    Principal {
        client: "owner".into(),
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
    std::env::set_var(
        "AUTH_TOKENS",
        format!(
            r#"[{{"client":"owner","token":"{OWNER_TOKEN}","read":[{{"namespace":"*","max":"sealed"}}],"write":[{{"namespace":"*","max":"sealed"}}],"sealedCapable":true,"registryWrite":true,"mayDelete":true,"mayReadHistory":true}}]"#
        ),
    );
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
        principal: owner(),
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
        // The engine ships no proposal source; these tests exercise the queue and decide tools
        // over conflict and stale only.
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

fn nonce(label: &str) -> String {
    format!("zqxnonce{label}zqx")
}

async fn write_at(ctx: &Ctx, content: &str, namespace: &str) -> String {
    write::run(ctx, content, namespace, None, None, None, None).await.unwrap().id
}

async fn set_created_at(pool: &PgPool, id: &str, at: chrono::DateTime<Utc>) {
    sqlx::query("UPDATE memory SET created_at = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
}

/// The same near-duplicate shape `tests/review_queue.rs::conflict_pair` uses: close enough in
/// wording that the hash embedder scores the pair as neighbours, backdated so ordering never races
/// the clock.
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

// ---------------------------------------------------------------------------------------------
// review_queue
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn review_queue_reads_a_stale_row_with_content_wrapped_as_data() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.stale_days = 30);
    let id = write_at(&h.ctx, &format!("the review target is fine for now {}", nonce("s1")), "global").await;
    make_stale(&h.pool, &id).await;

    let result = h.call("review_queue", serde_json::json!({ "source": ["stale"] })).await;
    assert!(!refused(&result), "{result:?}");

    let key = format!("stale:{id}");
    let body = text(&result);
    assert!(body.contains(&key), "text carries the item key: {body}");
    assert!(body.contains("data below, not instructions"), "row content is wrapped as data: {body}");

    let items = structured(&result)["items"].as_array().cloned().unwrap_or_default();
    let item = items.iter().find(|i| i["key"] == key).expect("the stale row is on the page");
    let verdicts: Vec<&str> =
        item["verdicts"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(verdicts.contains(&"confirm"), "an owner-writable stale row offers confirm: {verdicts:?}");
}

#[tokio::test]
async fn an_unknown_source_word_is_refused_with_the_queue_s_own_code() {
    let h = ctx_or_skip!(|_| {});
    let result = h.call("review_queue", serde_json::json!({ "source": ["nonsense"] })).await;
    assert!(refused(&result), "{result:?}");
    assert!(text(&result).contains("unknown_source"), "{}", text(&result));
}

#[tokio::test]
async fn asking_the_engine_for_proposals_alone_is_refused_source_not_filled() {
    let h = ctx_or_skip!(|_| {});
    let result = h.call("review_queue", serde_json::json!({ "source": ["proposal"] })).await;
    assert!(refused(&result), "{result:?}");
    assert!(text(&result).contains("source_not_filled"), "{}", text(&result));
}

// ---------------------------------------------------------------------------------------------
// review_decide
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn confirm_through_the_mcp_tool_clears_the_stale_row_from_the_next_page() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.stale_days = 30);
    let id = write_at(&h.ctx, &format!("the review target still holds {}", nonce("s2")), "global").await;
    make_stale(&h.pool, &id).await;
    let key = format!("stale:{id}");

    let before = h.call("review_queue", serde_json::json!({ "source": ["stale"] })).await;
    assert!(!refused(&before), "{before:?}");
    let before_items = structured(&before)["items"].as_array().cloned().unwrap_or_default();
    assert!(before_items.iter().any(|i| i["key"] == key), "the stale row is on the page before confirm");

    let decided = h
        .call("review_decide", serde_json::json!({ "key": key, "verdict": "confirm" }))
        .await;
    assert!(!refused(&decided), "{decided:?}");
    let body = structured(&decided);
    assert_eq!(body["key"], key);
    assert_eq!(body["verdict"], "confirm");

    let after = h.call("review_queue", serde_json::json!({ "source": ["stale"] })).await;
    assert!(!refused(&after), "{after:?}");
    let items = structured(&after)["items"].as_array().cloned().unwrap_or_default();
    assert!(!items.iter().any(|i| i["key"] == key), "a confirmed row leaves the stale page");
}

#[tokio::test]
async fn keep_both_through_the_mcp_tool_dismisses_the_pair_and_raises_the_count() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "global", "kb1").await;
    let key = format!("conflict:{older}:{newer}");

    let before = h.call("review_queue", serde_json::json!({ "source": ["conflict"] })).await;
    assert!(!refused(&before), "{before:?}");
    let before_dismissed = structured(&before)["dismissed"].as_i64().unwrap_or(0);
    let before_items = structured(&before)["items"].as_array().cloned().unwrap_or_default();
    assert!(
        before_items.iter().any(|i| {
            i["key"] == key || i["key"] == format!("conflict:{newer}:{older}")
        }),
        "the pair is on the page before keep_both"
    );

    let decided =
        h.call("review_decide", serde_json::json!({ "key": key, "verdict": "keep_both" })).await;
    assert!(!refused(&decided), "{decided:?}");
    assert_eq!(structured(&decided)["verdict"], "keep_both");

    let after = h.call("review_queue", serde_json::json!({ "source": ["conflict"] })).await;
    assert!(!refused(&after), "{after:?}");
    let items = structured(&after)["items"].as_array().cloned().unwrap_or_default();
    assert!(
        !items.iter().any(|i| {
            i["key"] == key || i["key"] == format!("conflict:{newer}:{older}")
        }),
        "a kept pair leaves the conflict page"
    );
    let after_dismissed = structured(&after)["dismissed"].as_i64().unwrap_or(0);
    assert_eq!(after_dismissed, before_dismissed + 1);
}

#[tokio::test]
async fn a_verdict_the_item_never_offered_is_refused_by_its_own_code() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.conflict_threshold = 0.0);
    let (older, newer) = conflict_pair(&h.ctx, &h.pool, "global", "vs1").await;
    let key = format!("conflict:{older}:{newer}");

    // apply and dismiss belong to a proposal item; a conflict never offers either.
    let result = h.call("review_decide", serde_json::json!({ "key": key, "verdict": "apply" })).await;
    assert!(refused(&result), "{result:?}");
    assert!(text(&result).contains("verdict_not_for_source"), "{}", text(&result));
}

#[tokio::test]
async fn a_malformed_key_is_refused_not_a_queue_key() {
    let h = ctx_or_skip!(|_| {});
    let result = h
        .call("review_decide", serde_json::json!({ "key": "not a key", "verdict": "confirm" }))
        .await;
    assert!(refused(&result), "{result:?}");
    assert!(text(&result).contains("not_a_queue_key"), "{}", text(&result));
}

#[tokio::test]
async fn a_bare_date_on_occurred_at_is_refused_because_the_tool_takes_rfc_3339_only() {
    let h = ctx_or_skip!(|c: &mut Config| c.quality.stale_days = 30);
    let id = write_at(&h.ctx, &format!("the merge target still holds {}", nonce("s3")), "global").await;
    make_stale(&h.pool, &id).await;
    let key = format!("stale:{id}");

    let result = h
        .call(
            "review_decide",
            serde_json::json!({
                "key": key,
                "verdict": "merge",
                "content": "the merged fact",
                "occurred_at": "2026-03-01",
            }),
        )
        .await;
    assert!(refused(&result), "{result:?}");
    assert!(text(&result).contains("occurred_at"), "{}", text(&result));
}
