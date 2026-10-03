//! HTTP surface. The MCP transport mounts at /mcp; health, stats, the built-in authorization server
//! and the operator endpoints sit beside it. Nothing here holds business logic: it authenticates,
//! translates, and delegates.
//!
//! The wire contract is snake_case throughout and pinned by a test. The operator endpoints exist
//! because `bin/lumberroom.mjs` calls them: their paths, methods and field names are that client's, and
//! this file matches the client rather than the other way round.

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::adapters::auth::{
    assert_writable, can_read, protected_resource_metadata, www_authenticate, Authenticator,
};
use crate::config::AuthMode;
use crate::domain::errors::DomainError;
use crate::domain::types::{Invocation, Memory, Principal, Sensitivity};
use crate::mcp::{AppState, Lumberroom, SessionId, SERVER_NAME, SERVER_VERSION};
use crate::ports::ingest::{EmissionProbe, ProposalFilter, ProposalSource, RunTotals};
use crate::ports::{AliasOrigin, ClientStats, Staleness, ToolCallStats};
// The service is aliased because `mod review` below is this module's own route file, and the two
// names would collide.
use crate::services::{
    alias, cleanup, currency, forget, graph, history, ingest, recall, registry,
    review as review_svc, review_queue, sealed, supersession, Ctx,
};

// The whole store out as one file, and back in. Its own module because both handlers carry a
// passphrase and the import needs a body limit no other route here wants.
mod archive;
// The queue, its decide path and the two ledger routes.
mod review;

pub const INVOCATION_HEADER: &str = "x-memory-invocation";

/// Per-client session correlation, recorded on every tool call.
///
/// One name, chosen here and documented here. `Mcp-Session-Id` is deliberately not read: the
/// 2026-07-28 revision removed sessions from the protocol, this server runs with
/// `legacy_session_mode = false`, and accepting that header would reintroduce the concept the
/// transport was configured to drop. Absent is fine everywhere: a client that sends nothing gets
/// per-call rows with a null session, which the stats layer buckets by hour and labels as an
/// approximation.
pub const SESSION_HEADER: &str = "x-session-id";

/// Longest session id accepted. It reaches a database column and a log line, and a client is free to
/// send anything; a bounded opaque string keeps both honest.
const MAX_SESSION_ID_CHARS: usize = 128;

#[derive(Clone)]
pub struct Http {
    pub state: Arc<AppState>,
    pub auth: Arc<dyn Authenticator>,
}

pub fn router(state: Arc<AppState>, auth: Arc<dyn Authenticator>) -> Router {
    let http = Http { state: Arc::clone(&state), auth: Arc::clone(&auth) };

    // Stateless. Sessions were removed from the protocol in the 2026-07-28 revision (SEP-2567),
    // which the PRD requires, and statelessness is what lets a redeploy leave connected clients
    // working instead of failing them with a 404 on a session that no longer exists. The default
    // is legacy session mode, and leaving it on rejects a client that posts initialize and a tool
    // call as two independent requests.
    // The config is #[non_exhaustive], so it is built from Default and then adjusted.
    let mut mcp_config = StreamableHttpServerConfig::default();
    mcp_config.legacy_session_mode = false;
    mcp_config.json_response = true;
    mcp_config.allowed_hosts = allowed_hosts(&state.cfg.public_url);

    let mcp = StreamableHttpService::new(
        {
            let state = Arc::clone(&state);
            move || Ok(Lumberroom::new(Arc::clone(&state)))
        },
        LocalSessionManager::default().into(),
        mcp_config,
    );

    // Authentication runs before the transport and puts the principal in request extensions,
    // which is where a tool handler reads it from.
    let mcp_routes = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(http.clone(), authenticate_mcp));

    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/statsz", get(statsz))
        .route("/admin/whoami", get(whoami))
        .route("/admin/recall", get(admin_recall))
        .route("/admin/registry", post(admin_registry))
        .route("/admin/registry/alias", post(admin_registry_alias))
        // What a registry key used to hold. Admin only and no MCP tool, deliberately: the registry
        // holds credential locations, and a model asking what one used to be is precisely what
        // `may_read_history` being off by default guards against.
        .route("/admin/registry/history", get(admin_registry_history))
        // A fact's timeline, and the names that denote one subject. Operator routes: no MCP tool
        // reaches either, because a model guessing a date range or inventing a rename is the
        // pattern this system refuses everywhere.
        .route("/admin/memory/{id}/history", get(admin_memory_history))
        .route(
            "/admin/alias",
            post(admin_alias_put).get(admin_alias_list).delete(admin_alias_forget),
        )
        .route("/admin/memory/{id}", get(admin_memory_get).delete(admin_memory_delete))
        .route("/admin/memory/{id}/supersede", post(admin_memory_supersede))
        .route("/admin/memory/{id}/fill-date", post(admin_memory_fill_date))
        .route("/admin/review/stale", get(admin_review_stale))
        .route("/admin/review/conflicts", get(admin_review_conflicts))
        .route("/admin/review/registry", get(admin_review_registry))
        .route("/admin/review/dates", get(admin_review_dates))
        .route("/admin/currency", post(admin_currency))
        .route("/admin/arity", get(admin_arity_list).post(admin_arity_declare))
        .route("/admin/arity/{tag}", get(admin_arity_preview).delete(admin_arity_forget))
        .route("/admin/supersession/run", post(admin_supersession_run))
        .route("/admin/graph/walk", get(admin_graph_walk))
        .route("/admin/graph/rebuild", post(admin_graph_rebuild))
        .route("/admin/export", get(admin_export))
        .route(
            "/admin/sealed",
            put(admin_sealed_put).get(admin_sealed_get).delete(admin_sealed_delete),
        )
        // Ingestion. Operator routes and nothing else: no MCP tool reaches this surface, because a
        // model that can post proposals can fill the queue and a queue the owner stops reading is
        // an approval gate in name only. Every one of them checks `may_ingest` first.
        .route("/admin/ingest/runs", post(admin_ingest_run_open))
        .route("/admin/ingest/runs/{id}", get(admin_ingest_run_report))
        .route("/admin/ingest/runs/{id}/close", post(admin_ingest_run_close))
        .route("/admin/ingest/scan", post(admin_ingest_scan))
        .route("/admin/ingest/emissions/check", post(admin_ingest_emissions_check))
        .route("/admin/ingest/proposals", post(admin_ingest_post).get(admin_ingest_list))
        .route("/admin/ingest/proposals/{id}", get(admin_ingest_show))
        .route("/admin/ingest/proposals/{id}/approve", post(admin_ingest_approve))
        .route("/admin/ingest/proposals/{id}/reject", post(admin_ingest_reject))
        .route("/admin/ingest/proposals/{id}/unreject", post(admin_ingest_unreject))
        .route(
            "/admin/ingest/watermarks",
            get(admin_ingest_watermarks).post(admin_ingest_watermark_advance),
        )
        .route("/admin/ingest/watermarks/unskip", post(admin_ingest_unskip))
        // Cleanup, behind the same capability. `may_ingest`'s own comment applies verbatim: a
        // client that can post proposals can fill a queue, and a queue the owner stops reading is
        // an approval gate in name only. Cleanup proposals name existing rows and ask to retire
        // them, which is the same trust and the same blast radius, so it takes the same grant
        // rather than a second one nobody would remember to set.
        .route("/admin/cleanup/run", post(admin_cleanup_run))
        .route("/admin/cleanup/proposals", get(admin_cleanup_list).post(admin_cleanup_post))
        .route("/admin/cleanup/proposals/{id}", get(admin_cleanup_show))
        .route("/admin/cleanup/proposals/{id}/apply", post(admin_cleanup_apply))
        .route("/admin/cleanup/proposals/{id}/reject", post(admin_cleanup_reject))
        .route("/admin/cleanup/proposals/{id}/resolve", post(admin_cleanup_resolve))
        .route("/admin/cleanup/proposals/{id}/unreject", post(admin_cleanup_unreject));

    // RFC 9728, in oauth mode as well as oidc: in oauth mode the document points at the built-in
    // server, and a hosted client reads it before it will show a login screen. Both paths, because
    // the spec inserts the resource path before the suffix and real clients also check the origin.
    if state.cfg.auth.mode.is_oauth_protected() {
        app = app
            .route("/.well-known/oauth-protected-resource", get(resource_metadata))
            .route("/.well-known/oauth-protected-resource/mcp", get(resource_metadata));
    }

    // Merged before `with_state`, because `archive::routes()` is a `Router<Http>` and still wants
    // the state this line applies. Its body-limit layer stays scoped to those two paths.
    let mut root = app
        .merge(archive::routes())
        .merge(review::routes())
        .with_state(http.clone())
        .merge(mcp_routes);

    // The built-in authorization server, at the root rather than under a prefix: the login and
    // consent forms post to the absolute paths /oauth/login and /oauth/consent.
    //
    // It also serves the RFC 8414 document at both /.well-known/oauth-authorization-server paths,
    // which is why nothing here routes them. Two routers claiming one path makes axum panic while
    // building, at boot, on every start. Its document is the one to keep: it gates
    // `registration_endpoint` on OAUTH_DCR_ENABLED, and advertising a registration endpoint that
    // answers 403 points a client at a dead end in the middle of its first handshake.
    //
    // Only in oauth mode: metadata advertising an authorization server that will not issue a token
    // is worse for a client than no metadata at all.
    if state.cfg.auth.mode == AuthMode::Oauth {
        root = root.merge(crate::authserver::router(
            Arc::clone(&state.cfg),
            Arc::clone(&state.oauth),
            Arc::clone(&auth),
        ));
    }

    // The console. Mounted in every mode: it answers with a page saying so when AUTH_MODE is not
    // oauth, which is the mode that configures the owner password its session gate needs. It claims
    // only paths under /console, which nothing else here routes, so the boot-time panic two routers
    // over one path would cause cannot happen.
    root = root.merge(crate::console::router(Arc::clone(&state)));

    root.fallback(not_found)
}

