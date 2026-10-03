//! `lumberroom login`: dynamic registration, PKCE S256, a loopback listener, code exchange.
//!
//! RFC 8252. A CLI cannot hold a redirect URI a browser would accept as a real origin, so it binds
//! loopback and registers that exact URI. The server compares `redirect_uri` byte for byte at both
//! `/authorize` and `/token`, never by prefix, which is the whole reason the port is pinned and
//! persisted rather than ephemeral: an ephemeral port works for exactly one login, and every later
//! one reuses the stored `client_id` while binding a port that client was never registered with.

use base64::Engine;
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::client::{err, Client, Payload, Result};
use crate::out;

/// The first-ever login's port. Fixed rather than ephemeral, and persisted with the client id.
pub const DEFAULT_LOOPBACK_PORT: u16 = 8976;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// The RFC 8414 document, relative to the API base the owner configured.
pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";

/// The hosted deployment's hosts, listed literally rather than derived from a pattern.
///
/// Two paths through this file, and the host of the configured base picks one. The hosted API base
/// and the hosted issuer are different hosts: the MCP endpoint answers on `mcp.lumberroom.cloud`
/// while the document served there names `lumberroom.cloud` as the authorization server, so
/// `{base}/oauth/authorize` addresses a host that is not the issuer. Only the hosted path reads the
/// metadata document. Every other host is somebody's self-hosted deployment and keeps the
/// base-relative construction it has always had, with no discovery fetch and no new failure mode.
/// A second hosted region gets added here and nowhere else.
pub const HOSTED_HOSTS: [&str; 2] = ["lumberroom.cloud", "mcp.lumberroom.cloud"];

/// Whether a base URL belongs to the hosted deployment.
///
/// The comparison is exact and case-insensitive. A suffix match would hand the whole OAuth flow to
/// `evil-lumberroom.cloud`, and a prefix match would hand it to `lumberroom.cloud.attacker.tld`.
///
/// The scheme is part of the test. This document decides where the browser is sent and where the
/// code verifier is posted, so fetching it over plain http lets anyone on the path answer it and
/// name their own endpoints. A base of `http://lumberroom.cloud` therefore takes the self-hosted
/// path, where nothing is fetched, rather than the hosted one.
pub fn is_hosted(base_url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(base_url) else { return false };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else { return false };
    HOSTED_HOSTS.iter().any(|hosted| host.eq_ignore_ascii_case(hosted))
}

/// Where this client sends each half of the OAuth flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub register: String,
    /// Nothing in this client revokes yet. It is carried because the document names it and the
    /// first caller should not have to plumb discovery a second time.
    pub revoke: Option<String>,
}

impl Endpoints {
    /// The self-hosted path, unchanged since the first release. A deployment on any host other
    /// than the hosted ones must not be able to tell this build apart from the last one.
    pub fn base_relative(http_base: &str) -> Self {
        Self {
            authorize: format!("{http_base}/oauth/authorize"),
            token: format!("{http_base}/oauth/token"),
            register: format!("{http_base}/oauth/register"),
            revoke: Some(format!("{http_base}/oauth/revoke")),
        }
    }

    /// The hosted path. Every endpoint this client calls has to be present and has to survive
    /// `is_trustworthy_endpoint`, and a document that misses one fails the command.
    ///
    /// Falling back to base-relative construction here would put back the exact bug this replaces,
    /// and it would do it silently. An error naming the discovery URL costs a confused minute; a
    /// browser sent to the wrong host costs an afternoon.
    pub fn from_metadata(metadata: &Value, discovery_url: &str) -> Result<Self> {
        Ok(Self {
            authorize: required(metadata, "authorization_endpoint", discovery_url)?,
            token: required(metadata, "token_endpoint", discovery_url)?,
            register: required(metadata, "registration_endpoint", discovery_url)?,
            revoke: metadata
                .get("revocation_endpoint")
                .and_then(Value::as_str)
                .filter(|url| is_trustworthy_endpoint(url))
                .map(str::to_string),
        })
    }
}

