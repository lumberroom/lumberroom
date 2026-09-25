//! The endpoints. RFC 8414 metadata, RFC 7591 registration, the authorization flow, RFC 6749 token
//! exchange with PKCE, RFC 7009 revocation, and the owner's client list.
//!
//! Two rules run through all of it.
//!
//! **Every URL comes from `cfg.public_url`, never from the Host header.** An issuer that disagrees
//! with the host a client reached is invisible until a real client's discovery fails, and behind a
//! reverse proxy the Host header is whatever the proxy passed on.
//!
//! **An error only travels to a redirect URI once that URI has been matched against the client
//! record.** Everything before that point renders as a page. Redirecting an error to an unverified
//! URI is the open-redirect path, and it is reachable with nothing but a crafted link.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::extract::{ConnectInfo, Extension, Form, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use zeroize::Zeroize;

use crate::adapters::auth::Authenticator;
use crate::authserver::limiter::{self, LoginLimiter};
use crate::authserver::pages::{self, ClientView, FlowFields};
use crate::authserver::session::{OwnerSession, Sessions};
use crate::config::{Config, ResourceAudience};
use crate::domain::errors::{DomainError, Result};
use crate::domain::oauth::{
    canonical_resource, hash_token, hashes_match, random_token, resource_matches,
    validate_redirect_uri, verify_pkce_s256, AuthorizeIntent, AuthorizeRequest, GrantProfile,
    OauthError, RegistrationRequest, RegistrationResponse, TokenResponse,
};
use crate::ports::{
    ClientGrantUpdate, CodeOutcome, NewAccessToken, NewAuthCode, NewOauthClient, NewRefreshToken,
    OauthClientRecord, OauthStore, RefreshOutcome,
};

/// Paid on every failed password, before the response is written.
///
/// It costs the owner three quarters of a second on a typo and costs a guesser the same on every
/// attempt, on top of Argon2's own cost and the per-minute window. Nothing on the ten-second client
/// budget (discovery, registration, token) passes through this handler.
const LOGIN_FAILURE_DELAY: Duration = Duration::from_millis(750);

/// Caps on attacker-supplied registration metadata. The name and the software id are rendered on the
/// consent page, and the redirect list is walked on every authorize.
const MAX_CLIENT_NAME: usize = 200;
const MAX_SOFTWARE_FIELD: usize = 100;
use crate::domain::oauth::{MAX_REDIRECT_URI, MAX_REDIRECT_URIS};

/// Informational only. Authorization is the `GrantProfile` the owner picked, which no client can
/// influence, so this string exists because RFC 6749 §5.1 has a field for it and clients display it.
const DEFAULT_SCOPE: &str = "lumberroom:memory";

#[derive(Clone)]
pub struct AuthServer {
    pub cfg: Arc<Config>,
    pub store: Arc<dyn OauthStore>,
    /// Used for one thing: proving the caller of `/oauth/clients` is the owner's own credential.
    pub auth: Arc<dyn Authenticator>,
    sessions: Sessions,
    limiter: Arc<LoginLimiter>,
    /// Separate from the login limiter on purpose. Both share the global window inside one
    /// limiter, so a registration storm counted there would lock the owner out of `/oauth/login`.
    register_limiter: Arc<LoginLimiter>,
}

impl AuthServer {
    pub fn new(cfg: Arc<Config>, store: Arc<dyn OauthStore>, auth: Arc<dyn Authenticator>) -> Self {
        let sessions = Sessions::from_config(&cfg);
        let limiter = Arc::new(LoginLimiter::new(cfg.oauth.login_attempts_per_minute));
        let register_limiter = Arc::new(LoginLimiter::new(cfg.oauth.registrations_per_minute));
        Self { cfg, store, auth, sessions, limiter, register_limiter }
    }

    /// The profile preselected on the consent screen.
    ///
    /// `OAUTH_DEFAULT_PROFILE` is not validated at boot, so a typo must not take the consent page
    /// down. An unrecognised value falls back to the middle profile and says so in the log.
    fn default_profile(&self) -> GrantProfile {
        match GrantProfile::parse(&self.cfg.oauth.default_profile) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    configured = %self.cfg.oauth.default_profile,
                    "OAUTH_DEFAULT_PROFILE is not full|standard|narrow, preselecting standard"
                );
                GrantProfile::Standard
            }
        }
    }
}

pub fn routes() -> Router<AuthServer> {
    Router::new()
        // Both paths. RFC 8414 puts the document at the origin for an origin issuer, and clients
        // that build the URL from the resource path ask for the suffixed form. Serving one and not
        // the other fails at discovery, before a client shows any error worth reading.
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/.well-known/oauth-authorization-server/mcp", get(metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/login", post(login))
        .route("/oauth/consent", post(consent))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .route("/oauth/clients", get(clients))
}

// ---- RFC 8414 metadata ----

