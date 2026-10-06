//! The owner names a client (decision 0023), over HTTP against the real router and a real Postgres,
//! in the shared `lumberroom_rust_test` database. Skipped when no database is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom cargo test --test client_label
//!
//! Every label here reaches the store through a surface the owner uses: the consent form, the
//! bearer route or the console. The consent driver below walks the authorization flow the way
//! `scripts/oauth-flow-test.sh` does, minus the password form: the session cookie comes from the
//! same signer the server verifies with, as in `tests/console.rs`, because what is under test is
//! what a signed-in owner's Allow writes, not how the signing happens.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{Duration, Utc};
use lumberroom_server::adapters::auth::{self, Authenticator};
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::authserver::session::Sessions;
use lumberroom_server::config::{self, AuthMode, Config, ResourceAudience};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::oauth::hash_token;
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::oauth::NewAccessToken;
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::{bootstrap, sources, write, Ctx, Repos};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";
const COOKIE_SECRET: &str = "label-test-cookie-secret-32chars";

/// Holds `registry_write`, the bar the rename route sets.
const OWNER_TOKEN: &str = "labelownerlabelownerlabelowner00";
/// Reads and writes `global` and holds nothing else.
const NARROW_TOKEN: &str = "labelnarrowlabelnarrowlabelnarr0";

/// Loopback, so the consent page raises no warning and a test reads the naming field alone.
const REDIRECT_URI: &str = "http://127.0.0.1:9/callback";

/// The domain's refusal for a name over 200 bytes, as every surface prints it.
const TOO_LONG: &str = "that name is too long: the limit is 200 bytes";

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
    authenticator: Arc<dyn Authenticator>,
    base: String,
    cookie: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    _db: common::DbGuard,
}

/// What the owner does with the "Name this connection" field before pressing Allow.
enum Field<'a> {
    /// A page rendered before the field existed: the POST carries no `label` at all.
    Absent,
    /// Pressed Allow without touching it: the POST carries whatever the page prefilled.
    AsPrefilled,
    /// Typed something.
    Typed(&'a str),
}

/// One answer from the server, with the Location header kept, since a consent that lands is a
/// redirect with no body.
struct Answer {
    status: u16,
    location: String,
    body: String,
}