fn required(metadata: &Value, field: &str, discovery_url: &str) -> Result<String> {
    let advertised = metadata.get(field).and_then(Value::as_str).unwrap_or_default();
    if !is_trustworthy_endpoint(advertised) {
        return Err(err(format!(
            "the authorization server metadata at {discovery_url} advertises no usable {field} \
(got {advertised:?}). An endpoint has to be an https URL on a host this client knows."
        )));
    }
    Ok(advertised.to_string())
}

/// Whether a URL out of a discovery document may be sent a credential.
///
/// These endpoints receive the code verifier, the client secret and the browser itself, so the
/// document decides where a credential goes. "Any https URL" is not a check: a document that named
/// `https://attacker.example/oauth/token` would pass it, and this client would post the code and
/// verifier there itself.
///
/// So the host has to be one of `HOSTED_HOSTS`. Only the hosted path reads a document at all, and
/// the hosted issuer is a host this client already knows, so an endpoint anywhere else is a
/// document saying something it has no business saying. That keeps the blast radius of a bad or
/// tampered document to the hosts the credential was configured for.
///
/// Plain http is refused outright, including on loopback. This function only ever sees URLs out of
/// a hosted document, and the hosted deployment does not serve loopback.
/// Whether a URL may carry this client's bearer token or refresh token.
///
/// https anywhere, and plain http on loopback alone. A token sent to `http://a-remote-host` is a
/// credential on the wire in the clear, and this client has always been willing to do that: the
/// base URL is whatever the operator configured and nothing checked its scheme. Routing every
/// endpoint through one function is what made the path visible, and the check belongs with it.
///
/// Loopback stays allowed because it is the default (`http://127.0.0.1:8787`) and the whole
/// self-hosted development story, and a token that never leaves the machine leaks to nobody.
pub fn may_carry_credential(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    match parsed.scheme() {
        "https" => true,
        "http" => parsed.host_str().is_some_and(is_loopback_host),
        _ => false,
    }
}

/// A URL that has passed `may_carry_credential`, and the only kind this client posts a token to.
///
/// The check and the send were two statements a reader had to hold together, one early return
/// apart. `refresh` puts a refresh token and a client secret in a form body, so a caller who
/// reordered those statements, or added a second send below them, would put both on the wire in
/// the clear against any host the operator had configured. Wrapping the checked value makes the
/// check the only way to obtain something the send accepts: skip it and there is nothing to pass.
///
/// `build` still calls `may_carry_credential` on its own, because it sends the request either way
/// and drops the authorization header alone. That is a header decision, not a destination one.
#[derive(Debug, Clone)]
pub struct CredentialUrl(String);

