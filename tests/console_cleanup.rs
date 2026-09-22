//! The cleanup page, over HTTP. Same real Postgres and hash embedder as the rest of the suite, in
//! the same `lumberroom_rust_test` database, skipped when no database is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom cargo test -j 1 --test console_cleanup
//!
//! Every request goes through `http::router`, so the mount is under test: a route the composition
//! root failed to merge answers 404 and every case below fails.
//!
//! # Why rendering the page is not the test
//!
//! Console handlers do not go through `ingest_ctx`, so no route gate runs on this path. What stands
//! between a drawn Apply button and a 403 at the moment the owner presses it is whether the
//! principal the handler builds carries the grant `services::review::supersede` and
//! `services::review::delete` ask for. Nothing about the rendered HTML reveals that. So every apply
//! here signs in, reads the token out of the form the server drew, posts it, and then asks the
//! database what moved.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use lumberroom_server::adapters::auth;
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::authserver::session::Sessions;
use lumberroom_server::config::{self, AuthMode, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::cleanup::{CleanupKind, Disposition};
use lumberroom_server::domain::policy::{NamespaceGrant, SensitivityDefaults};
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::mcp::AppState;
use lumberroom_server::ports::cleanup::{CleanupRepository, NewMember, NewProposal};
use lumberroom_server::ports::ingest::IngestRepository;
use lumberroom_server::ports::OauthStore;
use lumberroom_server::services::{bootstrap, write, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";

/// 32 characters, the length the cookie signer wants.
const COOKIE_SECRET: &str = "console-test-cookie-secret-32ch!";

/// Every test here truncates the shared test database, so they serialise themselves rather than
/// relying on `--test-threads=1` being remembered. Cargo runs one test binary at a time, so this
/// mutex and the ones in `console.rs` and `integration.rs` do not have to know about each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A setup step that is allowed to be missing, with the reason printed rather than swallowed. The
/// suite skips rather than fails, which makes that sentence the only thing standing between a
/// broken run and a run somebody reads as a pass.
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
    cleanup: Arc<dyn CleanupRepository>,
    pool: PgPool,
    base: String,
    cookie: String,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    /// Held for the whole test. The mutex above serialises this binary's own threads; this is what
    /// keeps the other five binaries out of the same database.
    _db: common::DbGuard,
}

impl Harness {
    async fn get(&self, path: &str) -> (u16, String) {
        self.request(path, Some(&self.cookie)).await
    }

    async fn get_anonymous(&self, path: &str) -> (u16, String) {
        self.request(path, None).await
    }

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
        (status, format!("{body}\n<!-- location: {location} -->"))
    }
}