/// Built from the pieces rather than from `Config`, so the document that a client's discovery
/// depends on can be asserted in a unit test without a process environment.
pub fn metadata_document(
    public_url: &str,
    scopes_supported: &[String],
    dcr_enabled: bool,
) -> serde_json::Value {
    let url = |path: &str| format!("{public_url}{path}");
    let mut doc = serde_json::json!({
        // The issuer is this origin, which is also where the metadata is served from. Never the Host
        // header: an issuer that disagrees with the host behind a reverse proxy is invisible until a
        // real client's discovery fails.
        "issuer": public_url,
        "authorization_endpoint": url("/oauth/authorize"),
        "token_endpoint": url("/oauth/token"),
        "revocation_endpoint": url("/oauth/revoke"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        // Newer clients refuse to start a flow when this is absent, and S256 is the only method
        // this server accepts: RFC 7636 defaults an omitted method to `plain`, which is PKCE
        // switched off while looking switched on.
        "code_challenge_methods_supported": ["S256"],
        // "none" is what a public client with PKCE sends, and every dynamically registered client
        // here is public. client_secret_post covers the confidential clients the owner issues by
        // hand. HTTP Basic is accepted by the token endpoint for compatibility but deliberately not
        // advertised, because it puts the secret in a header that proxies log.
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
        // Revocation takes the token and nothing else: possession of the token is what authorizes
        // revoking it, and a public client has no other credential to offer.
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": scopes_supported,
        "service_documentation": "https://github.com/lumberroom/lumberroom",
    });

    // Advertised only when it exists. A registration endpoint in the document that answers 403 is
    // worse than no endpoint, because a client that reads DCR support as available stops looking for
    // credentials it was given.
    if dcr_enabled {
        doc["registration_endpoint"] = serde_json::json!(url("/oauth/register"));
    }
    doc
}

async fn metadata(State(app): State<AuthServer>) -> Response {
    (
        // Discovery is on the ten-second budget and this document changes only on a redeploy.
        [(header::CACHE_CONTROL, "public, max-age=300")],
        Json(metadata_document(
            &app.cfg.public_url,
            &app.cfg.oauth.scopes_supported,
            app.cfg.oauth.dcr_enabled,
        )),
    )
        .into_response()
}

// ---- RFC 7591 dynamic client registration ----

async fn register(
    State(app): State<AuthServer>,
    // Optional for the same reason as in `login`: absent `ConnectInfo` degrades the accounting to
    // the global window rather than failing registration with a 500.
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    // The body extractor consumes the request and has to come last.
    Json(req): Json<RegistrationRequest>,
) -> Response {
    if !app.cfg.oauth.dcr_enabled {
        return registration_error(
            StatusCode::FORBIDDEN,
            "access_denied",
            "dynamic client registration is switched off on this server. Ask the owner to issue a \
             client id.",
        );
    }

    let addr = peer.map(|Extension(ConnectInfo(addr))| addr);
    let key = throttle_key(&headers, addr);
    let from = peer_key(addr);

    if req.redirect_uris.is_empty() {
        return registration_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris must hold at least one URI",
        );
    }
    if req.redirect_uris.len() > MAX_REDIRECT_URIS {
        return registration_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            format!("at most {MAX_REDIRECT_URIS} redirect URIs"),
        );
    }
    for uri in &req.redirect_uris {
        if uri.len() > MAX_REDIRECT_URI {
            return registration_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                format!("a redirect URI is longer than {MAX_REDIRECT_URI} characters"),
            );
        }
        if let Err(e) = validate_redirect_uri(uri) {
            return registration_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                e.client_message(),
            );
        }
    }

    // The grant types this server has. An implicit or password grant request is refused rather than
    // quietly downgraded, so a client cannot believe it holds something it does not.
    let grant_types = match req.grant_types {
        Some(asked) => {
            for g in &asked {
                if g != "authorization_code" && g != "refresh_token" {
                    return registration_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_client_metadata",
                        format!(
                            "grant_type {g:?} is not supported. This server issues \
                                 authorization codes and refresh tokens."
                        ),
                    );
                }
            }
            if !asked.iter().any(|g| g == "authorization_code") {
                return registration_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_client_metadata",
                    "grant_types must include authorization_code",
                );
            }
            asked
        }
        None => vec!["authorization_code".to_string(), "refresh_token".to_string()],
    };

    if let Some(types) = &req.response_types {
        if types.iter().any(|t| t != "code") {
            return registration_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "response_types must be [\"code\"]",
            );
        }
    }

    let client_name = match req.client_name.as_deref().map(str::trim) {
        Some(name) if name.len() > MAX_CLIENT_NAME => {
            return registration_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                format!("client_name is longer than {MAX_CLIENT_NAME} characters"),
            )
        }
        Some(name) if !name.is_empty() => name.to_string(),
        _ => "unnamed client".to_string(),
    };

    for (field, value) in
        [("software_id", &req.software_id), ("software_version", &req.software_version)]
    {
        if value.as_deref().is_some_and(|v| v.len() > MAX_SOFTWARE_FIELD) {
            return registration_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                format!("{field} is longer than {MAX_SOFTWARE_FIELD} characters"),
            );
        }
    }

    let client_id = match random_token(24) {
        Ok(id) => id,
        Err(e) => return internal_json(&e),
    };

    let record = NewOauthClient {
        client_id: client_id.clone(),
        // Public client. The response has no secret to carry, which is deliberate: a secret handed
        // to a browser client is not a secret, and PKCE is what actually binds the exchange.
        secret_hash: None,
        client_name: client_name.clone(),
        redirect_uris: req.redirect_uris.clone(),
        grant_types: grant_types.clone(),
        software_id: req.software_id.clone(),
        software_version: req.software_version.clone(),
        registered_via: "dcr".to_string(),
    };

    // Throttled here rather than at the top of the handler. What this window meters is rows
    // written, and taking the slot in the same breath as the insert is what stops a burst from
    // writing a hundred `oauth_client` rows against a ceiling of five: reading a budget first and
    // recording the row afterwards left every concurrent request seeing the same empty budget.
    // A request that fails validation wrote nothing, so it never reaches this and costs nothing.
    let Some(slot) = app.register_limiter.reserve(&key, &from, Instant::now()) else {
        tracing::warn!(key = %key, "registration throttled");
        return registration_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_requests",
            "too many registrations from this address. Wait a minute and try again.",
        );
    };

    if let Err(e) = app.store.register_client(record).await {
        slot.release();
        return internal_json(&e);
    }
    // Dropped rather than released: the row landed, and that is the event this window counts.
    drop(slot);

    tracing::info!(
        client_id = %client_id,
        client_name = %client_name,
        "registered a client with an empty grant"
    );

    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(RegistrationResponse {
            client_id,
            client_id_issued_at: chrono::Utc::now().timestamp(),
            client_name,
            redirect_uris: req.redirect_uris,
            grant_types,
            response_types: vec!["code".to_string()],
            // Answered rather than echoed. There is no secret, so a client that asked for
            // client_secret_post has to read this and use none.
            token_endpoint_auth_method: "none".to_string(),
        }),
    )
        .into_response()
}

// ---- GET /oauth/authorize ----

async fn authorize(
    State(app): State<AuthServer>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // A HashMap rather than a typed extractor: a missing parameter has to render a page that says
    // which one, and a 422 from an extractor is a blank wall in a browser tab.
    let request = authorize_request(&params);
    let (client, intent) = match resolve_request(&app, request).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    match app.sessions.verify(&headers, now()) {
        Some(session) => consent_page(&app, &client, &intent, &session, StatusCode::OK),
        None => login_page(&client, &intent, StatusCode::OK, None),
    }
}

// ---- POST /oauth/login ----

