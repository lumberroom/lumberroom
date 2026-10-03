//! The console: somewhere to look at the store.
//!
//! One reading surface over what is already stored. A namespace index with both-axes counts, a
//! namespace as a run of entries newest first, one entry with its provenance and everything that
//! value has been, search across what the reader may see, and the registry as exact keyed facts.
//! Most pages read. Four POST routes change the store: three decide a proposal on the ingest queue,
//! and one writes a fact the owner typed.
//!
//! # Who the console is
//!
//! The owner, not a client. Every page reads with the internal principal in `owner_reader`, which
//! is built here, used here and handed to nothing outside this module. What makes that safe is the
//! session in front of it: the reader has already proved they hold the owner password, the same
//! argon2 hash that gates the OAuth consent screen, and consent is the act that hands a stranger
//! the whole store. A session good enough to approve a client is good enough to read one.
//!
//! Writing reaches for `owner_approver` instead, which is `owner_reader` plus a write grant. Every
//! write is a form POST carrying a token signed for that row and that action, so a page the owner
//! happens to be visiting cannot spend his live session on a proposal he never saw or a fact he
//! never typed.
//!
//! # What the write surface is for
//!
//! One field. `occurred_at` says when a fact started holding in the world, and a person typing it
//! knows; a model reads a date out of context and invents one, which is what the near-now fence in
//! `write::run` exists to stop. Everything else the compose page offers is what `memory_write`
//! already takes.
//!
//! The handlers hold no write logic. `services::write::run` decides classification, refuses a
//! credential, applies the fence, collapses a duplicate and links a supersession, exactly as it
//! does for a tool call. That is the property the decision to let this console write rests on, so
//! a rule added here rather than there breaks it.
//!
//! # Why it lives only in oauth mode
//!
//! `OWNER_PASSWORD_HASH` is configured and validated in that mode and in no other, and a console
//! that cannot check a password is a console with no door. In the other modes every route here
//! answers with a page saying so. The guard is one function and every handler goes through it, so
//! no route can be added that forgets. `/console/logo.svg` is the one exception, and it is
//! deliberate: the sign-in form and the consent screen link the mark as their favicon and are read
//! before any session exists.
//!
//! # The cookie
//!
//! The same signer, the same secret, the same flags, at `Path=/console`. `Sessions::set_cookie`
//! writes `Path=/oauth`, which is right for the consent flow and would leave this surface with no
//! cookie at all. Two cookies of one name at two paths are both sent, in whichever order the
//! browser chooses, and `Sessions::verify` tries every one of them, which is what makes this work
//! rather than logging the owner out at random.

pub mod aliases;
pub mod cleanup;
pub mod clients;
pub mod data;
pub mod pages;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::extract::{ConnectInfo, Extension, Form, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use zeroize::Zeroize;

use crate::authserver::limiter::{self, LoginLimiter};
use crate::authserver::session::{OwnerSession, Sessions, COOKIE_NAME};
use crate::config::{AuthMode, Config};
use crate::domain::errors::DomainError;
use crate::domain::namespaces;
use crate::domain::policy::NamespaceGrant;
use crate::domain::types::{Invocation, Principal, Sensitivity};
use crate::mcp::AppState;
use crate::services::Ctx;
use data::Cursor;
use pages::Health;

/// Paid on every failed password before the response is written, the same as the consent screen's
/// login. A typo costs the owner three quarters of a second and a guesser the same on every try.
const LOGIN_FAILURE_DELAY: Duration = Duration::from_millis(750);

/// How many rows a search asks the store for. The bands print what came back; nothing is dropped
/// for want of room.
const SEARCH_LIMIT: i64 = 30;

/// The action every write token is minted for. A queue decision and a hand-written fact never share
/// a token, because the action is signed into it.
const WRITE_ACTION: &str = "write";

/// The token's second half when the write replaces nothing.
///
/// The half is the id of the row being retired, and a write that retires none still has to sign
/// something. A named value keeps the empty string out of the signing input, where it would be one
/// more thing a future id could collide with.
const NEW_FACT: &str = "new";

#[derive(Clone)]
pub struct Console {
    state: Arc<AppState>,
    /// `Some` only in oauth mode. A console with no password to check has no door, so the absence
    /// of a signer is what closes it, rather than a flag some later handler could forget to read.
    sessions: Option<Sessions>,
    limiter: Arc<LoginLimiter>,
}

impl Console {
    pub fn new(state: Arc<AppState>) -> Self {
        let sessions = match state.cfg.auth.mode {
            AuthMode::Oauth => Some(Sessions::from_config(&state.cfg)),
            _ => None,
        };
        let limiter = Arc::new(LoginLimiter::new(state.cfg.oauth.login_attempts_per_minute));
        Self { state, sessions, limiter }
    }

    fn cfg(&self) -> &Config {
        &self.state.cfg
    }

    /// The reader behind every page, and the only principal this module ever builds.
    ///
    /// Read `*` at a ceiling of sealed, no write grant, no delete, no registry write. It is safe
    /// because it is unreachable without a session, and a session means the owner password was
    /// checked: the credential that gates consent, which hands a client the whole store.
    ///
    /// `sealed_capable` is false and stays false. It asserts that the holder can decrypt a sealed
    /// item, and this server cannot by construction: the key lives on the machine that sealed the
    /// bytes and never reaches here. Setting it true would make the console claim it can read
    /// something it can only count.
    fn owner_reader(&self) -> Principal {
        Principal {
            client: "lumberroom-console".into(),
            token_id: "console".into(),
            mode: "console",
            scopes: vec![],
            read: vec![NamespaceGrant::new("*", Sensitivity::Sealed)],
            write: vec![],
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            // The fact page shows a value's whole timeline, and every version but the last is a
            // retired row. Decision 0006's argument reaches this without changing: the session
            // means `OWNER_PASSWORD_HASH` was checked, that password already hands a client the
            // whole live store through consent, and a fact the owner corrected is his own past. It
            // reads and nothing else. The write surface is `owner_approver` and stays where it is.
            may_read_history: true,
        }
    }