impl CredentialUrl {
    /// `None` when sending a credential here would put it in the clear.
    pub fn checked(url: &str) -> Option<Self> {
        may_carry_credential(url).then(|| Self(url.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether a host is this machine.
fn is_loopback_host(host: &str) -> bool {
    // `Url::host_str` hands back an IPv6 literal still wrapped in its brackets.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

pub fn is_trustworthy_endpoint(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    if parsed.scheme() != "https" {
        return false;
    }
    parsed
        .host_str()
        .is_some_and(|host| HOSTED_HOSTS.iter().any(|hosted| host.eq_ignore_ascii_case(hosted)))
}

pub fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| err(format!("no entropy available: {e}")))?;
    Ok(buf)
}

/// RFC 7636 §4.2: `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`, unpadded.
pub fn pkce_challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// Node writes `new Date(...).toISOString()` into the shared config file. Millisecond precision
/// with a `Z`, so the other client parses back what this one wrote.
pub fn expires_at(ttl_seconds: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(ttl_seconds))
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/c", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    // Best effort. The URL is always printed as well.
    let _ = std::process::Command::new(program).args(args).spawn();
}

/// What came back on the loopback callback.
enum Callback {
    Code(String),
    Failed(String),
}

/// Serve until one real `/callback` arrives.
///
/// Accepting once is not enough. Browsers open speculative connections they never write a request
/// on and ask for `/favicon.ico` unprompted, so a single accept can consume a connection that
/// carries no callback and then wait forever on one that never comes.
async fn wait_for_callback(
    listener: TcpListener,
    path: &str,
    expected_state: &str,
) -> Result<String> {
    let deadline = tokio::time::sleep(SIGN_IN_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        let (mut socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.map_err(|e| err(format!("loopback accept failed: {e}")))?,
            _ = &mut deadline => {
                return Err(err("timed out waiting for the browser sign-in (5 minutes)"));
            }
        };

        let mut buf = [0u8; 8192];
        let read = match socket.read(&mut buf).await {
            Ok(0) | Err(_) => continue,
            Ok(n) => n,
        };
        let request = String::from_utf8_lossy(&buf[..read]).to_string();
        let Some(target) = request.split_whitespace().nth(1) else { continue };

        let url = match reqwest::Url::parse(&format!("http://127.0.0.1{target}")) {
            Ok(u) => u,
            Err(_) => {
                respond(&mut socket, 400, "").await;
                continue;
            }
        };
        if url.path() != path {
            respond(&mut socket, 404, "not found").await;
            continue;
        }

        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut description = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                "error_description" => description = Some(v.into_owned()),
                _ => {}
            }
        }

        let (page, outcome) = if let Some(e) = error {
            (
                "lumberroom: sign-in was cancelled or failed. You can close this window.",
                Callback::Failed(callback_error(&e, description.as_deref())),
            )
        } else if state.as_deref() != Some(expected_state) {
            (
                "lumberroom: state mismatch, aborting. You can close this window.",
                Callback::Failed("state parameter mismatch on the OAuth callback".to_string()),
            )
        } else if let Some(c) = code {
            (
                "lumberroom: signed in. You can close this window and return to the terminal.",
                Callback::Code(c),
            )
        } else {
            (
                "lumberroom: no authorization code received. You can close this window.",
                Callback::Failed("callback carried no authorization code".to_string()),
            )
        };

        respond(&mut socket, 200, &format!("<html><body>{page}</body></html>")).await;
        match outcome {
            Callback::Code(c) => return Ok(c),
            Callback::Failed(message) => return Err(err(message)),
        }
    }
}