/// The login and consent forms both carry the authorization request back as hidden fields. Every
/// field is optional here so a malformed post renders a page instead of a 422, and every field is
/// re-validated against the client record rather than trusted for having been on the page.
#[derive(Debug, serde::Deserialize)]
struct FlowForm {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    response_type: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    code_challenge: Option<String>,
    #[serde(default)]
    code_challenge_method: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

impl FlowForm {
    fn authorize_request(&self) -> AuthorizeRequest {
        AuthorizeRequest {
            response_type: self.response_type.clone().unwrap_or_default(),
            client_id: self.client_id.clone().unwrap_or_default(),
            redirect_uri: self.redirect_uri.clone().unwrap_or_default(),
            code_challenge: self.code_challenge.clone().unwrap_or_default(),
            code_challenge_method: self.code_challenge_method.clone(),
            state: self.state.clone(),
            scope: self.scope.clone(),
            resource: self.resource.clone(),
        }
    }
}

async fn login(
    State(app): State<AuthServer>,
    // Optional so the handler still works when the server was not built with
    // `into_make_service_with_connect_info`. An absent address degrades the per-key accounting to the
    // limiter's global window rather than failing the login with a 500.
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Form(form): Form<FlowForm>,
) -> Response {
    let (client, intent) = match resolve_request(&app, form.authorize_request()).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    let addr = peer.map(|Extension(ConnectInfo(addr))| addr);
    let key = throttle_key(&headers, addr);
    let from = peer_key(addr);
    // Taken before the password check, not after it. The budget used to be read here and written
    // once Argon2id and the failure delay had both finished, so everything arriving inside that
    // window saw it empty.
    let Some(slot) = app.limiter.reserve(&key, &from, Instant::now()) else {
        tracing::warn!(key = %key, "login throttled");
        return login_page(
            &client,
            &intent,
            StatusCode::TOO_MANY_REQUESTS,
            Some("Too many attempts. Wait a minute and try again."),
        );
    };

    let Some(hash) = app.cfg.oauth.owner_password_hash.clone() else {
        slot.release();
        // Config refuses to boot in oauth mode without a hash, so this is a mode that was switched
        // on somewhere else. No password means no consent, never open consent.
        return page(
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::error_page(
                "not configured",
                "This server has no owner password set, so nothing can be approved. Set \
                 OWNER_PASSWORD_HASH and restart.",
            ),
        );
    };

    // A throttle key is whatever the caller says it is, so the limiter alone does not bound how
    // many hashes run at once. This does, and it answers rather than queueing.
    let Some(work) = limiter::password_slot() else {
        slot.release();
        tracing::warn!(key = %key, "login refused: every password-check slot is busy");
        return login_page(
            &client,
            &intent,
            StatusCode::SERVICE_UNAVAILABLE,
            Some("The server is busy checking another sign-in. Try again in a moment."),
        );
    };

    let password = form.password.clone().unwrap_or_default();
    match verify_owner_password(hash, password, work).await {
        Ok(true) => slot.release(),
        Ok(false) => {
            // Constant work regardless of which check failed, and the message never says whether
            // the client, the request or the password was the problem. The reservation is dropped
            // rather than released, which is what keeps the attempt on the budget.
            tokio::time::sleep(LOGIN_FAILURE_DELAY).await;
            tracing::warn!(key = %key, client_id = %client.client_id, "failed owner login");
            return login_page(
                &client,
                &intent,
                StatusCode::UNAUTHORIZED,
                Some("That password is not right."),
            );
        }
        Err(e) => return page_internal(&e),
    }

    let value = app.sessions.issue(now());
    let Some(session) = app.sessions.open(&value, now()) else {
        return page_internal(&DomainError::internal("issued a session that does not verify"));
    };

    let mut response = consent_page(&app, &client, &intent, &session, StatusCode::OK);
    // Set-Cookie is attached here rather than inside consent_page so the page renderer has no
    // reason to know about cookies.
    if let Ok(cookie) = header::HeaderValue::from_str(&app.sessions.set_cookie(&value)) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

// ---- POST /oauth/consent ----

async fn consent(
    State(app): State<AuthServer>,
    headers: HeaderMap,
    Form(form): Form<FlowForm>,
) -> Response {
    let Some(session) = app.sessions.verify(&headers, now()) else {
        return page(
            StatusCode::UNAUTHORIZED,
            pages::error_page(
                "sign-in expired",
                "Your sign-in has expired. Start the connection again from the client and sign in \
                 once more.",
            ),
        );
    };

    // Re-fetched and re-validated rather than trusted from the form. The client can be revoked
    // between the login and the Allow, and the registered redirect URIs are needed anyway.
    let (client, intent) = match resolve_request(&app, form.authorize_request()).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    if !app.sessions.csrf_ok(
        &session,
        &intent.client_id,
        &intent.redirect_uri,
        &intent.code_challenge,
        intent.state.as_deref().unwrap_or(""),
        form.csrf.as_deref().unwrap_or(""),
    ) {
        // Without this check, any page the owner visits can POST a consent for a client that is
        // already registered, with parameters the owner never saw, and the owner's live session makes
        // it succeed. The client is already registered by then, because registration is open.
        tracing::warn!(client_id = %intent.client_id, "consent rejected: CSRF token does not match");
        return page(
            StatusCode::FORBIDDEN,
            pages::error_page(
                "form not recognised",
                "This form did not come from a sign-in on this server, or the request changed after \
                 it was shown. Nothing was granted. Start again from the client.",
            ),
        );
    }

    // Anything that is not an explicit Allow is a refusal. `access_denied` is the one error that may
    // travel to the redirect URI, because by this point the URI has been matched against the client
    // record and the owner is the one sending it.
    if form.action.as_deref() != Some("allow") {
        tracing::info!(client_id = %client.client_id, "owner denied consent");
        return redirect_error(&intent, "access_denied", "the owner refused this request");
    }

    let Some(profile) = form.profile.as_deref().and_then(GrantProfile::parse) else {
        return page(
            StatusCode::BAD_REQUEST,
            pages::error_page(
                "no choice made",
                "Pick what the client may reach, then press Allow.",
            ),
        );
    };

    let update = ClientGrantUpdate {
        profile: Some(profile.as_str().to_string()),
        read: profile.read(),
        write: profile.write(),
        registry_write: profile.registry_write(),
        sealed_capable: profile.sealed_capable(),
        may_delete: profile.may_delete(),
        may_ingest: profile.may_ingest(),
        may_read_history: false,
    };
    if let Err(e) = app.store.set_client_grant(&client.client_id, update).await {
        return page_internal(&e);
    }

    let code = match random_token(32) {
        Ok(c) => c,
        Err(e) => return page_internal(&e),
    };
    let record = NewAuthCode {
        // Only the hash is stored. A code recovered from the database is a code that cannot be
        // spent, which is the point.
        code_hash: hash_token(&code),
        client_id: client.client_id.clone(),
        redirect_uri: intent.redirect_uri.clone(),
        code_challenge: intent.code_challenge.clone(),
        scope: intent.scope.clone(),
        resource: intent.resource.clone(),
        expires_at: chrono::Utc::now() + chrono::Duration::seconds(app.cfg.oauth.code_ttl_secs),
    };
    if let Err(e) = app.store.insert_code(record).await {
        return page_internal(&e);
    }

    tracing::info!(
        client_id = %client.client_id,
        profile = profile.as_str(),
        "owner granted access"
    );

    let mut pairs = vec![("code", code)];
    if let Some(state) = intent.state.as_deref().filter(|s| !s.is_empty()) {
        pairs.push(("state", state.to_string()));
    }
    redirect(&append_query(&intent.redirect_uri, &pairs))
}

// ---- POST /oauth/token ----

async fn token(
    State(app): State<AuthServer>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    // Form encoded, per RFC 6749 §3.2, while /oauth/register takes JSON. A stack wired for JSON only
    // answers 415 here while registration succeeds, which reads as almost-working.
    let field = |name: &str| form.get(name).map(String::as_str).filter(|v| !v.is_empty());

    let credentials = match client_credentials(&headers, &form) {
        Ok(c) => c,
        Err(e) => return oauth_error(e),
    };

    match field("grant_type") {
        Some("authorization_code") => exchange_code(&app, &form, credentials).await,
        Some("refresh_token") => rotate(&app, &form, credentials).await,
        Some(other) => oauth_error(OauthError::new(
            "unsupported_grant_type",
            format!(
                "grant_type {other:?} is not supported. Use authorization_code or refresh_token."
            ),
        )),
        None => oauth_error(OauthError::new("invalid_request", "grant_type is required")),
    }
}

async fn exchange_code(
    app: &AuthServer,
    form: &HashMap<String, String>,
    credentials: ClientCredentials,
) -> Response {
    let field = |name: &str| form.get(name).map(String::as_str).filter(|v| !v.is_empty());

    let (Some(code), Some(redirect_uri), Some(verifier)) =
        (field("code"), field("redirect_uri"), field("code_verifier"))
    else {
        return oauth_error(OauthError::new(
            "invalid_request",
            "code, redirect_uri and code_verifier are all required",
        ));
    };

    let code_hash = hash_token(code);
    let outcome = match app.store.consume_code(&code_hash).await {
        Ok(o) => o,
        Err(e) => return internal_oauth(&e),
    };

    let record = match outcome {
        CodeOutcome::Fresh(record) => record,
        CodeOutcome::AlreadyConsumed { client_id } => {
            // The code leaked. Whatever was issued from it dies, including the tokens the legitimate
            // client is holding, because there is no way to tell which of the two presentations was
            // the real one.
            let family = family_for(&code_hash);
            tracing::error!(
                client_id = %client_id,
                family = %family,
                "authorization code replayed, revoking the token family"
            );
            if let Err(e) = app.store.revoke_family(family).await {
                tracing::error!(error = %e.log_message(), "could not revoke the family");
            }
            return oauth_error(OauthError::new(
                "invalid_grant",
                "this authorization code has already been used. Every token issued from it has been \
                 revoked. Start the flow again.",
            ));
        }
        CodeOutcome::Expired => {
            return oauth_error(OauthError::new(
                "invalid_grant",
                "this authorization code has expired. Start the flow again.",
            ))
        }
        CodeOutcome::Unknown => {
            return oauth_error(OauthError::new("invalid_grant", "unknown authorization code"))
        }
    };

    // Bindings, all three. The code belongs to one client, one redirect URI and one PKCE challenge,
    // and a code that matches on two of the three is a code being used by someone else.
    //
    // A public client that sends no client_id is tolerated rather than refused: the code already
    // names its client, so the check below narrows nothing when the field is absent, and refusing it
    // would break clients that rely on the code binding alone.
    if let Some(id) = credentials.client_id.as_deref() {
        if id != record.client_id {
            return oauth_error(OauthError::new(
                "invalid_grant",
                "this code was not issued to that client",
            ));
        }
    }
    if redirect_uri != record.redirect_uri {
        return oauth_error(OauthError::new(
            "invalid_grant",
            "redirect_uri does not match the one this code was issued for",
        ));
    }
    if !verify_pkce_s256(&record.code_challenge, verifier) {
        tracing::warn!(client_id = %record.client_id, "PKCE verification failed");
        return oauth_error(OauthError::new(
            "invalid_grant",
            "code_verifier does not match the code_challenge this code was issued for",
        ));
    }

    let client = match live_consented_client(app, &record.client_id).await {
        Ok(c) => c,
        Err(e) => return oauth_error(e),
    };
    if let Err(e) = authenticate_client(&client, &credentials) {
        return oauth_error(e);
    }

    let resource = match resource_for_exchange(record.resource.as_deref(), field("resource")) {
        Ok(r) => r,
        Err(e) => return oauth_error(e),
    };

    issue_tokens(app, &client, family_for(&code_hash), &record.scope, resource).await
}

async fn rotate(
    app: &AuthServer,
    form: &HashMap<String, String>,
    credentials: ClientCredentials,
) -> Response {
    let Some(presented) = form.get("refresh_token").filter(|v| !v.is_empty()) else {
        return oauth_error(OauthError::new("invalid_request", "refresh_token is required"));
    };

    // Before the rotation, not after. `rotate_refresh` sets consumed_at, and a resource this
    // server does not serve is a fixable request error under RFC 6749 §5.2: the client corrects it
    // and retries. Refusing after the spend means the retry presents a token that is already
    // consumed, which reads as a replay and revokes the whole family. So the audience is settled
    // while the refresh token is still worth something.
    let resource = form.get("resource").filter(|v| !v.is_empty()).cloned();
    if let Err(e) = resource_for_issue(
        resource.as_deref(),
        &app.cfg.auth.resource_url,
        app.cfg.oauth.resource_audience,
    ) {
        return oauth_error(e);
    }

    let outcome = match app.store.rotate_refresh(&hash_token(presented)).await {
        Ok(o) => o,
        Err(e) => return internal_oauth(&e),
    };

    let (client_id, family) = match outcome {
        RefreshOutcome::Rotated { client_id, family_id } => (client_id, family_id),
        RefreshOutcome::Replayed { family_id } => {
            // A refresh token that comes back after it was spent means a copy exists somewhere. The
            // family dies; the legitimate client re-authorizes.
            tracing::error!(family = %family_id, "refresh token replayed, revoking the token family");
            if let Err(e) = app.store.revoke_family(family_id).await {
                tracing::error!(error = %e.log_message(), "could not revoke the family");
            }
            return oauth_error(OauthError::new(
                "invalid_grant",
                "this refresh token has already been used. Every token in its family has been \
                 revoked. Start the flow again.",
            ));
        }
        RefreshOutcome::Expired => {
            return oauth_error(OauthError::new("invalid_grant", "this refresh token has expired"))
        }
        RefreshOutcome::Revoked => {
            return oauth_error(OauthError::new("invalid_grant", "this refresh token was revoked"))
        }
        RefreshOutcome::Unknown => {
            return oauth_error(OauthError::new("invalid_grant", "unknown refresh token"))
        }
    };

    if let Some(id) = credentials.client_id.as_deref() {
        if id != client_id {
            return oauth_error(OauthError::new(
                "invalid_grant",
                "this refresh token was not issued to that client",
            ));
        }
    }

    let client = match live_consented_client(app, &client_id).await {
        Ok(c) => c,
        Err(e) => return oauth_error(e),
    };
    if let Err(e) = authenticate_client(&client, &credentials) {
        return oauth_error(e);
    }

    // The refresh row holds no resource of its own, so a rotated token inherits nothing. The value
    // the client asks for now was settled above, before the spend, and `issue_tokens` stamps this
    // deployment's own resource when the client named none. That default is safe here because this
    // server serves exactly one resource, so the only audience it can supply is the one the client
    // is already talking to.
    issue_tokens(app, &client, family, DEFAULT_SCOPE, resource).await
}

async fn issue_tokens(
    app: &AuthServer,
    client: &OauthClientRecord,
    family: uuid::Uuid,
    scope: &str,
    asked: Option<String>,
) -> Response {
    // The one funnel both grants reach, so the audience decision lands here once instead of in each
    // caller. Every token this server mints leaves with a resource on it.
    let resource = match resource_for_issue(
        asked.as_deref(),
        &app.cfg.auth.resource_url,
        app.cfg.oauth.resource_audience,
    ) {
        Ok(r) => r,
        Err(e) => return oauth_error(e),
    };

    let access = match random_token(32) {
        Ok(t) => t,
        Err(e) => return internal_oauth(&e),
    };
    let now = chrono::Utc::now();

    let record = NewAccessToken {
        token_hash: hash_token(&access),
        client_id: client.client_id.clone(),
        scope: scope.to_string(),
        resource: Some(resource),
        family_id: family,
        expires_at: now + chrono::Duration::seconds(app.cfg.oauth.access_ttl_secs),
    };
    if let Err(e) = app.store.insert_token(record).await {
        return internal_oauth(&e);
    }

    // Only for a client that registered for it. A client that never asked for a refresh token and is
    // handed one holds a long-lived credential it does not know to protect.
    let refresh = if client.grant_types.iter().any(|g| g == "refresh_token") {
        match random_token(32) {
            Ok(t) => {
                let record = NewRefreshToken {
                    token_hash: hash_token(&t),
                    client_id: client.client_id.clone(),
                    family_id: family,
                    expires_at: now + chrono::Duration::seconds(app.cfg.oauth.refresh_ttl_secs),
                };
                if let Err(e) = app.store.insert_refresh(record).await {
                    return internal_oauth(&e);
                }
                Some(t)
            }
            Err(e) => return internal_oauth(&e),
        }
    } else {
        None
    };

    (
        // RFC 6749 §5.1. A cached token response is a token in a proxy's disk cache.
        [(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")],
        Json(TokenResponse {
            access_token: access,
            token_type: "Bearer",
            expires_in: app.cfg.oauth.access_ttl_secs,
            refresh_token: refresh,
            scope: scope.to_string(),
        }),
    )
        .into_response()
}

// ---- POST /oauth/revoke ----

async fn revoke(
    State(app): State<AuthServer>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let Some(presented) = form.get("token").filter(|v| !v.is_empty()) else {
        return oauth_error(OauthError::new("invalid_request", "token is required"));
    };
    let hash = hash_token(presented);

    // No client authentication. Possession of the token is what authorizes its revocation, and
    // demanding credentials as well would refuse a public client that holds nothing else. RFC 7009
    // §2.2 also requires 200 for a token that is already invalid, so nothing here reports whether
    // the token existed: that answer would turn this endpoint into an oracle.
    match app.store.revoke_token(&hash).await {
        Ok(true) => {}
        Ok(false) => {
            // Not an access token. Spending it as a refresh token both consumes it and hands back
            // its family, which is exactly what revoking a refresh token has to do: RFC 7009 says
            // revoking a refresh token revokes the whole authorization.
            match app.store.rotate_refresh(&hash).await {
                Ok(RefreshOutcome::Rotated { family_id, .. })
                | Ok(RefreshOutcome::Replayed { family_id }) => {
                    if let Err(e) = app.store.revoke_family(family_id).await {
                        tracing::error!(error = %e.log_message(), "revocation could not kill the family");
                    }
                }
                Ok(_) => {}
                Err(e) => return internal_oauth(&e),
            }
        }
        Err(e) => return internal_oauth(&e),
    }

    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(serde_json::json!({})))
        .into_response()
}

// ---- GET /oauth/clients ----

async fn clients(
    State(app): State<AuthServer>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    // Two ways in, both the owner: the CLI sends a bearer token that holds registry_write, and a
    // browser already signed in at the consent screen carries the session cookie. Nothing else lists
    // the clients, because the list names every surface connected to this store.
    let signed_in = app.sessions.verify(&headers, now()).is_some();
    if !signed_in {
        let header = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
        match app.auth.authenticate(header).await {
            Ok(principal) if principal.registry_write => {}
            Ok(principal) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({
                        "error": "forbidden",
                        "detail": format!("client {} may not list OAuth clients", principal.client),
                    })),
                )
                    .into_response()
            }
            Err(_) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Bearer")],
                    Json(serde_json::json!({ "error": "unauthorized" })),
                )
                    .into_response()
            }
        }
    }

    let include_revoked =
        matches!(query.get("include_revoked").map(String::as_str), Some("1" | "true" | "yes"));

    match app.store.list_clients(include_revoked).await {
        Ok(records) => {
            // snake_case throughout, because the wire contract is snake_case and the CLI reads it.
            let clients: Vec<serde_json::Value> = records
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "client_id": c.client_id,
                        "client_name": c.client_name,
                        "redirect_uris": c.redirect_uris,
                        "grant_types": c.grant_types,
                        "registered_via": c.registered_via,
                        "software_id": c.software_id,
                        "profile": c.profile,
                        "read": c.read,
                        "write": c.write,
                        "registry_write": c.registry_write,
                        "sealed_capable": c.sealed_capable,
                        "may_delete": c.may_delete,
                        "may_ingest": c.may_ingest,
                        "consented_at": c.consented_at,
                        "created_at": c.created_at,
                        "last_used_at": c.last_used_at,
                        "revoked_at": c.revoked_at,
                        "confidential": c.secret_hash.is_some(),
                    })
                })
                .collect();
            (
                [(header::CACHE_CONTROL, "no-store")],
                Json(serde_json::json!({ "count": clients.len(), "clients": clients })),
            )
                .into_response()
        }
        Err(e) => internal_json(&e),
    }
}