/// Builds the store, the config, the app state and a live server on a loopback port, then returns a
/// session cookie minted by the same signer the console verifies with.
///
/// The password form is skipped on purpose. Argon2 costs real time on every attempt, and what these
/// tests are about is what a signed-in owner can do rather than how the signing happens.
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
    // The console lives in oauth mode alone. Both settings go on the struct rather than through the
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

    // Composed the way main.rs composes it.
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
        session_id: Some("console-cleanup-test".into()),
    };

    let cleanup: Arc<dyn CleanupRepository> =
        Arc::new(postgres::PgCleanupRepository::new(pool.clone()));
    let ingest: Arc<dyn IngestRepository> =
        Arc::new(postgres::PgIngestRepository::new(pool.clone()));
    let state = Arc::new(AppState {
        cleanup: Arc::clone(&cleanup),
        aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
        cfg: Arc::clone(&cfg),
        repos,
        oauth: Arc::clone(&oauth),
        ingest,
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
        cleanup,
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

// ---- the store, read and written straight ----

/// One live open row, and the content the store actually holds for it.
///
/// The content comes back out of the table rather than out of the string that went in. Apply
/// compares `seen_content` against the column byte for byte and refuses when they differ, so a
/// proposal built from the input would fail as MemberChanged and prove nothing.
async fn fact(h: &Harness, content: &str) -> (String, String) {
    let written = write::run(&h.ctx, content, "project:lumberroom", None, None, None, None)
        .await
        .expect("the fixture write has to land or the case proves nothing");
    let stored = stored_content(h, &written.id).await.expect("the row it just wrote");
    (written.id, stored)
}

async fn stored_content(h: &Harness, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT content FROM memory WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .fetch_optional(&h.pool)
        .await
        .unwrap()
}

async fn superseded_by(h: &Harness, id: &str) -> Option<String> {
    let found: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT superseded_by FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(id).unwrap())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    found.map(|u| u.to_string())
}

async fn state_of(h: &Harness, id: &str) -> String {
    sqlx::query_scalar("SELECT state FROM cleanup_proposal WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .fetch_one(&h.pool)
        .await
        .unwrap()
}

async fn reason_of(h: &Harness, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT reason FROM cleanup_proposal WHERE id = $1")
        .bind(uuid::Uuid::parse_str(id).unwrap())
        .fetch_one(&h.pool)
        .await
        .unwrap()
}

/// Queue one proposal the way a pass does, through the repository the service reads.
async fn queued(
    h: &Harness,
    kind: CleanupKind,
    rationale: &str,
    keep: Option<&(String, String)>,
    retire: &[&(String, String)],
) -> String {
    let mut members: Vec<NewMember> = Vec::new();
    if let Some((id, content)) = keep {
        members.push(NewMember {
            memory_id: id.clone(),
            disposition: Disposition::Keep,
            seen_content: content.clone(),
        });
    }
    for (id, content) in retire {
        members.push(NewMember {
            memory_id: id.clone(),
            disposition: Disposition::Retire,
            seen_content: content.clone(),
        });
    }
    let (_, id) = h
        .cleanup
        .queue(
            &h.ctx.cfg.tenant_id,
            NewProposal {
                kind,
                namespace: "project:lumberroom".into(),
                keep_id: keep.map(|(id, _)| id.clone()),
                rationale: rationale.into(),
                produced_by: "qwen/qwen3.7-flash".into(),
                similarity: Some(0.942),
                // The in-process pass wrote this one, which is what `None` means here.
                posted_by: None,
                members,
            },
        )
        .await
        .expect("the fixture proposal has to queue");
    id
}

/// The hidden token the page minted for one finding and one act.
fn token_for(html: &str, id: &str, action: &str) -> String {
    let form = format!("/console/cleanup/{id}/{action}");
    let at =
        html.find(&form).unwrap_or_else(|| panic!("no {action} form for {id} on the page: {html}"));
    let rest = &html[at..];
    let key = "name=\"csrf\" value=\"";
    let start = rest.find(key).expect("the form carries no csrf field") + key.len();
    let end = rest[start..].find('"').unwrap();
    rest[start..start + end].to_string()
}

// ---- the cases ----

/// The test this track exists for. Sign in, read the page the server drew, take the token out of
/// the form, post it, and ask the database what moved.
///
/// `owner_approver` alone would answer 403 here on a `stale` proposal and 200 on this one, which is
/// why the delete case below is separate rather than folded in.
#[tokio::test]
async fn the_owner_applies_a_paraphrase_from_the_page_and_the_store_retires_the_row() {
    let h = harness_or_skip!();
    let keep = fact(&h, &format!("the deploy box runs {}", nonce("keep"))).await;
    let retire = fact(&h, &format!("deployment happens on {}", nonce("retire"))).await;
    let id = queued(
        &h,
        CleanupKind::Paraphrase,
        "both rows name the same machine",
        Some(&keep),
        &[&retire],
    )
    .await;

    let (status, html) = h.get("/console/cleanup").await;
    assert_eq!(status, 200, "the cleanup page answered {status}");
    assert!(html.contains("both rows name the same machine"), "the rationale is on the page");
    assert!(html.contains(&nonce("retire")), "and the text of every member");
    assert!(
        html.contains(&format!("href=\"/console/fact/{}\"", retire.0)),
        "each member links to its own entry: {html}"
    );

    let csrf = token_for(&html, &id, "apply");
    let (status, body) =
        h.post(&format!("/console/cleanup/{id}/apply"), &[("csrf", &csrf)], Some(&h.cookie)).await;
    assert_eq!(status, 303, "an apply answers with a redirect so a refresh does not resubmit");
    assert!(body.contains("/console/cleanup?done=applied"), "the outcome travels back: {body}");

    // The database, which is the only place the answer lives.
    assert_eq!(
        superseded_by(&h, &retire.0).await.as_deref(),
        Some(keep.0.as_str()),
        "the retiring row has to point at the survivor"
    );
    assert_eq!(superseded_by(&h, &keep.0).await, None, "and the survivor stays live");
    assert_eq!(state_of(&h, &id).await, "applied");

    let (status, html) = h.get("/console/cleanup?done=applied").await;
    assert_eq!(status, 200);
    assert!(html.contains("Applied."), "the page says what happened: {html}");
}

/// A `stale` proposal deletes rather than retires, so it runs through `services::forget::by_id` and
/// refuses a principal with `may_delete` false. This is the case a console that reused
/// `owner_approver` unchanged would answer 403 to, with the button drawn.
#[tokio::test]
async fn applying_a_stale_finding_deletes_the_row_rather_than_answering_403() {
    let h = harness_or_skip!();
    let doomed = fact(&h, &format!("an old note nothing ever read {}", nonce("stale"))).await;
    let id =
        queued(&h, CleanupKind::Stale, "nothing has read this in ninety days", None, &[&doomed])
            .await;

    let (_, html) = h.get("/console/cleanup").await;
    assert!(html.contains("Apply, deleting"), "the button says which act it is: {html}");
    let csrf = token_for(&html, &id, "apply");

    let (status, body) =
        h.post(&format!("/console/cleanup/{id}/apply"), &[("csrf", &csrf)], Some(&h.cookie)).await;
    assert_eq!(status, 303, "a 403 here means the console's principal cannot delete: {body}");
    assert_eq!(stored_content(&h, &doomed.0).await, None, "the row has to be gone");
    assert_eq!(state_of(&h, &id).await, "applied");
}

/// Which of two conflicting facts holds is the owner's call, so the service refuses to apply this
/// kind and the page must not draw a control whose only outcome is that refusal.
#[tokio::test]
async fn a_contradiction_carries_no_apply_control_and_the_page_says_why() {
    let h = harness_or_skip!();
    let a = fact(&h, &format!("the port is 8080 {}", nonce("ca"))).await;
    let b = fact(&h, &format!("the port is 8787 {}", nonce("cb"))).await;
    let id =
        queued(&h, CleanupKind::Contradiction, "these two cannot both hold", None, &[&a, &b]).await;

    let (status, html) = h.get("/console/cleanup").await;
    assert_eq!(status, 200);
    assert!(
        !html.contains(&format!("/console/cleanup/{id}/apply")),
        "a contradiction cannot be applied, so no form posts to that address: {html}"
    );
    assert!(html.contains("A contradiction names no survivor"), "and the page says why");
    assert!(
        html.contains(&format!("/console/cleanup/{id}/resolve")),
        "no Apply, but there has to be a way to settle it"
    );
    assert!(html.contains(&format!("/console/cleanup/{id}/reject")), "it can still be refused");

    // The token the page did mint, spent on the act it was not minted for.
    let rejecting = token_for(&html, &id, "reject");
    let (status, _) = h
        .post(&format!("/console/cleanup/{id}/apply"), &[("csrf", &rejecting)], Some(&h.cookie))
        .await;
    assert_eq!(status, 403, "a token that refuses a finding must not carry one out");
    assert_eq!(state_of(&h, &id).await, "proposed");
    assert_eq!(superseded_by(&h, &a.0).await, None, "and neither row moved");
    assert_eq!(superseded_by(&h, &b.0).await, None);
}

/// The page draws every finding at once, so a token has to name its own.
#[tokio::test]
async fn a_token_minted_for_one_finding_cannot_decide_another() {
    let h = harness_or_skip!();
    let keep = fact(&h, &format!("the first survivor {}", nonce("mk"))).await;
    let mine = fact(&h, &format!("the first retiree {}", nonce("mr"))).await;
    let other_keep = fact(&h, &format!("the second survivor {}", nonce("tk"))).await;
    let other = fact(&h, &format!("the second retiree {}", nonce("tr"))).await;

    let first = queued(&h, CleanupKind::Paraphrase, "one finding", Some(&keep), &[&mine]).await;
    let second =
        queued(&h, CleanupKind::Exact, "another finding", Some(&other_keep), &[&other]).await;

    let (_, html) = h.get("/console/cleanup").await;
    let csrf = token_for(&html, &first, "apply");

    let (status, _) = h
        .post(&format!("/console/cleanup/{second}/apply"), &[("csrf", &csrf)], Some(&h.cookie))
        .await;
    assert_eq!(status, 403, "a token binds to the finding it was minted for");
    assert_eq!(state_of(&h, &second).await, "proposed", "the refused finding did not move");
    assert_eq!(state_of(&h, &first).await, "proposed", "and neither did the one it named");
    assert_eq!(superseded_by(&h, &other.0).await, None, "no row was retired");
    assert_eq!(superseded_by(&h, &mine.0).await, None);
}

#[tokio::test]
async fn a_decision_with_no_token_changes_nothing() {
    let h = harness_or_skip!();
    let keep = fact(&h, &format!("a survivor {}", nonce("bk"))).await;
    let retire = fact(&h, &format!("a retiree {}", nonce("br"))).await;
    let id = queued(&h, CleanupKind::Paraphrase, "posted bare", Some(&keep), &[&retire]).await;

    for action in ["apply", "reject"] {
        let (status, _) =
            h.post(&format!("/console/cleanup/{id}/{action}"), &[], Some(&h.cookie)).await;
        assert_eq!(status, 403, "{action} took a form with no token");
    }
    assert_eq!(state_of(&h, &id).await, "proposed");
    assert_eq!(superseded_by(&h, &retire.0).await, None);
}

#[tokio::test]
async fn a_stranger_holding_a_token_still_decides_nothing() {
    let h = harness_or_skip!();
    let keep = fact(&h, &format!("a survivor {}", nonce("sk"))).await;
    let retire = fact(&h, &format!("a retiree {}", nonce("sr"))).await;
    let id =
        queued(&h, CleanupKind::Paraphrase, "a finding a stranger wants", Some(&keep), &[&retire])
            .await;

    let (status, body) = h.get_anonymous("/console/cleanup").await;
    assert_eq!(status, 303, "the page answered {status} to a request with no session");
    assert!(body.contains("/console/login?next="), "and it remembers where they were going");

    let (_, html) = h.get("/console/cleanup").await;
    let csrf = token_for(&html, &id, "apply");
    for action in ["apply", "reject"] {
        let (status, body) =
            h.post(&format!("/console/cleanup/{id}/{action}"), &[("csrf", &csrf)], None).await;
        assert_eq!(status, 303, "{action} answered {status} to a request with no session");
        assert!(body.contains("/console/login?next="), "{action} has to send them to the form");
    }
    assert_eq!(state_of(&h, &id).await, "proposed");
    assert_eq!(superseded_by(&h, &retire.0).await, None);
}

/// Rejection is a signal rather than a delete: the same cluster is found again next hour, and the
/// note is the only record of why the owner refused it.
#[tokio::test]
async fn a_rejection_from_the_page_records_the_reason_and_retires_nothing() {
    let h = harness_or_skip!();
    let keep = fact(&h, &format!("a survivor {}", nonce("rk"))).await;
    let retire = fact(&h, &format!("a retiree {}", nonce("rr"))).await;
    let id =
        queued(&h, CleanupKind::Paraphrase, "not the same fact at all", Some(&keep), &[&retire])
            .await;

    let (_, html) = h.get("/console/cleanup").await;
    let csrf = token_for(&html, &id, "reject");
    let (status, body) = h
        .post(
            &format!("/console/cleanup/{id}/reject"),
            &[("csrf", &csrf), ("reason", "these are two different machines")],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303);
    assert!(body.contains("/console/cleanup?done=rejected"), "{body}");
    assert_eq!(state_of(&h, &id).await, "rejected");
    assert_eq!(reason_of(&h, &id).await.as_deref(), Some("these are two different machines"));
    assert_eq!(superseded_by(&h, &retire.0).await, None, "a rejection touches no memory");

    let (_, html) = h.get("/console/cleanup").await;
    assert!(html.contains("these are two different machines"), "the note comes back: {html}");
}

/// A proposal describes a cluster as it stood when a pass read it. The page marks a member the
/// store has moved, and the apply refuses rather than adapting.
#[tokio::test]
async fn a_member_edited_since_the_pass_read_it_is_marked_and_the_apply_is_refused() {
    let h = harness_or_skip!();
    let keep = fact(&h, &format!("a survivor {}", nonce("ek"))).await;
    let retire = fact(&h, &format!("a retiree {}", nonce("er"))).await;
    let id = queued(&h, CleanupKind::Paraphrase, "one of these has moved", Some(&keep), &[&retire])
        .await;

    // Straight at the column, which is what an edit through any path leaves behind.
    sqlx::query("UPDATE memory SET content = $2 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&retire.0).unwrap())
        .bind(format!("something else entirely {}", nonce("edited")))
        .execute(&h.pool)
        .await
        .unwrap();

    let (status, html) = h.get("/console/cleanup").await;
    assert_eq!(status, 200);
    assert!(
        html.contains("EDITED SINCE"),
        "the page marks it before the button is pressed: {html}"
    );

    let csrf = token_for(&html, &id, "apply");
    let (status, body) =
        h.post(&format!("/console/cleanup/{id}/apply"), &[("csrf", &csrf)], Some(&h.cookie)).await;
    assert_eq!(status, 409, "the store moved under the proposal, so it is refused: {body}");
    assert_eq!(state_of(&h, &id).await, "proposed", "and the finding is still waiting");
    assert_eq!(superseded_by(&h, &retire.0).await, None);
}

/// A rationale is a model's sentence about the owner's rows, so it is untrusted text.
///
/// Both directions. Escaping turns a quote into `&quot;`, so the words survive as inert text and an
/// assertion that they are absent would pass over a page that rendered the payload as markup. What
/// this asserts is that the escaped value lands whole and the raw one lands nowhere.
#[tokio::test]
async fn a_hostile_rationale_renders_as_text_and_not_as_markup() {
    let h = harness_or_skip!();
    let payload = "\"<script>alert(1)</script>";
    let keep = fact(&h, &format!("a survivor {}", nonce("xk"))).await;
    let retire = fact(&h, &format!("a retiree {}", nonce("xr"))).await;
    let id = queued(&h, CleanupKind::Paraphrase, payload, Some(&keep), &[&retire]).await;

    let (status, html) = h.get("/console/cleanup").await;
    assert_eq!(status, 200);
    assert!(
        html.contains("&quot;&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the rationale has to land escaped and whole: {html}"
    );
    assert!(!html.contains(payload), "the raw payload appears nowhere on the page");
    assert!(!html.contains("<script>alert"), "no script tag as markup");
    assert!(!html.contains("value=\"\"<script"), "and no raw quote broke out of an attribute");

    // The page still works after that, which is the other half: escaping that dropped the finding
    // would pass every assertion above.
    assert!(html.contains(&format!("/console/cleanup/{id}/apply")), "the finding is still drawn");
}

#[tokio::test]
async fn the_owner_settles_a_contradiction_from_the_page_and_the_other_row_retires_into_it() {
    // The operation the console had no way to perform. Its own note used to say "open either row
    // and supersede it with what is true", and the fact page's Replace composes a NEW memory
    // superseding the one being viewed, which on a contradiction leaves three rows: the new one,
    // the row it retired, and the other original still live.
    let h = harness_or_skip!();
    let keep = fact(&h, &format!("the nickname is QUARTZ-A {}", nonce("ka"))).await;
    let other = fact(&h, &format!("the nickname is QUARTZ-B {}", nonce("kb"))).await;
    let id = queued(
        &h,
        CleanupKind::Contradiction,
        "two values for the same nickname",
        None,
        &[&keep, &other],
    )
    .await;

    let (status, html) = h.get("/console/cleanup").await;
    assert_eq!(status, 200);
    assert!(
        html.contains(&format!("/console/cleanup/{id}/resolve")),
        "a contradiction has to offer a way to settle it: {html}"
    );
    assert!(html.contains("Keep this one"), "one button per row");

    let csrf = token_for(&html, &id, "resolve");
    let (status, body) = h
        .post(
            &format!("/console/cleanup/{id}/resolve"),
            &[("csrf", &csrf), ("keep_id", &keep.0)],
            Some(&h.cookie),
        )
        .await;
    assert_eq!(status, 303, "settling answers with a redirect so a refresh does not resubmit");
    assert!(body.contains("/console/cleanup?done=resolved"), "the outcome travels back: {body}");

    // The database, which is the only place the answer lives. Two rows in, two rows out: the
    // chosen one live, the other retired into it, and no third row anywhere.
    let superseded_by: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT superseded_by FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&other.0).unwrap())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(
        superseded_by.map(|u| u.to_string()),
        Some(keep.0.clone()),
        "the other row should be superseded into the one the owner kept"
    );
    let still_live: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT superseded_by FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&keep.0).unwrap())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert!(still_live.is_none(), "the kept row has to stay live");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM memory WHERE content LIKE $1")
        .bind(format!("%{}%", nonce("ka")))
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "settling must not write a third row");
}

#[tokio::test]
async fn a_row_the_finding_was_never_about_cannot_be_named_as_the_survivor() {
    // The form carries keep_id, so a hand-edited one could name any row in the store. The service
    // checks it against the proposal's own members.
    let h = harness_or_skip!();
    let a = fact(&h, &format!("the nickname is QUARTZ-C {}", nonce("na"))).await;
    let b = fact(&h, &format!("the nickname is QUARTZ-D {}", nonce("nb"))).await;
    let stranger = fact(&h, &format!("an unrelated fact {}", nonce("ns"))).await;
    let id =
        queued(&h, CleanupKind::Contradiction, "two values, one subject", None, &[&a, &b]).await;

    let (_, html) = h.get("/console/cleanup").await;
    let csrf = token_for(&html, &id, "resolve");
    let (status, _) = h
        .post(
            &format!("/console/cleanup/{id}/resolve"),
            &[("csrf", &csrf), ("keep_id", &stranger.0)],
            Some(&h.cookie),
        )
        .await;
    assert_ne!(status, 303, "naming an outside row should not settle anything");

    let touched: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT superseded_by FROM memory WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&a.0).unwrap())
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert!(touched.is_none(), "and nothing in the cluster should have moved");
}