    /// The principal behind a write, whether the owner approved it from the queue or typed it.
    ///
    /// The write grant is `*` at sealed because that is the ceiling the owner's own CLI credential
    /// already carries. This is the same person performing the same act through a second door, and
    /// the row he is approving was proposed against a namespace he chose. A narrower grant would
    /// refuse a proposal `lumberroom ingest approve` writes without complaint, which reads as a
    /// broken button and sends him to the terminal to do the thing he just asked for.
    ///
    /// The compose page reuses it rather than minting a third principal. It is the same session,
    /// the same person and the same `write::run` call, so a separate grant would differ from the
    /// CLI's in the same way and refuse a namespace `lumberroom write` accepts.
    ///
    /// Nothing widens past that. `may_delete` and `registry_write` stay false: a write goes through
    /// `services::write::run` and touches neither, so nothing on this surface removes a row or
    /// overwrites a registry key. `may_ingest` stays false too, because that flag gates filling the
    /// queue rather than deciding it, and only the admin HTTP route reads it.
    fn owner_approver(&self) -> Principal {
        Principal {
            write: vec![NamespaceGrant::new("*", Sensitivity::Sealed)],
            ..self.owner_reader()
        }
    }

    fn ctx(&self) -> Ctx {
        self.ctx_with(self.owner_reader())
    }

    /// The context a write runs on, from the queue or from the compose form. Every check that
    /// matters is downstream of it: the classification table, the credentials refusal, the
    /// tripwire, the ceiling and the near-now fence all live in `write::run`, and this hands that
    /// path a principal rather than a decision.
    fn ctx_writing(&self) -> Ctx {
        self.ctx_with(self.owner_approver())
    }

    fn ctx_with(&self, principal: Principal) -> Ctx {
        Ctx {
            cfg: Arc::clone(&self.state.cfg),
            repos: self.state.repos.clone(),
            embedder: Arc::clone(&self.state.embedder),
            keys: self.state.keys.clone(),
            kek_verified: self.state.kek_verified,
            principal,
            // The owner is at the keyboard. Nothing here records a tool call, and the flag exists
            // so an unprompted model read is distinguishable from a person looking.
            invocation: Invocation::User,
            session_id: None,
        }
    }

    /// The near-now fence, printed beside the date field so the form promises what the store
    /// enforces on a deployment that moved the setting.
    fn fence_secs(&self) -> u64 {
        self.cfg().policy.write_min_occurred_age_secs
    }

    fn health(&self) -> Health {
        Health {
            key_verified: self.state.kek_verified,
            keys_configured: self.state.keys.is_some(),
            embedder: self.cfg().embed.model.clone(),
            degraded_embedder: self.state.degraded_embedder,
            last_write: None,
            now: chrono::Utc::now(),
        }
    }

    /// Every route goes through this. Two refusals, in this order: a mode with no owner password,
    /// then no live session.
    ///
    /// The session comes back rather than a unit, because a CSRF token is signed against it and the
    /// queue would otherwise verify a cookie twice and mint against the second reading.
    fn guard(&self, headers: &HeaderMap, wanted: &str) -> Result<OwnerSession, Response> {
        let Some(sessions) = self.sessions.as_ref() else {
            return Err(closed());
        };
        if let Some(session) = sessions.verify(headers, now()) {
            return Ok(session);
        }
        Err(redirect(&format!("/console/login?next={}", encode_query(wanted))))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/console", get(index))
        .route("/console/logo.svg", get(logo))
        .route("/console/login", get(login_page).post(login))
        .route("/console/reading", get(reading))
        .route("/console/namespace", get(namespace))
        .route("/console/fact/{id}", get(fact))
        .route("/console/search", get(search))
        .route("/console/write", get(compose).post(write))
        .route("/console/registry", get(registry))
        .route("/console/queue", get(queue))
        .route("/console/retired", get(retired))
        .route("/console/queue/{id}/approve", post(queue_approve))
        .route("/console/queue/{id}/reject", post(queue_reject))
        .route("/console/queue/{id}/unreject", post(queue_unreject))
        .route("/console/cleanup", get(cleanup::index))
        .route("/console/cleanup/{id}/apply", post(cleanup::apply))
        .route("/console/cleanup/{id}/reject", post(cleanup::reject))
        .route("/console/cleanup/{id}/resolve", post(cleanup::resolve))
        .route("/console/cleanup/{id}/unreject", post(cleanup::unreject))
        .route("/console/clients", get(clients::index))
        .route("/console/clients/new", post(clients::create))
        .route("/console/clients/{id}/access", post(clients::access))
        .route("/console/clients/{id}/revoke", post(clients::revoke))
        .route("/console/aliases", get(aliases::index))
        .route("/console/aliases/record", post(aliases::record))
        .route("/console/aliases/forget", post(aliases::forget))
        .with_state(Console::new(state))
}

/// The mark, and the one route here that calls no guard.
///
/// Every page on this server points its favicon at this path, including the sign-in form and the
/// OAuth consent screen, which are read by someone who holds no session. Behind the guard it would
/// answer a redirect to the login page and render as a broken icon on the two screens that most
/// need to look like this product. The file is three flat shapes and carries nothing private.
///
/// A year of caching is safe because the bytes are compiled into the binary: a changed mark ships
/// with a new image, and the sign-in page is not somewhere a stale icon costs anything.
async fn logo() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        pages::LOGO,
    )
        .into_response()
}

// ---- sign in ----