// ---- shared machinery ----

/// What a client presented to authenticate itself at the token endpoint.
struct ClientCredentials {
    client_id: Option<String>,
    secret: Option<String>,
}

/// HTTP Basic first, then the form fields. RFC 6749 §2.3.1 prefers Basic and allows the form, and
/// real clients use both. A request that carries two different client ids is refused rather than
/// resolved by precedence, because guessing which one the client meant is how a request ends up
/// authenticated as the wrong client.
fn client_credentials(
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> std::result::Result<ClientCredentials, OauthError> {
    let from_form = ClientCredentials {
        client_id: form.get("client_id").filter(|v| !v.is_empty()).cloned(),
        secret: form.get("client_secret").filter(|v| !v.is_empty()).cloned(),
    };

    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic ")))
        .map(str::trim);

    let Some(encoded) = basic else { return Ok(from_form) };

    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let decoded = STANDARD
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| {
            OauthError::new("invalid_client", "the Basic credentials are not valid base64 utf-8")
        })?;

    let (id, secret) = decoded.split_once(':').ok_or_else(|| {
        OauthError::new("invalid_client", "Basic credentials must be client_id:client_secret")
    })?;

    if let Some(form_id) = from_form.client_id.as_deref() {
        if form_id != id {
            return Err(OauthError::new(
                "invalid_client",
                "the client_id in the body does not match the one in the Authorization header",
            ));
        }
    }

    Ok(ClientCredentials { client_id: Some(id.to_string()), secret: Some(secret.to_string()) })
}

