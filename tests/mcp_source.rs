//! The MCP tools name the app that wrote a row (decision 0020). Against a real Postgres and a bound
//! server, in the shape `tests/review_queue_mcp.rs` uses, skipped when no database is reachable.
//!
//! An OAuth writer here goes through the whole path a real client does: a consented row in
//! `oauth_client`, an access token in `oauth_token`, and a `memory_write` over MCP with that token
//! as its bearer. Only the authorization-code exchange is skipped; `scripts/oauth-flow-test.sh`
//! covers that.

use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone, Utc};
use lumberroom_server::adapters::auth;
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, AuthMode, Config, ResourceAudience};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::oauth::hash_token;
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::oauth::{ClientGrantUpdate, NewAccessToken, NewOauthClient};
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::{bootstrap, sources, write, Ctx, Repos};
use serde_json::{json, Value};
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
    oauth: Arc<dyn OauthStore>,
    base: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

/// One OAuth client with a live token, as the tests below need it.
struct Client {
    id: String,
    token: String,
}

impl Harness {
    async fn rpc(&self, bearer: &str, method: &str, params: Value) -> Value {
        let res = reqwest::Client::new()
            .post(format!("{}/mcp", self.base))
            .bearer_auth(bearer)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .unwrap();
        let status = res.status();
        let text = res.text().await.unwrap();
        assert!(status.is_success(), "{method} answered {status}: {text}");
        parse_body(&text)
    }

    /// One tool call, with a fresh `initialize` first, the way `tests/review_queue_mcp.rs` does it.
    async fn call_as(&self, bearer: &str, name: &str, args: Value) -> Value {
        let init = self
            .rpc(
                bearer,
                "initialize",
                json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "mcp-source-test", "version": "0.1.0" },
                }),
            )
            .await;
        assert!(init.get("error").is_none(), "initialize failed: {init}");
        let body = self.rpc(bearer, "tools/call", json!({ "name": name, "arguments": args })).await;
        assert!(body.get("error").is_none(), "{name} failed: {body}");
        let result = body["result"].clone();
        assert!(!result["isError"].as_bool().unwrap_or(false), "{name} refused: {result}");
        result
    }

    async fn call(&self, name: &str, args: Value) -> Value {
        self.call_as(OWNER_TOKEN, name, args).await
    }

    /// A consented client named `name`, holding a token. `added_at` backdates `created_at`, the
    /// instant a duplicate name's label is built from.
    async fn client(&self, name: &str, added_at: Option<DateTime<Utc>>) -> Client {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.oauth
            .register_client(NewOauthClient {
                client_id: id.clone(),
                secret_hash: None,
                client_name: name.into(),
                redirect_uris: vec!["http://127.0.0.1:9/callback".into()],
                grant_types: vec!["authorization_code".into()],
                software_id: None,
                software_version: None,
                registered_via: "dcr".into(),
            })
            .await
            .unwrap();
        self.oauth
            .set_client_grant(
                &id,
                ClientGrantUpdate {
                    profile: None,
                    read: NamespaceGrant::everything(),
                    write: vec![NamespaceGrant::open("*")],
                    registry_write: true,
                    sealed_capable: false,
                    may_delete: false,
                    may_ingest: false,
                    may_read_history: true,
                },
            )
            .await
            .unwrap();
        if let Some(at) = added_at {
            sqlx::query("UPDATE oauth_client SET created_at = $2 WHERE client_id = $1")
                .bind(&id)
                .bind(at)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        self.oauth
            .insert_token(NewAccessToken {
                token_hash: hash_token(&token),
                client_id: id.clone(),
                scope: String::new(),
                resource: None,
                family_id: uuid::Uuid::new_v4(),
                expires_at: Utc::now() + Duration::hours(1),
            })
            .await
            .unwrap();
        Client { id, token }
    }

    /// `memory_write` over MCP with the given bearer, returning the new row's id.
    async fn write_as(&self, bearer: &str, content: &str) -> String {
        let written = self
            .call_as(bearer, "memory_write", json!({ "content": content, "namespace": "global" }))
            .await;
        structured(&written)["id"].as_str().unwrap().to_string()
    }

    /// The `source` on one row of a `memory_search`, looked up by id.
    async fn searched_source(&self, query: &str, id: &str) -> (String, Value) {
        let result = self.call("memory_search", json!({ "query": query, "limit": 20 })).await;
        let hits = structured(&result)["hits"].as_array().cloned().unwrap_or_default();
        let hit = hits
            .iter()
            .find(|h| h["id"] == id)
            .unwrap_or_else(|| panic!("{id} not among the hits: {result}"))
            .clone();
        assert!(hit.get("source_client").is_none(), "a hit still carries source_client: {hit}");
        (hit["source"].as_str().unwrap_or_default().to_string(), result)
    }
}