#[derive(Deserialize)]
struct NextQuery {
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    next: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

async fn index(State(app): State<Console>, headers: HeaderMap) -> Response {
    if let Err(response) = app.guard(&headers, "/console/reading") {
        return response;
    }
    redirect("/console/reading")
}

async fn login_page(State(app): State<Console>, Query(q): Query<NextQuery>) -> Response {
    // A sign-in form on a server with no password to check is a door painted on a wall.
    if app.sessions.is_none() {
        return closed();
    }
    page(StatusCode::OK, pages::login(&safe_next(q.next.as_deref()), None))
}

/// The password check, then a cookie at `Path=/console`.
///
/// The verification is Argon2 over `OWNER_PASSWORD_HASH`, on a blocking thread, with the password
/// zeroized after. It is the same check the consent screen runs, spelled here because that one is
/// private to the authorization server's routes and this module owns no file over there.
async fn login(
    State(app): State<Console>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let next = safe_next(form.next.as_deref());

    let addr = peer.map(|Extension(ConnectInfo(addr))| addr);
    let key = throttle_key(&headers, addr);
    let from = peer_key(addr);
    // Taken before the password check, not after it. The budget used to be read here and written
    // once Argon2id and the failure delay had finished, so a burst arriving inside that window all
    // saw it empty and all ran a hash.
    let Some(slot) = app.limiter.reserve(&key, &from, Instant::now()) else {
        tracing::warn!(key = %key, "console login throttled");
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            pages::login(&next, Some("Too many attempts. Wait a minute and try again.")),
        );
    };

    let Some(hash) = app.cfg().oauth.owner_password_hash.clone() else {
        slot.release();
        return page(
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::notice(
                "not configured",
                "This server has no owner password set, so nothing here can be unlocked. Set \
                 OWNER_PASSWORD_HASH and restart.",
                None,
                None,
            ),
        );
    };

    // The limiter counts attempts per key, and a key is whatever the caller says it is. This counts
    // hashes, which is the resource, and it answers rather than queueing behind the ones running.
    let Some(work) = limiter::password_slot() else {
        slot.release();
        tracing::warn!(key = %key, "console login refused: every password-check slot is busy");
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            pages::login(
                &next,
                Some("The server is busy checking another sign-in. Try again in a moment."),
            ),
        );
    };

    match verify_owner_password(hash, form.password.clone().unwrap_or_default(), work).await {
        Ok(true) => slot.release(),
        Ok(false) => {
            // The reservation is dropped rather than released, which leaves the attempt charged.
            tokio::time::sleep(LOGIN_FAILURE_DELAY).await;
            tracing::warn!(key = %key, "failed console login");
            return page(
                StatusCode::UNAUTHORIZED,
                pages::login(&next, Some("That password is not right.")),
            );
        }
        Err(e) => {
            tracing::error!(error = %e.log_message(), "console password check failed");
            return page(
                StatusCode::INTERNAL_SERVER_ERROR,
                pages::notice(
                    "the password could not be checked",
                    "The owner password hash on this server is not a hash this build can read. \
                     Check OWNER_PASSWORD_HASH.",
                    None,
                    None,
                ),
            );
        }
    }

    let value = sessions.issue(now());
    let mut response = redirect(&next);
    let cookie = set_cookie(&app.cfg().public_url, app.cfg().oauth.session_ttl_secs, &value);
    if let Ok(cookie) = header::HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

// ---- reading ----

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct NamespaceQuery {
    #[serde(default)]
    ns: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: Option<String>,
}