/// A confidential client must prove it holds the secret. A public client has none, and presenting
/// one is a client that thinks it is talking to a different server.
fn authenticate_client(
    client: &OauthClientRecord,
    credentials: &ClientCredentials,
) -> std::result::Result<(), OauthError> {
    match (&client.secret_hash, &credentials.secret) {
        (Some(expected), Some(presented)) => {
            if hashes_match(&hash_token(presented), expected) {
                Ok(())
            } else {
                Err(OauthError::new("invalid_client", "client authentication failed"))
            }
        }
        (Some(_), None) => Err(OauthError::new(
            "invalid_client",
            "this client is confidential and must authenticate with its secret",
        )),
        (None, Some(_)) => Err(OauthError::new(
            "invalid_client",
            "this client has no secret. Authenticate with PKCE alone.",
        )),
        (None, None) => Ok(()),
    }
}

/// The audience the code exchange carries forward, from what the code was bound to and what the
/// token request asks for now.
///
/// RFC 8707 §2.2. A resource on the token request must not widen the audience the code was bound
/// to. An absent one means "the one from the authorization", which is the common case because the
/// client already sent it at /authorize.
///
/// The two sides go through [`resource_matches`] rather than string equality. `https://host/mcp`
/// and `https://host/mcp/` name one audience, and refusing the second is an outage the client
/// cannot read off the error.
///
/// A first resource arriving here is parsed before it is kept. `AuthorizeRequest::validate` is the
/// only other place a resource is checked and it never runs on the token endpoint, so without this
/// a client that omitted `resource` at /authorize writes any string it likes onto the token row.
fn resource_for_exchange(
    bound: Option<&str>,
    asked: Option<&str>,
) -> std::result::Result<Option<String>, OauthError> {
    match (bound, asked) {
        (Some(bound), Some(asked)) => {
            if resource_matches(bound, asked) {
                Ok(Some(bound.to_string()))
            } else {
                Err(OauthError::new(
                    "invalid_target",
                    "resource does not match the one this code was issued for",
                ))
            }
        }
        (Some(bound), None) => Ok(Some(bound.to_string())),
        (None, Some(asked)) => {
            if canonical_resource(asked).is_none() {
                return Err(OauthError::new(
                    "invalid_target",
                    "resource must be an absolute URI without a fragment",
                ));
            }
            Ok(Some(asked.to_string()))
        }
        (None, None) => Ok(None),
    }
}

/// The audience stamped on a token at issuance, from what the client asked for and what this
/// deployment serves.
///
/// The stamp on the absent case is what makes `OAUTH_RESOURCE_AUDIENCE=strict` reachable. The CLI
/// in this repository sends `resource` at /authorize and at the code exchange and never on the
/// refresh grant (crates/lumberroom/src/client.rs:375-382), and `oauth_refresh` holds no resource
/// column for a rotated token to inherit, so every token it rotates would carry NULL forever and
/// the pre-existing NULLs would never drain. With the stamp they drain within one
/// OAUTH_ACCESS_TTL_SECS.
///
/// A match keeps the client's own spelling. The row records what the client asserted, and
/// canonicalisation is a comparison rule rather than a storage rule.
///
/// `Off` still stamps and still refuses a resource that is not a URI, and it stops refusing one
/// this deployment does not serve. That is the whole point of the setting: a deployment reachable
/// under two names has clients that legitimately name the other one, and refusing them here would
/// leave the operator with a switch that turns off half the behaviour it is documented to turn off.
fn resource_for_issue(
    asked: Option<&str>,
    served: &str,
    audience: ResourceAudience,
) -> std::result::Result<String, OauthError> {
    let Some(asked) = asked else { return Ok(served.to_string()) };
    // A value that will not parse is refused whatever the setting says. It is not an audience
    // disagreement, it is not a URI, and storing it would put text on the token row that no
    // comparison can ever match.
    if canonical_resource(asked).is_none() {
        return Err(OauthError::new(
            "invalid_target",
            "resource must be an absolute URI without a fragment",
        ));
    }
    if audience != ResourceAudience::Off && !resource_matches(asked, served) {
        // RFC 8707 §2.2. The description names no value: the input came off a form and echoing it
        // back turns this error into a reflection point.
        return Err(OauthError::new(
            "invalid_target",
            "this server does not issue tokens for that resource",
        ));
    }
    // The row records what the client asserted. Canonicalisation is a comparison rule rather than
    // a storage rule.
    Ok(asked.to_string())
}

async fn live_consented_client(
    app: &AuthServer,
    client_id: &str,
) -> std::result::Result<OauthClientRecord, OauthError> {
    let record = app
        .store
        .find_client(client_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e.log_message(), "client lookup failed");
            OauthError::new("invalid_grant", "cannot verify this client right now")
        })?
        .ok_or_else(|| OauthError::new("invalid_client", "unknown client"))?;

    if !record.is_live() {
        return Err(OauthError::new("invalid_client", "this client has been revoked"));
    }
    // Registration is not authorization. A client that registered itself and was never approved
    // reaches this point with an empty grant and gets no token.
    if !record.has_consent() {
        return Err(OauthError::new(
            "invalid_grant",
            "this client has not been approved by the owner",
        ));
    }
    Ok(record)
}