/// Host authorities the MCP transport will answer to.
///
/// rmcp validates the `Host` header against this list to stop DNS rebinding, and its default is
/// loopback only. A deployment reached at its real domain therefore answers every health check,
/// every metadata document and every operator endpoint while refusing every single MCP request with
/// a 403 the client reports as a connection failure. This is the exact shape of failure the trap log
/// warns about, and it is invisible from a local test.
///
/// `PUBLIC_URL` is already the one source for every externally visible URL, so it is also the Host a
/// real client sends. The loopback names stay, because the CLI, the session hook and the container's
/// own health check all reach the server that way. An entry with no port matches any port, which is
/// what keeps this working behind a proxy that terminates on 443 and forwards to 8787.
fn allowed_hosts(public_url: &str) -> Vec<String> {
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];

    match url::Url::parse(public_url).ok().and_then(|u| u.host_str().map(str::to_string)) {
        Some(host) if !hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) => hosts.push(host),
        Some(_) => {}
        None => tracing::warn!(
            public_url,
            "PUBLIC_URL has no host, so the MCP endpoint accepts loopback callers only"
        ),
    }
    hosts
}

async fn authenticate_mcp(
    State(http): State<Http>,
    mut req: Request,
    next: Next,
) -> Result<Response, Response> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let invocation =
        Invocation::parse(req.headers().get(INVOCATION_HEADER).and_then(|v| v.to_str().ok()));
    let session = session_id(req.headers());

    match http.auth.authenticate(header.as_deref()).await {
        Ok(principal) => {
            req.extensions_mut().insert(principal);
            req.extensions_mut().insert(invocation);
            req.extensions_mut().insert(session);
            Ok(next.run(req).await)
        }
        Err(e) => Err(unauthorized(&http, header.as_deref(), &e)),
    }
}

/// A 200 carrying an error body is silently ignored by hosted Claude clients, which then fail
/// before showing a login screen. Claude Code's fallback probing hides that, so this must be a
/// real 401 with the challenge header even though one client tolerates its absence.
///
/// The challenge carries no error code when the caller presented no credential. RFC 6750 §3 wants a
/// bare challenge there, and a client that has never authenticated reads `error="invalid_token"` as
/// "the token I hold is bad" and can loop refreshing a token it does not have. The decision is made
/// from the header rather than from the error message: the authenticator chain's wording is not a
/// contract anything pins, and both callers already hold the header.
fn unauthorized(http: &Http, header: Option<&str>, e: &DomainError) -> Response {
    let presented = header.is_some_and(|h| !h.trim().is_empty());
    let error = if presented { "invalid_token" } else { "" };
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::WWW_AUTHENTICATE, www_authenticate(&http.state.cfg, error))],
        Json(serde_json::json!({ "error": "unauthorized", "detail": e.client_message() })),
    )
        .into_response()
}

/// Opaque to this server: it is correlation, never identity, and nothing authorizes on it.
fn session_id(headers: &HeaderMap) -> SessionId {
    let raw = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(MAX_SESSION_ID_CHARS).collect::<String>());
    SessionId(raw)
}

async fn resolve(http: &Http, headers: &HeaderMap) -> Result<Principal, Response> {
    let header = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    http.auth.authenticate(header).await.map_err(|e| unauthorized(http, header, &e))
}

/// The context for an operator call. `Invocation::Cli` because these paths are not a model's: they
/// are reached by `lumberroom`, and the instrumentation has to be able to tell the two apart.
fn ctx_for(http: &Http, principal: Principal, session: SessionId) -> Ctx {
    Ctx {
        cfg: Arc::clone(&http.state.cfg),
        repos: http.state.repos.clone(),
        embedder: Arc::clone(&http.state.embedder),
        keys: http.state.keys.clone(),
        kek_verified: http.state.kek_verified,
        principal,
        invocation: Invocation::Cli,
        session_id: session.0,
    }
}

/// Authenticate, then build the context. Every operator endpoint starts here, including the
/// destructive ones: the grant check that follows is the service's, and it is the same one a write
/// goes through.
async fn authed(http: &Http, headers: &HeaderMap) -> Result<Ctx, Response> {
    let principal = resolve(http, headers).await?;
    Ok(ctx_for(http, principal, session_id(headers)))
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true, "name": SERVER_NAME, "version": SERVER_VERSION }))
}

/// The part of the readiness answer that needs no round trip, split out so a test can pin the key
/// set without a database.
///
/// The build stamp is here rather than beside `db_ms` on purpose: a 503 body carries it too. When a
/// container is failing, the first question is whether it is even running the code you are reading,
/// and `docker restart` reuses the container's original image while a rebuilt one sits on disk.
/// `scripts/deploy-check.sh` compares `build_sha` against what the caller built. `unknown` is
/// honest and expected: a plain `cargo build` passes no stamp.
fn static_checks(
    embedder: &str,
    embedder_degraded: bool,
    auth_mode: &str,
    kek_provider: &str,
    kek_verified: bool,
) -> serde_json::Value {
    serde_json::json!({
        "embedder": embedder,
        "embedder_degraded": embedder_degraded,
        "auth_mode": auth_mode,
        "kek_provider": kek_provider,
        // A server whose KEK did not verify refuses every private write and looks healthy
        // otherwise. Reported here so the refusal is visible before somebody hits it.
        "kek_verified": kek_verified,
        "build_sha": crate::build_info::SHA,
        "build_tag": crate::build_info::TAG,
        "built_at": crate::build_info::BUILT_AT,
    })
}

async fn readyz(State(http): State<Http>) -> Response {
    let mut checks = static_checks(
        &http.state.embedder.id(),
        http.state.degraded_embedder,
        http.state.cfg.mode_str(),
        http.state.cfg.crypto.provider.as_str(),
        http.state.kek_verified,
    );

    let started = std::time::Instant::now();
    if let Err(e) = http.state.repos.tool_calls.ping().await {
        checks["error"] = serde_json::json!(e.client_message());
        return (StatusCode::SERVICE_UNAVAILABLE, Json(merge_ok(checks, false))).into_response();
    }
    checks["db_ms"] = serde_json::json!(started.elapsed().as_millis());
    checks["embedding_dim"] = serde_json::json!(http.state.embedder.dim());

    // An unverified KEK does not make the server unready. Open reads and writes work, and reporting
    // 503 would take a store that is serving most of its traffic out of rotation.
    let ok = !http.state.degraded_embedder;
    let status = if ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(merge_ok(checks, ok))).into_response()
}

fn merge_ok(mut v: serde_json::Value, ok: bool) -> serde_json::Value {
    if let Some(map) = v.as_object_mut() {
        map.insert("ok".into(), serde_json::json!(ok));
    }
    v
}

#[derive(Deserialize)]
struct StatsQuery {
    hours: Option<i64>,
    /// `client` switches to per-client rates. Absent keeps the per-tool shape, which the CLI and
    /// any existing dashboard read.
    by: Option<String>,
}

/// Instrumentation, scoped to the caller unless the caller holds the whole store.
///
/// Authenticating is not authorizing, and this route proved it twice over. Every row of `by_tool`
/// carries the `client` that made the calls, `by_client` is a list of every client that has called
/// anything, and `staleness` counts every row in the tenant. A token granted one namespace at open
/// therefore learned which other surfaces the owner runs, how often each one calls, how often each one
/// fails, and how large the store it cannot read is. None of that is content, and all of it is the
/// shape of somebody else's deployment.
///
/// `reads_whole_store` is the gate: a grant of `*` at `sealed`, which is the owner's own client and
/// the only credential that could already read every row these numbers count. Everything narrower gets
/// its own rows, no tenant totals, and `scope: "self"` saying so, because a report that silently means
/// something different than it did yesterday is worse than one that admits its bounds.
async fn statsz(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<StatsQuery>,
) -> Response {
    let principal = match resolve(&http, &headers).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let hours = q.hours.unwrap_or(168).clamp(1, 24 * 90);
    let whole = crate::services::reads_whole_store(&principal);

    if q.by.as_deref() == Some("client") {
        // Best effort, like the review queue's: a per-client report that fails because one summary
        // statistic did not compute is a report nobody runs twice. Skipped outright for a narrow
        // caller, so the round trip is not spent on a number that will be dropped either way.
        let staleness = match whole {
            false => None,
            true => match http.state.repos.memories.staleness(&http.state.cfg.tenant_id).await {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(error = %e.log_message(), "could not compute staleness for stats");
                    None
                }
            },
        };
        return match http.state.repos.tool_calls.client_stats(hours).await {
            Ok(rows) => Json(client_stats_body(rows, staleness, hours, &principal)).into_response(),
            Err(e) => internal(&e, "stats_failed"),
        };
    }

    match http.state.repos.tool_calls.stats(hours).await {
        Ok(rows) => Json(tool_stats_body(rows, hours, &principal)).into_response(),
        Err(e) => internal(&e, "stats_failed"),
    }
}