/// The terminal line for an error redirect. Anyone who can make the browser load a loopback URL
/// chooses these bytes, so control characters become spaces before an escape sequence can reach
/// the terminal.
fn callback_error(error: &str, description: Option<&str>) -> String {
    let clean =
        |s: &str| -> String { s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect() };
    match description {
        Some(d) => format!("authorization server returned error={}: {}", clean(error), clean(d)),
        None => format!("authorization server returned error={}", clean(error)),
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

pub async fn login(client: &Client, args: &crate::args::Args) -> Result<()> {
    // Before the saved client id and secret are read out of it. A file at 0644 is reported, never
    // used and never repaired.
    crate::config::refuse_loose_permissions(&client.file.borrow().path)
        .map_err(|e| err(e.to_string()))?;
    let state = hex(&random_bytes(16)?);
    let verifier = base64url(&random_bytes(32)?);
    let challenge = pkce_challenge(&verifier);

    let saved_client_id = client.file.borrow().oauth("client_id").map(str::to_string);
    let reregistering = args.present("reregister") || saved_client_id.is_none();
    let port = if reregistering {
        args.int("port", DEFAULT_LOOPBACK_PORT as i64) as u16
    } else {
        let saved_port = client
            .file
            .borrow()
            .oauth("redirect_uri")
            .and_then(|u| reqwest::Url::parse(u).ok())
            .and_then(|u| u.port())
            .unwrap_or(DEFAULT_LOOPBACK_PORT);
        if let Some(asked) = args.value("port").and_then(crate::args::parse_int_prefix) {
            if asked != saved_port as i64 {
                out(&format!(
                    "note: ignoring --port; reusing port {saved_port} already registered for client {}.",
                    saved_client_id.clone().unwrap_or_default()
                ));
                out("Pass --reregister to register a fresh client on a different port.");
            }
        }
        saved_port
    };
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        err(format!(
            "cannot bind the OAuth loopback listener on 127.0.0.1:{port} ({e}). Free that port, or \
run with --reregister --port <n> to register a fresh client on a different one."
        ))
    })?;

    let mut client_id = saved_client_id.clone().unwrap_or_default();
    let mut client_secret = client.file.borrow().oauth("client_secret").map(str::to_string);

    // Resolved once for the whole login. Every endpoint below comes out of it.
    let endpoints = client.oauth_endpoints().await?;

    if reregistering {
        let (status, body) = client
            .http_request_url(
                reqwest::Method::POST,
                &endpoints.register,
                Some(json!({
                    "client_name": "lumberroom",
                    "redirect_uris": [redirect_uri],
                    "grant_types": ["authorization_code", "refresh_token"],
                    "token_endpoint_auth_method": "none",
                    "software_id": "lumberroom",
                    "software_version": env!("CARGO_PKG_VERSION"),
                })),
            )
            .await?;
        if status == 404 {
            return Err(err(format!(
                "nothing answers registration at {}: the server is not running in oauth or oidc \
mode",
                endpoints.register
            )));
        }
        if status >= 300 {
            return Err(err(format!("client registration failed ({status}): {body}")));
        }
        let reg: crate::wire::RegistrationResponse =
            serde_json::from_value(body.clone()).map_err(|e| {
                err(format!("registration response is not the expected shape ({e}): {body}"))
            })?;
        client_id = reg.client_id;
        client_secret = reg.client_secret;
        out(&format!("registered client {client_id} on redirect {redirect_uri}"));
    } else {
        out(&format!(
            "reusing client {client_id} on redirect {redirect_uri} (--reregister to start over)"
        ));
    }

    let mut authorize = reqwest::Url::parse(&endpoints.authorize)
        .map_err(|e| err(format!("cannot build the authorize URL: {e}")))?;
    {
        let mut q = authorize.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &client_id);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("code_challenge", &challenge);
        // RFC 7636's default when this is omitted is `plain`, so it is always sent.
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("scope", args.value("scope").unwrap_or("memory.read memory.write"));
        // RFC 8707. The resource identifier is the MCP endpoint, matching the server's
        // AuthConfig.resource_url.
        q.append_pair("resource", &client.cfg.mcp_url);
        q.append_pair("state", &state);
    }
    let authorize = authorize.to_string();

    out("opening your browser to sign in. If nothing opens, visit:");
    out(&format!("  {authorize}"));
    open_browser(&authorize);

    let code = wait_for_callback(listener, "/callback", &state).await?;

    let mut form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code),
        ("redirect_uri".to_string(), redirect_uri.clone()),
        ("client_id".to_string(), client_id.clone()),
        ("code_verifier".to_string(), verifier),
        ("resource".to_string(), client.cfg.mcp_url.clone()),
    ];
    if let Some(secret) = client_secret.clone() {
        form.push(("client_secret".to_string(), secret));
    }

    let res = client.send(reqwest::Method::POST, &endpoints.token, Payload::Form(form)).await?;
    let status = res.status().as_u16();
    let body: Value = res.json().await.unwrap_or_else(|_| json!({}));
    let token: crate::wire::TokenResponse = match serde_json::from_value(body.clone()) {
        Ok(t) if status < 300 => t,
        _ => return Err(err(format!("token exchange failed ({status}): {body}"))),
    };

    let ttl = token.expires_in.unwrap_or(3600);
    let expires_at = expires_at(ttl);
    let mut oauth = Map::new();
    oauth.insert("client_id".into(), json!(client_id));
    oauth.insert("client_secret".into(), json!(client_secret));
    // Persisted so the next login rebinds this exact port. The redirect URI has to match byte for
    // byte on every future /authorize call.
    oauth.insert("redirect_uri".into(), json!(redirect_uri));
    oauth.insert("access_token".into(), json!(token.access_token));
    oauth.insert("refresh_token".into(), json!(token.refresh_token));
    oauth.insert("token_type".into(), json!(token.token_type.unwrap_or_else(|| "Bearer".into())));
    oauth.insert("expires_at".into(), json!(expires_at));
    // Beside the instant, the life it was measured from. The client refreshes in the last quarter
    // of a token's life, and a login that recorded only the end leaves it guessing that life until
    // the first refresh writes one.
    oauth.insert("expires_in".into(), json!(ttl));

    let mut patch = Map::new();
    patch.insert("url".into(), json!(client.cfg.base_url));
    patch.insert("oauth".into(), Value::Object(oauth));
    let had_static_token = client.file.borrow().str_field("token").is_some();
    client
        .file
        .borrow_mut()
        .save(patch)
        .map_err(|e| err(format!("cannot write the config file: {e}")))?;

    if had_static_token {
        out("note: config.json still has a static \"token\" too; that one wins until you remove it.");
    }
    out(&format!("signed in. Access token expires {expires_at}."));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc_7636_appendix_b_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_generated_verifier_is_url_safe_and_within_the_rfc_length_bounds() {
        let v = base64url(&random_bytes(32).unwrap());
        assert_eq!(v.len(), 43);
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        let c = pkce_challenge(&v);
        assert_eq!(c.len(), 43);
        assert!(!c.contains('='));
    }

    #[test]
    fn state_is_thirty_two_hex_characters() {
        let s = hex(&random_bytes(16).unwrap());
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn expires_at_is_an_iso_instant_node_can_parse() {
        let s = expires_at(3600);
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(s.len(), 24, "{s}");
        let parsed = chrono::DateTime::parse_from_rfc3339(&s).unwrap();
        let delta = parsed.timestamp() - Utc::now().timestamp();
        assert!((3595..=3605).contains(&delta), "{delta}");
    }

    fn hosted_document() -> Value {
        json!({
            "issuer": "https://lumberroom.cloud",
            "authorization_endpoint": "https://lumberroom.cloud/oauth/authorize",
            "token_endpoint": "https://lumberroom.cloud/oauth/token",
            "registration_endpoint": "https://lumberroom.cloud/oauth/register",
            "revocation_endpoint": "https://lumberroom.cloud/oauth/revoke",
        })
    }

    #[test]
    fn only_the_two_hosted_hosts_take_the_discovery_path() {
        assert!(is_hosted("https://lumberroom.cloud"));
        assert!(is_hosted("https://mcp.lumberroom.cloud"));
        assert!(is_hosted("https://MCP.Lumberroom.Cloud"));

        assert!(!is_hosted("https://evil-lumberroom.cloud"));
        assert!(!is_hosted("https://lumberroom.cloud.attacker.tld"));
        assert!(!is_hosted("https://memory.example"));
        assert!(!is_hosted("http://127.0.0.1:8787"));
        assert!(!is_hosted("not a url"));
    }

    #[test]
    fn the_hosted_issuer_wins_over_the_configured_base() {
        let e = Endpoints::from_metadata(
            &hosted_document(),
            "https://mcp.lumberroom.cloud/.well-known/oauth-authorization-server",
        )
        .unwrap();
        assert_eq!(e.authorize, "https://lumberroom.cloud/oauth/authorize");
        assert_eq!(e.token, "https://lumberroom.cloud/oauth/token");
        assert_eq!(e.register, "https://lumberroom.cloud/oauth/register");
        assert_eq!(e.revoke.as_deref(), Some("https://lumberroom.cloud/oauth/revoke"));
    }

    #[test]
    fn a_self_hosted_deployment_gets_the_urls_it_always_got() {
        let e = Endpoints::base_relative("https://memory.example");
        assert_eq!(e.authorize, "https://memory.example/oauth/authorize");
        assert_eq!(e.token, "https://memory.example/oauth/token");
        assert_eq!(e.register, "https://memory.example/oauth/register");
        assert_eq!(e.revoke.as_deref(), Some("https://memory.example/oauth/revoke"));
    }

    #[test]
    fn a_document_missing_an_endpoint_fails_and_names_where_it_came_from() {
        let mut doc = hosted_document();
        doc.as_object_mut().unwrap().remove("token_endpoint");
        let e = Endpoints::from_metadata(&doc, "https://mcp.lumberroom.cloud/.well-known/x")
            .unwrap_err();
        assert!(e.message.contains("token_endpoint"), "{}", e.message);
        assert!(e.message.contains("https://mcp.lumberroom.cloud/.well-known/x"), "{}", e.message);
    }

    #[test]
    fn an_endpoint_this_client_does_not_trust_fails_rather_than_being_used() {
        for advertised in [
            json!("http://evil.example/oauth/token"),
            json!("/oauth/token"),
            json!("ftp://memory.example/oauth/token"),
            json!("javascript:alert(1)"),
            json!(""),
            json!(42),
            json!(null),
        ] {
            let mut doc = hosted_document();
            doc.as_object_mut().unwrap().insert("token_endpoint".into(), advertised.clone());
            let e = Endpoints::from_metadata(&doc, "https://mcp.lumberroom.cloud/.well-known/x");
            assert!(e.is_err(), "{advertised} was accepted");
        }
    }

    #[test]
    fn a_revocation_endpoint_nobody_can_trust_is_dropped_rather_than_failing_the_login() {
        let mut doc = hosted_document();
        doc.as_object_mut()
            .unwrap()
            .insert("revocation_endpoint".into(), json!("http://evil.example/revoke"));
        let e =
            Endpoints::from_metadata(&doc, "https://mcp.lumberroom.cloud/.well-known/x").unwrap();
        assert_eq!(e.revoke, None);
    }

    #[test]
    fn an_endpoint_on_a_host_this_client_does_not_know_is_refused() {
        // The finding this pins: "any https URL" was the old rule, and a document naming
        // https://attacker.example would have passed it. This client posts the code and the
        // verifier to whatever it accepts here, so the accepting is the whole control.
        assert!(!is_trustworthy_endpoint("https://attacker.example/oauth/token"));
        assert!(!is_trustworthy_endpoint("https://evil-lumberroom.cloud/oauth/token"));
        assert!(!is_trustworthy_endpoint("https://lumberroom.cloud.attacker.tld/oauth/token"));
        assert!(!is_trustworthy_endpoint("https://memory.example/oauth/token"));

        assert!(is_trustworthy_endpoint("https://lumberroom.cloud/oauth/token"));
        assert!(is_trustworthy_endpoint("https://mcp.lumberroom.cloud/oauth/token"));
        assert!(is_trustworthy_endpoint("https://LUMBERROOM.CLOUD/oauth/token"));
    }

    #[test]
    fn plain_http_is_refused_everywhere_including_loopback() {
        // Only a hosted document reaches this check, and the hosted deployment serves no loopback.
        assert!(!is_trustworthy_endpoint("http://lumberroom.cloud/oauth/token"));
        assert!(!is_trustworthy_endpoint("http://127.0.0.1:8080/oauth/token"));
        assert!(!is_trustworthy_endpoint("http://localhost:8080/oauth/token"));
    }

    #[test]
    fn a_credential_travels_over_https_or_loopback_and_nowhere_else() {
        // The base URL is whatever the operator configured and nothing checked its scheme, so a
        // base of http://a-remote-host used to put a bearer token in the clear on every request.
        assert!(may_carry_credential("https://memory.example/admin/whoami"));
        assert!(may_carry_credential("https://lumberroom.cloud/oauth/token"));

        // The default base, and the whole self-hosted development story.
        assert!(may_carry_credential("http://127.0.0.1:8787/admin/whoami"));
        assert!(may_carry_credential("http://localhost:8787/admin/whoami"));
        assert!(may_carry_credential("http://[::1]:8787/admin/whoami"));

        assert!(!may_carry_credential("http://memory.example/admin/whoami"));
        assert!(!may_carry_credential("http://192.168.1.10:8787/admin/whoami"));
        // Not loopback, whatever it looks like.
        assert!(!may_carry_credential("http://localhost.evil.example/admin/whoami"));
        assert!(!may_carry_credential("ftp://memory.example/"));
        assert!(!may_carry_credential("not a url"));
    }

    #[test]
    fn only_a_checked_url_can_be_handed_to_a_credential_carrying_send() {
        assert_eq!(
            CredentialUrl::checked("https://lumberroom.cloud/oauth/token")
                .map(|u| u.as_str().to_string()),
            Some("https://lumberroom.cloud/oauth/token".to_string())
        );
        assert!(CredentialUrl::checked("http://127.0.0.1:8787/oauth/token").is_some());
        assert!(CredentialUrl::checked("http://memory.example/oauth/token").is_none());
        assert!(CredentialUrl::checked("not a url").is_none());
    }

    #[test]
    fn a_hosted_host_over_plain_http_takes_the_self_hosted_path() {
        // Fetching this document in the clear lets anyone on the path name the endpoints. A base
        // that cannot protect the fetch does not get to use it.
        assert!(!is_hosted("http://lumberroom.cloud"));
        assert!(!is_hosted("http://mcp.lumberroom.cloud:8787"));
        assert!(is_hosted("https://lumberroom.cloud"));
    }

    /// A real listener, a real browser-shaped preconnect, a real callback.
    #[tokio::test]
    async fn the_listener_survives_a_connection_that_sends_nothing() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle =
            tokio::spawn(async move { wait_for_callback(listener, "/callback", "st8").await });

        // A speculative connection that closes without writing, then a favicon, then the callback.
        let dead = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        drop(dead);
        let _ = reqwest::get(format!("http://127.0.0.1:{port}/favicon.ico")).await;
        let page = reqwest::get(format!("http://127.0.0.1:{port}/callback?code=abc123&state=st8"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(page.contains("signed in"));
        assert_eq!(handle.await.unwrap().unwrap(), "abc123");
    }

    #[tokio::test]
    async fn a_state_mismatch_is_refused() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle =
            tokio::spawn(async move { wait_for_callback(listener, "/callback", "expected").await });
        let _ =
            reqwest::get(format!("http://127.0.0.1:{port}/callback?code=abc&state=forged")).await;
        let e = handle.await.unwrap().unwrap_err();
        assert!(e.message.contains("state parameter mismatch"), "{}", e.message);
    }

    #[tokio::test]
    async fn an_error_redirect_is_reported_rather_than_waited_out() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle =
            tokio::spawn(async move { wait_for_callback(listener, "/callback", "st8").await });
        let _ =
            reqwest::get(format!("http://127.0.0.1:{port}/callback?error=access_denied&state=st8"))
                .await;
        let e = handle.await.unwrap().unwrap_err();
        assert!(e.message.contains("error=access_denied"), "{}", e.message);
    }

    /// Drive one callback carrying `query` through a real listener and return the failure text.
    async fn callback_failure(query: &str) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle =
            tokio::spawn(async move { wait_for_callback(listener, "/callback", "st8").await });
        let _ = reqwest::get(format!("http://127.0.0.1:{port}/callback?{query}")).await;
        handle.await.unwrap().unwrap_err().message
    }

    #[tokio::test]
    async fn an_error_description_prints_beside_the_error() {
        let message = callback_failure(
            "error=access_denied&error_description=this+account+is+not+enrolled&state=st8",
        )
        .await;
        assert_eq!(
            message,
            "authorization server returned error=access_denied: this account is not enrolled"
        );
    }

    #[tokio::test]
    async fn a_description_carrying_terminal_escapes_prints_without_them() {
        // ESC [ 2 J clears the screen; BEL and a newline would forge a second line of output.
        let message =
            callback_failure("error=access_denied&error_description=a%1B%5B2Jb%07c%0Ad&state=st8")
                .await;
        assert!(!message.chars().any(char::is_control), "{message:?}");
        assert_eq!(message, "authorization server returned error=access_denied: a [2Jb c d");
    }

    #[tokio::test]
    async fn no_description_prints_the_error_alone() {
        let message = callback_failure("error=access_denied&state=st8").await;
        assert_eq!(message, "authorization server returned error=access_denied");
    }
}