async fn reading(
    State(app): State<Console>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    if let Err(response) = app.guard(&headers, "/console/reading") {
        return response;
    }
    let ctx = app.ctx();
    let mut health = app.health();

    let readable = match data::readable(&ctx).await {
        Ok(r) => r,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    let contents = match data::contents(&ctx, &readable).await {
        Ok(c) => c,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    // Arrivals are live rows. A retired fact belongs beside the one that replaced it, which is the
    // namespace page and the entry page, not the run of what arrived this week.
    let listing = match data::page(
        &ctx,
        &readable,
        None,
        q.before.as_deref().and_then(Cursor::parse),
        data::page_size(q.limit),
        false,
        &[],
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return failed(&app, "the entries did not load", &e),
    };

    page(StatusCode::OK, pages::reading(&contents, &listing, None, &health))
}

async fn namespace(
    State(app): State<Console>,
    headers: HeaderMap,
    Query(q): Query<NamespaceQuery>,
) -> Response {
    let asked = q.ns.unwrap_or_default();
    let wanted = format!("/console/namespace?ns={asked}");
    if let Err(response) = app.guard(&headers, &wanted) {
        return response;
    }
    let Ok(ns) = namespaces::normalize(&asked) else {
        return page(
            StatusCode::NOT_FOUND,
            pages::notice(
                "no such namespace",
                "A namespace is 'global', 'user:me', 'project:<slug>', 'personal:<slug>' or \
                 'credentials:<slug>'. Nothing is stored under the name in that link.",
                None,
                None,
            ),
        );
    };

    let ctx = app.ctx();
    let mut health = app.health();
    let readable = match data::readable(&ctx).await {
        Ok(r) => r,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    let contents = match data::contents(&ctx, &readable).await {
        Ok(c) => c,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    // History alongside the live rows here: a correction reads as a revision struck in place, which
    // is the whole reason this page is a document rather than a list.
    let listing = match data::page(
        &ctx,
        &readable,
        Some(&ns),
        q.before.as_deref().and_then(Cursor::parse),
        data::page_size(q.limit),
        true,
        &[],
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return failed(&app, "the entries did not load", &e),
    };

    page(StatusCode::OK, pages::reading(&contents, &listing, Some(&ns), &health))
}

async fn fact(State(app): State<Console>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let wanted = format!("/console/fact/{id}");
    let session = match app.guard(&headers, &wanted) {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let ctx = app.ctx();
    let mut health = app.health();

    let readable = match data::readable(&ctx).await {
        Ok(r) => r,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    let contents = match data::contents(&ctx, &readable).await {
        Ok(c) => c,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    match data::leaf(&ctx, &id).await {
        Ok(Some(leaf)) => {
            // Minted against the id the store returned rather than the one in the address, so the
            // token and the form's hidden target are the same string by construction.
            let csrf = sessions.console_csrf(&session, WRITE_ACTION, &leaf.entry.id);
            page(StatusCode::OK, pages::fact(&leaf, &contents, &health, &csrf, app.fence_secs()))
        }
        Ok(None) => page(
            StatusCode::NOT_FOUND,
            pages::notice(
                "no such entry",
                "Nothing in this store carries that id. A deleted entry leaves no trace here, \
                 which is what a hard delete means.",
                None,
                Some(&health),
            ),
        ),
        Err(e) => failed(&app, "the entry did not load", &e),
    }
}

async fn search(
    State(app): State<Console>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Response {
    if let Err(response) = app.guard(&headers, "/console/search") {
        return response;
    }
    let ctx = app.ctx();
    let mut health = app.health();

    let readable = match data::readable(&ctx).await {
        Ok(r) => r,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    let contents = match data::contents(&ctx, &readable).await {
        Ok(c) => c,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    let query = q.q.unwrap_or_default();
    // An empty box is answered rather than refused. The service rejects an empty query, and a
    // reader who pressed Ask with nothing typed asked a fair question of the page.
    let answer = if query.trim().is_empty() {
        data::Answer::default()
    } else {
        match data::answer(&ctx, &readable, query.trim(), SEARCH_LIMIT).await {
            Ok(a) => a,
            Err(e) => return failed(&app, "the search did not run", &e),
        }
    };

    page(StatusCode::OK, pages::search(&answer, &contents, &health))
}

// ---- writing a fact ----
//
// One form, one route, and no write logic of its own. Everything that decides whether these fields
// may become a row lives in `services::write::run`: the classification table, the credentials
// refusal, the tripwire, the ceiling, the duplicate collapse, the supersession link and the
// near-now fence. This layer parses strings a browser sent, checks a token, and calls the function
// the MCP tool calls.
//
// The page exists for one field. A person typing a fact knows when it started holding and can say
// so, where a model reads a date out of context and invents one; the console is the surface where
// `occurred_at` is worth trusting.
//
// A refused write comes back as the same form with the message and the typed values, never as a
// notice. The reader wrote prose by hand and a page that hands it back empty gets used once.

#[derive(Deserialize)]
struct ComposeQuery {
    #[serde(default)]
    supersedes: Option<String>,
}

#[derive(Deserialize)]
struct WriteForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    sensitivity: String,
    #[serde(default)]
    occurred_at: String,
    #[serde(default)]
    supersedes: String,
}

impl WriteForm {
    fn draft(&self) -> pages::Draft {
        pages::Draft {
            content: self.content.clone(),
            namespace: self.namespace.clone(),
            tags: self.tags.clone(),
            sensitivity: self.sensitivity.clone(),
            occurred_at: self.occurred_at.clone(),
            supersedes: trimmed(&self.supersedes).map(str::to_string),
            replacing: None,
        }
    }
}

/// The compose page, empty or prefilled from the row a replacement would retire.
async fn compose(
    State(app): State<Console>,
    headers: HeaderMap,
    Query(q): Query<ComposeQuery>,
) -> Response {
    let target = q.supersedes.as_deref().and_then(trimmed).map(str::to_string);
    let wanted = match &target {
        Some(id) => format!("/console/write?supersedes={}", encode_query(id)),
        None => "/console/write".to_string(),
    };
    let session = match app.guard(&headers, &wanted) {
        Ok(s) => s,
        Err(response) => return response,
    };

    let mut draft = pages::Draft::default();
    if let Some(id) = &target {
        match data::leaf(&app.ctx(), id).await {
            Ok(Some(leaf)) => match prefill(&leaf) {
                Some(filled) => draft = filled,
                None => return unreplaceable(&app),
            },
            Ok(None) => return page(
                StatusCode::NOT_FOUND,
                pages::notice(
                    "no such entry",
                    "Nothing in this store carries the id in that link, so there is nothing to \
                         replace.",
                    None,
                    None,
                ),
            ),
            Err(e) => return failed(&app, "the entry did not load", &e),
        }
    }

    composed(&app, &session, draft, None, StatusCode::OK).await
}

/// The write itself, and the only place in this module that calls `write::run`.
///
/// Token first, before a character of the form reaches the store. Then the date, because
/// `parse_occurred_at` refuses rather than dropping a malformed one, and a silent `None` there
/// would leave the owner believing they recorded a date the store never saw.
async fn write(
    State(app): State<Console>,
    headers: HeaderMap,
    Form(form): Form<WriteForm>,
) -> Response {
    let session = match app.guard(&headers, "/console/write") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };

    let supersedes = trimmed(&form.supersedes);
    if !sessions.console_csrf_ok(&session, WRITE_ACTION, target_key(supersedes), &form.csrf) {
        tracing::warn!(
            replacing = supersedes.is_some(),
            "console write refused: the form token did not match"
        );
        return page(
            StatusCode::FORBIDDEN,
            pages::notice(
                "that form went stale",
                "The token on it was minted for another session or another entry, so nothing was \
                 written. Reload the page and write it again.",
                None,
                None,
            ),
        );
    }

    let occurred_at = match trimmed(&form.occurred_at) {
        Some(raw) => match crate::mcp::tools::parse_occurred_at(raw) {
            Ok(at) => Some(at),
            Err(e) => return rejected(&app, &session, &form, &e).await,
        },
        None => None,
    };

    match crate::services::write::run(
        &app.ctx_writing(),
        &form.content,
        &form.namespace,
        typed_tags(&form.tags),
        supersedes,
        trimmed(&form.sensitivity),
        occurred_at,
    )
    .await
    {
        // Land on the fact that was written, so the owner reads the result. The address carries an
        // id and nothing else: content in a query string lands in browser history and in the proxy
        // log of every hop between here and the browser.
        Ok(outcome) => redirect(&format!("/console/fact/{}", outcome.id)),
        Err(e) => rejected(&app, &session, &form, &e).await,
    }
}

/// The form again, with what the store said and everything that was typed.
///
/// A server fault takes the trouble page instead: the reader can do nothing about it, and offering
/// the same form back would invite them to press the button until the log fills.
async fn rejected(
    app: &Console,
    session: &OwnerSession,
    form: &WriteForm,
    e: &DomainError,
) -> Response {
    let status = StatusCode::from_u16(e.kind.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    if status.is_server_error() {
        return failed(app, "the write did not run", e);
    }
    tracing::warn!(error = %e.log_message(), "console refused a write");
    let draft = hydrated(app, form.draft()).await;
    composed(app, session, draft, Some(&e.client_message()), status).await
}

/// Draw the compose page. One place mints the token, so the form and the check downstream agree
/// about which write it is for.
async fn composed(
    app: &Console,
    session: &OwnerSession,
    draft: pages::Draft,
    error: Option<&str>,
    status: StatusCode,
) -> Response {
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let ctx = app.ctx();
    let mut health = app.health();

    let contents = match data::readable(&ctx).await {
        Ok(readable) => match data::contents(&ctx, &readable).await {
            Ok(c) => c,
            Err(e) => return failed(app, "the store did not answer", &e),
        },
        Err(e) => return failed(app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    let csrf =
        sessions.console_csrf(session, WRITE_ACTION, target_key(draft.supersedes.as_deref()));
    page(status, pages::compose(&draft, &contents, &health, &csrf, app.fence_secs(), error))
}

/// The form filled from the entry a replacement would retire, or nothing for an entry that cannot
/// be replaced from here.
///
/// A retired row already has a successor and `write::run` refuses a second one. An expired row has
/// a closed period and `write::validate_supersedes_target` refuses that too. A sealed row is bytes
/// this server cannot read, so the box would come up empty and the submit would overwrite a secret
/// with a blank.
fn prefill(leaf: &data::Leaf) -> Option<pages::Draft> {
    let e = &leaf.entry;
    if e.retired || e.expired || e.withheld {
        return None;
    }
    // The same prefill the control inside an entry uses, plus the old wording, because this page
    // stands on its own and the reader cannot see the row above the box.
    let mut draft = pages::Draft::replacing(e);
    draft.replacing = Some(e.content.clone());
    Some(draft)
}

/// Put the old wording back beside a refused replacement. The reader is looking at a form that
/// failed, and an id does not say which fact it was ending.
async fn hydrated(app: &Console, mut draft: pages::Draft) -> pages::Draft {
    let Some(id) = draft.supersedes.clone() else {
        return draft;
    };
    if let Ok(Some(leaf)) = data::leaf(&app.ctx(), &id).await {
        if !leaf.entry.withheld {
            draft.replacing = Some(leaf.entry.content.clone());
        }
    }
    draft
}

fn unreplaceable(app: &Console) -> Response {
    page(
        StatusCode::CONFLICT,
        pages::notice(
            "that entry cannot be replaced from here",
            "A row that has already been replaced takes no second successor, a row whose period \
             expired takes none either, and a sealed row is encrypted on the machine that wrote \
             it. Open the entry itself to see which of the three it is.",
            None,
            Some(&app.health()),
        ),
    )
}

/// Which write the token is signed for: the row being retired, or a fresh fact.
fn target_key(supersedes: Option<&str>) -> &str {
    supersedes.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(NEW_FACT)
}

/// The tags line, split. Empty means no tags rather than one tag that is an empty string.
fn typed_tags(raw: &str) -> Option<Vec<String>> {
    let tags: Vec<String> =
        raw.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect();
    (!tags.is_empty()).then_some(tags)
}

/// A field the browser sent empty is a field nobody filled in.
///
/// Every optional argument downstream reads `None` that way, and an empty string would reach
/// `parse_occurred_at` as a date and come back with a refusal written for a model.
fn trimmed(raw: &str) -> Option<&str> {
    let value = raw.trim();
    (!value.is_empty()).then_some(value)
}

async fn registry(State(app): State<Console>, headers: HeaderMap) -> Response {
    if let Err(response) = app.guard(&headers, "/console/registry") {
        return response;
    }
    let ctx = app.ctx();
    let mut health = app.health();

    let readable = match data::readable(&ctx).await {
        Ok(r) => r,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    let contents = match data::contents(&ctx, &readable).await {
        Ok(c) => c,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    match data::registry(&ctx, &readable).await {
        Ok(groups) => page(StatusCode::OK, pages::registry(&groups, &contents, &health)),
        Err(e) => failed(&app, "the registry did not load", &e),
    }
}

/// The proposal queue: what ingestion is asking the owner to approve, and the controls that answer.
///
/// The ingest store hangs off `AppState` rather than `Ctx.repos`, because the proposal queue is an
/// operator surface and no MCP tool reaches it. The controls this page draws post back to the three
/// routes below, each of which calls the same `services::ingest` function `lumberroom ingest` calls.
/// What got retired lately.
///
/// Read-only, and the owner's own view: no grant narrower than theirs reaches this console, and the
/// query still runs both axes rather than trusting that.
async fn retired(State(app): State<Console>, headers: HeaderMap) -> Response {
    if let Err(response) = app.guard(&headers, "/console/retired") {
        return response;
    }
    let ctx = app.ctx();
    let mut health = app.health();

    let readable = match data::readable(&ctx).await {
        Ok(r) => r,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    let contents = match data::contents(&ctx, &readable).await {
        Ok(c) => c,
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    let rows = match data::retired(&ctx, &readable).await {
        Ok(r) => r,
        Err(e) => return failed(&app, "the retired list did not load", &e),
    };
    page(StatusCode::OK, pages::retired(&rows, &contents, &health))
}

async fn queue(
    State(app): State<Console>,
    headers: HeaderMap,
    Query(q): Query<DoneQuery>,
) -> Response {
    let session = match app.guard(&headers, "/console/queue") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let ctx = app.ctx();
    let mut health = app.health();

    let contents = match data::readable(&ctx).await {
        Ok(readable) => match data::contents(&ctx, &readable).await {
            Ok(c) => c,
            Err(e) => return failed(&app, "the store did not answer", &e),
        },
        Err(e) => return failed(&app, "the store did not answer", &e),
    };
    health.last_write = contents.last_write;

    let view = match data::queue(&ctx, app.state.ingest.as_ref()).await {
        Ok(v) => v,
        Err(e) => return failed(&app, "the queue did not load", &e),
    };

    let csrf = |action: &str, id: &str| sessions.console_csrf(&session, action, id);
    page(StatusCode::OK, pages::queue(&view, &contents, &health, &csrf, q.done.as_deref()))
}

// ---- deciding a proposal ----
//
// Three routes, one shape. Verify the token against the live session, this action and this id, then
// call the `services::ingest` function the CLI calls and redirect. Every rule that decides whether a
// proposal may become a memory lives in `write::run`, downstream of all three, so nothing here has
// a judgement of its own to make.
//
// The redirect carries an outcome word and never the proposal's content or a refusal's reason. A
// browser keeps every address it visited, a proxy logs the same line, and a refused row already
// prints its rule name on the page it comes back to.

#[derive(Deserialize)]
struct DoneQuery {
    #[serde(default)]
    done: Option<String>,
}

#[derive(Deserialize)]
struct DecisionForm {
    #[serde(default)]
    csrf: String,
}

/// The token check, before anything reaches the store.
///
/// A stale form is the common case and it is answered with a page rather than a redirect, because a
/// redirect to the queue would look like the decision went through.
fn decided(
    app: &Console,
    headers: &HeaderMap,
    action: &str,
    id: &str,
    presented: &str,
) -> Result<(), Response> {
    let session = app.guard(headers, "/console/queue")?;
    let Some(sessions) = app.sessions.as_ref() else {
        return Err(closed());
    };
    if sessions.console_csrf_ok(&session, action, id, presented) {
        return Ok(());
    }
    tracing::warn!(action, "console decision refused: the form token did not match");
    Err(page(
        StatusCode::FORBIDDEN,
        pages::notice(
            "that form went stale",
            "The token on it was minted for another session or another row, so nothing was \
             changed. Reload the queue and decide again.",
            None,
            None,
        ),
    ))
}

fn proposal_id(id: &str) -> Result<uuid::Uuid, Response> {
    uuid::Uuid::parse_str(id.trim()).map_err(|_| {
        page(
            StatusCode::NOT_FOUND,
            pages::notice(
                "no such proposal",
                "A proposal is named by a uuid and nothing in the queue carries the one in that \
                 address.",
                None,
                None,
            ),
        )
    })
}

/// Approve: the one path from the queue into the store, and it is `services::write::run`.
///
/// A refusal is a decision the write path made and reported, so it answers 303 like any other
/// outcome. The row stays at `proposed` carrying the rule that stopped it, which the page prints.
async fn queue_approve(
    State(app): State<Console>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> Response {
    if let Err(response) = decided(&app, &headers, "approve", &id, &form.csrf) {
        return response;
    }
    let uuid = match proposal_id(&id) {
        Ok(u) => u,
        Err(response) => return response,
    };
    match crate::services::ingest::approve(&app.ctx_writing(), app.state.ingest.as_ref(), uuid)
        .await
    {
        Ok(outcome) if outcome.refused.is_some() => done("refused"),
        Ok(outcome) if outcome.deduplicated => done("deduplicated"),
        Ok(_) => done("written"),
        Err(e) => refusal(&app, "the approval did not run", &e),
    }
}

/// Reject: the content stays blocked by fingerprint, and Return to queue is the undo.
async fn queue_reject(
    State(app): State<Console>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> Response {
    if let Err(response) = decided(&app, &headers, "reject", &id, &form.csrf) {
        return response;
    }
    let uuid = match proposal_id(&id) {
        Ok(u) => u,
        Err(response) => return response,
    };
    match crate::services::ingest::reject(&app.ctx(), app.state.ingest.as_ref(), uuid, None).await {
        Ok(true) => done("rejected"),
        // The row had already left `proposed`, which happens when a second tab decided it first.
        Ok(false) => done("unchanged"),
        Err(e) => refusal(&app, "the rejection did not run", &e),
    }
}

async fn queue_unreject(
    State(app): State<Console>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> Response {
    if let Err(response) = decided(&app, &headers, "unreject", &id, &form.csrf) {
        return response;
    }
    let uuid = match proposal_id(&id) {
        Ok(u) => u,
        Err(response) => return response,
    };
    match crate::services::ingest::unreject(&app.ctx(), app.state.ingest.as_ref(), uuid).await {
        Ok(true) => done("returned"),
        Ok(false) => done("unchanged"),
        Err(e) => failed(&app, "the row did not return to the queue", &e),
    }
}

/// Back to the queue with one word saying what happened. 303, so a refresh does not decide twice.
fn done(outcome: &str) -> Response {
    redirect(&format!("/console/queue?done={outcome}"))
}

// ---- pieces ----

/// The console in a mode that configures no owner password.
fn closed() -> Response {
    page(
        StatusCode::NOT_FOUND,
        pages::notice(
            "the console needs oauth mode",
            "This server runs with AUTH_MODE set to something other than oauth, and that is the \
             mode that configures an owner password. Without one there is nothing to check at the \
             door, so the console stays shut. Set AUTH_MODE=oauth and OWNER_PASSWORD_HASH, then \
             restart.",
            None,
            None,
        ),
    )
}

fn page(status: StatusCode, html: String) -> Response {
    (status, Html(html)).into_response()
}

/// 303, so the browser follows with a GET after the login POST and a refresh does not resubmit the
/// password.
fn redirect(location: &str) -> Response {
    match header::HeaderValue::from_str(location) {
        Ok(value) => (StatusCode::SEE_OTHER, [(header::LOCATION, value)]).into_response(),
        Err(_) => page(
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::notice(
                "that link cannot be followed",
                "The address held a character a redirect cannot carry.",
                None,
                None,
            ),
        ),
    }
}

/// A decision the store refused, which is usually the owner and not the server.
///
/// The realistic path here is a conflict: two tabs open, and the row this one is approving was
/// rejected in the other. `failed` would call that a 500 and say the server could not answer, which
/// misdescribes it twice. The domain error already carries a client-safe sentence naming what to do
/// about it, so the reader gets that and the status the kind implies.
fn refusal(app: &Console, title: &str, e: &DomainError) -> Response {
    tracing::warn!(error = %e.log_message(), title, "console refused a decision");
    let status = StatusCode::from_u16(e.kind.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    if status.is_server_error() {
        return failed(app, title, e);
    }
    let health = app.health();
    page(status, pages::notice(title, &e.client_message(), None, Some(&health)))
}

/// A route that failed after the reader was already standing on a page.
fn failed(app: &Console, title: &str, e: &DomainError) -> Response {
    tracing::error!(error = %e.log_message(), title, "console page failed");
    page(
        StatusCode::INTERNAL_SERVER_ERROR,
        pages::trouble(
            title,
            "The console reached the server and the server could not answer. Nothing was changed. \
             The server log carries the reason.",
            &app.health(),
        ),
    )
}

/// Where to go after signing in, narrowed to this console.
///
/// An open redirect is reachable with nothing but a crafted link, and this one would arrive with
/// the owner's password typed into it. Anything that is not a path under `/console` becomes the
/// reading page: `//host` is a protocol-relative URL, `\` is read as `/` by some browsers, and a
/// scheme goes anywhere at all.
fn safe_next(raw: Option<&str>) -> String {
    const HOME: &str = "/console/reading";
    let Some(next) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return HOME.to_string();
    };
    if !next.starts_with("/console")
        || next.starts_with("//")
        || next.contains('\\')
        || next.contains("://")
        || next.chars().any(|c| c.is_control())
    {
        return HOME.to_string();
    }
    next.to_string()
}

/// Percent-encode the few characters that would end the value early. The paths this carries are
/// built here and hold a namespace or a uuid, so the set is small and closed.
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '&' => out.push_str("%26"),
            '#' => out.push_str("%23"),
            '+' => out.push_str("%2B"),
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            '"' => out.push_str("%22"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            _ => out.push(c),
        }
    }
    out
}

/// The session cookie, at this console's path.
///
/// Every flag is the consent screen's and every reason is the one `session.rs` gives: `HttpOnly`
/// so no injected script reads it, `SameSite=Lax`, and `Secure` everywhere except a loopback
/// deployment, decided from the configured public URL rather than the request, because behind a
/// proxy that terminated TLS the request looks like plain http from here.
///
/// The path is the one difference. `Path=/oauth` keeps the consent cookie off `/mcp`; `Path=/console`
/// does the same for this one, and neither is sent to a tool call.
fn set_cookie(public_url: &str, ttl_secs: i64, value: &str) -> String {
    let mut cookie =
        format!("{COOKIE_NAME}={value}; Path=/console; HttpOnly; SameSite=Lax; Max-Age={ttl_secs}");
    if !is_loopback(public_url) {
        cookie.push_str("; Secure");
    }
    cookie
}

fn is_loopback(public_url: &str) -> bool {
    public_url.starts_with("http://127.0.0.1")
        || public_url.starts_with("http://localhost")
        || public_url.starts_with("http://[::1]")
}

/// Argon2 over the configured hash, off the async runtime, with the password zeroized after.
///
/// `work` rides into the blocking closure so the permit is held for as long as the hash memory is,
/// rather than for as long as the caller waits for it.
async fn verify_owner_password(
    hash: String,
    password: String,
    work: tokio::sync::SemaphorePermit<'static>,
) -> crate::domain::errors::Result<bool> {
    tokio::task::spawn_blocking(move || {
        let _work = work;
        let mut password = password;
        let parsed = PasswordHash::new(&hash).map_err(|e| {
            DomainError::internal(format!("OWNER_PASSWORD_HASH is not a valid PHC string: {e}"))
        })?;
        // Argon2 reads its cost parameters out of the hash, and the comparison inside is constant
        // time.
        let ok = Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok();
        password.zeroize();
        Ok(ok)
    })
    .await
    .map_err(|e| DomainError::internal("the password check did not finish").with_source(e))?
}

/// The throttling key. The forwarded address when there is one, because behind a proxy the peer is
/// the proxy; the limiter's peer window is what makes inventing a forwarded address pointless.
fn throttle_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = forwarded.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return format!("fwd:{}", first.chars().take(48).collect::<String>());
        }
    }
    match peer {
        Some(addr) => format!("peer:{}", addr.ip()),
        None => "unknown".to_string(),
    }
}

/// The socket the request arrived on, which no header can change. The limiter's second window hangs
/// off this, so a caller inventing a forwarded address per request still spends one budget.
fn peer_key(peer: Option<SocketAddr>) -> String {
    match peer {
        Some(addr) => addr.ip().to_string(),
        None => "unknown".to_string(),
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy;

    fn reader() -> Principal {
        // The constructor needs an AppState, and the principal it returns depends on nothing in
        // one. This mirrors `Console::owner_reader` field for field, and the test below pins that
        // they agree by asserting the properties the comment there promises.
        Principal {
            client: "lumberroom-console".into(),
            token_id: "console".into(),
            mode: "console",
            scopes: vec![],
            read: vec![NamespaceGrant::new("*", Sensitivity::Sealed)],
            write: vec![],
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: true,
        }
    }

    #[test]
    fn the_console_reader_reads_everything_and_writes_nothing() {
        let p = reader();
        for namespace in ["global", "user:me", "personal:finance", "credentials:lumberroom"] {
            assert!(policy::admits(&p.read, namespace, Sensitivity::Sealed));
            assert!(
                !policy::admits(&p.write, namespace, Sensitivity::Open),
                "the console has no write grant at any level"
            );
        }
        assert!(p.write.is_empty());
        assert!(!p.registry_write);
        assert!(!p.may_delete);
    }

    /// The timeline on the fact page is made of retired rows, and the port refuses to hand them to
    /// a principal without this. A grant over live rows is not a grant over the history behind
    /// them, so the flag is set here on purpose rather than inherited.
    #[test]
    fn the_console_reader_may_read_what_no_longer_holds() {
        assert!(reader().may_read_history);
        assert!(approver().may_read_history, "the same person, through the same session");
    }

    fn approver() -> Principal {
        Principal { write: vec![NamespaceGrant::new("*", Sensitivity::Sealed)], ..reader() }
    }

    /// The server holds no key for a sealed item and never can, so the console counts them and
    /// claims nothing more.
    #[test]
    fn the_console_reader_is_never_sealed_capable() {
        assert!(!reader().sealed_capable);
    }

    /// The write grant matches the ceiling the owner's CLI credential holds, because approving from
    /// the queue is the act `lumberroom ingest approve` performs. It widens nowhere else.
    #[test]
    fn the_approver_writes_at_the_ceiling_the_cli_already_holds_and_gains_nothing_else() {
        let p = approver();
        for namespace in ["global", "user:me", "personal:finance"] {
            assert!(policy::admits(&p.write, namespace, Sensitivity::Private));
            assert!(policy::admits(&p.write, namespace, Sensitivity::Sealed));
        }
        assert!(!p.registry_write);
        assert!(!p.may_delete);
        assert!(!p.sealed_capable);
        assert!(!p.may_ingest, "the flag gates filling the queue, not deciding it");
    }

    /// Reading and approving are two principals, and only one of them can write.
    #[test]
    fn the_reader_and_the_approver_differ_in_the_write_grant_alone() {
        let (r, a) = (reader(), approver());
        assert!(r.write.is_empty());
        assert!(!a.write.is_empty());
        assert_eq!(r.read.len(), a.read.len());
        assert_eq!(r.client, a.client);
    }

    #[test]
    fn the_console_cookie_carries_the_consent_flags_at_its_own_path() {
        let cookie = set_cookie("https://lumberroom.example", 900, "v1.1000.abc");
        assert!(cookie.contains("Path=/console"), "the consent cookie's path never reaches here");
        assert!(!cookie.contains("Path=/oauth"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(!cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.starts_with(&format!("{COOKIE_NAME}=")));
    }

    #[test]
    fn the_secure_flag_is_dropped_only_for_a_loopback_deployment() {
        for url in ["http://127.0.0.1:8787", "http://localhost:8787", "http://[::1]:8787"] {
            assert!(!set_cookie(url, 900, "x").contains("Secure"), "{url}");
        }
        assert!(set_cookie("http://lumberroom.example", 900, "x").contains("Secure"));
    }

    #[test]
    fn a_return_path_outside_the_console_is_refused() {
        for hostile in [
            "https://evil.example/steal",
            "//evil.example/steal",
            "/oauth/authorize",
            "/admin/memory",
            "\\\\evil.example",
            "/console\\..\\oauth",
            "javascript:alert(1)",
            "",
            "   ",
        ] {
            assert_eq!(
                safe_next(Some(hostile)),
                "/console/reading",
                "{hostile:?} must not survive as a redirect target"
            );
        }
        assert_eq!(safe_next(None), "/console/reading");
    }

    #[test]
    fn a_return_path_inside_the_console_survives() {
        for good in ["/console/reading", "/console/fact/3f9c1d2a", "/console/namespace?ns=user:me"]
        {
            assert_eq!(safe_next(Some(good)), good);
        }
    }

    /// The token binds to the row a write retires, so one minted on the compose page cannot be
    /// posted back with a supersedes id typed into the form.
    #[test]
    fn a_fresh_write_and_a_replacement_sign_different_tokens() {
        assert_eq!(target_key(None), NEW_FACT);
        assert_eq!(target_key(Some("   ")), NEW_FACT);
        assert_eq!(target_key(Some("3f9c1d2a")), "3f9c1d2a");
        assert_ne!(target_key(Some("3f9c1d2a")), target_key(None));
    }

    /// A browser posts every field it drew, filled or not. `None` is what says nobody filled one
    /// in, and an empty `occurred_at` reaching the parser would come back refused.
    #[test]
    fn an_untouched_field_arrives_as_nothing_rather_than_as_an_empty_value() {
        assert_eq!(trimmed(""), None);
        assert_eq!(trimmed("  \t "), None);
        assert_eq!(trimmed("  2026-03-01 "), Some("2026-03-01"));
    }

    #[test]
    fn the_tags_line_splits_on_commas_and_keeps_nothing_empty() {
        assert_eq!(typed_tags(""), None);
        assert_eq!(typed_tags(" , ,"), None);
        assert_eq!(
            typed_tags(" deploy , postgres,"),
            Some(vec!["deploy".to_string(), "postgres".to_string()])
        );
    }

    /// The console is one door for reading and writing, and only the grant tells them apart. The
    /// compose form goes through the same principal the queue's approve button does, which is what
    /// keeps a hand-written fact and an approved proposal from following two sets of rules.
    #[test]
    fn a_typed_fact_and_an_approved_proposal_write_as_the_same_principal() {
        let a = approver();
        assert!(policy::admits(&a.write, "project:lumberroom", Sensitivity::Private));
        assert!(!a.may_delete, "nothing on this surface removes a row");
        assert!(!a.registry_write, "nothing on this surface overwrites a registry key");
    }

    #[test]
    fn a_query_value_cannot_end_the_parameter_early() {
        assert_eq!(
            encode_query("/console/namespace?ns=user:me&admin=1"),
            "/console/namespace?ns=user:me%26admin=1"
        );
        assert_eq!(encode_query("/console/fact/a b"), "/console/fact/a%20b");
    }
}
