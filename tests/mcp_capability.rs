//! The capability gate on the MCP surface, against a real database and a real server. Skipped
//! when no database is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom cargo test --test mcp_capability
//!
//! Both directions of the gate, because they are two mechanisms and only one of them is a filter.
//! `list_tools` decides what a model ever tries. The service decides what happens when a client
//! hard-codes a tool name and calls it anyway, which is what a client that read the tool list once
//! and cached it does. A suite that checked the list alone would pass with every service check
//! deleted.
//!
//! One property here is not about capabilities at all. `alias_list` returns rows carrying namespace
//! names, and a name is a disclosure that a content filter never sees: an unfiltered list hands a
//! narrow credential the names of the namespaces it may not open. `scripts/policy-test.sh` makes
//! the same argument about counts published beside refused rows.
//!
//! The tool list from the owner's own credential is held against `TOOL_CAPABILITIES`. That is the
//! wiring check: a tool registered in a router nobody added to `Lumberroom::new` is absent here while
//! every unit test still passes.

use std::net::SocketAddr;
use std::sync::Arc;

use lumberroom_server::adapters::auth;
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::mcp::capability::TOOL_CAPABILITIES;
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::{alias, bootstrap, write, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";

/// Five credentials, one per capability plus the owner's own. Two would not do: proving that
/// `mayReadHistory` opens the history tools and nothing else needs a credential holding it alone.
const OWNER_TOKEN: &str = "oooooooooooooooooooooooooooooooo";
const BARE_TOKEN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const DELETE_TOKEN: &str = "dddddddddddddddddddddddddddddddd";
const HISTORY_TOKEN: &str = "hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh";
const REGISTRY_TOKEN: &str = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr";

/// The client name on the bare credential, which every refusal it triggers has to name.
const BARE_CLIENT: &str = "narrow";

/// A namespace the bare grant does not reach, holding content the defaults classify private.
const HIDDEN_NS: &str = "personal:finance";

/// What a client with no capability at all sees, spelled out rather than derived from the table.
/// Deriving it would make this test agree with any table, including a wrong one.
const OPEN_TOOLS: [&str; 7] = [
    "alias_list",
    "context_bootstrap",
    "memory_search",
    "memory_write",
    "registry_get",
    "review_decide",
    "review_queue",
];

/// Every test here truncates the shared test database, so they serialise themselves rather than
/// relying on `--test-threads=1` being remembered. Cargo runs one test binary at a time, so this
/// mutex and the ones in `integration.rs`, `ingest.rs` and `console.rs` do not have to know about
/// each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A setup step that is allowed to be missing, with the reason printed rather than swallowed.
///
/// The suite skips rather than fails when no database is reachable, which makes this printed
/// sentence the only thing standing between a broken run and a run somebody reads as a pass.
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
    /// The owner's context, for seeding through the services rather than through SQL.
    ctx: Ctx,
    base: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    /// Held for the whole test. The mutex above serialises this binary's own threads; this is what
    /// keeps the other five binaries out of the same database.
    _db: common::DbGuard,
}

impl Harness {
    /// One JSON-RPC round trip, mirroring `bin/lumberroom.mjs`: the same accept header, the same
    /// initialize before every call. The transport runs stateless with `json_response`, so a reply
    /// arrives as JSON, and the SSE branch is here because the accept header allows it.
    async fn rpc(&self, token: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
        let res = reqwest::Client::new()
            .post(format!("{}/mcp", self.base))
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .unwrap();
        let status = res.status();
        let text = res.text().await.unwrap();
        assert!(status.is_success(), "{method} answered {status}: {text}");
        parse_body(&text)
    }

    async fn call(
        &self,
        token: &str,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let init = self
            .rpc(
                token,
                "initialize",
                serde_json::json!({
                    // 2026-07-28 removed sessions, which is what makes a bare initialize-then-call
                    // pair valid with no session id to carry.
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "mcp-capability-test", "version": "0.1.0" },
                }),
            )
            .await;
        assert!(init.get("error").is_none(), "initialize failed: {init}");

        let body = self.rpc(token, method, params).await;
        assert!(body.get("error").is_none(), "{method} failed: {body}");
        body["result"].clone()
    }

    /// The tool names this credential is offered, sorted.
    async fn tools(&self, token: &str) -> Vec<String> {
        let result = self.call(token, "tools/list", serde_json::json!({})).await;
        let mut names: Vec<String> = result["tools"]
            .as_array()
            .expect("tools/list returns an array")
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    async fn tool_call(
        &self,
        token: &str,
        name: &str,
        args: serde_json::Value,
    ) -> serde_json::Value {
        self.call(token, "tools/call", serde_json::json!({ "name": name, "arguments": args })).await
    }
}