/// Which client's rows this caller may be shown, and the label for the bound.
fn stats_scope(principal: &Principal) -> (bool, &'static str) {
    let whole = crate::services::reads_whole_store(principal);
    (whole, if whole { "tenant" } else { "self" })
}

/// The per-tool report, scoped and totalled.
///
/// Pure, and separate from the route, because the ordering is the whole fix: the rows are narrowed
/// before the totals are summed. Totals over every client sitting beside rows from one would hand back
/// the same cross-client call volume the rows had just stopped naming.
fn tool_stats_body(
    rows: Vec<ToolCallStats>,
    hours: i64,
    principal: &Principal,
) -> serde_json::Value {
    let (whole, scope) = stats_scope(principal);
    let rows: Vec<ToolCallStats> =
        rows.into_iter().filter(|r| whole || r.client == principal.client).collect();
    let calls: i64 = rows.iter().map(|r| r.calls).sum();
    let failures: i64 = rows.iter().map(|r| r.failures).sum();
    let unprompted: i64 = rows.iter().map(|r| r.unprompted).sum();
    let rate = |n: i64| {
        if calls > 0 {
            Some((n as f64 / calls as f64 * 1000.0).round() / 1000.0)
        } else {
            None
        }
    };
    serde_json::json!({
        "window_hours": hours,
        "scope": scope,
        "totals": {
            "calls": calls,
            "failures": failures,
            "unprompted": unprompted,
            "unprompted_rate": rate(unprompted),
            "failure_rate": rate(failures),
        },
        // The wire contract stays snake_case: the CLI reads p50_ms, and a rename on the domain side
        // once turned every latency into "-ms" with nothing failing.
        "by_tool": rows,
    })
}

/// The per-client report. One row per client is the disclosure here, so a narrow caller gets its own.
///
/// The staleness argument is dropped rather than trusted. The route already skips the query for a
/// narrow caller; doing it again here means the field cannot come back through a later edit that
/// fetches the number for its own reasons.
fn client_stats_body(
    rows: Vec<ClientStats>,
    staleness: Option<Staleness>,
    hours: i64,
    principal: &Principal,
) -> serde_json::Value {
    let (whole, scope) = stats_scope(principal);
    let rows: Vec<ClientStats> =
        rows.into_iter().filter(|r| whole || r.client == principal.client).collect();
    serde_json::json!({
        "window_hours": hours,
        "scope": scope,
        "by_client": rows,
        "staleness": if whole { staleness } else { None },
    })
}

async fn whoami(State(http): State<Http>, headers: HeaderMap) -> Response {
    let principal = match resolve(&http, &headers).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    Json(serde_json::json!({
        "client": principal.client,
        "mode": principal.mode,
        "token_fingerprint": principal.token_id,
        "read": principal.read,
        "write": principal.write,
        "registry_write": principal.registry_write,
        "sealed_capable": principal.sealed_capable,
        "may_delete": principal.may_delete,
        "may_ingest": principal.may_ingest,
        // Reported because it gates two tools. Omitted, the only way to learn whether a credential
        // holds it is to notice memory_history missing from a list nobody reads closely.
        "may_read_history": principal.may_read_history,
        "scopes": principal.scopes,
        "tenant": http.state.cfg.tenant_id,
        "embedder": http.state.embedder.id(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RecallQuery {
    sample: Option<i64>,
    k: Option<i64>,
}

async fn admin_recall(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<RecallQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let sample = q.sample.unwrap_or(25).clamp(1, 500);
    let k = q.k.unwrap_or(10).clamp(1, 50);
    match recall::measure(&ctx, sample, k).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => domain_error(&e, "recall_failed"),
    }
}

#[derive(Deserialize)]
struct RegistryWrite {
    namespace: String,
    kind: String,
    key: String,
    value: serde_json::Value,
    /// Optional, and the CLI does not send it. The namespace default applies when it is absent and
    /// a request can only raise the level, never lower it.
    #[serde(default)]
    sensitivity: Option<String>,
}

/// Registry writes are an operator action rather than a fifth tool: the registry is where
/// credential locations live.
///
/// The service owns provenance, canonical-key enforcement, the review interval and the
/// rejected-guess alias. The transport used to build a `Provenance` itself, which made this a second
/// path to the same table with its own rules.
async fn admin_registry(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<RegistryWrite>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match registry::set(
        &ctx,
        &body.namespace,
        &body.kind,
        body.key.trim(),
        &body.value,
        body.sensitivity.as_deref(),
        None,
    )
    .await
    {
        Ok(result) => Json(result).into_response(),
        Err(e) => domain_error(&e, "registry_write_failed"),
    }
}

#[derive(Deserialize)]
struct AliasWrite {
    namespace: String,
    kind: String,
    alias_key: String,
    canonical: String,
}

/// A hand-written redirect from a key somebody will type to the key that holds the fact.
///
/// The origin is fixed to `Manual` here and never taken from the request: `RejectedWrite` is a
/// model's guess and loses to an existing mapping, and a caller able to choose the origin could
/// promote a guess to a decision.
async fn admin_registry_alias(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<AliasWrite>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if !ctx.principal.registry_write {
        return forbidden(&DomainError::forbidden(format!(
            "client {} may not write to the registry",
            ctx.principal.client
        )));
    }
    let namespace = match crate::domain::namespaces::normalize(&body.namespace) {
        Ok(ns) => ns,
        Err(e) => return domain_error(&e, "alias_failed"),
    };
    // An alias is a registry write into this namespace, so it needs the same write grant a value
    // does, at the level the namespace classifies at.
    let level = ctx.cfg.policy.defaults.resolve_for_write(&namespace, None);
    if let Err(e) = assert_writable(&ctx.principal, &namespace, level) {
        return forbidden(&e);
    }
    let kind = match crate::domain::canonical::validate_kind(&body.kind) {
        Ok(k) => k,
        Err(e) => return domain_error(&e, "alias_failed"),
    };

    match ctx
        .repos
        .registry
        .add_alias(
            ctx.tenant(),
            &namespace,
            &kind,
            body.alias_key.trim(),
            body.canonical.trim(),
            AliasOrigin::Manual,
        )
        .await
    {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "namespace": namespace,
            "kind": kind,
            "alias_key": body.alias_key.trim(),
            "canonical": body.canonical.trim(),
        }))
        .into_response(),
        Err(e) => domain_error(&e, "alias_failed"),
    }
}

/// One memory, for a preview before a delete.
///
/// One 404 covers "no such row" and "not yours": naming a namespace the caller cannot read tells it
/// that namespace exists.
/// One fact and every version of it, oldest first.
///
/// Behind `may_read_history`, which `services::history` checks. The check lives there because it
/// was written here and in the console reader once each, and the two disagreed: this answered 403
/// while the console quietly returned one version.
#[derive(Deserialize)]
struct RegistryHistoryQuery {
    kind: String,
    key: String,
    /// Omitted runs the same precedence walk `registry_get` runs, so history answers from wherever
    /// the value would have answered from.
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn admin_registry_history(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<RegistryHistoryQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match registry::history(
        &ctx,
        &q.kind,
        &q.key,
        q.namespace.as_deref(),
        q.project.as_deref(),
        q.limit,
    )
    .await
    {
        Ok(result) => Json(result).into_response(),
        Err(e) => domain_error(&e, "registry_history_failed"),
    }
}

async fn admin_memory_history(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let uuid = match uuid::Uuid::parse_str(id.trim()) {
        Ok(u) => u,
        Err(_) => {
            return domain_error(
                &DomainError::validation(format!("{id:?} is not a memory id")),
                "history_failed",
            )
        }
    };
    // The namespace is still validated so a malformed one is refused, and it no longer bounds the
    // walk. A chain may cross a namespace, and the grant filters each version rather than the walk
    // stopping at the first one it cannot read.
    if let Some(ns) = q.namespace.as_deref() {
        if let Err(e) = crate::domain::namespaces::normalize(ns) {
            return domain_error(&e, "history_failed");
        }
    }
    match history::of(&ctx, uuid).await {
        Ok(timeline) => Json(timeline).into_response(),
        Err(e) => domain_error(&e, "history_failed"),
    }
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// Optional, and validated when present. The walk crosses namespaces by design, so the answer
    /// does not depend on this; demanding it asked the caller for something that changed nothing.
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct AliasPutBody {
    namespace: String,
    alias: String,
    canonical: String,
    #[serde(default)]
    since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    origin: Option<String>,
}

async fn admin_alias_put(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<AliasPutBody>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match alias::put(
        &ctx,
        http.state.aliases.as_ref(),
        &body.namespace,
        &body.alias,
        &body.canonical,
        body.since,
        body.until,
        body.origin.as_deref(),
    )
    .await
    {
        Ok(record) => Json(record).into_response(),
        Err(e) => domain_error(&e, "alias_failed"),
    }
}

#[derive(Deserialize)]
struct AliasQuery {
    #[serde(default)]
    namespace: Option<String>,
}

async fn admin_alias_list(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<AliasQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match alias::list(&ctx, http.state.aliases.as_ref(), q.namespace.as_deref()).await {
        Ok(rows) => Json(serde_json::json!({ "aliases": rows })).into_response(),
        Err(e) => domain_error(&e, "alias_failed"),
    }
}

#[derive(Deserialize)]
struct AliasForgetBody {
    namespace: String,
    alias: String,
}

async fn admin_alias_forget(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<AliasForgetBody>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match alias::forget(&ctx, http.state.aliases.as_ref(), &body.namespace, &body.alias).await {
        Ok(done) => Json(serde_json::json!({ "forgotten": done })).into_response(),
        Err(e) => domain_error(&e, "alias_failed"),
    }
}

async fn admin_memory_get(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match visible_memory(&ctx, &id).await {
        Ok(Some(m)) => Json(m).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not_found", "detail": "no such memory" })),
        )
            .into_response(),
        Err(e) => domain_error(&e, "memory_lookup_failed"),
    }
}

/// Delete, through the same service the tool uses.
///
/// `may_delete` lives in `forget::by_id`, so this endpoint cannot become a laxer second path to the
/// same mutation. That is why it is not a `repos.memories.delete` call.
async fn admin_memory_delete(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match forget::by_id(&ctx, &id, Some("deleted through lumberroom"), false).await {
        Ok(outcome) => Json(serde_json::json!({
            "deleted": outcome.count > 0,
            "count": outcome.count,
            "rows": outcome.rows,
            "revived": outcome.revived,
            "spliced": outcome.spliced,
            "blocked": outcome.blocked,
        }))
        .into_response(),
        Err(e) => domain_error(&e, "delete_failed"),
    }
}

#[derive(Deserialize)]
struct SupersedeBody {
    new_id: String,
}

/// Retire one memory in favour of another. A target that is already superseded comes back as 409
/// naming the live head, which is what makes a retry against the head possible.
async fn admin_memory_supersede(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SupersedeBody>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match review_svc::supersede(&ctx, &id, &body.new_id).await {
        Ok(resolved) => Json(resolved).into_response(),
        Err(e) => domain_error(&e, "supersede_failed"),
    }
}

/// The currency measure. Coverage always; accuracy when the caller posts cases.
///
/// POST rather than GET because the fixture is the body. A GET with the cases in a query string
/// would put the owner's own questions in every proxy log between here and the browser.
async fn admin_currency(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<CurrencyBody>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match currency::run(&ctx, &body.cases).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => domain_error(&e, "currency_failed"),
    }
}

#[derive(Deserialize, Default)]
struct CurrencyBody {
    /// Absent runs coverage alone, which is the number 0014 wants first and needs no fixture.
    #[serde(default)]
    cases: Vec<currency::CurrencyCase>,
}

#[derive(Deserialize)]
struct WalkQuery {
    q: String,
    /// A node with more readable edges than this is skipped. Default 60: high enough that ordinary
    /// subjects survive, low enough that a filing-convention tag does not drag the whole store in.
    #[serde(default)]
    degree_cap: Option<i64>,
    /// Walk whatever the router says. For calibration, not for callers.
    #[serde(default)]
    force: Option<bool>,
}

/// Walk out from what search finds. Reads only.
async fn admin_graph_walk(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<WalkQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let cap = q.degree_cap.unwrap_or(60);
    // `force` skips the router. It exists for calibration: comparing what a walk would have found
    // against what the router decided needs a way to walk a question the router refused.
    let result = if q.force.unwrap_or(false) {
        graph::walk(&ctx, &q.q, cap).await
    } else {
        graph::routed(&ctx, &q.q, cap).await
    };
    match result {
        Ok(w) => Json(w).into_response(),
        Err(e) => domain_error(&e, "graph_failed"),
    }
}

/// Rebuild the edge table from structure. Idempotent, calls no model.
async fn admin_graph_rebuild(State(http): State<Http>, headers: HeaderMap) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match graph::rebuild(&ctx).await {
        Ok(total) => Json(serde_json::json!({ "edges": total })).into_response(),
        Err(e) => domain_error(&e, "graph_failed"),
    }
}