/// The consent page as the owner sees it.
struct ConsentPage {
    html: String,
    csrf: String,
    /// The `value` of the naming input, unescaped. `None` when the page carries no such input.
    prefilled: Option<String>,
    flow: Vec<(&'static str, String)>,
}

fn http() -> reqwest::Client {
    // A redirect the client follows is one the test cannot see, and the consent redirect carrying
    // the code is the evidence that a grant landed.
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

async fn answer(res: reqwest::Response) -> Answer {
    let status = res.status().as_u16();
    let location =
        res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = res.text().await.unwrap();
    Answer { status, location, body }
}

impl Harness {
    /// `POST /oauth/register`, the anonymous door every self-registered client comes through.
    async fn register_raw(&self, body: Value) -> (u16, Value) {
        let res =
            http().post(format!("{}/oauth/register", self.base)).json(&body).send().await.unwrap();
        let status = res.status().as_u16();
        let text = res.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// A self-registered public client named `name`, returning its id.
    async fn register(&self, name: &str) -> String {
        let (status, body) = self
            .register_raw(json!({
                "client_name": name,
                "redirect_uris": [REDIRECT_URI],
                "token_endpoint_auth_method": "none",
            }))
            .await;
        assert!((200..300).contains(&status), "registering {name} answered {status}: {body}");
        body["client_id"].as_str().unwrap().to_string()
    }

    /// `GET /oauth/authorize` with the owner session, which renders the consent page directly.
    async fn consent_page(&self, client_id: &str) -> ConsentPage {
        let verifier =
            format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = uuid::Uuid::new_v4().simple().to_string();
        let flow = vec![
            ("response_type", "code".to_string()),
            ("client_id", client_id.to_string()),
            ("redirect_uri", REDIRECT_URI.to_string()),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256".to_string()),
            ("state", state),
        ];
        let res = http()
            .get(format!("{}/oauth/authorize", self.base))
            .query(&flow)
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        let page = answer(res).await;
        assert_eq!(page.status, 200, "the consent page did not render: {}", page.body);
        let csrf = attribute_after(&page.body, "name=\"csrf\" value=\"")
            .unwrap_or_else(|| panic!("the consent page carries no csrf field: {}", page.body));
        let prefilled =
            attribute_after(&page.body, "name=\"label\" type=\"text\" maxlength=\"200\" value=\"");
        ConsentPage { html: page.body, csrf, prefilled, flow }
    }

    /// The owner opens the consent page, does `field` to the name, picks the full profile and
    /// presses Allow.
    async fn allow(&self, client_id: &str, field: Field<'_>) -> Answer {
        let page = self.consent_page(client_id).await;
        let mut form: Vec<(&str, String)> = page.flow.clone();
        form.push(("csrf", page.csrf.clone()));
        form.push(("profile", "full".into()));
        form.push(("action", "allow".into()));
        match field {
            Field::Absent => {}
            Field::AsPrefilled => form.push((
                "label",
                page.prefilled.clone().expect("the page carries no naming field to leave alone"),
            )),
            Field::Typed(text) => form.push(("label", text.to_string())),
        }
        let res = http()
            .post(format!("{}/oauth/consent", self.base))
            .header("cookie", &self.cookie)
            .form(&form)
            .send()
            .await
            .unwrap();
        answer(res).await
    }

    /// Asserts the consent landed: a redirect to the client carrying a code.
    async fn approve(&self, client_id: &str, field: Field<'_>) {
        let a = self.allow(client_id, field).await;
        assert_eq!(a.status, 302, "the consent did not redirect: {}", a.body);
        assert!(
            a.location.starts_with(REDIRECT_URI) && a.location.contains("code="),
            "the consent redirect carries no code: {}",
            a.location
        );
    }

    /// `POST /oauth/clients/{id}/label` with whatever credential the caller passes.
    async fn rename_with(
        &self,
        client_id: &str,
        body: Value,
        bearer: Option<&str>,
        cookie: Option<&str>,
    ) -> (u16, Value) {
        let mut req =
            http().post(format!("{}/oauth/clients/{client_id}/label", self.base)).json(&body);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let text = res.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// The owner renames a client through the bearer route; `None` clears the name.
    async fn rename(&self, client_id: &str, label: Option<&str>) -> Value {
        let (status, body) =
            self.rename_with(client_id, json!({ "label": label }), Some(OWNER_TOKEN), None).await;
        assert_eq!(status, 200, "the rename answered {status}: {body}");
        body
    }

    /// `GET /oauth/clients?include_revoked=1`, one entry per client keyed by id.
    async fn listing(&self) -> std::collections::HashMap<String, Value> {
        let res = http()
            .get(format!("{}/oauth/clients?include_revoked=1", self.base))
            .bearer_auth(OWNER_TOKEN)
            .send()
            .await
            .unwrap();
        assert!(res.status().is_success(), "the listing answered {}", res.status());
        let body: Value = res.json().await.unwrap();
        body["clients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["client_id"].as_str().unwrap().to_string(), c.clone()))
            .collect()
    }

    async fn console(&self) -> String {
        let res = http()
            .get(format!("{}/console/clients", self.base))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap();
        let a = answer(res).await;
        assert_eq!(a.status, 200, "the clients page did not render: {}", a.body);
        a.body
    }

    /// The owner types `label` into one card's rename form and presses Save.
    async fn console_rename(&self, client_id: &str, label: &str) -> Answer {
        let html = self.console().await;
        let token = card_label_token(&html, client_id);
        let res = http()
            .post(format!("{}/console/clients/{client_id}/label", self.base))
            .header("cookie", &self.cookie)
            .form(&[("csrf", token.as_str()), ("label", label)])
            .send()
            .await
            .unwrap();
        answer(res).await
    }

    async fn owner_label(&self, client_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT owner_label FROM oauth_client WHERE client_id = $1")
            .bind(client_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn consented(&self, client_id: &str) -> bool {
        sqlx::query_scalar("SELECT consented_at IS NOT NULL FROM oauth_client WHERE client_id = $1")
            .bind(client_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn codes(&self, client_id: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM oauth_code WHERE client_id = $1")
            .bind(client_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// The whole client row except the one column a rename may touch.
    async fn row_without_label(&self, client_id: &str) -> Value {
        sqlx::query_scalar(
            "SELECT to_jsonb(c) - 'owner_label' FROM oauth_client c WHERE client_id = $1",
        )
        .bind(client_id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn token_rows(&self, client_id: &str) -> Value {
        sqlx::query_scalar(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY token_hash), '[]'::jsonb)
               FROM oauth_token t WHERE client_id = $1",
        )
        .bind(client_id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// One `tools/call`, with a fresh `initialize` first, as the owner.
    async fn call(&self, name: &str, args: Value) -> Value {
        let rpc = |method: &'static str, params: Value| async move {
            let res = http()
                .post(format!("{}/mcp", self.base))
                .bearer_auth(OWNER_TOKEN)
                .header("accept", "application/json, text/event-stream")
                .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
                .send()
                .await
                .unwrap();
            let status = res.status();
            let text = res.text().await.unwrap();
            assert!(status.is_success(), "{method} answered {status}: {text}");
            parse_body(&text)
        };
        let init = rpc(
            "initialize",
            json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": { "name": "client-label-test", "version": "0.1.0" },
            }),
        )
        .await;
        assert!(init.get("error").is_none(), "initialize failed: {init}");
        let body = rpc("tools/call", json!({ "name": name, "arguments": args })).await;
        assert!(body.get("error").is_none(), "{name} failed: {body}");
        body["result"].clone()
    }
}

/// The text of an HTML attribute that starts right after `key`, unescaped the way `escape` writes it.
fn attribute_after(html: &str, key: &str) -> Option<String> {
    let at = html.find(key)? + key.len();
    let end = html[at..].find('"')?;
    Some(unescape(&html[at..at + end]))
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The token minted for one card's rename form. Every card carries several forms, each with its own
/// token, so the search starts at this card's label action.
fn card_label_token(html: &str, client_id: &str) -> String {
    let form = format!("action=\"/console/clients/{client_id}/label\"");
    let at = html.find(&form).unwrap_or_else(|| panic!("no rename form for {client_id}: {html}"));
    attribute_after(&html[at..], "name=\"csrf\" value=\"").expect("the rename form carries no csrf")
}

fn parse_body(text: &str) -> Value {
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text).unwrap_or_else(|e| panic!("body {text:?}: {e}"));
    }
    let last = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .rfind(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("no JSON and no SSE frame in {text:?}"));
    serde_json::from_str(last).unwrap_or_else(|e| panic!("SSE frame {last:?}: {e}"))
}

fn digest_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|blocks| {
            blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default()
}

/// The parts of a principal a grant decides. `token_id` is left out: it fingerprints the token,
/// which this file never changes.
fn reach(p: &Principal) -> String {
    format!(
        "client={} read={:?} write={:?} registry_write={} sealed={} delete={} ingest={} history={}",
        p.client,
        p.read,
        p.write,
        p.registry_write,
        p.sealed_capable,
        p.may_delete,
        p.may_ingest,
        p.may_read_history
    )
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
            r#"[{{"client":"owner","token":"{OWNER_TOKEN}","read":[{{"namespace":"*","max":"sealed"}}],"write":[{{"namespace":"*","max":"sealed"}}],"sealedCapable":true,"registryWrite":true,"mayDelete":true,"mayReadHistory":true}},{{"client":"narrow","token":"{NARROW_TOKEN}","read":["global"],"write":["global"]}}]"#
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
    // Set on the struct rather than through the environment, which the next test in this binary
    // would inherit.
    cfg.auth.mode = AuthMode::Oauth;
    cfg.oauth.cookie_secret = COOKIE_SECRET.into();
    // The default of five a minute would refuse the fifth registration in the Hermes test, and
    // what that refusal proves belongs to the limiter's own tests.
    cfg.oauth.registrations_per_minute = 1000;
    // Tokens below are minted straight into the table with no resource indicator.
    cfg.oauth.resource_audience = ResourceAudience::Off;
    let cfg = Arc::new(cfg);

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
    // Wired the way main.rs wires it, so the digest resolves sources through the same store the
    // rename writes to.
    let repos = Repos {
        memories: memories.clone(),
        registry: Arc::new(postgres::PgRegistryRepository::new(pool.clone())),
        tool_calls: Arc::new(postgres::PgToolCallRepository::new(pool.clone())),
        sealed: Some(Arc::new(postgres::PgSealedRepository::new(pool.clone()))),
        ciphertext: Some(memories),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        oauth: sources::label_store(AuthMode::Oauth, &oauth),
    };
    let ctx = Ctx {
        cfg: Arc::clone(&cfg),
        repos: repos.clone(),
        embedder: Arc::new(HashEmbedder::new(768)),
        keys: Some(Arc::clone(&keys)),
        kek_verified,
        principal: owner(),
        invocation: Invocation::Cli,
        session_id: Some("client-label-test".into()),
    };

    let state = Arc::new(AppState {
        cleanup: Arc::new(postgres::PgCleanupRepository::new(pool.clone())),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        cfg: Arc::clone(&cfg),
        repos,
        oauth: Arc::clone(&oauth),
        ingest: Arc::new(postgres::PgIngestRepository::new(pool.clone())),
        embedder: Arc::clone(&ctx.embedder),
        degraded_embedder: false,
        keys: Some(keys),
        kek_verified,
        proposals: Vec::new(),
    });
    let authenticator = auth::create(&cfg, Some(Arc::clone(&oauth))).ok()?;
    let app: Router = lumberroom_server::http::router(state, Arc::clone(&authenticator));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr: SocketAddr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });

    let session = Sessions::from_config(&cfg).issue(Utc::now().timestamp());
    bootstrap::clear_cache();
    Some(Harness {
        ctx,
        pool,
        oauth,
        authenticator,
        base: format!("http://{addr}"),
        cookie: format!("lumberroom_owner={session}"),
        _serial: guard,
        _db: db_lock,
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

// ---- registration ------------------------------------------------------------------------------

/// A stranger sends both spellings a label could take. serde drops them, `NewOauthClient` has no
/// field to carry one, and the CHECK would refuse it on an unapproved row anyway.
#[tokio::test]
async fn registration_never_stores_a_label() {
    let h = harness_or_skip!();
    let (status, body) = h
        .register_raw(json!({
            "client_name": "Helper",
            "redirect_uris": [REDIRECT_URI],
            "token_endpoint_auth_method": "none",
            "owner_label": "Claude Desktop",
            "label": "Claude Desktop",
        }))
        .await;
    assert!((200..300).contains(&status), "registration answered {status}: {body}");
    assert!(body.get("owner_label").is_none() && body.get("label").is_none(), "{body}");
    let id = body["client_id"].as_str().unwrap();
    assert_eq!(h.owner_label(id).await, None);
    assert!(!h.listing().await[id].to_string().contains("Claude Desktop"));
}

#[tokio::test]
async fn registration_refuses_a_stamp_shaped_name() {
    let h = harness_or_skip!();
    for name in ["Codex (added 1 Sep)", "Codex (Added 1 Sep, 13:02)", "Codex (not approved)"] {
        let (status, body) =
            h.register_raw(json!({ "client_name": name, "redirect_uris": [REDIRECT_URI] })).await;
        assert_eq!(status, 400, "{name} answered {status}: {body}");
        assert_eq!(body["error"], "invalid_client_metadata", "{name}: {body}");
        assert_eq!(
            body["error_description"],
            "client_name cannot end in \"(added ...)\" or \"(not approved)\": this server adds \
             those words itself",
            "{name}: {body}"
        );
    }
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_client").fetch_one(&h.pool).await.unwrap();
    assert_eq!(stored, 0, "a refused registration left a row");

    // Only the last parenthesised group counts, so a plugin's own parenthesis still registers.
    h.register("Claude Code (plugin:lumberroom-memory:lumberroom)").await;
}

/// The backstop under every surface: a label on a client nobody approved is refused by Postgres,
/// whatever code wrote it.
#[tokio::test]
async fn the_database_refuses_a_label_on_an_unapproved_client() {
    let h = harness_or_skip!();
    let id = h.register("Helper").await;
    let err =
        sqlx::query("UPDATE oauth_client SET owner_label = 'Claude Desktop' WHERE client_id = $1")
            .bind(&id)
            .execute(&h.pool)
            .await
            .expect_err("the store accepted a label on an unapproved client");
    let code = err.as_database_error().and_then(|e| e.code()).map(|c| c.to_string());
    assert_eq!(code.as_deref(), Some("23514"), "{err}");
    assert_eq!(h.owner_label(&id).await, None);
}

// ---- consent -----------------------------------------------------------------------------------

#[tokio::test]
async fn consent_with_a_changed_name_stores_it() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    h.approve(&id, Field::Typed("  Work laptop ")).await;
    assert_eq!(h.owner_label(&id).await.as_deref(), Some("Work laptop"));
    assert_eq!(h.listing().await[&id]["label"], "Work laptop");
}

/// Review focus 2: four Hermes registrations, Allow pressed four times without touching the field.
/// Equal-to-registered means "chose nothing", so the rows stay unnamed and keep their dates.
#[tokio::test]
async fn consent_with_the_prefilled_name_stores_nothing() {
    let h = harness_or_skip!();
    let name = "Hermes Agent (lumberroom)";
    let mut ids = Vec::new();
    for _ in 0..4 {
        let id = h.register(name).await;
        let page = h.consent_page(&id).await;
        assert_eq!(page.prefilled.as_deref(), Some(name), "the field opens on the registered name");
        h.approve(&id, Field::AsPrefilled).await;
        ids.push(id);
    }
    let listing = h.listing().await;
    let mut labels = std::collections::HashSet::new();
    for id in &ids {
        assert_eq!(h.owner_label(id).await, None, "an untouched field stored a name on {id}");
        let label = listing[id]["label"].as_str().unwrap().to_string();
        assert!(label.starts_with("Hermes Agent (lumberroom) (added "), "{label}");
        labels.insert(label);
    }
    assert_eq!(labels.len(), 4, "the four still read apart: {labels:?}");
}

/// Review focus 5: a page rendered before the deploy has no `label` field, and its Allow must not
/// clear a name the owner set since.
#[tokio::test]
async fn consent_without_the_field_keeps_an_existing_label() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    h.approve(&id, Field::Typed("Desk")).await;
    h.approve(&id, Field::Absent).await;
    assert_eq!(h.owner_label(&id).await.as_deref(), Some("Desk"));
}

/// Review focus 3: 70 Devanagari letters are 210 bytes. The name is checked before the grant is
/// written, so the refusal leaves the client exactly as unapproved as it was.
#[tokio::test]
async fn consent_with_an_overlong_name_grants_nothing() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    let long = "\u{0915}".repeat(70);
    assert_eq!(long.len(), 210);
    let a = h.allow(&id, Field::Typed(&long)).await;
    assert_eq!(a.status, 400, "{}", a.body);
    assert!(a.body.contains("name not saved"), "{}", a.body);
    assert!(a.body.contains(TOO_LONG), "{}", a.body);
    assert!(a.body.contains("Nothing was granted. Go back and change it."), "{}", a.body);
    assert!(a.location.is_empty(), "a refused consent redirected: {}", a.location);
    assert!(!h.consented(&id).await, "a refused name still granted access");
    assert_eq!(h.codes(&id).await, 0, "a refused name still minted a code");
    assert_eq!(h.owner_label(&id).await, None);
}

// ---- the bearer route --------------------------------------------------------------------------

/// Review focus 4. A rename is cosmetic: every other column of the row, every token row and what
/// the client's live token may reach are the same afterwards.
#[tokio::test]
async fn a_rename_changes_no_other_column() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    h.approve(&id, Field::AsPrefilled).await;
    let token = uuid::Uuid::new_v4().simple().to_string();
    h.oauth
        .insert_token(NewAccessToken {
            token_hash: hash_token(&token),
            client_id: id.clone(),
            scope: "memory.read memory.write".into(),
            resource: None,
            family_id: uuid::Uuid::new_v4(),
            expires_at: Utc::now() + Duration::hours(1),
        })
        .await
        .unwrap();
    let bearer = format!("Bearer {token}");
    let before_reach = reach(&h.authenticator.authenticate(Some(&bearer)).await.unwrap());
    // Authenticating touches `last_used_at` off the request path. Waiting for that write keeps it
    // from landing between the two snapshots and reading as something the rename did.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let row_before = h.row_without_label(&id).await;
    let tokens_before = h.token_rows(&id).await;
    let answer = h.rename(&id, Some("Build box")).await;
    assert_eq!(answer["owner_label"], "Build box");
    assert_eq!(h.row_without_label(&id).await, row_before, "a rename moved another column");
    assert_eq!(h.token_rows(&id).await, tokens_before, "a rename touched a token row");
    assert_eq!(h.owner_label(&id).await.as_deref(), Some("Build box"));

    let after = h.authenticator.authenticate(Some(&bearer)).await.unwrap();
    assert_eq!(reach(&after), before_reach, "a rename changed what the token reaches");
}

#[tokio::test]
async fn the_label_route_refuses_a_cookie_a_missing_token_and_a_narrow_token() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    h.approve(&id, Field::AsPrefilled).await;
    let body = json!({ "label": "Stolen" });

    let (status, answer) = h.rename_with(&id, body.clone(), None, Some(&h.cookie)).await;
    assert_eq!(status, 401, "the owner's session cookie renamed a client: {answer}");
    assert_eq!(answer, json!({ "error": "unauthorized" }));

    let (status, answer) = h.rename_with(&id, body.clone(), None, None).await;
    assert_eq!(status, 401, "{answer}");

    let (status, answer) = h.rename_with(&id, body.clone(), Some(NARROW_TOKEN), None).await;
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer["error"], "forbidden");
    assert_eq!(answer["detail"], "client narrow may not rename OAuth clients");

    assert_eq!(h.owner_label(&id).await, None, "a refused rename wrote a name");
}

#[tokio::test]
async fn the_label_route_answers_404_for_unknown_and_unapproved_clients() {
    let h = harness_or_skip!();
    let pending = h.register("Codex").await;
    for id in [pending.as_str(), "no-such-client"] {
        let (status, answer) =
            h.rename_with(id, json!({ "label": "Desk" }), Some(OWNER_TOKEN), None).await;
        assert_eq!(status, 404, "{id}: {answer}");
        assert_eq!(
            answer,
            json!({ "error": "not_found", "detail": "there is no approved client with that id" })
        );
    }
    assert_eq!(h.owner_label(&pending).await, None);
    assert!(!h.consented(&pending).await, "a rename approved a client");
}

/// A revoked client keeps the rows it wrote, so the owner still needs to tell it apart.
#[tokio::test]
async fn a_revoked_client_can_be_renamed() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    h.approve(&id, Field::AsPrefilled).await;
    assert!(h.oauth.revoke_client(&id).await.unwrap());
    let answer = h.rename(&id, Some("Old laptop")).await;
    assert_eq!(answer["label"], "Old laptop");
    assert_eq!(h.owner_label(&id).await.as_deref(), Some("Old laptop"));
    assert_eq!(h.listing().await[&id]["label"], "Old laptop");
}

#[tokio::test]
async fn clearing_falls_back_to_the_registered_name() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    h.approve(&id, Field::Typed("Desk")).await;
    let cleared = h.rename(&id, None).await;
    assert_eq!(
        cleared,
        json!({ "client_id": id, "client_name": "Codex", "owner_label": null, "label": "Codex" })
    );
    assert_eq!(h.owner_label(&id).await, None);
    let entry = &h.listing().await[&id];
    assert!(entry["owner_label"].is_null(), "{entry}");
    assert_eq!(entry["label"], "Codex");
}

// ---- listing and console -----------------------------------------------------------------------

/// Two approved clients registered as "Codex" read with dates. The owner names one from the console
/// and both lose their dates: the listing prints what the resolver prints.
#[tokio::test]
async fn the_listing_carries_owner_label_and_the_resolved_label() {
    let h = harness_or_skip!();
    let kept = h.register("Codex").await;
    let named = h.register("Codex").await;
    h.approve(&kept, Field::AsPrefilled).await;
    h.approve(&named, Field::AsPrefilled).await;
    let before = h.listing().await;
    for id in [&kept, &named] {
        let label = before[id]["label"].as_str().unwrap();
        assert!(label.starts_with("Codex (added "), "{label}");
        assert!(before[id]["owner_label"].is_null());
    }

    let saved = h.console_rename(&named, "Desk").await;
    assert_eq!(saved.status, 303, "{}", saved.body);
    assert_eq!(saved.location, "/console/clients?done=renamed");

    let after = h.listing().await;
    assert_eq!(after[&named]["owner_label"], "Desk");
    assert_eq!(after[&named]["label"], "Desk");
    assert_eq!(after[&named]["client_name"], "Codex");
    assert!(after[&kept]["owner_label"].is_null());
    assert_eq!(after[&kept]["label"], "Codex");

    let page = h.console().await;
    assert!(page.contains("<span class=\"cli-name\">Desk</span>"), "{page}");
    assert!(page.contains("registered as Codex"), "{page}");
}

#[tokio::test]
async fn a_hostile_label_is_escaped_on_the_consent_page_and_the_console() {
    let h = harness_or_skip!();
    let id = h.register("Codex").await;
    h.approve(&id, Field::AsPrefilled).await;
    let hostile = "\"><script>alert(1)</script>";
    h.rename(&id, Some(hostile)).await;
    let escaped = "&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;";

    let consent = h.consent_page(&id).await;
    assert!(!consent.html.contains("<script>alert(1)"), "{}", consent.html);
    assert!(consent.html.contains(&format!("value=\"{escaped}\"")), "{}", consent.html);
    assert_eq!(consent.prefilled.as_deref(), Some(hostile));
    // The headline judges what registered, never what the owner called it.
    assert!(consent.html.contains("<h1>Give <b>Codex</b> access"), "{}", consent.html);

    let console = h.console().await;
    assert!(!console.contains("<script>alert(1)"), "{console}");
    assert!(console.contains(&format!("<span class=\"cli-name\">{escaped}</span>")), "{console}");
    assert!(console.contains(&format!("value=\"{escaped}\"")), "{console}");
}

// ---- the digest --------------------------------------------------------------------------------

/// Review focus 6. `context_bootstrap` caches its rendered digest, source names included, for
/// thirty seconds. A rename inside that window has to show on the next call.
#[tokio::test]
async fn the_digest_prints_a_new_name_inside_the_cache_window() {
    let h = harness_or_skip!();
    assert!(h.ctx.cfg.bootstrap.cache_ms >= 5_000, "the cache window is what this test is about");
    let id = h.register("Codex").await;
    h.approve(&id, Field::AsPrefilled).await;
    let mut as_codex = h.ctx.clone();
    as_codex.principal = Principal { client: id.clone(), ..owner() };
    write::run(
        &as_codex,
        "the codex build box runs nightly at two",
        "global",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let first = digest_text(&h.call("context_bootstrap", json!({})).await);
    assert!(first.contains("via Codex"), "{first}");

    h.rename(&id, Some("Build box")).await;
    let second = digest_text(&h.call("context_bootstrap", json!({})).await);
    assert!(second.contains("via Build box"), "the digest kept the old name: {second}");
    assert!(!second.contains("via Codex"), "{second}");
}