/// Streamable HTTP replies with either JSON or a single SSE frame; accept both, the way the CLI
/// does.
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

fn refused(result: &serde_json::Value) -> bool {
    result.get("isError").and_then(serde_json::Value::as_bool).unwrap_or(false)
}

fn message(result: &serde_json::Value) -> String {
    result["content"]
        .as_array()
        .map(|blocks| {
            blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default()
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
            r#"[{{"client":"owner","token":"{OWNER_TOKEN}","read":[{{"namespace":"*","max":"sealed"}}],"write":[{{"namespace":"*","max":"sealed"}}],"sealedCapable":true,"registryWrite":true,"mayDelete":true,"mayReadHistory":true}},
                {{"client":"{BARE_CLIENT}","token":"{BARE_TOKEN}","read":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}],"write":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}]}},
                {{"client":"deleter","token":"{DELETE_TOKEN}","read":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}],"write":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}],"mayDelete":true}},
                {{"client":"historian","token":"{HISTORY_TOKEN}","read":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}],"write":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}],"mayReadHistory":true}},
                {{"client":"registrar","token":"{REGISTRY_TOKEN}","read":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}],"write":[{{"namespace":"user:me","max":"open"}},{{"namespace":"global","max":"open"}}],"registryWrite":true}}]"#
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
        },
        embedder: Arc::new(HashEmbedder::new(768)),
        keys: Some(keys),
        kek_verified,
        principal: owner(),
        invocation: Invocation::Cli,
        session_id: Some("test-session".into()),
    };
    bootstrap::clear_cache();

    let oauth: Arc<dyn OauthStore> = Arc::new(postgres::PgOauthStore::new(pool.clone()));
    let state = Arc::new(AppState {
        cleanup: Arc::new(postgres::PgCleanupRepository::new(pool.clone())),
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

    Some(Harness { ctx, base: format!("http://{addr}"), _serial: guard, _db: db_lock })
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

/// A fact and the correction that retired it, so `memory_history` has a chain to walk.
async fn seed_chain(h: &Harness) -> String {
    let first = write::run(
        &h.ctx,
        "The dev Postgres for lumberroom listens on port 5433",
        "user:me",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let second = write::run(
        &h.ctx,
        "The dev Postgres for lumberroom listens on port 5544 since the move",
        "user:me",
        None,
        Some(&first.id),
        None,
        None,
    )
    .await
    .unwrap();
    second.id
}

fn expected(extra: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = OPEN_TOOLS.iter().map(|n| (*n).to_string()).collect();
    names.extend(extra.iter().map(|n| (*n).to_string()));
    names.sort();
    names
}

// -- what a credential is offered ----------------------------------------------------------------

/// The owner holds every capability, so his list is the router's whole list. Holding it against the
/// table catches both halves of the wiring: a tool registered and never entered in the table ships
/// ungated, and a tool entered in the table but never added to `Lumberroom::new` is documented and absent.
#[tokio::test]
async fn the_owners_tool_list_is_exactly_the_capability_table() {
    let h = harness_or_skip!();
    let mut declared: Vec<String> =
        TOOL_CAPABILITIES.iter().map(|(name, _)| (*name).to_string()).collect();
    declared.sort();
    assert_eq!(h.tools(OWNER_TOKEN).await, declared);
}

#[tokio::test]
async fn a_bare_grant_is_offered_only_the_open_tools() {
    let h = harness_or_skip!();
    assert_eq!(h.tools(BARE_TOKEN).await, expected(&[]));
}

#[tokio::test]
async fn each_capability_adds_its_own_tools_and_nothing_else() {
    let h = harness_or_skip!();
    assert_eq!(h.tools(DELETE_TOKEN).await, expected(&["memory_forget"]));
    assert_eq!(h.tools(HISTORY_TOKEN).await, expected(&["memory_history", "registry_history"]));
    assert_eq!(h.tools(REGISTRY_TOKEN).await, expected(&["alias_set", "registry_set"]));
}

// -- what a credential is refused ----------------------------------------------------------------

/// The property that matters. A client that names a tool absent from its list gets the same answer
/// it would get if the filter did not exist, because the filter is not the enforcement.
#[tokio::test]
async fn naming_a_tool_the_grant_excludes_is_refused_by_the_service() {
    let h = harness_or_skip!();
    let id = seed_chain(&h).await;

    let cases: Vec<(&str, serde_json::Value, &str)> = vec![
        (
            "memory_history",
            serde_json::json!({ "id": id }),
            "may not read facts that no longer hold",
        ),
        (
            "registry_history",
            serde_json::json!({ "kind": "service", "key": "services.lumberroom.port" }),
            "may not read values the registry no longer holds",
        ),
        (
            "registry_set",
            serde_json::json!({
                "kind": "service",
                "key": "services.lumberroom.port",
                "value": 8787,
                "namespace": "global",
            }),
            "may not write to the registry",
        ),
        (
            "alias_set",
            serde_json::json!({
                "alias": "warden",
                "canonical": "lumen",
                "namespace": "user:me",
            }),
            "may not record an alias",
        ),
        (
            "memory_forget",
            serde_json::json!({ "id": id, "reason": "testing the gate", "dry_run": true }),
            "may not delete",
        ),
    ];

    for (tool, args, phrase) in cases {
        let result = h.tool_call(BARE_TOKEN, tool, args).await;
        let text = message(&result);
        assert!(refused(&result), "{tool} was not refused: {text}");
        assert!(text.contains(phrase), "{tool} refused with the wrong reason: {text}");
        assert!(text.contains(BARE_CLIENT), "{tool} refusal does not name the client: {text}");
    }
}

/// A refusal names the client and nothing about what was asked for. A client told which key it may
/// not read the history of has learned that the key exists.
#[tokio::test]
async fn a_refusal_says_who_and_never_what() {
    let h = harness_or_skip!();
    let result = h
        .tool_call(
            BARE_TOKEN,
            "registry_history",
            serde_json::json!({ "kind": "credential-ref", "key": "credentials.stripe.live" }),
        )
        .await;
    let text = message(&result);
    assert!(refused(&result), "{text}");
    assert!(!text.contains("credentials.stripe.live"), "the refusal repeats the key: {text}");
    assert!(!text.contains("stripe"), "the refusal repeats the subject: {text}");
}

// -- what the capability turns on ------------------------------------------------------------

#[tokio::test]
async fn the_history_capability_returns_the_versions_a_correction_retired() {
    let h = harness_or_skip!();
    let id = seed_chain(&h).await;

    let result =
        h.tool_call(HISTORY_TOKEN, "memory_history", serde_json::json!({ "id": id })).await;
    assert!(!refused(&result), "{}", message(&result));
    let versions = result["structuredContent"]["versions"].as_array().unwrap().clone();
    assert_eq!(versions.len(), 2, "the chain is the write and its correction: {versions:?}");
    let oldest = versions[0]["content"].as_str().unwrap();
    assert!(oldest.contains("5433"), "oldest first, and the retired version is first: {oldest}");
}

#[tokio::test]
async fn the_registry_capability_writes_a_key_and_the_history_capability_reads_what_it_replaced() {
    let h = harness_or_skip!();

    for port in [8787, 8788] {
        let written = h
            .tool_call(
                REGISTRY_TOKEN,
                "registry_set",
                serde_json::json!({
                    "kind": "service",
                    "key": "services.lumberroom.port",
                    "value": port,
                    "namespace": "global",
                }),
            )
            .await;
        assert!(!refused(&written), "{}", message(&written));
        assert_eq!(written["structuredContent"]["key"], "services.lumberroom.port");
    }

    let history = h
        .tool_call(
            OWNER_TOKEN,
            "registry_history",
            serde_json::json!({
                "kind": "service",
                "key": "services.lumberroom.port",
                "namespace": "global",
            }),
        )
        .await;
    assert!(!refused(&history), "{}", message(&history));
    let entries = history["structuredContent"]["entries"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 1, "one replacement, one archived version: {entries:?}");
    assert_eq!(entries[0]["value"], 8787, "the archive holds what the key stopped holding");
}

#[tokio::test]
async fn the_registry_capability_records_an_alias_and_a_bare_grant_can_read_it() {
    let h = harness_or_skip!();

    let written = h
        .tool_call(
            REGISTRY_TOKEN,
            "alias_set",
            serde_json::json!({
                "alias": "warden",
                "canonical": "lumen",
                "namespace": "user:me",
                "since": "2026-03-01",
            }),
        )
        .await;
    assert!(!refused(&written), "{}", message(&written));
    assert_eq!(written["structuredContent"]["canonical"], "lumen");
    assert_eq!(written["structuredContent"]["origin"], "manual");

    let listed = h.tool_call(BARE_TOKEN, "alias_list", serde_json::json!({})).await;
    assert!(!refused(&listed), "{}", message(&listed));
    let rows = listed["structuredContent"]["aliases"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["alias"], "warden");
}

#[tokio::test]
async fn an_instant_argument_is_refused_by_the_name_the_caller_sent() {
    let h = harness_or_skip!();
    let result = h
        .tool_call(
            REGISTRY_TOKEN,
            "alias_set",
            serde_json::json!({
                "alias": "warden",
                "canonical": "lumen",
                "namespace": "user:me",
                "since": "last March",
            }),
        )
        .await;
    let text = message(&result);
    assert!(refused(&result), "{text}");
    assert!(text.contains("since"), "a model told to fix occurred_at fixes nothing here: {text}");
}

#[tokio::test]
async fn the_delete_capability_reaches_the_dry_run() {
    let h = harness_or_skip!();
    let id = seed_chain(&h).await;

    let result = h
        .tool_call(
            DELETE_TOKEN,
            "memory_forget",
            serde_json::json!({ "id": id, "reason": "testing the gate", "dry_run": true }),
        )
        .await;
    assert!(!refused(&result), "{}", message(&result));
    assert_eq!(result["structuredContent"]["dry_run"], true);
}

// -- names are a disclosure a content filter never sees ------------------------------------------

/// The trap this tool was written around. An alias row carries no content and two names, so nothing
/// in it trips a sensitivity filter, and listing every row would hand a narrow credential the names
/// of namespaces it cannot open along with the subjects filed in them.
#[tokio::test]
async fn alias_list_never_names_a_namespace_the_caller_may_not_read() {
    let h = harness_or_skip!();

    alias::put(
        &h.ctx,
        h.ctx.repos.aliases.as_ref(),
        HIDDEN_NS,
        "vault-old",
        "vault",
        None,
        None,
        None,
    )
    .await
    .unwrap();
    alias::put(
        &h.ctx,
        h.ctx.repos.aliases.as_ref(),
        "user:me",
        "warden",
        "lumen",
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let owner_view = h.tool_call(OWNER_TOKEN, "alias_list", serde_json::json!({})).await;
    assert_eq!(
        owner_view["structuredContent"]["aliases"].as_array().unwrap().len(),
        2,
        "the owner reads both, so a missing row below is the filter and not an empty store"
    );

    let narrow_view = h.tool_call(BARE_TOKEN, "alias_list", serde_json::json!({})).await;
    assert!(!refused(&narrow_view), "{}", message(&narrow_view));
    let body = narrow_view.to_string();
    assert!(!body.contains(HIDDEN_NS), "the list names a namespace the grant excludes: {body}");
    assert!(!body.contains("vault"), "the list names a subject the grant excludes: {body}");
    assert!(body.contains("warden"), "the list dropped a row the grant admits: {body}");
}

/// Asking for the namespace by name answers empty rather than refusing. A refusal would confirm the
/// namespace exists, which is the same disclosure by another route.
#[tokio::test]
async fn naming_an_unreadable_namespace_answers_empty_rather_than_refusing() {
    let h = harness_or_skip!();
    alias::put(
        &h.ctx,
        h.ctx.repos.aliases.as_ref(),
        HIDDEN_NS,
        "vault-old",
        "vault",
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let result =
        h.tool_call(BARE_TOKEN, "alias_list", serde_json::json!({ "namespace": HIDDEN_NS })).await;
    assert!(!refused(&result), "{}", message(&result));
    assert!(result["structuredContent"]["aliases"].as_array().unwrap().is_empty());
}

/// The tool holds the capability and still writes through the same service `/admin` writes
/// through, so a key the canonical rule refuses is refused here in the same words. The handler
/// adding a repair of its own is the failure this pins.
#[tokio::test]
async fn a_key_that_is_not_canonical_is_refused_with_the_repair_named() {
    let h = harness_or_skip!();
    let refusal = h
        .tool_call(
            REGISTRY_TOKEN,
            "registry_set",
            serde_json::json!({
                "kind": "service",
                "key": "lumberroom port",
                "value": 8787,
                "namespace": "global",
            }),
        )
        .await;
    let text = message(&refusal);
    assert!(refused(&refusal), "{text}");
    assert!(text.contains("invalid registry key"), "{text}");
    assert!(
        text.contains("Closest valid key"),
        "a rejection with no repair breeds a variant: {text}"
    );

    let retried = h
        .tool_call(
            REGISTRY_TOKEN,
            "registry_set",
            serde_json::json!({
                "kind": "service",
                "key": "services.lumberroom.port",
                "value": 8787,
                "namespace": "global",
            }),
        )
        .await;
    assert!(!refused(&retried), "{}", message(&retried));
    assert_eq!(retried["structuredContent"]["version"], 1);
}

#[tokio::test]
async fn whoami_reports_every_capability_that_gates_a_tool() {
    // A credential's own report has to name each flag the tool table reads, or the only way to
    // learn whether one is held is to notice a tool missing from a list nobody reads closely.
    let h = harness_or_skip!();
    let res = reqwest::Client::new()
        .get(format!("{}/admin/whoami", h.base))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let body = res.text().await.unwrap();
    for field in ["may_delete", "may_ingest", "may_read_history", "registry_write"] {
        assert!(body.contains(field), "whoami omits {field}, which gates a tool: {body}");
    }
}