/// Load the client and validate the authorization request against it.
///
/// The error side is always a rendered page. `AuthorizeRequest::validate` checks the redirect URI
/// first precisely so that a caller here cannot report any of these failures by redirecting.
async fn resolve_request(
    app: &AuthServer,
    request: AuthorizeRequest,
) -> std::result::Result<(OauthClientRecord, AuthorizeIntent), Response> {
    if request.client_id.is_empty() {
        return Err(page(
            StatusCode::BAD_REQUEST,
            pages::error_page("missing client_id", "This link has no client_id on it."),
        ));
    }

    let record = match app.store.find_client(&request.client_id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return Err(page(
                StatusCode::BAD_REQUEST,
                pages::error_page(
                    "unknown client",
                    "No client is registered under that id on this server.",
                ),
            ))
        }
        Err(e) => return Err(page_internal(&e)),
    };

    if !record.is_live() {
        return Err(page(
            StatusCode::BAD_REQUEST,
            pages::error_page("client revoked", "This client's access was revoked."),
        ));
    }

    let intent = match request.validate(&record.redirect_uris) {
        Ok(intent) => intent,
        Err(e) => {
            return Err(page(
                StatusCode::BAD_REQUEST,
                pages::error_page("cannot start", e.client_message()),
            ))
        }
    };

    // RFC 8707 §2.1 wants a resource this server does not serve refused here, at the authorization
    // endpoint. Doing it only at /token means the owner reads a consent screen, approves a client,
    // and the exchange then fails after the code has been consumed, so the client has to run the
    // whole flow again to see the same error. Refusing before a code exists costs one comparison.
    //
    // Rendered as a page rather than redirected with `error=`, which is the contract every error
    // out of here follows: redirecting before the request is known to be sound is what turns this
    // endpoint into an open redirector.
    if app.cfg.oauth.resource_audience != ResourceAudience::Off {
        if let Some(asked) = intent.resource.as_deref() {
            if !resource_matches(asked, &app.cfg.auth.resource_url) {
                return Err(page(
                    StatusCode::BAD_REQUEST,
                    pages::error_page(
                        "cannot start",
                        "This client asked for a resource this server does not serve.",
                    ),
                ));
            }
        }
    }

    Ok((record, intent))
}

fn authorize_request(params: &HashMap<String, String>) -> AuthorizeRequest {
    // Nothing is trimmed. The redirect URI and the challenge are compared byte for byte, and a
    // server that tidies them up here compares something the client did not send.
    let get = |key: &str| params.get(key).cloned();
    AuthorizeRequest {
        response_type: get("response_type").unwrap_or_default(),
        client_id: get("client_id").unwrap_or_default(),
        redirect_uri: get("redirect_uri").unwrap_or_default(),
        code_challenge: get("code_challenge").unwrap_or_default(),
        code_challenge_method: get("code_challenge_method"),
        state: get("state"),
        scope: get("scope"),
        resource: get("resource"),
    }
}

fn flow_fields<'a>(intent: &'a AuthorizeIntent) -> FlowFields<'a> {
    FlowFields {
        client_id: &intent.client_id,
        redirect_uri: &intent.redirect_uri,
        code_challenge: &intent.code_challenge,
        // Validation has already refused anything else, so this is not read back off the form as a
        // way to change the method.
        code_challenge_method: "S256",
        response_type: "code",
        state: intent.state.as_deref(),
        scope: Some(&intent.scope),
        resource: intent.resource.as_deref(),
    }
}

fn login_page(
    client: &OauthClientRecord,
    intent: &AuthorizeIntent,
    status: StatusCode,
    error: Option<&str>,
) -> Response {
    page(status, pages::login(&flow_fields(intent), &client.client_name, error))
}

fn consent_page(
    app: &AuthServer,
    client: &OauthClientRecord,
    intent: &AuthorizeIntent,
    session: &OwnerSession,
    status: StatusCode,
) -> Response {
    let csrf = app.sessions.csrf(
        session,
        &intent.client_id,
        &intent.redirect_uri,
        &intent.code_challenge,
        intent.state.as_deref().unwrap_or(""),
    );
    let view = ClientView {
        client_name: &client.client_name,
        client_id: &client.client_id,
        redirect_uri: &intent.redirect_uri,
        software_id: client.software_id.as_deref(),
        self_registered: client.registered_via == "dcr",
        current_profile: client.profile.as_deref(),
    };
    page(status, pages::consent(&flow_fields(intent), &view, &csrf, app.default_profile()))
}

fn page(status: StatusCode, body: String) -> Response {
    // no-store on every page: they carry a CSRF token bound to a session, and a cached consent screen
    // is a form that outlives the login it belongs to.
    (status, [(header::CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

fn page_internal(e: &DomainError) -> Response {
    tracing::error!(error = %e.log_message(), "authorization server failed");
    page(
        StatusCode::INTERNAL_SERVER_ERROR,
        pages::error_page(
            "something broke",
            "This server could not complete the request. Nothing was granted. The failure is in its \
             log.",
        ),
    )
}

fn redirect(location: &str) -> Response {
    match header::HeaderValue::from_str(location) {
        Ok(value) => (
            StatusCode::FOUND,
            [
                (header::LOCATION, value),
                (header::CACHE_CONTROL, header::HeaderValue::from_static("no-store")),
            ],
        )
            .into_response(),
        // A redirect URI that cannot be a header value never passed registration, so this is a bug
        // rather than a client error. It must not fall through to a 200.
        Err(_) => page_internal(&DomainError::internal(format!(
            "redirect target is not a valid header value: {location}"
        ))),
    }
}

/// An OAuth error reported to the client, by redirect. Only reachable once the redirect URI has been
/// matched against the client record.
fn redirect_error(intent: &AuthorizeIntent, error: &str, description: &str) -> Response {
    let mut pairs =
        vec![("error", error.to_string()), ("error_description", description.to_string())];
    if let Some(state) = intent.state.as_deref().filter(|s| !s.is_empty()) {
        pairs.push(("state", state.to_string()));
    }
    redirect(&append_query(&intent.redirect_uri, &pairs))
}

fn oauth_error(e: OauthError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    (status, [(header::CACHE_CONTROL, "no-store")], Json(e)).into_response()
}

/// A fault of ours, in the shape a client can read. `server_error` is a registered code, so a client
/// shows something better than a parse failure.
fn internal_oauth(e: &DomainError) -> Response {
    tracing::error!(error = %e.log_message(), "token endpoint failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "error": "server_error",
            "error_description": "this server could not complete the request",
        })),
    )
        .into_response()
}

fn registration_error(
    status: StatusCode,
    error: &'static str,
    description: impl Into<String>,
) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "error": error,
            "error_description": description.into(),
        })),
    )
        .into_response()
}

fn internal_json(e: &DomainError) -> Response {
    tracing::error!(error = %e.log_message(), "authorization server failed");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "server_error" })))
        .into_response()
}

/// Argon2id verification, off the reactor.
///
/// A password hash is deliberately expensive, tens of milliseconds of CPU, and running it inline
/// would stall every other request sharing the thread. The password is zeroized once the answer is
/// known rather than left for the allocator.
///
/// `work` travels into the blocking closure so the permit is held for as long as the memory is,
/// rather than for as long as the caller waits.
async fn verify_owner_password(
    hash: String,
    password: String,
    work: tokio::sync::SemaphorePermit<'static>,
) -> Result<bool> {
    tokio::task::spawn_blocking(move || {
        let _work = work;
        let mut password = password;
        let parsed = PasswordHash::new(&hash).map_err(|e| {
            DomainError::internal(format!("OWNER_PASSWORD_HASH is not a valid PHC string: {e}"))
        })?;
        // Argon2 reads its own cost parameters out of the hash, and the comparison inside is
        // constant time.
        let ok = Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok();
        password.zeroize();
        Ok(ok)
    })
    .await
    .map_err(|e| DomainError::internal("the password check did not finish").with_source(e))?
}