#[derive(Deserialize)]
struct ArityBody {
    tag: String,
    /// 'single' or 'many'. Refused rather than defaulted: a default here decides whether facts get
    /// hidden, and the owner is the only one who knows.
    arity: String,
    #[serde(default)]
    note: Option<String>,
}

/// Declare a subject's arity. The preview is a separate read, so a caller can look first.
async fn admin_arity_declare(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<ArityBody>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(arity) = crate::ports::cleanup::Arity::parse(&body.arity) else {
        return domain_error(
            &DomainError::validation(format!("arity {:?} is not one of single, many", body.arity)),
            "arity_failed",
        );
    };
    match supersession::declare(
        &ctx,
        http.state.cleanup.as_ref(),
        &body.tag,
        arity,
        body.note.as_deref(),
    )
    .await
    {
        Ok(()) => Json(serde_json::json!({ "tag": body.tag, "arity": body.arity })).into_response(),
        Err(e) => domain_error(&e, "arity_failed"),
    }
}

async fn admin_arity_list(State(http): State<Http>, headers: HeaderMap) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match supersession::declarations(&ctx, http.state.cleanup.as_ref()).await {
        Ok(rows) => Json(serde_json::json!({ "rows": rows })).into_response(),
        Err(e) => domain_error(&e, "arity_failed"),
    }
}

/// What declaring this tag `single` would end. Reads only.
async fn admin_arity_preview(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(tag): Path<String>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match supersession::preview(&ctx, http.state.cleanup.as_ref(), &tag).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => domain_error(&e, "arity_failed"),
    }
}

async fn admin_arity_forget(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(tag): Path<String>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match supersession::forget(&ctx, http.state.cleanup.as_ref(), &tag).await {
        Ok(true) => Json(serde_json::json!({ "forgot": tag })).into_response(),
        Ok(false) => domain_error(
            &DomainError::not_found(format!("no declaration for {tag}")),
            "arity_failed",
        ),
        Err(e) => domain_error(&e, "arity_failed"),
    }
}

/// Run the supersession pass over declared subjects. Proposes, never applies.
async fn admin_supersession_run(State(http): State<Http>, headers: HeaderMap) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match supersession::run(&ctx, http.state.cleanup.as_ref()).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => domain_error(&e, "supersession_failed"),
    }
}

#[derive(Deserialize)]
struct DateReviewQuery {
    limit: Option<i64>,
}

/// Undated live rows whose own text names a day. A review list, never a filler.
async fn admin_review_dates(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<DateReviewQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match review_svc::date_candidates(&ctx, q.limit).await {
        Ok(rows) => Json(serde_json::json!({ "rows": rows })).into_response(),
        Err(e) => domain_error(&e, "date_review_failed"),
    }
}

#[derive(Deserialize)]
struct FillDateBody {
    /// `YYYY-MM-DD` or a full RFC 3339 instant, the two forms `memory_write` takes.
    occurred_at: String,
}

/// Fill a start date the row never carried. Refuses to move one it already has.
async fn admin_memory_fill_date(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<FillDateBody>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let when = match crate::mcp::tools::parse_occurred_at(&body.occurred_at) {
        Ok(w) => w,
        Err(e) => return domain_error(&e, "fill_date_failed"),
    };
    match review_svc::fill_date(&ctx, &id, when).await {
        Ok(resolved) => Json(resolved).into_response(),
        Err(e) => domain_error(&e, "fill_date_failed"),
    }
}

#[derive(Deserialize)]
struct StaleQuery {
    days: Option<i32>,
    limit: Option<i64>,
}

/// Live rows never retrieved and older than `days`. A review list, never a reaper.
///
/// Reads `review_queue::queue` filtered to one source, so the grant rule and the live-row
/// predicate live in one place instead of two. `days` and `limit` clamp inside that call now,
/// not here.
async fn admin_review_stale(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<StaleQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    // This route's own default, kept: a client that sent no limit asked for 25 before the queue
    // existed and still means 25.
    let query = review_queue::QueueQuery {
        sources: Some(vec![review_queue::Source::Stale]),
        limit: Some(q.limit.unwrap_or(25)),
        offset: Some(0),
        days: q.days,
        min_similarity: None,
    };
    let envelope = match review_queue::queue(&ctx, &http.state.proposals, query).await {
        Ok(e) => e,
        Err(e) => return domain_error(&e, e.code().unwrap_or("review_failed")),
    };
    // The legacy shape drops unopened rows, as `readable_rows` did; `/admin/review/queue` is where
    // an unreadable row surfaces with `opened: false`.
    let rows: Vec<_> =
        envelope.items.iter().flat_map(|item| item.rows.iter()).filter(|r| r.opened).collect();
    Json(serde_json::json!({ "days": envelope.stale_days, "rows": rows })).into_response()
}

#[derive(Deserialize)]
struct ConflictQuery {
    min_similarity: Option<f64>,
    limit: Option<i64>,
}