async fn setup(mode: AuthMode) -> Option<Harness> {
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
    cfg.auth.mode = mode;
    // The tokens below are minted straight into the table with no resource indicator, which the
    // shipped `lenient` default admits too. `Off` says so rather than leaning on the default.
    cfg.oauth.resource_audience = ResourceAudience::Off;

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

    let oauth: Arc<dyn OauthStore> = Arc::new(postgres::PgOauthStore::new(pool.clone()));
    let memories = Arc::new(postgres::PgMemoryRepository::new(pool.clone()));
    // Wired the way main.rs wires it, so the token-mode test exercises the real decision.
    let repos = Repos {
        memories: memories.clone(),
        registry: Arc::new(postgres::PgRegistryRepository::new(pool.clone())),
        tool_calls: Arc::new(postgres::PgToolCallRepository::new(pool.clone())),
        sealed: Some(Arc::new(postgres::PgSealedRepository::new(pool.clone()))),
        ciphertext: Some(memories),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        oauth: sources::label_store(mode, &oauth),
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
    bootstrap::clear_cache();

    let state = Arc::new(AppState {
        cleanup: Arc::new(postgres::PgCleanupRepository::new(pool.clone())),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        cfg: cfg.clone(),
        repos,
        oauth: Arc::clone(&oauth),
        ingest: Arc::new(postgres::PgIngestRepository::new(pool.clone())),
        embedder: Arc::clone(&ctx.embedder),
        degraded_embedder: false,
        keys: ctx.keys.clone(),
        kek_verified: ctx.kek_verified,
        proposals: vec![],
    });
    let authenticator = auth::create(&ctx.cfg, Some(Arc::clone(&oauth))).ok()?;
    let app = lumberroom_server::http::router(state, authenticator);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr: SocketAddr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });

    Some(Harness { ctx, pool, oauth, base: format!("http://{addr}"), _serial: guard, _db: db_lock })
}