/// The token family for one authorization, derived from the code that produced it.
///
/// Derived rather than random on purpose. `oauth_code` carries no family column and
/// `CodeOutcome::AlreadyConsumed` carries no family either, so a replayed code has to be traceable to
/// the tokens it minted from the code alone. The first 16 bytes of the code's SHA-256 are already
/// unpredictable and unique to that code, which is everything a family id needs to be.
fn family_for(code_hash: &str) -> uuid::Uuid {
    let mut id = [0u8; 16];
    // The input is always our own `hash_token` output, so the decode cannot fail. A malformed input
    // would produce a constant family rather than a panic, and the caller has already refused the
    // request by then.
    if let Ok(bytes) = hex::decode(code_hash) {
        for (slot, byte) in id.iter_mut().zip(bytes) {
            *slot = byte;
        }
    }
    uuid::Uuid::from_bytes(id)
}

/// Append query parameters to a redirect URI without assuming it looks like an https URL.
///
/// A private-use scheme such as `com.example.app:callback` is a URL that cannot be a base, and
/// `Url::query_pairs_mut` panics on one. That URI is legal at registration under RFC 8252 §7.1, so
/// the string path is not a fallback for malformed input, it is the path for a whole class of native
/// clients.
fn append_query(uri: &str, pairs: &[(&str, String)]) -> String {
    match url::Url::parse(uri) {
        Ok(mut parsed) if !parsed.cannot_be_a_base() => {
            {
                let mut query = parsed.query_pairs_mut();
                for (key, value) in pairs {
                    query.append_pair(key, value);
                }
            }
            parsed.to_string()
        }
        _ => {
            let mut out = uri.to_string();
            for (key, value) in pairs {
                out.push(if out.contains('?') { '&' } else { '?' });
                out.push_str(key);
                out.push('=');
                out.push_str(&urlencode(value));
            }
            out
        }
    }
}

/// Percent-encode everything outside the unreserved set. Deliberately conservative: a `state` value
/// is opaque text chosen by the client and may hold anything.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The key the login limiter counts against.
///
/// Behind a reverse proxy the peer address is the proxy, so a forwarded address is preferred when
/// present even though it cannot be verified. The peer window in the limiter is what makes
/// inventing one pointless. Truncated, because the key is a map key and the header is attacker
/// supplied.
fn throttle_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let first = forwarded.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return format!("fwd:{}", first.chars().take(48).collect::<String>());
        }
    }
    match peer {
        Some(addr) => format!("peer:{}", addr.ip()),
        // ConnectInfo is absent when the server was not built with it. The limiter still holds the
        // peer window, so an absent address degrades the accounting rather than removing it.
        None => "unknown".to_string(),
    }
}