/// Near-duplicate live pairs. Both halves have to be visible to this caller: showing one side
/// invites a supersede against a row the caller cannot see.
///
/// Reads `review_queue::queue` filtered to one source. `min_similarity` and `limit` clamp inside
/// that call now; each pair still prints the same five fields `conflict_side` used to.
async fn admin_review_conflicts(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<ConflictQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    // This route's own default, kept, as on the stale route above.
    let query = review_queue::QueueQuery {
        sources: Some(vec![review_queue::Source::Conflict]),
        limit: Some(q.limit.unwrap_or(25)),
        offset: Some(0),
        days: None,
        min_similarity: q.min_similarity,
    };
    let envelope = match review_queue::queue(&ctx, &http.state.proposals, query).await {
        Ok(e) => e,
        Err(e) => return domain_error(&e, e.code().unwrap_or("review_failed")),
    };

    let pairs: Vec<_> = envelope
        .items
        .iter()
        .filter_map(|item| {
            let [older, newer] = item.rows.as_slice() else { return None };
            Some(serde_json::json!({
                "similarity": item.similarity,
                "older": {
                    "id": older.id,
                    "namespace": older.namespace,
                    "content": older.content,
                    "sensitivity": older.sensitivity,
                    "created_at": older.created_at,
                },
                "newer": {
                    "id": newer.id,
                    "namespace": newer.namespace,
                    "content": newer.content,
                    "sensitivity": newer.sensitivity,
                    "created_at": newer.created_at,
                },
                "resolve_with": format!("lumberroom supersede {} {}", older.id, newer.id),
            }))
        })
        .collect();
    Json(serde_json::json!({ "min_similarity": envelope.min_similarity, "pairs": pairs }))
        .into_response()
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

/// Registry entries past their review date, and keys that never matched the canonical scheme.
async fn admin_review_registry(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<LimitQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(25).clamp(1, 500);

    let due =
        match ctx.repos.registry.due_for_review(ctx.tenant(), limit, &ctx.principal.read).await {
            Ok(rows) => rows,
            Err(e) => return domain_error(&e, "review_failed"),
        };
    let non_canonical = match ctx.repos.registry.non_canonical(ctx.tenant()).await {
        Ok(rows) => rows,
        Err(e) => return domain_error(&e, "review_failed"),
    };

    // Neither port takes ceilings: an operator surface is not a grant, so each entry is checked at
    // its stored level before it appears.
    let keep = |e: &crate::domain::types::RegistryEntry| {
        can_read(&ctx.principal, &e.namespace, e.sensitivity)
    };
    let due: Vec<_> = due.into_iter().filter(keep).collect();
    let non_canonical: Vec<_> =
        non_canonical.into_iter().filter(keep).take(limit as usize).collect();

    Json(serde_json::json!({ "due_for_review": due, "non_canonical": non_canonical }))
        .into_response()
}

#[derive(Deserialize)]
struct ExportQuery {
    max_sensitivity: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Rows for the Obsidian mirror, one page at a time.
///
/// The ceiling narrows and never widens: `EXPORT_MAX_SENSITIVITY` bounds the request, and `sealed`
/// is excluded outright because that content lives in another table and has no plaintext to render.
/// Turning the export on for private content is a deployment decision, not a per-call argument.
///
/// The grant is applied after the page comes back, because `list_for_export` takes no ceilings. A
/// restricted caller can therefore receive a short page while more rows exist, which stops a client
/// that pages until it sees a short page. Harmless for the owner's own credential, which is the only
/// one that exports today, and named in the return value rather than papered over.
async fn admin_export(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<ExportQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let configured = ctx.cfg.quality.export_max_sensitivity;
    let ceiling = match q.max_sensitivity.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => match Sensitivity::parse(raw) {
            Some(asked) => asked.min(configured),
            None => {
                return domain_error(
                    &DomainError::validation(format!(
                        "max_sensitivity {raw:?} is not one of open, private, sealed"
                    )),
                    "export_failed",
                )
            }
        },
        None => configured,
    }
    .min(Sensitivity::Private);

    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);

    let rows = match ctx.repos.memories.list_for_export(ctx.tenant(), ceiling, limit, offset).await
    {
        Ok(r) => r,
        Err(e) => return domain_error(&e, "export_failed"),
    };
    Json(serde_json::json!({
        "max_sensitivity": ceiling,
        "limit": limit,
        "offset": offset,
        "rows": readable_rows(&ctx, rows).await,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SealedPutBody {
    namespace: String,
    key_hmac: String,
    /// Base64, and opaque here by construction: this server holds no key for it.
    ciphertext: String,
    alg: String,
}

async fn admin_sealed_put(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<SealedPutBody>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    // `source_client` arrives in the body from the CLI and is ignored: attribution is the
    // authenticated client, not a self-declared label.
    match sealed::put(&ctx, &body.namespace, &body.key_hmac, &body.ciphertext, &body.alg).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => domain_error(&e, "seal_failed"),
    }
}

#[derive(Deserialize)]
struct SealedQuery {
    namespace: String,
    key_hmac: String,
}

/// One sealed item, as ciphertext. Flattened to the item's own fields because that is what the
/// client reads, and 404 rather than `found:false` so a miss is a status code.
async fn admin_sealed_get(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<SealedQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match sealed::get(&ctx, &q.key_hmac, Some(vec![q.namespace.clone()])).await {
        Ok(result) => match result.item {
            Some(item) => {
                let mut json = serde_json::to_value(&item).unwrap_or_default();
                if let Some(map) = json.as_object_mut() {
                    // Whether these bytes are of any use to this client. The bytes are the same
                    // either way; this is the honest label on them.
                    map.insert("decryptable".into(), serde_json::json!(result.decryptable));
                }
                Json(json).into_response()
            }
            None => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "not_found", "detail": "nothing sealed there" })),
            )
                .into_response(),
        },
        Err(e) => domain_error(&e, "unseal_failed"),
    }
}

/// Deleting a sealed item removes the only copy: the server holds no key and cannot help recover it.
/// Same grant flag as any other delete.
async fn admin_sealed_delete(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<SealedQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match forget::sealed_item(
        &ctx,
        &q.namespace,
        &q.key_hmac,
        Some("deleted through lumberroom"),
        false,
    )
    .await
    {
        Ok(outcome) => {
            Json(serde_json::json!({ "deleted": outcome.count > 0, "count": outcome.count }))
                .into_response()
        }
        Err(e) => domain_error(&e, "unseal_failed"),
    }
}

// ---- ingestion ---------------------------------------------------------------------------------
//
// Thirteen routes, no tool behind any of them. The queue is the reason: a model that can post a
// proposal can fill it, and an owner who stops reading the queue is approving nothing. So the only
// thing that creates a proposal is a process the owner started, and `may_ingest` is what says which
// process that is.
//
// Approval carries no checks of its own. It calls `services::ingest::approve`, which calls
// `services::write::run`, which is where the credentials-namespace refusal, the classification
// table, the ceiling and grant checks, the tripwire, duplicate collapse, the dedupe bands and
// supersession validation all live. A handler that inserted a memory here would be a second write
// path with none of them.

/// Authenticate, then refuse a client the owner has not granted ingestion to.
///
/// The refusal names the capability, because the fix is a grant the owner can edit and an error
/// that says only "forbidden" sends him to read a log for it. Same shape as the delete flag: the
/// header that tells `lumberroom ingest` apart from a model is one a model can set for free, so the grant
/// is the boundary rather than the caller's own claim about itself.
async fn ingest_ctx(http: &Http, headers: &HeaderMap) -> Result<Ctx, Response> {
    let ctx = authed(http, headers).await?;
    if !ctx.principal.may_ingest {
        return Err(forbidden(&DomainError::forbidden(format!(
            "client {} may not ingest: the grant carries no may_ingest. Ingestion fills a queue \
             the owner has to read, so it is off unless he granted it. Set \"mayIngest\":true on \
             this client in AUTH_TOKENS, or give it the full profile at the consent screen.",
            ctx.principal.client
        ))));
    }
    Ok(ctx)
}

/// A path id, or a 400 that says so. `Uuid::parse_str` failing is a caller typing an id, not a
/// missing row, and answering 404 there sends them looking for a proposal that was never named.
fn ingest_id(raw: &str) -> Result<uuid::Uuid, Response> {
    uuid::Uuid::parse_str(raw.trim()).map_err(|_| {
        domain_error(
            &DomainError::validation(format!("{raw:?} is not a proposal id")),
            "ingest_failed",
        )
    })
}

#[derive(Deserialize)]
struct RunOpen {
    extractor: String,
    /// Roots, project filter and date window, as the client resolved them. Free-form because the
    /// scope of a run is a client concern and a column per option would be a migration per flag.
    #[serde(default)]
    scope: serde_json::Value,
}