macro_rules! harness_or_skip {
    ($mode:expr) => {
        match setup($mode).await {
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

fn parse_body(text: &str) -> Value {
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

fn structured(result: &Value) -> Value {
    result["structuredContent"].clone()
}

fn text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|blocks| {
            blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default()
}

/// The whole tool result, text block and structured copy, must never carry the id.
fn assert_no_id(tool: &str, result: &Value, client_id: &str) {
    let raw = result.to_string();
    assert!(!raw.contains(client_id), "{tool} leaked the client_id {client_id}: {raw}");
    assert!(!raw.contains("source_client"), "{tool} still carries source_client: {raw}");
}

fn nonce(label: &str) -> String {
    format!("zqxsrc{label}{}zqx", uuid::Uuid::new_v4().simple())
}

fn at(d: u32, h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, d, h, m, 0).unwrap()
}

#[tokio::test]
async fn an_oauth_writer_reads_as_its_name_and_its_client_id_never_reaches_mcp() {
    let h = harness_or_skip!(AuthMode::Oauth);
    let codex = h.client("Codex", None).await;
    let content = format!("the codex build box runs nightly at two {}", nonce("oauth"));
    let id = h.write_as(&codex.token, &content).await;

    let (source, search) = h.searched_source(&content, &id).await;
    assert_eq!(source, "Codex");
    assert_no_id("memory_search", &search, &codex.id);

    let history = h.call("memory_history", json!({ "id": id })).await;
    let versions = structured(&history)["versions"].as_array().cloned().unwrap_or_default();
    assert_eq!(versions.len(), 1, "{history}");
    assert_eq!(versions[0]["source"], "Codex");
    assert_eq!(versions[0]["id"], id);
    assert_no_id("memory_history", &history, &codex.id);

    bootstrap::clear_cache();
    let digest = h.call("context_bootstrap", json!({})).await;
    assert!(text(&digest).contains("via Codex"), "{}", text(&digest));
    // `global` feeds the profile section as well as the recent one, so look in every section.
    let payload = structured(&digest);
    let facts: Vec<Value> = ["profile", "project_context", "recent"]
        .iter()
        .flat_map(|section| payload[*section].as_array().cloned().unwrap_or_default())
        .collect();
    let fact = facts.iter().find(|f| f["id"] == id).unwrap_or_else(|| panic!("{digest}"));
    assert_eq!(fact["source"], "Codex");
    assert_no_id("context_bootstrap", &digest, &codex.id);

    let forget =
        h.call("memory_forget", json!({ "id": id, "reason": "test", "dry_run": true })).await;
    assert_eq!(structured(&forget)["rows"][0]["source"], "Codex", "{forget}");
    assert_no_id("memory_forget", &forget, &codex.id);
}

#[tokio::test]
async fn an_oauth_writer_reads_as_its_name_in_registry_get_and_registry_history() {
    let h = harness_or_skip!(AuthMode::Oauth);
    let codex = h.client("Codex", None).await;
    for port in [8080, 8443] {
        let args = json!({
            "kind": "service",
            "key": "services.lumberroom.port",
            "value": port,
            "namespace": "global",
        });
        h.call_as(&codex.token, "registry_set", args).await;
    }

    let got = h
        .call("registry_get", json!({ "kind": "service", "key": "services.lumberroom.port" }))
        .await;
    let got_json = structured(&got);
    assert_eq!(got_json["value"], 8443, "{got}");
    assert_eq!(got_json["source"], "Codex", "{got}");
    assert!(got_json["provenance"].is_object(), "provenance stays beside source: {got}");
    assert_no_id("registry_get", &got, &codex.id);

    let past = h
        .call("registry_history", json!({ "kind": "service", "key": "services.lumberroom.port" }))
        .await;
    let entries = structured(&past)["entries"].as_array().cloned().unwrap_or_default();
    assert_eq!(entries.len(), 1, "{past}");
    assert_eq!(entries[0]["value"], 8080);
    assert_eq!(entries[0]["source"], "Codex");
    assert_no_id("registry_history", &past, &codex.id);
}

#[tokio::test]
async fn a_static_token_writer_reads_as_its_token_label() {
    let h = harness_or_skip!(AuthMode::Oauth);
    let content = format!("the owner keeps the backups on the nas {}", nonce("token"));
    let id = h.write_as(OWNER_TOKEN, &content).await;

    let (source, _) = h.searched_source(&content, &id).await;
    assert_eq!(source, "owner");

    bootstrap::clear_cache();
    let digest = h.call("context_bootstrap", json!({})).await;
    assert!(text(&digest).contains("via owner"), "{}", text(&digest));
}

#[tokio::test]
async fn a_revoked_client_s_facts_keep_its_name() {
    let h = harness_or_skip!(AuthMode::Oauth);
    let codex = h.client("Codex", None).await;
    let content = format!("the codex cache lives on the scratch disk {}", nonce("revoked"));
    let id = h.write_as(&codex.token, &content).await;
    assert!(h.oauth.revoke_client(&codex.id).await.unwrap());

    let (source, search) = h.searched_source(&content, &id).await;
    assert_eq!(source, "Codex");
    assert_no_id("memory_search", &search, &codex.id);
}

#[tokio::test]
async fn two_clients_named_codex_read_as_two_different_labels() {
    let h = harness_or_skip!(AuthMode::Oauth);
    let first = h.client("Codex", Some(at(1, 13, 2))).await;
    let second = h.client("Codex", Some(at(3, 10, 0))).await;
    let tag = nonce("pair");
    let a = h.write_as(&first.token, &format!("the first codex uses the ssd pool {tag}")).await;
    let b = h.write_as(&second.token, &format!("the second codex uses the hdd pool {tag}")).await;

    let (source_a, search) = h.searched_source(&tag, &a).await;
    let (source_b, _) = h.searched_source(&tag, &b).await;
    assert_eq!(source_a, "Codex (added 1 Sep)");
    assert_eq!(source_b, "Codex (added 3 Sep)");
    assert_no_id("memory_search", &search, &first.id);
    assert_no_id("memory_search", &search, &second.id);
}

#[tokio::test]
async fn two_clients_named_codex_added_the_same_day_carry_the_time() {
    let h = harness_or_skip!(AuthMode::Oauth);
    let first = h.client("Codex", Some(at(1, 13, 2))).await;
    let second = h.client("Codex", Some(at(1, 15, 40))).await;
    let tag = nonce("tie");
    let a = h.write_as(&first.token, &format!("the morning codex owns the migrations {tag}")).await;
    let b = h.write_as(&second.token, &format!("the evening codex owns the release {tag}")).await;

    let (source_a, _) = h.searched_source(&tag, &a).await;
    let (source_b, _) = h.searched_source(&tag, &b).await;
    assert_eq!(source_a, "Codex (added 1 Sep, 13:02)");
    assert_eq!(source_b, "Codex (added 1 Sep, 15:40)");
}

/// Token mode wires no store into `Repos`, so a row an OAuth client wrote under an earlier
/// configuration prints its stored id. The client row sits in the table the whole time: had
/// `labels` read it, the answer would be "Codex".
#[tokio::test]
async fn in_token_mode_a_stored_client_id_prints_as_stored() {
    let h = harness_or_skip!(AuthMode::Token);
    assert!(h.ctx.repos.oauth.is_none());
    let codex = h.client("Codex", None).await;
    let mut as_codex = h.ctx.clone();
    as_codex.principal = Principal { client: codex.id.clone(), ..owner() };
    let content = format!("the codex runner pins node twenty two {}", nonce("tokenmode"));
    let id = write::run(&as_codex, &content, "global", None, None, None, None).await.unwrap().id;

    let (source, _) = h.searched_source(&content, &id).await;
    assert_eq!(source, codex.id);
}

/// `tags` on `memory_search` reaches the query and means all of them. A second row answers the
/// same text without the tag and has to stay out. The filter arrives in a spelling the write
/// would have cleaned, which is what a model copying a tag from a user's sentence sends.
#[tokio::test]
async fn memory_search_keeps_only_hits_carrying_every_tag_it_was_given() {
    let h = harness_or_skip!(AuthMode::Token);
    let stem = nonce("tags");
    let write = |content: String, tags: Value| {
        let h = &h;
        async move {
            let written = h
                .call(
                    "memory_write",
                    json!({ "content": content, "namespace": "global", "tags": tags }),
                )
                .await;
            structured(&written)["id"].as_str().unwrap().to_string()
        }
    };
    let both =
        write(format!("the {stem} release train leaves on thursdays"), json!(["infra", "release"]))
            .await;
    let one = write(format!("the {stem} release notes live in the wiki"), json!(["release"])).await;

    let search = |tags: Value| {
        let h = &h;
        let stem = stem.clone();
        async move {
            let result = h
                .call(
                    "memory_search",
                    json!({ "query": format!("{stem} release"), "limit": 20, "tags": tags }),
                )
                .await;
            structured(&result)["hits"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|hit| hit["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    let release = search(json!(["Release "])).await;
    assert!(release.contains(&both) && release.contains(&one), "{release:?}");
    assert_eq!(search(json!(["release", "INFRA"])).await, vec![both.clone()]);
    assert!(search(json!(["release", "nowhere"])).await.is_empty(), "all of, never any of");
}