/// The socket the request arrived on, which no header can change.
///
/// The limiter's second window hangs off this. A caller inventing a forwarded address per request
/// still arrives on one socket and spends one budget, and a caller on a different socket is not
/// charged for that.
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

    #[test]
    fn a_family_id_is_stable_for_one_code_and_different_for_another() {
        let a = family_for(&hash_token("code-a"));
        let b = family_for(&hash_token("code-b"));
        assert_eq!(
            a,
            family_for(&hash_token("code-a")),
            "a replay must resolve to the same family"
        );
        assert_ne!(a, b);
        assert_ne!(a, uuid::Uuid::nil());
    }

    #[test]
    fn a_family_id_from_a_malformed_hash_does_not_panic() {
        assert_eq!(family_for("not hex"), uuid::Uuid::nil());
        assert_eq!(family_for(""), uuid::Uuid::nil());
    }

    #[test]
    fn query_parameters_are_appended_to_an_https_redirect_uri() {
        let out = append_query("https://claude.ai/cb", &[("code", "abc".into())]);
        assert_eq!(out, "https://claude.ai/cb?code=abc");
    }

    #[test]
    fn query_parameters_join_an_existing_query_rather_than_replacing_it() {
        let out = append_query(
            "https://claude.ai/cb?next=%2Fhome",
            &[("code", "abc".into()), ("state", "s 1".into())],
        );
        assert!(out.starts_with("https://claude.ai/cb?next=%2Fhome&"));
        assert!(out.contains("code=abc"));
        assert!(out.contains("state=s+1") || out.contains("state=s%201"));
    }

    #[test]
    fn a_loopback_redirect_uri_keeps_its_port_and_path() {
        let out = append_query("http://127.0.0.1:7711/callback", &[("code", "abc".into())]);
        assert_eq!(out, "http://127.0.0.1:7711/callback?code=abc");
    }

    #[test]
    fn a_private_use_scheme_that_cannot_be_a_base_still_gets_its_parameters() {
        // Url::query_pairs_mut panics on this shape, and it is a legal registered redirect URI.
        let out = append_query("com.example.app:callback", &[("code", "abc".into())]);
        assert_eq!(out, "com.example.app:callback?code=abc");
    }

    #[test]
    fn a_state_value_is_percent_encoded_on_the_string_path() {
        let out = append_query("com.example.app:cb", &[("state", "a b&c=d".into())]);
        assert_eq!(out, "com.example.app:cb?state=a%20b%26c%3Dd");
    }

    #[test]
    fn a_forwarded_address_is_preferred_and_truncated() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        assert_eq!(throttle_key(&headers, Some(peer)), "fwd:203.0.113.9");

        let long = "x".repeat(200);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", long.parse().unwrap());
        assert!(throttle_key(&headers, None).len() <= 52);
    }

    #[test]
    fn the_peer_address_is_used_when_nothing_was_forwarded() {
        let peer: SocketAddr = "198.51.100.7:44000".parse().unwrap();
        assert_eq!(throttle_key(&HeaderMap::new(), Some(peer)), "peer:198.51.100.7");
        assert_eq!(throttle_key(&HeaderMap::new(), None), "unknown");
    }

    /// The limiter's second window hangs off this, and it has to ignore the header a caller can
    /// write. Ports differ per connection, so the address alone is the key.
    #[test]
    fn the_peer_key_ignores_the_forwarded_header_and_the_source_port() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        let one: SocketAddr = "198.51.100.7:44000".parse().unwrap();
        let two: SocketAddr = "198.51.100.7:44001".parse().unwrap();
        assert_eq!(peer_key(Some(one)), "198.51.100.7");
        assert_eq!(peer_key(Some(two)), peer_key(Some(one)));
        assert_ne!(peer_key(Some(one)), throttle_key(&headers, Some(one)));
        assert_eq!(peer_key(None), "unknown");
    }

    #[test]
    fn basic_credentials_are_read_and_must_agree_with_the_body() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("abc:s3cret")).parse().unwrap(),
        );

        let empty = HashMap::new();
        let c = client_credentials(&headers, &empty).unwrap();
        assert_eq!(c.client_id.as_deref(), Some("abc"));
        assert_eq!(c.secret.as_deref(), Some("s3cret"));

        let mut form = HashMap::new();
        form.insert("client_id".to_string(), "someone-else".to_string());
        assert!(client_credentials(&headers, &form).is_err());
    }

    #[test]
    fn form_credentials_are_used_when_there_is_no_basic_header() {
        let mut form = HashMap::new();
        form.insert("client_id".to_string(), "abc".to_string());
        form.insert("client_secret".to_string(), "s3cret".to_string());
        let c = client_credentials(&HeaderMap::new(), &form).unwrap();
        assert_eq!(c.client_id.as_deref(), Some("abc"));
        assert_eq!(c.secret.as_deref(), Some("s3cret"));
    }

    #[test]
    fn an_empty_form_field_is_the_same_as_an_absent_one() {
        let mut form = HashMap::new();
        form.insert("client_id".to_string(), String::new());
        let c = client_credentials(&HeaderMap::new(), &form).unwrap();
        assert!(c.client_id.is_none());
    }

    fn record(secret_hash: Option<&str>) -> OauthClientRecord {
        OauthClientRecord {
            client_id: "abc".into(),
            secret_hash: secret_hash.map(str::to_string),
            client_name: "Claude".into(),
            redirect_uris: vec!["https://claude.ai/cb".into()],
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            registered_via: "dcr".into(),
            software_id: None,
            read: vec![],
            write: vec![],
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
            consented_at: Some(chrono::Utc::now()),
            profile: Some("standard".into()),
            created_at: chrono::Utc::now(),
            last_used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn a_public_client_authenticates_with_pkce_alone() {
        let c = ClientCredentials { client_id: Some("abc".into()), secret: None };
        assert!(authenticate_client(&record(None), &c).is_ok());
    }

    #[test]
    fn a_public_client_presenting_a_secret_is_refused() {
        let c = ClientCredentials { client_id: Some("abc".into()), secret: Some("x".into()) };
        let e = authenticate_client(&record(None), &c).unwrap_err();
        assert_eq!(e.error, "invalid_client");
    }

    #[test]
    fn a_confidential_client_must_present_the_right_secret() {
        let stored = hash_token("s3cret");
        let good =
            ClientCredentials { client_id: Some("abc".into()), secret: Some("s3cret".into()) };
        assert!(authenticate_client(&record(Some(&stored)), &good).is_ok());

        let bad = ClientCredentials { client_id: Some("abc".into()), secret: Some("wrong".into()) };
        assert_eq!(
            authenticate_client(&record(Some(&stored)), &bad).unwrap_err().error,
            "invalid_client"
        );

        let missing = ClientCredentials { client_id: Some("abc".into()), secret: None };
        assert_eq!(
            authenticate_client(&record(Some(&stored)), &missing).unwrap_err().error,
            "invalid_client"
        );
    }

    fn document(dcr: bool) -> serde_json::Value {
        metadata_document("https://lumberroom.example", &["memory.read".to_string()], dcr)
    }

    #[test]
    fn every_metadata_url_comes_from_the_public_url() {
        let doc = document(true);
        assert_eq!(doc["issuer"], "https://lumberroom.example");
        assert_eq!(doc["authorization_endpoint"], "https://lumberroom.example/oauth/authorize");
        assert_eq!(doc["token_endpoint"], "https://lumberroom.example/oauth/token");
        assert_eq!(doc["revocation_endpoint"], "https://lumberroom.example/oauth/revoke");
        assert_eq!(doc["registration_endpoint"], "https://lumberroom.example/oauth/register");
    }

    #[test]
    fn the_metadata_advertises_s256_and_nothing_else() {
        let doc = document(true);
        assert_eq!(doc["code_challenge_methods_supported"], serde_json::json!(["S256"]));
        assert_eq!(doc["response_types_supported"], serde_json::json!(["code"]));
        assert_eq!(
            doc["grant_types_supported"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
    }

    #[test]
    fn a_disabled_registration_endpoint_is_not_advertised() {
        let doc = document(false);
        assert!(doc.get("registration_endpoint").is_none());
    }

    // ---- RFC 8707 audience decisions ----

    /// The audience this deployment serves, in the shape `cfg.auth.resource_url` carries.
    const SERVED: &str = "https://memory.example.com/mcp";

    #[test]
    fn a_bound_resource_matches_the_same_one_asked_for_with_a_trailing_slash() {
        let asked = "https://memory.example.com/mcp/";
        let out = resource_for_exchange(Some(SERVED), Some(asked)).unwrap();
        assert_eq!(out.as_deref(), Some(SERVED));
    }

    #[test]
    fn a_bound_resource_matches_the_same_one_asked_for_with_its_default_port() {
        let asked = "https://memory.example.com:443/mcp";
        let out = resource_for_exchange(Some(SERVED), Some(asked)).unwrap();
        assert_eq!(out.as_deref(), Some(SERVED));
    }

    #[test]
    fn a_resource_naming_another_host_than_the_code_did_is_refused() {
        let asked = "https://elsewhere.example.com/mcp";
        let e = resource_for_exchange(Some(SERVED), Some(asked)).unwrap_err();
        assert_eq!(e.error, "invalid_target");
    }

    #[test]
    fn a_resource_that_is_not_an_absolute_uri_never_reaches_the_token_row() {
        for asked in ["/mcp", "https://memory.example.com/mcp#frag", "what the client typed"] {
            let e = resource_for_exchange(None, Some(asked)).unwrap_err();
            assert_eq!(e.error, "invalid_target", "{asked} must not be stored");
        }
    }

    #[test]
    fn an_absent_resource_on_the_token_request_keeps_the_one_from_the_authorization() {
        let out = resource_for_exchange(Some(SERVED), None).unwrap();
        assert_eq!(out.as_deref(), Some(SERVED));
    }

    #[test]
    fn a_first_resource_arriving_at_the_token_endpoint_is_kept_once_it_parses() {
        let out = resource_for_exchange(None, Some(SERVED)).unwrap();
        assert_eq!(out.as_deref(), Some(SERVED));
    }

    /// The pair that drains the NULLs. A client that named no resource anywhere leaves the exchange
    /// with none, and issuance is what puts this deployment's own resource on the row.
    #[test]
    fn a_token_from_an_exchange_that_named_no_resource_still_carries_the_served_one() {
        let out = resource_for_exchange(None, None).unwrap();
        assert_eq!(out, None);
        assert_eq!(
            resource_for_issue(out.as_deref(), SERVED, ResourceAudience::Lenient).unwrap(),
            SERVED
        );
    }

    #[test]
    fn issuance_stamps_the_served_resource_when_the_client_asked_for_nothing() {
        assert_eq!(resource_for_issue(None, SERVED, ResourceAudience::Lenient).unwrap(), SERVED);
    }

    #[test]
    fn issuance_keeps_the_spelling_the_client_sent_rather_than_the_canonical_form() {
        let asked = "https://memory.example.com:443/mcp/";
        assert_eq!(
            resource_for_issue(Some(asked), SERVED, ResourceAudience::Lenient).unwrap(),
            asked
        );
    }

    #[test]
    fn issuance_refuses_a_resource_this_deployment_does_not_serve() {
        let asked = "https://elsewhere.example.com/mcp";
        let e = resource_for_issue(Some(asked), SERVED, ResourceAudience::Lenient).unwrap_err();
        assert_eq!(e.error, "invalid_target");

        let description = e.error_description.unwrap_or_default();
        assert!(
            !description.contains("elsewhere"),
            "an attacker-chosen value must not come back in the error: {description}"
        );
    }

    /// The setting is documented as the escape hatch for a deployment reachable under two names.
    /// If it only relaxed the authenticator, the operator would set it and still watch the token
    /// endpoint refuse the client it was set for.
    #[test]
    fn issuance_stops_refusing_a_foreign_resource_when_the_check_is_off() {
        let out = resource_for_issue(
            Some("https://elsewhere.example.com/mcp"),
            SERVED,
            ResourceAudience::Off,
        )
        .unwrap();
        assert_eq!(out, "https://elsewhere.example.com/mcp");
    }

    /// `off` relaxes the audience and nothing else. A value that is not a URI can never match any
    /// comparison, so storing it would put dead text on the token row.
    #[test]
    fn issuance_still_refuses_an_unparseable_resource_when_the_check_is_off() {
        let e = resource_for_issue(Some("../mcp"), SERVED, ResourceAudience::Off).unwrap_err();
        assert_eq!(e.error, "invalid_target");
    }

    #[test]
    fn issuance_still_stamps_the_served_resource_when_the_check_is_off() {
        assert_eq!(resource_for_issue(None, SERVED, ResourceAudience::Off).unwrap(), SERVED);
    }

    #[test]
    fn issuance_refuses_a_resource_it_cannot_parse() {
        let e = resource_for_issue(Some("../mcp"), SERVED, ResourceAudience::Lenient).unwrap_err();
        assert_eq!(e.error, "invalid_target");
    }
}