async fn admin_ingest_run_open(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<RunOpen>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match ingest::open_run(&ctx, http.state.ingest.as_ref(), &body.extractor, body.scope).await {
        Ok(id) => Json(serde_json::json!({ "run_id": id })).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

/// Stamp a run's counters and set `finished_at`.
///
/// A run with no `finished_at` is a run still in flight, and §7.3 uses that to bound a fence a
/// later plan has to resolve. Without this route a run stayed open forever and every fence it
/// opened stayed unbounded.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RunClose {
    files_seen: i32,
    files_skipped: serde_json::Value,
    entries_seen: i64,
    entries_excluded: serde_json::Value,
    unknown_types: serde_json::Value,
    spans_cut: i32,
    chunks: i32,
    chunks_missing: i32,
    chunks_failed: i32,
    files_held_back: serde_json::Value,
    fenced_entries: i32,
    fences_unclosed: i32,
    proposals_new: i32,
    proposals_reinforced: i32,
    confirmations: i32,
    traversal_capped: bool,
    artifact_sessions: serde_json::Value,
}

async fn admin_ingest_run_close(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<RunClose>>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let id = match ingest_id(&id) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let totals = RunTotals {
        files_seen: b.files_seen,
        files_skipped: nonnull(b.files_skipped, serde_json::json!({})),
        entries_seen: b.entries_seen,
        entries_excluded: nonnull(b.entries_excluded, serde_json::json!({})),
        unknown_types: nonnull(b.unknown_types, serde_json::json!({})),
        spans_cut: b.spans_cut,
        chunks: b.chunks,
        chunks_missing: b.chunks_missing,
        chunks_failed: b.chunks_failed,
        files_held_back: nonnull(b.files_held_back, serde_json::json!([])),
        fenced_entries: b.fenced_entries,
        fences_unclosed: b.fences_unclosed,
        proposals_new: b.proposals_new,
        proposals_reinforced: b.proposals_reinforced,
        confirmations: b.confirmations,
        traversal_capped: b.traversal_capped,
        artifact_sessions: nonnull(b.artifact_sessions, serde_json::json!([])),
    };
    match ingest::close_run(&ctx, http.state.ingest.as_ref(), id, totals).await {
        Ok(()) => Json(serde_json::json!({ "closed": true })).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

/// A jsonb column with a NOT NULL default takes the default rather than a JSON null, so an omitted
/// counter has to become the empty shape here.
fn nonnull(v: serde_json::Value, fallback: serde_json::Value) -> serde_json::Value {
    if v.is_null() {
        fallback
    } else {
        v
    }
}

async fn admin_ingest_run_report(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let id = match ingest_id(&id) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match ingest::run_report(&ctx, http.state.ingest.as_ref(), id).await {
        Ok(Some(report)) => Json(report).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not_found", "detail": "no such run" })),
        )
            .into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

#[derive(Deserialize)]
struct ScanBody {
    texts: Vec<String>,
}

/// The tripwire, for a client that cannot call it in process.
///
/// Rule names only, in the order the texts arrived, `null` where nothing fired. The matched text
/// never travels: this answer goes back to a client that is about to write it into a run report.
async fn admin_ingest_scan(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<ScanBody>,
) -> Response {
    if let Err(r) = ingest_ctx(&http, &headers).await {
        return r;
    }
    Json(serde_json::json!({ "rules": ingest::scan(&body.texts) })).into_response()
}

/// One candidate fact, asking whether the store handed this content out before the transcript
/// recorded it.
///
/// `content` is the only field to send. The server hashes it with the same function that produces
/// a proposal's fingerprint, which is the only reason the two can ever meet; a client that computed
/// its own hash would be the second normaliser this layer was already built wrong by once, and a
/// caller-supplied digest is also the shape an offline guess arrives in.
#[derive(Deserialize)]
struct ProbeBody {
    content: String,
    /// The source span's timestamp. The direction is the whole test, so an absent one is checked
    /// against now, which is the strictest reading available.
    #[serde(default)]
    observed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize)]
struct EmissionsCheck {
    probes: Vec<ProbeBody>,
}

/// The probe digest is computed here from the content and never accepted from the client, through
/// the same keyed `Digester` search and bootstrap record with. A second function would give the
/// lookup one scheme and the store another, and the check would answer false forever.
async fn probes(ctx: &Ctx, bodies: &[ProbeBody]) -> Result<Vec<EmissionProbe>, DomainError> {
    if bodies.len() > ingest::MAX_EMISSION_PROBES {
        return Err(DomainError::validation(format!(
            "at most {} probes per check, got {}",
            ingest::MAX_EMISSION_PROBES,
            bodies.len()
        )));
    }
    let digester = crate::crypto::Digester::from_provider(ctx.keys.as_ref()).await?;
    Ok(bodies
        .iter()
        .map(|b| EmissionProbe {
            content_sha256: digester.digest(&b.content),
            observed_at: b.observed_at.unwrap_or_else(chrono::Utc::now),
        })
        .collect())
}

/// The read-only half of the anti-loop check, for a dry run and for the report.
///
/// The authoritative check runs again inside `POST /admin/ingest/proposals`, so a client that
/// skipped this one changes nothing.
///
/// Answers one boolean per probe, in probe order, and nothing else. A hit used to carry the memory
/// id and tool of the row that matched, which told whoever guessed a sentence that the store holds
/// it and where. The only consumer counts hits. The lookup itself runs inside the caller's grant,
/// so an emission of a row the caller may not read is not an echo for that caller.
async fn admin_ingest_emissions_check(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<EmissionsCheck>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let probes = match probes(&ctx, &body.probes).await {
        Ok(p) => p,
        Err(e) => return domain_error(&e, "ingest_failed"),
    };
    match ingest::check_emissions(&ctx, http.state.ingest.as_ref(), &probes).await {
        Ok(hits) => {
            Json(serde_json::json!({ "echoes": ingest::echoes(&probes, &hits) })).into_response()
        }
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

#[derive(Deserialize)]
struct SourceBody {
    file_path: String,
    #[serde(default)]
    entry_uuid: Option<String>,
    /// `file_path '#' entry_uuid` when the caller omits it, which is what makes a re-post idempotent
    /// at the source grain.
    #[serde(default)]
    source_key: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    speaker: Option<String>,
    #[serde(default)]
    observed_at: Option<chrono::DateTime<chrono::Utc>>,
    run_id: uuid::Uuid,
}

#[derive(Deserialize)]
struct FactBody {
    content: String,
    namespace: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    supersedes: Option<uuid::Uuid>,
    speaker: String,
    #[serde(default)]
    quote: Option<String>,
    /// The span this fact was drawn from, as the client read it. The server checks that the
    /// content is a substring of it, which binds an honest extractor to quoting rather than
    /// paraphrasing; it cannot bind the poster, since both strings arrive in the same request.
    /// What binds the poster is its write grant on the namespace, and `auto` is set only when
    /// both hold. `auto` is not a field here and never will be: a client that could set it would
    /// be approving its own writes.
    #[serde(default)]
    span_text: Option<String>,
    source: SourceBody,
}

#[derive(Deserialize)]
struct PostProposals {
    extractor: String,
    facts: Vec<FactBody>,
}

fn fact_input(f: FactBody) -> ingest::FactInput {
    let source = ProposalSource {
        source_key: f.source.source_key.clone().unwrap_or_else(|| match &f.source.entry_uuid {
            Some(entry) => format!("{}#{entry}", f.source.file_path),
            None => f.source.file_path.clone(),
        }),
        file_path: f.source.file_path,
        session_id: f.source.session_id,
        is_sidechain: f.source.is_sidechain,
        entry_uuid: f.source.entry_uuid,
        speaker: f.source.speaker.unwrap_or_else(|| f.speaker.clone()),
        observed_at: f.source.observed_at,
        run_id: f.source.run_id,
    };
    ingest::FactInput {
        content: f.content,
        namespace: f.namespace,
        tags: f.tags,
        supersedes: f.supersedes,
        speaker: f.speaker,
        quote: f.quote,
        span_text: f.span_text,
        source,
    }
}

async fn admin_ingest_post(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<PostProposals>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let facts: Vec<ingest::FactInput> = body.facts.into_iter().map(fact_input).collect();
    match ingest::post(&ctx, http.state.ingest.as_ref(), &body.extractor, facts).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

#[derive(Deserialize)]
struct ProposalQuery {
    state: Option<String>,
    run_id: Option<uuid::Uuid>,
    speaker: Option<String>,
    auto: Option<bool>,
    limit: Option<i64>,
}

async fn admin_ingest_list(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<ProposalQuery>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let filter = ProposalFilter {
        state: q.state,
        run_id: q.run_id,
        speaker: q.speaker,
        auto: q.auto,
        limit: q.limit.unwrap_or(50).clamp(1, 500),
        // The service fills this from the credential. Nothing in the query string names a reader.
        reader: Default::default(),
    };
    match ingest::list(&ctx, http.state.ingest.as_ref(), filter).await {
        Ok(rows) => Json(serde_json::json!({ "proposals": rows })).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

/// One proposal with every source that stated it, and the strongest speaker across them.
///
/// The parent's speaker is frozen at first insert, so this is how the owner learns that the fact he
/// is looking at was also typed by him somewhere.
async fn admin_ingest_show(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let id = match ingest_id(&id) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match ingest::show(&ctx, http.state.ingest.as_ref(), id).await {
        Ok((proposal, sources)) => {
            let strongest = ingest::strongest_speaker(&sources).map(|s| s.speaker.clone());
            Json(serde_json::json!({
                "proposal": proposal,
                "sources": sources,
                "strongest_speaker": strongest,
            }))
            .into_response()
        }
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

/// The only path from the queue into the store, and it is `services::write::run`.
///
/// A refusal is a 200 carrying `refused`, not an error: the row stays at `proposed` with the reason
/// on it, and the owner reads the refusal in the queue rather than finding a row that stopped
/// moving.
async fn admin_ingest_approve(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let id = match ingest_id(&id) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match ingest::approve(&ctx, http.state.ingest.as_ref(), id).await {
        Ok(outcome) => Json(outcome).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

#[derive(Deserialize)]
struct RejectBody {
    #[serde(default)]
    reason: Option<String>,
}

async fn admin_ingest_reject(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<RejectBody>>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let id = match ingest_id(&id) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let reason = body.and_then(|Json(b)| b.reason);
    match ingest::reject(&ctx, http.state.ingest.as_ref(), id, reason.as_deref()).await {
        Ok(done) => Json(serde_json::json!({ "rejected": done })).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

/// Return a rejected row to the queue. Permanent and irreversible are different claims: a rejection
/// blocks its fingerprint forever, and a queue read at speed is exactly where the wrong uuid gets
/// typed.
async fn admin_ingest_unreject(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let id = match ingest_id(&id) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match ingest::unreject(&ctx, http.state.ingest.as_ref(), id).await {
        Ok(done) => Json(serde_json::json!({ "unrejected": done })).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

#[derive(Deserialize)]
struct WatermarkQuery {
    skipped: Option<bool>,
}

async fn admin_ingest_watermarks(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<WatermarkQuery>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match ingest::watermarks(&ctx, http.state.ingest.as_ref(), q.skipped.unwrap_or(false)).await {
        Ok(rows) => Json(serde_json::json!({ "watermarks": rows })).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

#[derive(Deserialize)]
struct FileAdvanceBody {
    file_path: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    plan_ceiling: i64,
    prefix_sha256: String,
    #[serde(default)]
    entries_seen: i64,
    /// The first byte of every span from this file that came back missing or failed. Empty means
    /// every span was extracted, or that the file produced no span at all.
    #[serde(default)]
    unextracted_from: Vec<i64>,
}

#[derive(Deserialize)]
struct AdvanceBody {
    run_id: uuid::Uuid,
    files: Vec<FileAdvanceBody>,
}

/// Move the watermarks at the end of a run. The one place this pipeline can lose data, so the
/// service decides how far each file goes and the repository takes the greater of that and what is
/// stored.
async fn admin_ingest_watermark_advance(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<AdvanceBody>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let files: Vec<ingest::FileAdvance> = body
        .files
        .into_iter()
        .map(|f| ingest::FileAdvance {
            file_path: f.file_path,
            session_id: f.session_id,
            is_sidechain: f.is_sidechain,
            plan_ceiling: f.plan_ceiling,
            prefix_sha256: f.prefix_sha256,
            entries_seen: f.entries_seen,
            unextracted_from: f.unextracted_from,
        })
        .collect();
    match ingest::advance_watermarks(&ctx, http.state.ingest.as_ref(), body.run_id, &files).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

#[derive(Deserialize)]
struct UnskipBody {
    file_path: String,
}

/// Clear one file's skip, by hand only. A skip that expired on its own would let a run eat its own
/// output the next night.
async fn admin_ingest_unskip(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<UnskipBody>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match ingest::unskip(&ctx, http.state.ingest.as_ref(), body.file_path.trim()).await {
        Ok(done) => Json(serde_json::json!({ "unskipped": done })).into_response(),
        Err(e) => domain_error(&e, "ingest_failed"),
    }
}

async fn resource_metadata(State(http): State<Http>) -> Response {
    Json(protected_resource_metadata(&http.state.cfg)).into_response()
}

/// The row, if this caller may read it at its stored level, with its plaintext filled in.
async fn visible_memory(ctx: &Ctx, id: &str) -> crate::domain::errors::Result<Option<Memory>> {
    let Ok(uuid) = uuid::Uuid::parse_str(id.trim()) else { return Ok(None) };
    let row = ctx.repos.memories.find_by_id(ctx.tenant(), uuid).await?;
    let mut row = row.filter(|m| can_read(&ctx.principal, &m.namespace, m.sensitivity));
    if let Some(m) = row.as_mut() {
        // A row that will not open keeps its empty content rather than disappearing: an unreadable
        // row is itself something the operator should see.
        let _ = crate::services::decrypt(ctx, vec![m]).await;
    }
    Ok(row)
}

/// Narrow a repository page to what this caller may read, and fill in the private rows.
///
/// `stale` and `list_for_export` take no ceilings, so the grant is applied here. Rows that will not
/// decrypt are dropped: a note with no body is worse than a note that is absent.
async fn readable_rows(ctx: &Ctx, rows: Vec<Memory>) -> Vec<Memory> {
    let mut rows: Vec<Memory> = rows
        .into_iter()
        .filter(|m| can_read(&ctx.principal, &m.namespace, m.sensitivity))
        .collect();
    let unopened = crate::services::decrypt(ctx, rows.iter_mut().collect()).await;
    if !unopened.is_empty() {
        rows.retain(|m| !unopened.contains(&m.id));
    }
    rows
}

async fn not_found(req: Request) -> Response {
    let path = req.uri().path().to_string();
    (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "not_found", "path": path })))
        .into_response()
}

fn forbidden(e: &DomainError) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": "forbidden", "detail": e.client_message() })),
    )
        .into_response()
}

/// One mapping from a domain error to a response, so a new endpoint cannot invent a new shape.
///
/// Everything the caller could act on carries its message: a 403 that says only "failed" sends the
/// owner to read a log for a grant they could have fixed from the error. `Internal` is the exception
/// and says nothing, because its message is ours.
fn domain_error(e: &DomainError, code: &str) -> Response {
    use crate::domain::errors::Kind;
    if matches!(e.kind, Kind::Internal) {
        return internal(e, code);
    }
    if matches!(e.kind, Kind::Unavailable) {
        tracing::error!(error = %e.log_message(), code, "request failed");
    }
    (
        StatusCode::from_u16(e.kind.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(serde_json::json!({ "error": code, "detail": e.client_message() })),
    )
        .into_response()
}

fn internal(e: &DomainError, code: &str) -> Response {
    tracing::error!(error = %e.log_message(), code, "request failed");
    (
        StatusCode::from_u16(e.kind.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(serde_json::json!({ "error": code })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy::NamespaceGrant;

    fn caller(client: &str, read: Vec<NamespaceGrant>) -> Principal {
        Principal {
            client: client.into(),
            token_id: "test".into(),
            mode: "token",
            scopes: vec![],
            read,
            write: vec![],
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
        }
    }

    fn tool_row(client: &str, calls: i64, failures: i64) -> ToolCallStats {
        ToolCallStats {
            tool: "memory_search".into(),
            client: client.into(),
            calls,
            failures,
            unprompted: 0,
            p50_ms: Some(12),
            p95_ms: Some(40),
        }
    }

    fn client_row(client: &str) -> ClientStats {
        ClientStats {
            client: client.into(),
            calls: 9,
            reads: 9,
            writes: 0,
            failures: 0,
            sessions: 1,
            sessions_with_unprompted_read: 0,
            sessions_with_unprompted_write: 0,
            unprompted_read_rate: None,
            unprompted_write_rate: None,
            write_to_read_ratio: None,
        }
    }

    #[test]
    fn a_narrow_token_never_learns_that_another_client_exists() {
        let rows = vec![tool_row("browser", 3, 1), tool_row("claude-code-mac", 40, 7)];
        let body =
            tool_stats_body(rows, 24, &caller("browser", vec![NamespaceGrant::open("user:me")]));
        assert_eq!(body["scope"], "self");
        let printed = body["by_tool"].as_array().expect("by_tool is a list");
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0]["client"], "browser");
        // The totals are the second half of the same disclosure: 43 calls beside one row saying 3
        // reports the other client's volume without naming it.
        assert_eq!(body["totals"]["calls"], 3);
        assert_eq!(body["totals"]["failures"], 1);
    }

    #[test]
    fn the_owners_own_token_still_sees_every_client() {
        let rows = vec![tool_row("browser", 3, 1), tool_row("claude-code-mac", 40, 7)];
        let body =
            tool_stats_body(rows, 24, &caller("claude-code-mac", NamespaceGrant::everything()));
        assert_eq!(body["scope"], "tenant");
        assert_eq!(body["by_tool"].as_array().expect("by_tool is a list").len(), 2);
        assert_eq!(body["totals"]["calls"], 43);
    }

    #[test]
    fn a_narrow_token_gets_no_tenant_wide_staleness() {
        let rows = vec![client_row("browser"), client_row("claude-code-mac")];
        let numbers = Staleness { live_rows: 412, ..Staleness::default() };
        let body = client_stats_body(
            rows,
            Some(numbers),
            24,
            &caller("browser", vec![NamespaceGrant::open("user:me")]),
        );
        assert!(body["staleness"].is_null(), "412 live rows is the size of a store it cannot read");
        assert_eq!(body["by_client"].as_array().expect("by_client is a list").len(), 1);
    }

    #[test]
    fn the_owners_own_token_still_gets_the_decay_numbers() {
        let body = client_stats_body(
            vec![client_row("claude-code-mac")],
            Some(Staleness { live_rows: 412, ..Staleness::default() }),
            24,
            &caller("claude-code-mac", NamespaceGrant::everything()),
        );
        assert_eq!(body["staleness"]["live_rows"], 412);
    }

    #[test]
    fn the_public_host_is_accepted_alongside_loopback() {
        let hosts = allowed_hosts("https://lumberroom.example.com");
        assert!(hosts.contains(&"lumberroom.example.com".to_string()));
        // The CLI and the hook reach the server on loopback whatever the public URL says.
        assert!(hosts.contains(&"127.0.0.1".to_string()));
        assert!(hosts.contains(&"localhost".to_string()));
    }

    #[test]
    fn a_loopback_public_url_adds_no_duplicate() {
        assert_eq!(allowed_hosts("http://127.0.0.1:8787").len(), 3);
    }

    #[test]
    fn an_unparseable_public_url_leaves_loopback_rather_than_everything() {
        // Empty is the fail-closed direction: rmcp treats an empty list as "allow every host".
        let hosts = allowed_hosts("not-a-url");
        assert_eq!(hosts.len(), 3);
        assert!(!hosts.is_empty(), "an empty list would switch host validation off entirely");
    }

    #[test]
    fn a_session_id_is_bounded_and_a_blank_one_is_absent() {
        let mut headers = HeaderMap::new();
        headers.insert(SESSION_HEADER, "  conv-42  ".parse().unwrap());
        assert_eq!(session_id(&headers).0.as_deref(), Some("conv-42"));

        headers.insert(SESSION_HEADER, "   ".parse().unwrap());
        assert_eq!(session_id(&headers).0, None);
        assert_eq!(session_id(&HeaderMap::new()).0, None);

        headers.insert(SESSION_HEADER, "x".repeat(500).parse().unwrap());
        assert_eq!(session_id(&headers).0.unwrap().len(), MAX_SESSION_ID_CHARS);
    }

    /// The wire contract is snake_case and scripts/deploy-check.sh reads three of these keys by
    /// name. A rename here turns the stale-image check into a permanent warning that nobody trusts.
    #[test]
    fn readyz_publishes_the_build_stamp_under_the_names_deploy_check_reads() {
        let v = static_checks("hash-32", false, "token", "none", false);
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "auth_mode",
                "build_sha",
                "build_tag",
                "built_at",
                "embedder",
                "embedder_degraded",
                "kek_provider",
                "kek_verified",
            ]
        );

        // Under `cargo test` nothing sets the stamp, so this asserts the default rather than a
        // commit. A test that demanded a real sha would fail every local run.
        for key in ["build_sha", "build_tag", "built_at"] {
            assert!(v[key].is_string(), "{key} must be a string on the wire");
            assert!(!v[key].as_str().unwrap().is_empty(), "{key} is never blank, it is 'unknown'");
        }

        // merge_ok runs over this on the 503 path too, so the stamp survives a failing ping.
        let failed = merge_ok(static_checks("hash-32", true, "token", "none", false), false);
        assert_eq!(failed["ok"], serde_json::json!(false));
        assert_eq!(failed["build_sha"], serde_json::json!(crate::build_info::SHA));
    }
}

// ---- cleanup -----------------------------------------------------------------------------------
//
// Five routes, no tool behind any of them, for the reason ingestion has none: a model that can
// propose retiring the owner's memories can fill a queue he stops reading.
//
// `/run` is the deterministic pass. It returns what it queued and, separately, the pairs it was not
// confident enough to call. Those go to whoever asks a model, which is `lumberroom cleanup run` and
// not this process: the provider path, the keys and the retry all live in the client, and a server
// that called out to a third party would need every one of them again.

/// The pass, and the candidates it could not decide.
#[derive(Deserialize)]
struct CleanupRun {
    /// A namespace glob inside the caller's read grant. Absent means the whole grant, which for
    /// the owner's own client is the whole store and for anyone else is less.
    #[serde(default)]
    namespace: Option<String>,
    /// `hourly` or `daily`. Daily is the one that looks at staleness.
    #[serde(default)]
    cadence: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    /// The floor for the band handed to a model. Absent means the built-in one.
    #[serde(default)]
    min_similarity: Option<f64>,
}

async fn admin_cleanup_run(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<CleanupRun>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let cadence = match body.cadence.as_deref().unwrap_or("hourly") {
        c @ ("hourly" | "daily") => c.to_string(),
        other => {
            return domain_error(
                &DomainError::validation(format!("cadence is hourly or daily, got {other:?}")),
                "cleanup_failed",
            )
        }
    };
    let limit = body.limit.unwrap_or(500).clamp(1, 5000);
    // `run_as`, never `run`. The tenant-only entry is the scheduler's and reads the whole store;
    // this caller reads what its grant says, pushed into the query.
    match cleanup::run_as(
        &ctx,
        http.state.cleanup.as_ref(),
        body.namespace.as_deref(),
        &cadence,
        limit,
        body.min_similarity,
    )
    .await
    {
        Ok((report, candidates)) => {
            Json(serde_json::json!({ "report": report, "for_the_model": candidates }))
                .into_response()
        }
        Err(e) => domain_error(&e, "cleanup_failed"),
    }
}

#[derive(Deserialize)]
struct CleanupListQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn admin_cleanup_list(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<CleanupListQuery>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match cleanup::list(
        &ctx,
        http.state.cleanup.as_ref(),
        q.state.as_deref(),
        q.limit.unwrap_or(50),
    )
    .await
    {
        Ok(rows) => Json(serde_json::json!({ "proposals": rows })).into_response(),
        Err(e) => domain_error(&e, "cleanup_failed"),
    }
}

/// What a model pass posts back: the clusters it decided on, in the same shape the pass produces.
#[derive(Deserialize)]
struct CleanupPost {
    proposals: Vec<CleanupPostItem>,
}

#[derive(Deserialize)]
struct CleanupPostItem {
    kind: String,
    namespace: String,
    #[serde(default)]
    keep_id: Option<String>,
    rationale: String,
    /// The model that produced it. Recorded rather than trusted: the queue has to say which tier
    /// spoke, because the cheap one and the expensive one disagree often enough to matter.
    produced_by: String,
    #[serde(default)]
    similarity: Option<f64>,
    members: Vec<CleanupPostMember>,
}

#[derive(Deserialize)]
struct CleanupPostMember {
    memory_id: String,
    disposition: String,
    seen_content: String,
}

async fn admin_cleanup_post(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<CleanupPost>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let mut queued = 0usize;
    let mut already_known = 0usize;
    let mut ids = Vec::new();
    for item in body.proposals {
        let Some(kind) = crate::domain::cleanup::CleanupKind::parse(&item.kind) else {
            return domain_error(
                &DomainError::validation(format!("unknown cleanup kind {:?}", item.kind)),
                "cleanup_failed",
            );
        };
        let mut members = Vec::with_capacity(item.members.len());
        for m in item.members {
            let Some(disposition) = crate::domain::cleanup::Disposition::parse(&m.disposition)
            else {
                return domain_error(
                    &DomainError::validation(format!("unknown disposition {:?}", m.disposition)),
                    "cleanup_failed",
                );
            };
            members.push(crate::ports::cleanup::NewMember {
                memory_id: m.memory_id,
                disposition,
                seen_content: m.seen_content,
            });
        }
        let proposal = crate::ports::cleanup::NewProposal {
            kind,
            namespace: item.namespace,
            keep_id: item.keep_id,
            rationale: item.rationale,
            produced_by: item.produced_by,
            similarity: item.similarity,
            // Set by the service from the credential; nothing in the body names the poster.
            posted_by: None,
            members,
        };
        // Through `queue_posted`, never `queue_checked`. The posted shape is the pass's own, and a
        // client holding this route can fill it in by hand naming any memory id; the service
        // resolves every member against the caller's grant before anything is queued, and then
        // applies the same valid-time reconciliation the deterministic pass gets.
        match cleanup::queue_posted(&ctx, http.state.cleanup.as_ref(), proposal).await {
            Ok((crate::ports::cleanup::QueueOutcome::Queued, id)) => {
                queued += 1;
                ids.push(id);
            }
            Ok((crate::ports::cleanup::QueueOutcome::AlreadyKnown, _)) => already_known += 1,
            Err(e) => return domain_error(&e, "cleanup_failed"),
        }
    }
    Json(serde_json::json!({ "queued": queued, "already_known": already_known, "ids": ids }))
        .into_response()
}

async fn admin_cleanup_show(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match cleanup::get(&ctx, http.state.cleanup.as_ref(), &id).await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => domain_error(
            &DomainError::not_found(format!("no cleanup proposal {id}")),
            "cleanup_failed",
        ),
        Err(e) => domain_error(&e, "cleanup_failed"),
    }
}

async fn admin_cleanup_apply(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match cleanup::apply(&ctx, http.state.cleanup.as_ref(), &id).await {
        Ok(applied) => Json(applied).into_response(),
        Err(e) => domain_error(&e, "cleanup_failed"),
    }
}

#[derive(Deserialize)]
struct CleanupReject {
    #[serde(default)]
    reason: Option<String>,
}

async fn admin_cleanup_reject(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CleanupReject>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match cleanup::reject(&ctx, http.state.cleanup.as_ref(), &id, body.reason.as_deref()).await {
        Ok(()) => Json(serde_json::json!({ "rejected": id })).into_response(),
        Err(e) => domain_error(&e, "cleanup_failed"),
    }
}

/// Return a refused finding to the queue.
///
/// A rejection blocks its cluster key for good, which is what makes an hourly pass safe to run
/// hourly. It blocks the replacement too when the pass that wrote the finding was what was wrong,
/// and the way out used to be a DELETE typed into psql.
async fn admin_cleanup_unreject(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match cleanup::unreject(&ctx, http.state.cleanup.as_ref(), &id).await {
        Ok(()) => Json(serde_json::json!({ "unrejected": id })).into_response(),
        Err(e) => domain_error(&e, "cleanup_failed"),
    }
}

/// Settle a contradiction by naming which of its rows holds.
#[derive(Deserialize)]
struct CleanupResolve {
    keep_id: String,
}

async fn admin_cleanup_resolve(
    State(http): State<Http>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CleanupResolve>,
) -> Response {
    let ctx = match ingest_ctx(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match cleanup::resolve(&ctx, http.state.cleanup.as_ref(), &id, &body.keep_id).await {
        Ok(applied) => Json(applied).into_response(),
        Err(e) => domain_error(&e, "cleanup_failed"),
    }
}
