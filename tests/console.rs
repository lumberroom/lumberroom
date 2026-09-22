//! The console, over HTTP. Same real Postgres and hash embedder as the rest of the suite, in the
//! same `lumberroom_rust_test` database, skipped when no database is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom cargo test --test console
//!
//! Every request here goes through `http::router`, not `console::router`, so the mount itself is
//! under test: a console route the composition root failed to merge answers 404 and every case
//! below fails.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use lumberroom_server::adapters::auth;
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::authserver::session::Sessions;
use lumberroom_server::config::{self, AuthMode, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::{NamespaceGrant, SensitivityDefaults};
use lumberroom_server::domain::types::{Invocation, Principal, Sensitivity};
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::ingest::{IngestRepository, NewProposal, ProposalSource};
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::{bootstrap, ingest, write, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";

/// 32 characters, the length the cookie signer wants. Fixed so a failure leaves a cookie the next
/// run can still open.
const COOKIE_SECRET: &str = "console-test-cookie-secret-32ch!";

/// Every test here truncates the shared test database, so they serialise themselves rather than
/// relying on `--test-threads=1` being remembered. Cargo runs one test binary at a time, so this
/// mutex and the one in `integration.rs` do not have to know about each other.
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
    ingest: Arc<dyn IngestRepository>,
    pool: PgPool,
    base: String,
    cookie: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    /// Held for the whole test. The mutex above serialises this binary's own threads; this is what
    /// keeps the other five binaries out of the same database.
    _db: common::DbGuard,
}

impl Harness {
    /// A GET with the owner session attached, returning the status and the body.
    async fn get(&self, path: &str) -> (u16, String) {
        self.request(path, Some(&self.cookie)).await
    }

    /// A GET with no cookie, which is what a stranger sends.
    async fn get_anonymous(&self, path: &str) -> (u16, String) {
        self.request(path, None).await
    }

    /// A form POST, which is the only shape the console's write routes accept.
    async fn post(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> (u16, String) {
        let client =
            reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let mut req = client.post(format!("{}{path}", self.base)).form(fields);
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let location =
            res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = res.text().await.unwrap();
        (status, format!("{body}\n<!-- location: {location} -->"))
    }

    async fn request(&self, path: &str, cookie: Option<&str>) -> (u16, String) {
        let client = reqwest::Client::builder()
            // A redirect the client follows is a redirect the test cannot see, and the sign-in
            // bounce is one of the things under test.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let mut req = client.get(format!("{}{path}", self.base));
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let location =
            res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = res.text().await.unwrap();
        // A redirect carries no body, so the Location is what an assertion has to read. Appending
        // it keeps one accessor for both cases.
        (status, format!("{body}\n<!-- location: {location} -->"))
    }
}

/// Builds the store, the config, the app state and a live server on a loopback port, then returns
/// a session cookie minted by the same signer the console verifies with.
///
/// The password form is skipped on purpose. Argon2 is tuned to cost real time on every attempt,
/// and what these tests are about is what a signed-in reader sees, not how the signing happens.
/// `session.rs` already pins the cookie's own behaviour.
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
        // DDL takes no bind parameter, so this one statement is built as a string. Audited:
        // TEST_DB is a compile-time constant with no external input.
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

    let mut cfg: Config = step!("loading the config", config::load());
    // The console lives in oauth mode alone, and a `personal:*` write has to land private for the
    // decryption path to be exercised at all. Both are set on the struct rather than through the
    // process environment, which the next test in this binary would inherit.
    cfg.auth.mode = AuthMode::Oauth;
    cfg.oauth.cookie_secret = COOKIE_SECRET.into();
    cfg.policy.defaults = SensitivityDefaults::seeded();
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

    // Composed the way main.rs composes it: one memory repository handed up as both the port the
    // services read through and the ciphertext reader they decrypt through.
    let memories = Arc::new(postgres::PgMemoryRepository::new(pool.clone()));
    let oauth: Arc<dyn OauthStore> = Arc::new(postgres::PgOauthStore::new(pool.clone()));
    let repos = Repos {
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        memories: memories.clone(),
        registry: Arc::new(postgres::PgRegistryRepository::new(pool.clone())),
        tool_calls: Arc::new(postgres::PgToolCallRepository::new(pool.clone())),
        sealed: Some(Arc::new(postgres::PgSealedRepository::new(pool.clone()))),
        ciphertext: Some(memories),
    };
    let embedder = Arc::new(HashEmbedder::new(768));

    let ctx = Ctx {
        cfg: Arc::clone(&cfg),
        repos: repos.clone(),
        embedder: embedder.clone(),
        keys: Some(Arc::clone(&keys)),
        kek_verified,
        principal: owner_like(),
        invocation: Invocation::Cli,
        session_id: Some("console-test".into()),
    };

    let ingest: Arc<dyn IngestRepository> =
        Arc::new(postgres::PgIngestRepository::new(pool.clone()));
    let state = Arc::new(AppState {
        cleanup: Arc::new(postgres::PgCleanupRepository::new(pool.clone())),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        cfg: Arc::clone(&cfg),
        repos,
        oauth: Arc::clone(&oauth),
        ingest: Arc::clone(&ingest),
        embedder,
        degraded_embedder: false,
        keys: Some(keys),
        kek_verified,
        proposals: Vec::new(),
    });
    let authenticator = auth::create(&cfg, Some(oauth)).ok()?;
    let app: Router = lumberroom_server::http::router(state, authenticator);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr: SocketAddr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });

    let value = Sessions::from_config(&cfg).issue(chrono::Utc::now().timestamp());
    bootstrap::clear_cache();
    Some(Harness {
        ctx,
        ingest,
        pool: pool.clone(),
        base: format!("http://{addr}"),
        cookie: format!("lumberroom_owner={value}"),
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

fn owner_like() -> Principal {
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

/// A string that cannot appear by accident, so "absent" means the content is absent rather than
/// that the assertion looked in the wrong place.
fn nonce(label: &str) -> String {
    format!("zqxnonce{label}zqx")
}

#[tokio::test]
async fn a_stranger_is_sent_to_the_sign_in_form_rather_than_shown_a_page() {
    let h = harness_or_skip!();
    for path in [
        "/console",
        "/console/reading",
        "/console/registry",
        "/console/search?q=anything",
        "/console/queue",
    ] {
        let (status, body) = h.get_anonymous(path).await;
        assert_eq!(status, 303, "{path} answered {status} to a request with no session");
        assert!(
            body.contains("/console/login?next="),
            "{path} has to send the reader to the form and remember where they were going: {body}"
        );
    }
}

#[tokio::test]
async fn the_console_is_mounted_and_answers_every_read_route() {
    let h = harness_or_skip!();
    let fact = write::run(
        &h.ctx,
        "the office wifi is on the second router",
        "project:lumberroom",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    for path in [
        "/console/reading".to_string(),
        "/console/namespace?ns=project:lumberroom".to_string(),
        format!("/console/fact/{}", fact.id),
        "/console/search?q=office%20wifi".to_string(),
        "/console/registry".to_string(),
        "/console/queue".to_string(),
    ] {
        let (status, body) = h.get(&path).await;
        assert_eq!(status, 200, "{path} answered {status}");
        assert!(body.contains("<!doctype html>"), "{path} returned something other than a page");
    }

    // The placeholder says the queue is absent rather than drawing an empty one.
    let (_, queue) = h.get("/console/queue").await;
    assert!(
        queue.to_lowercase().contains("ingestion"),
        "the queue page has to say what it waits on"
    );
}

#[tokio::test]
async fn a_private_fact_is_decrypted_for_the_owner_and_marked_private() {
    let h = harness_or_skip!();
    let secret = nonce("consoleprivate");
    // No explicit level. The seeded namespace rule is what classifies it, which is the product
    // claim: nobody classifies anything in the normal case.
    let w = write::run(
        &h.ctx,
        &format!("the retainer is {secret}"),
        "personal:finance",
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
        "the seeded rule has to fire or this proves nothing"
    );

    let (status, body) = h.get(&format!("/console/fact/{}", w.id)).await;
    assert_eq!(status, 200);
    assert!(
        body.contains(&secret),
        "the owner reads the plaintext, or the console is a locked box"
    );
    assert!(body.contains("Live entry, private"), "and the page has to say so");

    let (status, reading) = h.get("/console/reading").await;
    assert_eq!(status, 200);
    assert!(
        reading.contains(&secret),
        "a private row the owner may read belongs in the reading view"
    );
    assert!(reading.contains("class=\"e private\""), "and it is marked there too");
}

#[tokio::test]
async fn a_sealed_item_is_counted_and_its_bytes_reach_no_page() {
    use base64::Engine as _;
    let h = harness_or_skip!();
    let blob = nonce("consolesealed");
    let b64 = base64::engine::general_purpose::STANDARD.encode(blob.as_bytes());
    lumberroom_server::services::sealed::put(
        &h.ctx,
        "credentials:aws",
        "root-key",
        &b64,
        "aes-256-gcm/client-v1",
    )
    .await
    .unwrap();

    let (status, body) = h.get("/console/reading").await;
    assert_eq!(status, 200);
    assert!(body.contains("credentials:aws"), "the namespace is named: {body}");
    assert!(body.contains("Sealed"), "the page says what the block is");
    assert!(!body.contains(&b64), "the ciphertext must never reach a page");
    assert!(!body.contains(&blob), "and neither may the plaintext behind it");
    assert!(!body.contains("root-key"), "the key of a sealed item is not shown either");
}

/// The security test. A model wrote some of what is stored here, and a prompt-injection payload in
/// a fact has to render as text. Both directions are asserted: the raw tag absent, the escaped one
/// present, because a page that dropped the content entirely would pass the first check alone.
#[tokio::test]
async fn hostile_content_in_a_fact_renders_as_text_and_not_as_markup() {
    let h = harness_or_skip!();
    let marker = nonce("consolexss");
    let payload = format!(
        "<script>alert('{marker}')</script> and an <img src=x onerror=\"steal()\"> \
         plus a \"quoted\" & ampersand"
    );
    let w =
        write::run(&h.ctx, &payload, "project:lumberroom", None, None, None, None).await.unwrap();

    for path in [
        format!("/console/fact/{}", w.id),
        "/console/reading".to_string(),
        "/console/namespace?ns=project:lumberroom".to_string(),
    ] {
        let (status, body) = h.get(&path).await;
        assert_eq!(status, 200, "{path} answered {status}");
        assert!(body.contains(&marker), "{path} dropped the content instead of escaping it");
        assert!(!body.contains("<script>alert"), "{path} rendered a script tag as markup");
        assert!(
            !body.contains("onerror=\"steal()\""),
            "{path} rendered an event handler as markup"
        );
        assert!(body.contains("&lt;script&gt;"), "{path} has to show the tag as text");
        assert!(body.contains("&amp;"), "{path} has to escape the ampersand");
    }

    // Search runs the same escape over whatever it finds. What it finds depends on the embedder
    // and on the lexical index, so the assertion here is the one that holds either way: nothing
    // this page prints becomes markup.
    let (status, found) = h.get(&format!("/console/search?q={marker}")).await;
    assert_eq!(status, 200);
    assert!(!found.contains("<script>alert"), "search rendered a script tag as markup");
    assert!(!found.contains("onerror=\"steal()\""), "search rendered an event handler as markup");
}

/// One proposal in the queue, returned by id.
async fn queued(h: &Harness, content: &str) -> uuid::Uuid {
    let run =
        ingest::open_run(&h.ctx, h.ingest.as_ref(), "test", serde_json::json!({ "roots": [] }))
            .await
            .unwrap();
    let upsert = h
        .ingest
        .insert_proposal(
            &h.ctx.cfg.tenant_id,
            NewProposal {
                fingerprint: ingest::fingerprint(&h.ctx, content).await.unwrap(),
                content: content.into(),
                namespace: "project:lumberroom".into(),
                tags: vec![],
                supersedes: None,
                speaker: "owner_typed".into(),
                quote: None,
                auto: false,
                extractor: "test".into(),
                posted_by: "test".into(),
                source: ProposalSource {
                    source_key: format!("/p/a.jsonl#{content}"),
                    file_path: "/p/a.jsonl".into(),
                    session_id: None,
                    is_sidechain: false,
                    entry_uuid: Some("e1".into()),
                    speaker: "owner_typed".into(),
                    observed_at: None,
                    run_id: run,
                },
            },
        )
        .await
        .unwrap();
    upsert.proposal().id
}

/// The hidden token the queue page minted for one row and one action.
fn token_for(html: &str, id: uuid::Uuid, action: &str) -> String {
    let form = format!("/console/queue/{id}/{action}");
    let at = html.find(&form).unwrap_or_else(|| panic!("no {action} form for {id} on the page"));
    let rest = &html[at..];
    let key = "name=\"csrf\" value=\"";
    let start = rest.find(key).expect("the form carries no csrf field") + key.len();
    let end = rest[start..].find('"').unwrap();
    rest[start..start + end].to_string()
}

async fn state_of(h: &Harness, id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM ingest_proposal WHERE id = $1")
        .bind(id)
        .fetch_one(&h.pool)
        .await
        .unwrap()
}

/// The owner's click on Approve writes into a namespace the poster may have no grant on, so the row
/// has to distinguish what the poster said about itself from what the server recorded. `speaker` and
/// `auto` are the poster's own words, and the auto gate compares two fields the same request
/// supplied.
#[tokio::test]
async fn the_queue_page_separates_what_the_poster_claimed_from_the_credential_it_arrived_on() {
    let h = harness_or_skip!();
    queued(&h, &format!("the deploy box runs {}", nonce("provenance"))).await;

    let (status, html) = h.get("/console/queue").await;
    assert_eq!(status, 200);
    assert!(html.contains("claimed: owner_typed"), "the speaker is the poster's own word");
    assert!(html.contains("posted by test"), "the credential is what the server knows: {html}");
    assert!(
        html.contains("what the posting client said about itself"),
        "and the page says which is which"
    );
}

#[tokio::test]
async fn the_owner_approves_a_proposal_from_the_page_and_it_reaches_the_store() {
    let h = harness_or_skip!();
    let id = queued(&h, &format!("the deploy box runs {}", nonce("approve"))).await;

    let (status, html) = h.get("/console/queue").await;
    assert_eq!(status, 200);
    let csrf = token_for(&html, id, "approve");

    let (status, body) =
        h.post(&format!("/console/queue/{id}/approve"), &[("csrf", &csrf)], Some(&h.cookie)).await;
    assert_eq!(status, 303, "an approval answers with a redirect so a refresh does not resubmit");
    assert!(body.contains("/console/queue?done="), "the outcome travels in the redirect: {body}");
    assert_eq!(state_of(&h, id).await, "written");

    let memory_id: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT memory_id FROM ingest_proposal WHERE id = $1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    let content: String = sqlx::query_scalar("SELECT content FROM memory WHERE id = $1")
        .bind(memory_id.expect("an approved proposal carries the row it wrote"))
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert!(content.contains(&nonce("approve")));
}

#[tokio::test]
async fn a_token_minted_for_one_row_cannot_decide_another() {
    let h = harness_or_skip!();
    let mine = queued(&h, &format!("the first fact {}", nonce("mine"))).await;
    let theirs = queued(&h, &format!("the second fact {}", nonce("theirs"))).await;

    let (_, html) = h.get("/console/queue").await;
    let csrf = token_for(&html, mine, "approve");

    let (status, _) = h
        .post(&format!("/console/queue/{theirs}/approve"), &[("csrf", &csrf)], Some(&h.cookie))
        .await;
    assert_eq!(status, 403, "a token binds to the row it was minted for");
    assert_eq!(state_of(&h, theirs).await, "proposed", "the refused row did not move");
    assert_eq!(state_of(&h, mine).await, "proposed", "and neither did the one it was minted for");
}

#[tokio::test]
async fn a_token_minted_to_approve_cannot_reject() {
    let h = harness_or_skip!();
    let id = queued(&h, &format!("a fact worth keeping {}", nonce("action"))).await;

    let (_, html) = h.get("/console/queue").await;
    let csrf = token_for(&html, id, "approve");

    let (status, _) =
        h.post(&format!("/console/queue/{id}/reject"), &[("csrf", &csrf)], Some(&h.cookie)).await;
    assert_eq!(status, 403);
    assert_eq!(state_of(&h, id).await, "proposed");
}

#[tokio::test]
async fn a_rejection_from_the_page_is_reversible_from_the_page() {
    let h = harness_or_skip!();
    let id = queued(&h, &format!("a fact to take back {}", nonce("reject"))).await;

    let (_, html) = h.get("/console/queue").await;
    let (status, _) = h
        .post(
            &format!("/console/queue/{id}/reject"),
            &[("csrf", &token_for(&html, id, "reject"))],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303);
    assert_eq!(state_of(&h, id).await, "rejected");

    let (_, html) = h.get("/console/queue").await;
    let (status, _) = h
        .post(
            &format!("/console/queue/{id}/unreject"),
            &[("csrf", &token_for(&html, id, "unreject"))],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303);
    assert_eq!(state_of(&h, id).await, "proposed", "a mistyped rejection is recoverable");
}

#[tokio::test]
async fn a_stranger_cannot_decide_anything_even_holding_a_token() {
    let h = harness_or_skip!();
    let id = queued(&h, &format!("a fact a stranger wants {}", nonce("stranger"))).await;
    let (_, html) = h.get("/console/queue").await;
    let csrf = token_for(&html, id, "approve");

    for action in ["approve", "reject", "unreject"] {
        let (status, body) =
            h.post(&format!("/console/queue/{id}/{action}"), &[("csrf", &csrf)], None).await;
        assert_eq!(status, 303, "{action} answered {status} to a request with no session");
        assert!(body.contains("/console/login?next="), "{action} has to send them to the form");
    }
    assert_eq!(state_of(&h, id).await, "proposed");
}

#[tokio::test]
async fn a_decision_with_no_token_changes_nothing() {
    let h = harness_or_skip!();
    let id = queued(&h, &format!("a fact posted bare {}", nonce("notoken"))).await;

    let (status, _) = h.post(&format!("/console/queue/{id}/approve"), &[], Some(&h.cookie)).await;
    assert_eq!(status, 403);
    assert_eq!(state_of(&h, id).await, "proposed");
}

// -- aliases -------------------------------------------------------------------------------------

/// The hidden token a form on the aliases page was minted with.
///
/// Found from the form's action rather than from the first token on the page, because that page
/// draws a forget form per recorded name and one record form under them all.
fn alias_token(html: &str, action: &str) -> String {
    let at = html
        .find(&format!("action=\"{action}\""))
        .unwrap_or_else(|| panic!("no form posting to {action} on the page: {html}"));
    let rest = &html[at..];
    let key = "name=\"csrf\" value=\"";
    let start = rest.find(key).expect("the form carries no csrf field") + key.len();
    let end = rest[start..].find('"').unwrap();
    rest[start..start + end].to_string()
}

/// What the store holds for one name, read straight out of the table the page writes to.
async fn stored_alias(h: &Harness, namespace: &str, alias: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT canonical FROM entity_alias
          WHERE tenant_id = $1 AND namespace = $2 AND alias = $3",
    )
    .bind(&h.ctx.cfg.tenant_id)
    .bind(namespace)
    .bind(alias)
    .fetch_optional(&h.pool)
    .await
    .unwrap()
}

async fn alias_count(h: &Harness) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM entity_alias").fetch_one(&h.pool).await.unwrap()
}

/// Sign in, read the form, post it, and find the row.
///
/// Every step goes over HTTP through `http::router`, so the mount, the guard, the token and the
/// service all sit inside the assertion. Recorded over HTTP and visible nowhere is the state this
/// page was built to end.
#[tokio::test]
async fn the_owner_records_an_alias_from_the_page_and_it_reaches_the_store() {
    let h = harness_or_skip!();

    let (status, html) = h.get("/console/aliases").await;
    assert_eq!(status, 200);
    assert!(html.contains("No name here answers to another."), "the empty page says so: {html}");
    let csrf = alias_token(&html, "/console/aliases/record");

    let (status, body) = h
        .post(
            "/console/aliases/record",
            &[
                ("csrf", csrf.as_str()),
                ("namespace", "project:lumen"),
                // Mixed case on purpose. The store folds a name on the way in, and a page that
                // passed the typed spelling through would record a name no lookup ever matches.
                ("alias", "Warden"),
                ("canonical", "lumen"),
                ("since", "2026-03-01"),
                ("until", "2026-06-01"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303, "a record answers with a redirect so a refresh does not resubmit");
    assert!(body.contains("/console/aliases?done=recorded"), "the outcome travels back: {body}");

    assert_eq!(
        stored_alias(&h, "project:lumen", "warden").await.as_deref(),
        Some("lumen"),
        "the row has to be in entity_alias, lowercased"
    );

    let (period_since, period_until): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT since, until FROM entity_alias
              WHERE tenant_id = $1 AND namespace = $2 AND alias = $3",
    )
    .bind(&h.ctx.cfg.tenant_id)
    .bind("project:lumen")
    .bind("warden")
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(period_since.map(|t| t.date_naive().to_string()).as_deref(), Some("2026-03-01"));
    assert_eq!(period_until.map(|t| t.date_naive().to_string()).as_deref(), Some("2026-06-01"));

    // And the page draws what was recorded, which is the half a store-only assertion misses.
    let (status, html) = h.get("/console/aliases?done=recorded").await;
    assert_eq!(status, 200);
    assert!(html.contains("warden"), "the name is on the page: {html}");
    assert!(html.contains("lumen"));
    assert!(html.contains("2026-03-01 to 2026-06-01"), "with the period it was current for");
    assert!(html.contains("Recorded."), "and the line saying what happened");
}

/// A token binds to the act it was minted for.
///
/// Both directions, because both acts change what every later search returns and neither is
/// visible anywhere else. A forget token spendable on the record form would let a page the owner
/// left open point a name at a subject of somebody else's choosing.
#[tokio::test]
async fn an_alias_token_minted_for_one_act_cannot_perform_the_other() {
    let h = harness_or_skip!();

    // One recorded alias, so the page carries a forget form to take a token from.
    let (_, html) = h.get("/console/aliases").await;
    let record = alias_token(&html, "/console/aliases/record");
    let (status, _) = h
        .post(
            "/console/aliases/record",
            &[
                ("csrf", record.as_str()),
                ("namespace", "project:lumen"),
                ("alias", "warden"),
                ("canonical", "lumen"),
                ("since", ""),
                ("until", ""),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303);
    assert_eq!(alias_count(&h).await, 1);

    let (_, html) = h.get("/console/aliases").await;
    let forget = alias_token(&html, "/console/aliases/forget");
    let record = alias_token(&html, "/console/aliases/record");
    assert_ne!(forget, record, "one token for both acts is the whole failure this refuses");

    // The forget token, spent on the form that records.
    let (status, _) = h
        .post(
            "/console/aliases/record",
            &[
                ("csrf", forget.as_str()),
                ("namespace", "project:lumen"),
                ("alias", "quill"),
                ("canonical", "lumen"),
                ("since", ""),
                ("until", ""),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 403);
    assert!(stored_alias(&h, "project:lumen", "quill").await.is_none(), "and nothing was written");
    assert_eq!(alias_count(&h).await, 1);

    // The record token, spent on the form that forgets. `confirm` is set, so a token that was
    // accepted here would remove the row rather than draw the question.
    let (status, _) = h
        .post(
            "/console/aliases/forget",
            &[
                ("csrf", record.as_str()),
                ("namespace", "project:lumen"),
                ("alias", "warden"),
                ("confirm", "yes"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 403);
    assert_eq!(
        stored_alias(&h, "project:lumen", "warden").await.as_deref(),
        Some("lumen"),
        "the row the token could not decide is still there"
    );
    assert_eq!(alias_count(&h).await, 1);
}

/// Every class a screen renders has a rule in that screen's own stylesheet.
///
/// The audit this exists for started from a screenshot. The contradiction controls were written
/// with `cl-keeps` and `cl-keep-text`, neither had a rule, and the buttons rendered as raw browser
/// controls with the text running into them. The handler was right, the markup was right, the tests
/// asserted on that HTML and passed, and the page was unusable. A rendered class with no rule is
/// invisible to every check this suite had.
///
/// Checked against the `<style>` block in each response rather than against a constant, so it holds
/// for all three modules and keeps holding if a fourth arrives with its own sheet.
fn classes_without_rules(html: &str) -> Vec<String> {
    let Some(open) = html.find("<style>") else {
        return vec!["<no stylesheet in this page at all>".to_string()];
    };
    let Some(close) = html[open..].find("</style>") else {
        return vec!["<unterminated stylesheet>".to_string()];
    };
    let style = &html[open + 7..open + close];

    let mut missing: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("class=\"") {
        rest = &rest[at + 7..];
        let Some(end) = rest.find('"') else { break };
        for class in rest[..end].split_whitespace() {
            let rule = format!(".{class}");
            let defined = style.match_indices(&rule).any(|(i, _)| {
                style[i + rule.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_')
            });
            if !defined && !missing.iter().any(|m| m == class) {
                missing.push(class.to_string());
            }
        }
        rest = &rest[end..];
    }
    missing
}

#[tokio::test]
async fn every_screen_answers_and_carries_its_own_chrome() {
    let h = harness_or_skip!();
    let fact = write::run(
        &h.ctx,
        "the audit walks every screen the console registers",
        "project:lumberroom",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let fact_path = format!("/console/fact/{}", fact.id);

    // Every GET the router registers. Kept as a list rather than derived, so a route added without
    // a line here is a route nobody drove.
    // `/console` is a redirect to arrivals rather than a screen of its own, so it is checked for
    // where it sends you and left out of the render loop. `/console/logo.svg` is an asset and not a
    // screen; `the_mark_is_served_to_a_reader_with_no_session` drives it.
    let (status, body) = h.get("/console").await;
    assert!(
        status == 303 && body.contains("/console/reading"),
        "/console should send the owner to arrivals, answered {status}: {body}"
    );

    let screens: Vec<(&str, &str)> = vec![
        ("/console/reading", "arrivals"),
        ("/console/namespace?ns=project:lumberroom", "one namespace"),
        (&fact_path, "one fact"),
        ("/console/search", "search"),
        ("/console/write", "compose"),
        ("/console/registry", "the registry"),
        ("/console/queue", "the ingest queue"),
        ("/console/cleanup", "the cleanup queue"),
        ("/console/aliases", "aliases"),
        ("/console/clients", "clients"),
    ];

    // A namespace page is only ever reached from a link that names one. A bare visit is a 404 by
    // design; what matters is that it leaves the reader somewhere to go rather than at a dead end.
    let (status, html) = h.get("/console/namespace").await;
    assert_eq!(status, 404, "a namespace page with no namespace should say so");
    assert!(
        html.contains("/console/reading") || html.contains("/console\""),
        "the not-found notice strands the reader with no way back: {html}"
    );

    for (path, what) in &screens {
        let (status, html) = h.get(path).await;
        assert_eq!(status, 200, "{what} ({path}) answered {status}");

        // A full page rather than a fragment or an error body. Every screen renders the nav, so its
        // absence means the handler bailed before the chrome.
        assert!(
            html.contains("/console/reading") && html.contains("/console/queue"),
            "{what} ({path}) rendered without the nav, so it is not a whole page: {}",
            &html[..html.len().min(400)]
        );
        assert!(
            html.contains("/console/cleanup"),
            "{what} ({path}) has a nav missing the cleanup tab, so its copy of the tab list drifted"
        );

        let missing = classes_without_rules(&html);
        assert!(
            missing.is_empty(),
            "{what} ({path}) renders classes with no rule behind them, so whatever they lay out \
             does not: {missing:?}"
        );
    }
}

/// A stranger reaches no screen at all.
#[tokio::test]
async fn every_screen_refuses_a_request_with_no_session() {
    let h = harness_or_skip!();
    for path in [
        "/console",
        "/console/reading",
        "/console/namespace",
        "/console/search",
        "/console/write",
        "/console/registry",
        "/console/queue",
        "/console/cleanup",
        "/console/aliases",
        "/console/clients",
    ] {
        let (status, body) = h.get_anonymous(path).await;
        assert!(
            status == 303 || status == 302 || body.contains("/console/login"),
            "{path} answered {status} to a request with no session, and did not send it to login"
        );
    }
}

/// The mark is the one route here a stranger may have, and it has to stay that way.
///
/// Every page on this server links it as its favicon, and two of those pages are the sign-in form
/// and the OAuth consent screen, read by someone who holds no session. Behind the guard it answers
/// the login redirect and the icon breaks on the screens that most need to look like this product.
#[tokio::test]
async fn the_mark_is_served_to_a_reader_with_no_session() {
    let h = harness_or_skip!();
    let client =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let res = client.get(format!("{}/console/logo.svg", h.base)).send().await.unwrap();

    let status = res.status().as_u16();
    let content_type =
        res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = res.text().await.unwrap();

    assert_eq!(status, 200, "the favicon answered {status} to a request with no cookie");
    assert_eq!(content_type, "image/svg+xml", "a browser will not draw it as anything else");
    assert!(body.contains("<svg"), "the body is not the mark: {body}");
}

/// The Write screen's only button, driven.
///
/// The audit that added this found the compose page rendered, its form resolved, its classes had
/// rules, and nothing had ever pressed the button. A screen that renders is not a screen that
/// works, and this is the difference.
#[tokio::test]
async fn the_owner_writes_a_fact_from_the_page_and_it_reaches_the_store() {
    let h = harness_or_skip!();
    let (status, html) = h.get("/console/write").await;
    assert_eq!(status, 200);

    // The token the page minted for a fresh write, read out of the page the way a browser would.
    let key = "name=\"csrf\" value=\"";
    let at = html.find(key).expect("the compose form carries no csrf field") + key.len();
    let end = html[at..].find('"').unwrap();
    let csrf = &html[at..at + end];

    let content = "the console write path is exercised by a test that presses the button";
    let (status, body) = h
        .post(
            "/console/write",
            &[
                ("csrf", csrf),
                ("content", content),
                ("namespace", "project:lumberroom"),
                ("tags", "audit, console"),
                ("sensitivity", ""),
                ("occurred_at", ""),
                ("supersedes", ""),
            ],
            Some(&h.cookie),
        )
        .await;
    assert!(status == 303 || status == 200, "the write answered {status}: {body}");

    // The store, which is the only place the answer lives.
    let found: i64 = sqlx::query_scalar("SELECT count(*) FROM memory WHERE content = $1")
        .bind(content)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(found, 1, "the button rendered and wrote nothing");

    let namespace: String = sqlx::query_scalar("SELECT namespace FROM memory WHERE content = $1")
        .bind(content)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(namespace, "project:lumberroom", "the form's namespace was ignored");
}

/// The same button with a token minted for something else.
#[tokio::test]
async fn a_write_with_a_token_from_another_form_stores_nothing() {
    let h = harness_or_skip!();
    let content = "this sentence must never reach the store";
    let (status, _) = h
        .post(
            "/console/write",
            &[
                ("csrf", "a-token-this-session-never-minted"),
                ("content", content),
                ("namespace", "project:lumberroom"),
                ("tags", ""),
                ("sensitivity", ""),
                ("occurred_at", ""),
                ("supersedes", ""),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_ne!(status, 303, "a bad token should not write");

    let found: i64 = sqlx::query_scalar("SELECT count(*) FROM memory WHERE content = $1")
        .bind(content)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(found, 0, "a forged token wrote a memory");
}

// ---- clients ------------------------------------------------------------------------------------
//
// A client created here reaches the store, so what matters is not that the page renders. It is that
// the grant written is the grant chosen, that the secret is shown once and never again, and that a
// revoked client is revoked in the database rather than only on the page.

/// The create form's token. It is the last csrf field on the page, after every revoke form.
fn client_form_csrf(html: &str) -> String {
    let key = "name=\"csrf\" value=\"";
    let at = html.rfind(key).expect("the clients page carries no csrf field") + key.len();
    let end = html[at..].find('"').unwrap();
    html[at..at + end].to_string()
}

/// The token minted for one client's revoke form.
fn client_revoke_token(html: &str, id: &str) -> String {
    let form = format!("/console/clients/{id}/revoke");
    let at = html.find(&form).unwrap_or_else(|| panic!("no revoke form for {id}"));
    let rest = &html[at..];
    let key = "name=\"csrf\" value=\"";
    let start = rest.find(key).expect("the revoke form carries no csrf") + key.len();
    let end = rest[start..].find('"').unwrap();
    rest[start..start + end].to_string()
}

async fn client_row(
    h: &Harness,
    name: &str,
) -> (String, serde_json::Value, serde_json::Value, bool, bool, bool, bool, bool, bool, String, bool)
{
    let row: (
        String,
        serde_json::Value,
        serde_json::Value,
        bool,
        bool,
        bool,
        bool,
        bool,
        Option<chrono::DateTime<chrono::Utc>>,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT client_id, grant_read, grant_write, registry_write, sealed_capable, may_delete,
                    may_ingest, may_read_history, consented_at, registered_via, revoked_at
               FROM oauth_client WHERE client_name = $1",
    )
    .bind(name)
    .fetch_one(&h.pool)
    .await
    .unwrap_or_else(|e| panic!("no client named {name} in the store: {e}"));
    (
        row.0,
        row.1,
        row.2,
        row.3,
        row.4,
        row.5,
        row.6,
        row.7,
        row.8.is_some(),
        row.9,
        row.10.is_some(),
    )
}

#[tokio::test]
async fn the_clients_page_offers_every_shape_with_a_reason_to_pick_it() {
    let h = harness_or_skip!();
    let (status, html) = h.get("/console/clients").await;
    assert_eq!(status, 200);
    for p in lumberroom_server::domain::presets::Preset::ALL {
        assert!(html.contains(p.title()), "{} is not offered", p.as_str());
        let head: String = p.detail().chars().take(40).collect();
        assert!(html.contains(&head), "{} is offered with no explanation", p.as_str());
    }
    assert!(html.contains("mayDelete"), "the advanced view has to expose every capability");
}

#[tokio::test]
async fn a_client_created_from_a_shape_gets_that_shape_and_is_consented_to() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    let (status, _) = h
        .post(
            "/console/clients/new",
            &[("csrf", &csrf), ("name", "audit-read-only"), ("preset", "read-only")],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 200, "creating a client should answer with the page carrying its id");

    let (_, read, write, reg, _sealed, del, ing, _hist, consented, via, _revoked) =
        client_row(&h, "audit-read-only").await;
    assert_eq!(read, serde_json::json!([{"namespace":"*","max":"sealed"}]));
    assert_eq!(write, serde_json::json!([]), "read-only wrote a write grant");
    assert!(!reg && !del && !ing);
    assert_eq!(via, "manual", "a client the owner issued is not a self-registration");
    assert!(consented, "an owner filling in this form has consented already");
}

#[tokio::test]
async fn the_advanced_view_replaces_the_shape_rather_than_merging_with_it() {
    // The trap: a checkbox nobody saw clearing a capability the shape granted. Ticking `advanced`
    // is what says the fields below are the answer.
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    let (status, _) = h
        .post(
            "/console/clients/new",
            &[
                ("csrf", &csrf),
                ("name", "audit-adjusted"),
                ("preset", "full"),
                ("advanced", "1"),
                ("read", "project:*@private"),
                ("write", "project:lumberroom@open"),
                ("may_ingest", "1"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 200);

    let (_, read, _w, reg, sealed, _del, ing, hist, _c, _v, _r) =
        client_row(&h, "audit-adjusted").await;
    assert_eq!(read, serde_json::json!([{"namespace":"project:*","max":"private"}]));
    assert!(ing, "a ticked box was ignored");
    assert!(
        !reg && !sealed && !hist,
        "the full shape's capabilities survived a form that did not tick them"
    );
}

/// `/oauth/register` refuses these shapes, and a client the owner typed in reaches `/authorize` by
/// the same path a self-registered one does. One form checking and the other not is how a fragment
/// or a plain-http host ends up stored on the surface nobody thought to look at.
#[tokio::test]
async fn the_clients_form_refuses_a_redirect_uri_that_registration_would_refuse() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);

    for (label, uri) in [
        ("fragment", "https://claude.ai/cb#frag"),
        ("plain http off the loopback", "http://claude.ai/cb"),
        ("javascript", "javascript:alert(1)"),
        ("not absolute", "/callback"),
        ("userinfo", "https://user:pw@claude.ai/cb"),
    ] {
        let name = format!("audit-redirect-{label}");
        let (status, body) = h
            .post(
                "/console/clients/new",
                &[
                    ("csrf", &csrf),
                    ("name", &name),
                    ("preset", "read-only"),
                    ("redirect_uris", uri),
                ],
                Some(&h.cookie),
            )
            .await;
        assert_eq!(status, 400, "{label} was accepted: {uri}");
        assert!(body.contains("redirect_uri"), "{label} was refused without saying why");

        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM oauth_client WHERE client_name = $1")
                .bind(name.as_str())
                .fetch_one(&h.pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "{label} was refused and the client was written anyway");
    }
}

#[tokio::test]
async fn a_loopback_redirect_uri_is_still_accepted_from_the_clients_form() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    let (status, _) = h
        .post(
            "/console/clients/new",
            &[
                ("csrf", &csrf),
                ("name", "audit-redirect-loopback"),
                ("preset", "read-only"),
                ("redirect_uris", "http://127.0.0.1:7711/callback, https://claude.ai/cb"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 200, "a CLI's loopback listener is the shape RFC 8252 allows");
    let uris: Vec<String> =
        sqlx::query_scalar("SELECT redirect_uris FROM oauth_client WHERE client_name = $1")
            .bind("audit-redirect-loopback")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(uris, ["http://127.0.0.1:7711/callback", "https://claude.ai/cb"]);
}

#[tokio::test]
async fn no_shape_can_grant_deletion_through_the_clients_form() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    for p in lumberroom_server::domain::presets::Preset::ALL {
        let name = format!("audit-delete-{}", p.as_str());
        let (status, _) = h
            .post(
                "/console/clients/new",
                &[("csrf", &csrf), ("name", &name), ("preset", p.as_str())],
                Some(&h.cookie),
            )
            .await;
        assert_eq!(status, 200);
        let (.., del, _i, _h2, _c, _v, _r) = {
            let r = client_row(&h, &name).await;
            (r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10)
        };
        assert!(!del, "{} granted deletion without anyone asking", p.as_str());
    }
}

#[tokio::test]
async fn a_client_secret_is_shown_once_and_the_store_keeps_only_a_hash() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    let (status, body) = h
        .post(
            "/console/clients/new",
            &[
                ("csrf", &csrf),
                ("name", "audit-confidential"),
                ("preset", "read-write"),
                ("confidential", "1"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 200);
    assert!(body.contains("Client secret"), "a confidential client showed no secret");
    assert!(body.contains("only a hash"), "the page has to say it cannot be shown again");

    let key = "<p>Client secret</p><p><code>";
    let at = body.find(key).expect("no secret on the page") + key.len();
    let end = body[at..].find('<').unwrap();
    let secret = body[at..at + end].to_string();
    assert!(secret.len() >= 32, "the secret is too short to be one: {secret:?}");

    let stored: Option<String> =
        sqlx::query_scalar("SELECT secret_hash FROM oauth_client WHERE client_name = $1")
            .bind("audit-confidential")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    let stored = stored.expect("a confidential client stored no hash");
    assert_ne!(stored, secret, "the secret is in the database in the clear");
    assert_eq!(
        stored,
        lumberroom_server::domain::oauth::hash_token(&secret),
        "the hash is not this secret's"
    );

    let (_, later) = h.get("/console/clients").await;
    assert!(!later.contains(&secret), "the secret is readable from the list afterwards");
}

#[tokio::test]
async fn a_public_client_gets_no_secret_and_the_page_says_what_secures_it() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    let (status, body) = h
        .post(
            "/console/clients/new",
            &[("csrf", &csrf), ("name", "audit-public"), ("preset", "read-write")],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 200);
    assert!(!body.contains("Client secret"));
    assert!(body.contains("PKCE"));

    let stored: Option<String> =
        sqlx::query_scalar("SELECT secret_hash FROM oauth_client WHERE client_name = $1")
            .bind("audit-public")
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert!(stored.is_none(), "a public client was given a secret");
}

#[tokio::test]
async fn revoking_a_client_from_the_page_revokes_it_in_the_store() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    h.post(
        "/console/clients/new",
        &[("csrf", &csrf), ("name", "audit-doomed"), ("preset", "read-only")],
        Some(&h.cookie),
    )
    .await;
    let (id, ..) = client_row(&h, "audit-doomed").await;

    let (_, page) = h.get("/console/clients").await;
    let token = client_revoke_token(&page, &id);
    let (status, _) = h
        .post(&format!("/console/clients/{id}/revoke"), &[("csrf", &token)], Some(&h.cookie))
        .await;
    assert_eq!(status, 303, "revoking should redirect so a refresh does not repeat it");

    let (.., revoked) = client_row(&h, "audit-doomed").await;
    assert!(revoked, "the button redirected and revoked nothing");
}

#[tokio::test]
async fn a_token_minted_for_one_client_cannot_revoke_another() {
    let h = harness_or_skip!();
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    for name in ["audit-a", "audit-b"] {
        h.post(
            "/console/clients/new",
            &[("csrf", &csrf), ("name", name), ("preset", "read-only")],
            Some(&h.cookie),
        )
        .await;
    }
    let (a, ..) = client_row(&h, "audit-a").await;
    let (b, ..) = client_row(&h, "audit-b").await;

    let (_, page) = h.get("/console/clients").await;
    let for_a = client_revoke_token(&page, &a);
    let (status, _) =
        h.post(&format!("/console/clients/{b}/revoke"), &[("csrf", &for_a)], Some(&h.cookie)).await;
    assert_ne!(status, 303, "a token minted for one client decided another");

    let (.., revoked) = client_row(&h, "audit-b").await;
    assert!(!revoked, "the wrong client was revoked");
}

#[tokio::test]
async fn a_stranger_creates_no_client() {
    let h = harness_or_skip!();
    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_client").fetch_one(&h.pool).await.unwrap();
    let (status, _) = h
        .post(
            "/console/clients/new",
            &[("csrf", "not-a-token"), ("name", "audit-stranger"), ("preset", "full")],
            None,
        )
        .await;
    assert_ne!(status, 200);
    let after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_client").fetch_one(&h.pool).await.unwrap();
    assert_eq!(after, before, "a request with no session created a client");
}

// ---- changing a client's access ------------------------------------------------------------------
//
// Almost every client arrives by registering itself, so the grant it holds is whatever the consent
// screen offered on the day. What matters here is that the owner can narrow that grant afterwards
// without revoking anything, that the narrowing reaches the database, and that a revoked client
// stays revoked.

/// The token minted for one client's access form.
fn client_access_token(html: &str, id: &str) -> String {
    let form = format!("/console/clients/{id}/access");
    let at = html.find(&form).unwrap_or_else(|| panic!("no access form for {id}"));
    let rest = &html[at..];
    let key = "name=\"csrf\" value=\"";
    let start = rest.find(key).expect("the access form carries no csrf") + key.len();
    let end = rest[start..].find('"').unwrap();
    rest[start..start + end].to_string()
}

/// A consented client with the read-write shape, and its id.
async fn a_client(h: &Harness, name: &str) -> String {
    let (_, html) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&html);
    let (status, _) = h
        .post(
            "/console/clients/new",
            &[("csrf", &csrf), ("name", name), ("preset", "read-write")],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 200, "the fixture client was not created");
    client_row(h, name).await.0
}

#[tokio::test]
async fn the_owner_narrows_a_client_to_the_namespaces_it_should_see() {
    // The request this page exists for: a client that reaches everything, cut down to one project
    // and the owner's own namespace, without revoking it or sending it back through the flow.
    let h = harness_or_skip!();
    let id = a_client(&h, "audit-scoped").await;
    let (_, page) = h.get("/console/clients").await;
    let token = client_access_token(&page, &id);

    let (status, _) = h
        .post(
            &format!("/console/clients/{id}/access"),
            &[
                ("csrf", &token),
                ("preset", "read-write"),
                ("scope", "chosen"),
                ("ns", "user:me"),
                ("ns", "project:lumberroom"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303, "saving access should redirect so a refresh does not repeat it");

    let (_, read, write, ..) = client_row(&h, "audit-scoped").await;
    // The shape kept its levels and lost its reach: read at sealed, write at open, both only where
    // the owner pointed them.
    assert_eq!(
        read,
        serde_json::json!([
            {"namespace":"user:me","max":"sealed"},
            {"namespace":"project:lumberroom","max":"sealed"}
        ]),
        "the read grant was not narrowed to the namespaces picked"
    );
    assert_eq!(
        write,
        serde_json::json!([
            {"namespace":"user:me","max":"open"},
            {"namespace":"project:lumberroom","max":"open"}
        ]),
        "the write grant kept the shape's level but not its scope"
    );
}

#[tokio::test]
async fn a_scoped_grant_takes_the_namespaces_typed_beside_the_list() {
    // A namespace with nothing in it yet is not on the list, and it is exactly the namespace a new
    // project needs granted before its first write.
    let h = harness_or_skip!();
    let id = a_client(&h, "audit-typed").await;
    let (_, page) = h.get("/console/clients").await;
    let token = client_access_token(&page, &id);

    let (status, _) = h
        .post(
            &format!("/console/clients/{id}/access"),
            &[
                ("csrf", &token),
                ("preset", "read-only"),
                ("scope", "chosen"),
                ("more", "project:brand-new, project:*"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303);

    let (_, read, write, ..) = client_row(&h, "audit-typed").await;
    assert_eq!(
        read,
        serde_json::json!([
            {"namespace":"project:brand-new","max":"sealed"},
            {"namespace":"project:*","max":"sealed"}
        ])
    );
    assert_eq!(write, serde_json::json!([]), "read-only was scoped into a write grant");
}

#[tokio::test]
async fn a_scope_of_nothing_is_refused_rather_than_written_as_no_access() {
    // Ticking "only these" and picking none reads as a mistake, not as an intent to cut the client
    // off. Writing it would leave a surface that authenticates and then fails every call.
    let h = harness_or_skip!();
    let id = a_client(&h, "audit-empty-scope").await;
    let (_, page) = h.get("/console/clients").await;
    let token = client_access_token(&page, &id);

    let (status, body) = h
        .post(
            &format!("/console/clients/{id}/access"),
            &[("csrf", &token), ("preset", "read-write"), ("scope", "chosen")],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 400);
    assert!(body.contains("Pick at least one namespace"), "it was refused without saying why");

    let (_, read, ..) = client_row(&h, "audit-empty-scope").await;
    assert_eq!(read, serde_json::json!([{"namespace":"*","max":"sealed"}]), "the grant changed");
}

#[tokio::test]
async fn the_advanced_fields_still_replace_the_shape_when_access_is_changed() {
    let h = harness_or_skip!();
    let id = a_client(&h, "audit-access-advanced").await;
    let (_, page) = h.get("/console/clients").await;
    let token = client_access_token(&page, &id);

    let (status, _) = h
        .post(
            &format!("/console/clients/{id}/access"),
            &[
                ("csrf", &token),
                ("preset", "full"),
                ("advanced", "1"),
                ("read", "project:*@private"),
                ("write", ""),
                ("may_ingest", "1"),
            ],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303);

    let (_, read, _w, reg, sealed, _del, ing, hist, ..) =
        client_row(&h, "audit-access-advanced").await;
    assert_eq!(read, serde_json::json!([{"namespace":"project:*","max":"private"}]));
    assert!(ing, "a ticked box was ignored");
    assert!(!reg && !sealed && !hist, "the full shape's capabilities survived an untouched box");
}

#[tokio::test]
async fn a_revoked_client_cannot_be_granted_access_again() {
    let h = harness_or_skip!();
    let id = a_client(&h, "audit-revoked-grant").await;
    let (_, page) = h.get("/console/clients").await;
    let token = client_access_token(&page, &id);
    let revoke = client_revoke_token(&page, &id);
    h.post(&format!("/console/clients/{id}/revoke"), &[("csrf", &revoke)], Some(&h.cookie)).await;

    let (status, _) = h
        .post(
            &format!("/console/clients/{id}/access"),
            &[("csrf", &token), ("preset", "full")],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 400, "a revoked client was given a grant");

    let (.., reg, _s, _d, _i, _hh, _c, _v, revoked) = client_row(&h, "audit-revoked-grant").await;
    assert!(revoked, "the row stopped being revoked");
    assert!(!reg, "the full shape landed on a revoked client");
}

#[tokio::test]
async fn a_token_minted_for_one_client_cannot_change_another_client_access() {
    let h = harness_or_skip!();
    let a = a_client(&h, "audit-access-a").await;
    let b = a_client(&h, "audit-access-b").await;
    let (_, page) = h.get("/console/clients").await;
    let for_a = client_access_token(&page, &a);

    let (status, _) = h
        .post(
            &format!("/console/clients/{b}/access"),
            &[("csrf", &for_a), ("preset", "full")],
            Some(&h.cookie),
        )
        .await;
    assert_ne!(status, 303, "a token minted for one client changed another");

    let (.., reg, _s, _d, _i, _hh, _c, _v, _r) = client_row(&h, "audit-access-b").await;
    assert!(!reg, "the wrong client was widened");
}

#[tokio::test]
async fn a_stranger_changes_no_client_access() {
    let h = harness_or_skip!();
    let id = a_client(&h, "audit-access-stranger").await;
    let (_, body) = h
        .post(
            &format!("/console/clients/{id}/access"),
            &[("csrf", "not-a-token"), ("preset", "full")],
            None,
        )
        .await;
    // A stranger is sent to the sign-in form, which is a redirect like a saved grant is. What
    // separates them is where it points.
    assert!(body.contains("location: /console/login"), "a stranger was not sent to sign in");
    let (.., reg, _s, _d, _i, _hh, _c, _v, _r) = client_row(&h, "audit-access-stranger").await;
    assert!(!reg, "a request with no session widened a client");
}

// ---- rendering every screen for design work ------------------------------------------------------

/// Writes every console screen to `design/current/` with real rows behind it.
///
/// Not a check. A screen that renders is what the suite above asserts; what it cannot see is a
/// screen that reads badly, and reading one means looking at it. `dump_console.rs` renders the
/// clients page from fixtures with no server; this drives the whole console through HTTP so the
/// pages that need a store behind them have one.
///
///   ./scripts/cargo.sh test --test console -- --ignored dump_every_screen
#[tokio::test]
#[ignore = "writes files; a tool rather than a check"]
async fn dump_every_screen() {
    let Some(h) = setup().await else {
        eprintln!("no database reachable, so nothing was rendered");
        return;
    };

    // Enough rows for the layouts to have something to lay out. Three namespaces, one long fact,
    // one private one, and a correction, because a page of one short row hides everything.
    for (content, ns) in [
        ("the owner runs lumberroom on a host of his own", "global"),
        ("aditya prefers a plain answer before the context", "user:me"),
        ("the console renders server side with no javascript", "project:lumberroom"),
        (
            "the sensitivity filter runs inside the query rather than as a pass over results, so a \
             row a client may not see never enters that client's process",
            "project:lumberroom",
        ),
        ("the embedder is bge-base-en-v1.5 quantised to eight bits", "project:lumberroom"),
    ] {
        write::run(&h.ctx, content, ns, None, None, None, None).await.unwrap();
    }
    let first = write::run(
        &h.ctx,
        "the cleanup pass runs every six hours",
        "project:lumberroom",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    write::run(
        &h.ctx,
        "the cleanup pass runs every twelve hours",
        "project:lumberroom",
        None,
        Some(&first.id.to_string()),
        None,
        None,
    )
    .await
    .unwrap();
    let private = write::run(
        &h.ctx,
        "the owner's home network sits behind a tunnel",
        "user:me",
        None,
        None,
        Some("private"),
        None,
    )
    .await
    .unwrap();

    lumberroom_server::services::registry::set(
        &h.ctx,
        "global",
        "host",
        "services.lumberroom.mcp-endpoint",
        &serde_json::json!("https://memory.example.com/mcp"),
        None,
        None,
    )
    .await
    .unwrap();
    lumberroom_server::services::registry::set(
        &h.ctx,
        "project:lumberroom",
        "service",
        "services.lumberroom.database-host",
        &serde_json::json!({ "host": "db.example.com", "port": 5432 }),
        Some("private"),
        None,
    )
    .await
    .unwrap();

    queued(&h, "the owner keeps his notes in one place").await;
    queued(&h, "a proposal waiting on the owner reads as a claim and not as a fact").await;

    let (_, aliases) = h.get("/console/aliases").await;
    let record = alias_token(&aliases, "/console/aliases/record");
    h.post(
        "/console/aliases/record",
        &[
            ("csrf", &record),
            ("namespace", "project:lumberroom"),
            ("alias", "the console"),
            ("canonical", "the lumberroom console"),
        ],
        Some(&h.cookie),
    )
    .await;

    let (_, clients) = h.get("/console/clients").await;
    let csrf = client_form_csrf(&clients);
    for (name, preset) in [("claude-desktop", "read-write"), ("transcript-reader", "ingest-bot")] {
        h.post(
            "/console/clients/new",
            &[("csrf", &csrf), ("name", name), ("preset", preset)],
            Some(&h.cookie),
        )
        .await;
    }

    std::fs::create_dir_all("design/current").unwrap();
    let screens: Vec<(&str, String)> = vec![
        ("reading", "/console/reading".to_string()),
        ("namespace", "/console/namespace?ns=project:lumberroom".to_string()),
        ("fact", format!("/console/fact/{}", private.id)),
        ("search", "/console/search?q=cleanup%20pass".to_string()),
        ("search-empty", "/console/search".to_string()),
        ("write", "/console/write".to_string()),
        ("registry", "/console/registry".to_string()),
        ("queue", "/console/queue".to_string()),
        ("cleanup", "/console/cleanup".to_string()),
        ("aliases", "/console/aliases".to_string()),
        ("clients-live", "/console/clients".to_string()),
    ];
    for (name, path) in &screens {
        let (status, html) = h.get(path).await;
        let file = format!("design/current/{name}.html");
        std::fs::write(&file, &html).unwrap();
        println!("wrote {file} ({status}, {} bytes) from {path}", html.len());
    }

    let (_, login) = h.get_anonymous("/console/login").await;
    std::fs::write("design/current/login.html", &login).unwrap();
    println!("wrote design/current/login.html ({} bytes)", login.len());
}
