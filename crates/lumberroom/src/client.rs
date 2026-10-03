//! HTTP and MCP transport.
//!
//! Two surfaces, one credential. The admin routes are plain JSON over `http_base`, and the tools
//! are JSON-RPC over Streamable HTTP at `/mcp`. Both go through `send`, so the automatic refresh
//! on 401 covers every call rather than the ones somebody remembered to wrap.

use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::config::{FileConfig, Resolved};
use crate::oauth::{Endpoints, METADATA_PATH};

/// A failure with the exit code the owner and the acceptance scripts read.
///
/// 1 general, 2 auth or no credential, 3 timeout. These are `bin/lumberroom.mjs`'s codes and they are
/// part of the contract: `scripts/policy-test.sh` distinguishes a refused call from a broken one.
#[derive(Debug)]
pub struct CliError {
    pub message: String,
    pub code: i32,
}

pub type Result<T> = std::result::Result<T, CliError>;

pub fn err<S: Into<String>>(message: S) -> CliError {
    CliError { message: message.into(), code: 1 }
}

pub fn err_code<S: Into<String>>(message: S, code: i32) -> CliError {
    CliError { message: message.into(), code }
}

/// What a request carries, if anything. The token endpoint takes form encoding and nothing else:
/// a stack wired only for JSON returns 415 there while `/oauth/register` keeps working, which
/// reads as almost-working.
pub enum Payload {
    None,
    Json(Value),
    Form(Vec<(String, String)>),
}

pub struct ToolOutput {
    pub structured: Value,
    pub text: String,
}

/// How much life left on the token found on disk counts as "somebody else already refreshed this".
///
/// Wide enough to cover the retry the caller is about to send and the clock skew between two
/// processes on one machine, short enough to stay well inside any access token lifetime this
/// client has been handed.
const FRESH_ENOUGH_SECS: i64 = 60;

/// The share of an access token's life during which a request may refresh before it goes out.
const PROACTIVE_FRACTION: f64 = 0.25;

/// The lifetime assumed for a token whose response carried no `expires_in`, and for a config file
/// written before this client started recording one.
const DEFAULT_TOKEN_LIFETIME_SECS: i64 = 3600;

/// How early this process starts refreshing, as a share of `PROACTIVE_FRACTION`. Uniform over
/// [0.5, 1.0], drawn once per client.
///
/// Without the draw, every lumberroom process on the machine crosses the trigger point in the same
/// second, because they all hold a token that expires at the same instant. That is the herd this
/// whole change exists to break up, and a fixed threshold moves it earlier rather than spreading
/// it. Entropy that fails falls back to the middle of the range, so the worst case is the fixed
/// threshold nobody is worse off for.
fn random_lead() -> f64 {
    match crate::oauth::random_bytes(2) {
        Ok(bytes) => {
            let n = u16::from_le_bytes([bytes[0], bytes[1]]);
            0.5 + 0.5 * (f64::from(n) / f64::from(u16::MAX))
        }
        Err(_) => 0.75,
    }
}

/// Seconds of life left on the access token this config file holds, when the file says.
///
/// `expires_at` has been written since the first release and read only by `doctor`, so nothing has
/// ever depended on its shape being right. Both callers treat `None` as "no answer" rather than as
/// "expired": a file written by hand, or by a client that spelled the instant differently, must not
/// be able to turn a working credential into a refresh on every request or into a skipped one.
fn seconds_until_expiry(file: &FileConfig) -> Option<i64> {
    let at = chrono::DateTime::parse_from_rfc3339(file.oauth("expires_at")?).ok()?;
    Some(at.timestamp() - chrono::Utc::now().timestamp())
}

/// The access token's whole life in seconds, as the token endpoint reported it.
///
/// The proactive refresh needs the life and not just the end of it: a quarter of an hour of margin
/// on a token that lives an hour is prudence, and on one that lives five minutes it is a refresh
/// before every request. `expires_in` lands beside `expires_at` from this file's own refresh, and a
/// config last written by `login` on an older build does not carry it.
fn access_token_lifetime(file: &FileConfig) -> i64 {
    file.value
        .get("oauth")
        .and_then(|oauth| oauth.get("expires_in"))
        .and_then(Value::as_i64)
        .filter(|ttl| *ttl > 0)
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS)
}

pub struct Client {
    http: reqwest::Client,
    pub cfg: Resolved,
    pub file: RefCell<FileConfig>,
    token: RefCell<String>,
    /// This process's share of the proactive refresh window, drawn once. See `random_lead`.
    refresh_lead: Cell<f64>,
    request_id: Cell<i64>,
    endpoints: RefCell<Option<Rc<Endpoints>>>,
}

impl Client {
    pub fn new(cfg: Resolved, file: FileConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(cfg.timeout_ms))
            // Redirects are off: a 302 on an authenticated admin route would replay the bearer
            // token at whatever host the response named.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| err(format!("cannot build the HTTP client: {e}")))?;
        let token = RefCell::new(cfg.token.clone());
        Ok(Self {
            http,
            cfg,
            file: RefCell::new(file),
            token,
            refresh_lead: Cell::new(random_lead()),
            request_id: Cell::new(0),
            endpoints: RefCell::new(None),
        })
    }

    /// Where the OAuth flow goes, resolved once per run and reused.
    ///
    /// Against the hosted deployment this reads the RFC 8414 document, because its issuer and its
    /// API base are different hosts. Against anything else it builds the endpoints off the base
    /// exactly as this client always has, and makes no request at all: the deployments that were
    /// never broken get no new latency and no new way to fail.
    ///
    /// The discovery request carries no credential and skips `send`. Discovery is public, and a 401
    /// there must not start the refresh this call may itself be serving.
    pub async fn oauth_endpoints(&self) -> Result<Rc<Endpoints>> {
        let cached = self.endpoints.borrow().clone();
        if let Some(endpoints) = cached {
            return Ok(endpoints);
        }

        let endpoints = if crate::oauth::is_hosted(&self.cfg.http_base) {
            let url = format!("{}{}", self.cfg.http_base, METADATA_PATH);
            let res =
                self.http.get(&url).header("accept", "application/json").send().await.map_err(
                    |e| {
                        err(format!("cannot reach the authorization server metadata at {url}: {e}"))
                    },
                )?;
            let status = res.status().as_u16();
            if status >= 300 {
                return Err(err(format!(
                    "the authorization server metadata at {url} answered {status}"
                )));
            }
            let document = res.json::<Value>().await.map_err(|e| {
                err(format!("the authorization server metadata at {url} is not JSON: {e}"))
            })?;
            Endpoints::from_metadata(&document, &url)?
        } else {
            Endpoints::base_relative(&self.cfg.http_base)
        };

        let endpoints = Rc::new(endpoints);
        *self.endpoints.borrow_mut() = Some(endpoints.clone());
        Ok(endpoints)
    }

    pub fn token(&self) -> String {
        self.token.borrow().clone()
    }

    pub fn has_token(&self) -> bool {
        !self.token.borrow().is_empty()
    }

    fn net_err(&self, e: reqwest::Error) -> CliError {
        if e.is_timeout() {
            err_code(
                format!(
                    "timed out after {}ms talking to {}",
                    self.cfg.timeout_ms, self.cfg.mcp_url
                ),
                3,
            )
        } else {
            err(e.to_string())
        }
    }

    fn build(
        &self,
        method: reqwest::Method,
        url: &str,
        payload: &Payload,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, url)
            .header("accept", "application/json, text/event-stream")
            // How the instrumentation tells "the hook asked" apart from "the model chose to".
            .header("x-memory-invocation", self.cfg.invocation.as_str());
        // The credential goes only where it cannot be read off the wire. Every URL this client
        // reaches now comes from `oauth_endpoints`, and one of its two paths builds from an
        // operator-supplied base whose scheme nothing checked, so a base of `http://a-remote-host`
        // used to put a bearer token in the clear on every request. Dropping the header rather than
        // failing keeps the refusal at the server, which answers 401 and says so.
        let token = self.token.borrow().clone();
        if !token.is_empty() && crate::oauth::may_carry_credential(url) {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        match payload {
            Payload::None => req.header("content-type", "application/json"),
            Payload::Json(v) => req.header("content-type", "application/json").body(v.to_string()),
            Payload::Form(pairs) => req.form(pairs),
        }
    }

    /// One request, one automatic refresh on 401, one retry. Never a loop.
    ///
    /// The request is rebuilt for the retry rather than replayed, so the second attempt carries the
    /// refreshed token instead of the stale header that caused the 401.
    pub async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        payload: Payload,
    ) -> Result<reqwest::Response> {
        // Refresh before the request when the token is near the end of its life, rather than after
        // the 401 it is about to earn. Waiting for the 401 costs every process a guaranteed failed
        // round trip, and they all pay it in the same second: they hold tokens that expire
        // together, so they queue on the token endpoint together. A proactive refresh that fails
        // has already said why on stderr, and the request goes out on the old token regardless, so
        // the 401 handler below stays the fallback it has always been.
        if self.refresh_is_due() {
            self.refresh().await;
        }
        let res =
            self.build(method.clone(), url, &payload).send().await.map_err(|e| self.net_err(e))?;
        if res.status().as_u16() != 401 {
            return Ok(res);
        }
        if self.file.borrow().oauth("refresh_token").is_none() {
            return Ok(res);
        }
        if !self.refresh().await {
            return Ok(res);
        }
        self.build(method, url, &payload).send().await.map_err(|e| self.net_err(e))
    }

    /// Whether the next request should refresh first instead of waiting for its 401.
    ///
    /// True once the token is inside this process's slice of the last `PROACTIVE_FRACTION` of its
    /// life. A file with no readable `expires_at` answers false and leaves the credential to the
    /// 401 path: an unreadable instant is not a reason to refresh before every request forever.
    fn refresh_is_due(&self) -> bool {
        let file = self.file.borrow();
        if file.oauth("refresh_token").is_none() {
            return false;
        }
        let Some(left) = seconds_until_expiry(&file) else {
            return false;
        };
        let window = access_token_lifetime(&file) as f64 * PROACTIVE_FRACTION;
        left <= (window * self.refresh_lead.get()) as i64
    }

    /// The access token the config file holds, when it is worth adopting instead of rotating.
    ///
    /// Two conditions, and the first is what makes this safe on the 401 path. A caller reaches
    /// `refresh` because the server refused the token this client is holding, so a file claiming
    /// the credential is still good is only believable when the file holds a *different* token
    /// from the refused one. The same token with a future `expires_at` means the server and the
    /// clock disagree, and the server wins.
    ///
    /// The second is that the token has real life left. An absent or unparseable `expires_at`
    /// falls through to the refresh: a config written by an older client is not evidence.
    fn fresh_token_on_disk(&self) -> Option<String> {
        let file = self.file.borrow();
        let access = file.oauth("access_token")?;
        if access == self.token.borrow().as_str() {
            return None;
        }
        if seconds_until_expiry(&file)? <= FRESH_ENOUGH_SECS {
            return None;
        }
        Some(access.to_string())
    }

    /// Exchange the refresh token, persist the result, adopt the new access token.
    ///
    /// A refresh that fails returns false and the caller's own 401 handling reports it. Retrying a
    /// bad refresh token is how a revoked credential turns into a hang instead of an error.
    ///
    /// The config lock is held across the whole exchange, from the read of the refresh token to
    /// the save of its replacement. A lock around the save alone cannot prevent the lockout: two
    /// processes both read the same token, both send it, and the server, which rotates on first
    /// presentation and revokes the family on the second, kills the credential for good. Under
    /// the lock the second process re-reads the file before it sends anything, so it presents the
    /// rotated token instead of the spent one. The wait for the lock is this client's request
    /// timeout plus margin, so a holder stuck on a slow token endpoint cannot block every other
    /// lumberroom process on the machine indefinitely; a waiter that outlasts it gets an error
    /// naming the lock file. That wait yields rather than sleeping the thread, because two
    /// refreshes in one process contend for the same flock and a blocking wait deadlocks them.
    pub async fn refresh(&self) -> bool {
        let path = self.file.borrow().path.clone();
        // The refresh token is read out of this file and sent. A file every local account can
        // read is reported here, and the 401 the caller is handling stands.
        if let Err(e) = crate::config::refuse_loose_permissions(&path) {
            eprintln!("{e}");
            return false;
        }
        let lock_wait = std::time::Duration::from_millis(self.cfg.timeout_ms)
            + std::time::Duration::from_secs(5);
        // The async wait matters as much as the lock. flock conflicts between two open file
        // descriptions in one process too, and `eval` runs four writes through one client on one
        // current-thread runtime, so a waiter that sleeps the thread parks the holder it is
        // waiting for.
        let lock = match crate::config::FileConfig::lock_config_async(&path, lock_wait).await {
            Ok(lock) => lock,
            Err(e) => {
                eprintln!("cannot lock the config file for the refresh: {e}");
                return false;
            }
        };
        // The in-memory copy of the file predates every other process's refresh since this one
        // started. The token to send is whatever is on disk now, read under the lock so nothing
        // rotates it in between.
        *self.file.borrow_mut() = FileConfig::load(path);

        // Somebody else may have run this exchange while this process waited for the lock, and
        // their result is on disk: a different access token with life ahead of it. Sending anyway
        // spends a rotation for nothing, and every rotation is another window in which a crash
        // between the POST and the save leaves a spent refresh token on disk and the whole family
        // dead on the next run. Adopt the token instead and let the caller retry with it.
        if let Some(fresh) = self.fresh_token_on_disk() {
            *self.token.borrow_mut() = fresh;
            drop(lock);
            return true;
        }

        let (refresh_token, client_id, client_secret, existing) = {
            let file = self.file.borrow();
            let Some(rt) = file.oauth("refresh_token") else {
                eprintln!(
                    "cannot refresh: the config file has no refresh_token. Run `lumberroom \
                     login` to sign in again."
                );
                return false;
            };
            let Some(cid) = file.oauth("client_id") else {
                eprintln!(
                    "cannot refresh: the config file has no client_id, which `lumberroom login` \
                     writes when it registers the client. Sign in again."
                );
                return false;
            };
            let secret = file.oauth("client_secret").map(str::to_string);
            let existing = file.value.get("oauth").cloned().unwrap_or_else(|| json!({}));
            (rt.to_string(), cid.to_string(), secret, existing)
        };

        let mut form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.clone()),
            ("client_id".to_string(), client_id),
        ];
        if let Some(secret) = client_secret {
            form.push(("client_secret".to_string(), secret));
        }

        let url = match self.oauth_endpoints().await {
            Ok(endpoints) => endpoints.token.clone(),
            Err(e) => {
                eprintln!("{}", e.message);
                return false;
            }
        };
        // The refresh token is the longest-lived credential this client holds, so the same rule
        // applies and this one refuses outright rather than continuing without it. The check hands
        // back the value the post takes, so the guarantee survives whatever the next edit does to
        // the order of these lines: an unchecked URL is not a thing this call can be given.
        let Some(endpoint) = crate::oauth::CredentialUrl::checked(&url) else {
            // The endpoint is deliberately not printed. It carries no credential, but it is derived
            // from the same value the token travels to, and a refusal message is not worth teaching
            // the next reader that anything off that path is safe to log. The operator configured
            // the URL and can read it back from their own config.
            eprintln!(
                "refusing to send a refresh token over plain http to a host that is not loopback: \
it would go on the wire in the clear. Point the CLI at https, or at 127.0.0.1."
            );
            return false;
        };
        let res = match self.http.post(endpoint.as_str()).form(&form).send().await {
            Ok(res) => res,
            Err(e) => {
                eprintln!("cannot reach the token endpoint: {e}");
                return false;
            }
        };
        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            eprintln!(
                "the token endpoint refused the refresh ({status}): {}",
                truncate(&body, 300)
            );
            if body.contains("invalid_grant") {
                eprintln!(
                    "the server refused the refresh token itself (spent, expired, or revoked \
                     when an earlier run failed mid-rotation). Run `lumberroom login` to sign \
                     in again."
                );
            }
            return false;
        }
        let body = match res.json::<Value>().await {
            Ok(body) => body,
            Err(e) => {
                eprintln!("the token endpoint's answer is not JSON: {e}");
                return false;
            }
        };
        let Some(access) = body.get("access_token").and_then(Value::as_str) else {
            eprintln!("the token endpoint's answer has no access_token in it");
            return false;
        };

        let mut oauth = existing.as_object().cloned().unwrap_or_default();
        oauth.insert("access_token".into(), json!(access));
        oauth.insert(
            "refresh_token".into(),
            body.get("refresh_token").cloned().unwrap_or_else(|| json!(refresh_token)),
        );
        oauth.insert(
            "token_type".into(),
            body.get("token_type").cloned().unwrap_or_else(|| json!("Bearer")),
        );
        let ttl =
            body.get("expires_in").and_then(Value::as_i64).unwrap_or(DEFAULT_TOKEN_LIFETIME_SECS);
        oauth.insert("expires_at".into(), json!(crate::oauth::expires_at(ttl)));
        // The end of the token's life is on its own not enough to decide when to refresh early.
        // `access_token_lifetime` reads this back.
        oauth.insert("expires_in".into(), json!(ttl));

        let mut patch = Map::new();
        patch.insert("oauth".into(), Value::Object(oauth));
        if let Err(e) = self.file.borrow_mut().save_locked(&lock, patch) {
            // The rotation already happened server-side, so the token still on disk is spent.
            // Saying so here is the difference between a diagnosable failure and the next
            // run's invalid_grant arriving with no explanation.
            eprintln!(
                "cannot write the refreshed credential to the config file: {e}\nThe new tokens \
                 were not saved and the refresh token on disk has already been rotated \
                 server-side, so the next refresh will be refused. Run `lumberroom login`."
            );
            return false;
        }
        *self.token.borrow_mut() = access.to_string();
        drop(lock);
        true
    }

    /// An admin or health route. Returns the status beside the body, because every caller here
    /// branches on the status and node's `httpRequest` does the same.
    pub async fn http_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let url = format!("{}{}", self.cfg.http_base, path);
        self.http_request_url(method, &url, body).await
    }

    /// The same call against an absolute URL, for the endpoints discovery names. Those can sit on
    /// a different host from `http_base`, so a path is not enough to address them.
    pub async fn http_request_url(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let payload = match body {
            Some(v) => Payload::Json(v),
            None => Payload::None,
        };
        let res = self.send(method, url, payload).await?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| self.net_err(e))?;
        let json = serde_json::from_str::<Value>(&text)
            .unwrap_or_else(|_| json!({ "raw": truncate(&text, 300) }));
        Ok((status, json))
    }

    pub async fn http_get(&self, path: &str) -> Result<(u16, Value)> {
        self.http_request(reqwest::Method::GET, path, None).await
    }

    /// A route whose response is a file rather than JSON. Same auth, same refresh, raw bytes.
    ///
    /// `http_request` reads the body with `res.text()`, which substitutes U+FFFD for every byte
    /// sequence that is not valid UTF-8. An archive is gzip inside age, so that path hands back a
    /// file that is corrupt and the corruption surfaces at restore time. The caller decodes the
    /// body itself when the status says the bytes are an error message.
    pub async fn http_send_bytes(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Vec<u8>)> {
        let url = format!("{}{}", self.cfg.http_base, path);
        let payload = match body {
            Some(v) => Payload::Json(v),
            None => Payload::None,
        };
        let res = self.send(method, &url, payload).await?;
        let status = res.status().as_u16();
        let bytes = res.bytes().await.map_err(|e| self.net_err(e))?;
        Ok((status, bytes.to_vec()))
    }

    /// One JSON-RPC call over the MCP transport.
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.request_id.set(self.request_id.get() + 1);
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.request_id.get(),
            "method": method,
            "params": params,
        });
        let res = self.send(reqwest::Method::POST, &self.cfg.mcp_url, Payload::Json(body)).await?;
        let status = res.status().as_u16();
        if status == 401 || status == 403 {
            let detail = res.text().await.unwrap_or_default();
            return Err(err_code(
                format!("auth rejected ({status}). Check the token. {}", truncate(&detail, 200)),
                2,
            ));
        }
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = res.text().await.map_err(|e| self.net_err(e))?;
        let body = read_body(&content_type, &text, status)?;
        if let Some(e) = body.get("error").filter(|v| !v.is_null()) {
            let message = e
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string());
            return Err(err(format!("{method}: {message}")));
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Streamable HTTP dropped sessions in revision 2026-07-28, which is what makes a bare
    /// initialize-then-call pair valid with no session id to carry. Sending it before every call
    /// keeps this client restart-proof.
    pub async fn initialize(&self) -> Result<()> {
        self.rpc(
            "initialize",
            json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": { "name": format!("lumberroom-{}", self.cfg.invocation), "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await
        .map(|_| ())
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<ToolOutput> {
        self.initialize().await?;
        let result = self.rpc("tools/call", json!({ "name": name, "arguments": args })).await?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            return Err(err(if text.is_empty() { "tool error".to_string() } else { text }));
        }
        Ok(ToolOutput {
            structured: result.get("structuredContent").cloned().unwrap_or(Value::Null),
            text,
        })
    }
}

/// Streamable HTTP answers with either JSON or a single SSE frame. Accept both, and take the last
/// data line: a server that streams progress notifications puts the result at the end.
pub fn read_body(content_type: &str, text: &str, status: u16) -> Result<Value> {
    if content_type.contains("text/event-stream") {
        let last = text
            .split('\n')
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .next_back();
        let Some(last) = last else {
            return Err(err(format!("empty SSE response: {}", truncate(text, 200))));
        };
        return serde_json::from_str(last)
            .map_err(|e| err(format!("unparseable SSE frame: {e}: {}", truncate(last, 200))));
    }
    serde_json::from_str(text)
        .map_err(|_| err(format!("unexpected response ({status}): {}", truncate(text, 300))))
}

/// Character-safe truncation. Byte slicing a UTF-8 error body panics on a multi-byte boundary, and
/// an error path that panics hides the error it was reporting.
pub fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_yield_the_last_payload() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n\n";
        let v = read_body("text/event-stream", body, 200).unwrap();
        assert_eq!(v["result"]["a"], json!(1));
    }

    #[test]
    fn plain_json_parses_too() {
        let v = read_body("application/json", "{\"result\":{\"b\":2}}", 200).unwrap();
        assert_eq!(v["result"]["b"], json!(2));
    }

    #[test]
    fn a_non_json_body_reports_the_status_and_the_text() {
        let e = read_body("text/html", "<html>502 upstream</html>", 502).unwrap_err();
        assert!(e.message.contains("unexpected response (502)"), "{}", e.message);
        assert!(e.message.contains("502 upstream"));
    }

    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        assert_eq!(truncate("héllo", 3), "hél");
    }

    /// The double-refresh race, against a token endpoint that rotates and refuses replays the
    /// way the server does: the first presentation of a refresh token gets a new pair, the
    /// second presentation of a spent one gets invalid_grant and the family is dead.
    mod refresh_race {
        use super::*;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        struct TokenServer {
            current: String,
            issues: u64,
            /// Every refresh_token value ever presented, spent or not.
            presentations: Vec<String>,
            /// The bearer token on every request that was not a token exchange.
            api_calls: Vec<String>,
        }

        fn token_server() -> Arc<Mutex<TokenServer>> {
            Arc::new(Mutex::new(TokenServer {
                current: "r0".into(),
                issues: 0,
                presentations: Vec::new(),
                api_calls: Vec::new(),
            }))
        }

        /// Answer up to `n` requests and stop. Returned rather than awaited by the tests that
        /// prove a request was never sent: those leave the accept loop parked, so the test aborts
        /// the task instead of joining it.
        fn serve(
            listener: TcpListener,
            state: Arc<Mutex<TokenServer>>,
            n: usize,
        ) -> tokio::task::JoinHandle<()> {
            tokio::spawn(async move {
                for _ in 0..n {
                    let Ok((mut socket, _)) = listener.accept().await else { break };
                    serve_one(&mut socket, &state).await;
                }
            })
        }

        /// One request in, one response out. Just enough HTTP for a form-encoded token request;
        /// the crate has no server framework by design, and the login tests in `oauth.rs` use the
        /// same hand-rolled shape.
        async fn serve_one(socket: &mut tokio::net::TcpStream, state: &Mutex<TokenServer>) {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(at) = find_header_end(&buf) {
                    break at;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let length = headers
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buf.len() < header_end + 4 + length {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let body =
                String::from_utf8_lossy(&buf[header_end + 4..header_end + 4 + length]).to_string();

            // Anything that is not the token endpoint is an ordinary API call, and what the test
            // wants from it is which access token it carried.
            let path = headers.lines().next().unwrap_or_default().split(' ').nth(1).unwrap_or("");
            if !path.contains("/oauth/token") {
                let bearer = headers
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim().trim_start_matches("Bearer ").to_string())
                    .unwrap_or_default();
                state.lock().unwrap().api_calls.push(bearer);
                let answer = json!({ "ok": true }).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                return;
            }

            let presented = body
                .split('&')
                .find_map(|pair| pair.strip_prefix("refresh_token="))
                .unwrap_or_default()
                .to_string();

            // The lock is scoped so no await below holds the guard: a MutexGuard across an
            // await makes the whole server future !Send and tokio::spawn refuses it.
            let (status, answer) = {
                let mut st = state.lock().unwrap();
                st.presentations.push(presented.clone());
                if presented == st.current {
                    st.issues += 1;
                    let issued = st.issues;
                    st.current = format!("r{issued}");
                    (
                        200,
                        json!({
                            "access_token": format!("a{issued}"),
                            "refresh_token": format!("r{issued}"),
                            "token_type": "Bearer",
                            "expires_in": 3600,
                        })
                        .to_string(),
                    )
                } else {
                    (
                        400,
                        json!({ "error": "invalid_grant", "error_description": "already used" })
                            .to_string(),
                    )
                }
            };
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: \
                 {}\r\nconnection: close\r\n\r\n{answer}",
                answer.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }

        fn find_header_end(buf: &[u8]) -> Option<usize> {
            buf.windows(4).position(|w| w == b"\r\n\r\n")
        }

        fn fixture_config() -> std::path::PathBuf {
            use crate::config::restrict;
            let dir = std::env::temp_dir().join(format!(
                "lumberroom-refresh-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.json");
            std::fs::write(&path, r#"{"oauth":{"refresh_token":"r0","client_id":"cid"}}"#).unwrap();
            restrict(&path).unwrap();
            path
        }

        /// The same fixture with the oauth object spelled out, for the tests that turn on what
        /// `expires_at` says.
        fn fixture_config_with(oauth: Value) -> std::path::PathBuf {
            let path = fixture_config();
            std::fs::write(&path, json!({ "oauth": oauth }).to_string()).unwrap();
            path
        }

        /// An instant `secs` from now, in the format the config file uses.
        fn in_seconds(secs: i64) -> String {
            crate::oauth::expires_at(secs)
        }

        fn client_on(path: &std::path::Path, port: u16) -> Client {
            let env: HashMap<String, String> =
                HashMap::from([("LUMBERROOM_URL".to_string(), format!("http://127.0.0.1:{port}"))]);
            let file = FileConfig::load(path.to_path_buf());
            let resolved = crate::config::resolve(&env, &file, None, None, None, false, None);
            Client::new(resolved, file).unwrap()
        }

        /// One CLI process: its own runtime, its own client, its own in-memory view of the
        /// config file. A thread with a runtime of its own is the closest a test gets to a
        /// second `lumberroom` invocation on one machine. The barrier stands between loading
        /// the file and refreshing, so both processes hold the same stale view when they start,
        /// which is the state two invocations that began earlier are in.
        fn refresh_as_a_process(
            path: std::path::PathBuf,
            port: u16,
            barrier: &std::sync::Barrier,
        ) -> (bool, String) {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let adopted = rt.block_on(async move {
                let env: HashMap<String, String> = HashMap::from([(
                    "LUMBERROOM_URL".to_string(),
                    format!("http://127.0.0.1:{port}"),
                )]);
                let file = FileConfig::load(path);
                let resolved = crate::config::resolve(&env, &file, None, None, None, false, None);
                let client = Client::new(resolved, file).unwrap();
                barrier.wait();
                let refreshed = client.refresh().await;
                (refreshed, client.token())
            });
            drop(rt);
            adopted
        }

        /// One process refreshing twice at once, which is what `eval` does: four writes share a
        /// client on one current-thread runtime and two of them can take a 401 in the same wave.
        /// flock attaches to the open file description, so the second refresh's try-lock fails
        /// against the first one in its own process. A wait that sleeps the thread then holds the
        /// only runtime thread the holder's request needs, and the holder wakes to its own
        /// timeout on a token the server has already rotated. The spent token stays on disk and
        /// the next run gets invalid_grant, which is the lockout this whole file exists to stop,
        /// arriving by timeout instead of replay.
        #[tokio::test]
        async fn two_refreshes_in_one_process_do_not_starve_each_other() {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = token_server();
            let server = serve(listener, state.clone(), 2);

            let path = fixture_config();
            // A one second request timeout makes the starvation quick rather than subtle. The
            // lock wait is that timeout plus five seconds, so a waiter that sleeps the thread
            // outlasts the holder's request deadline by five, and the holder fails.
            let env: HashMap<String, String> = HashMap::from([
                ("LUMBERROOM_URL".to_string(), format!("http://127.0.0.1:{port}")),
                ("LUMBERROOM_TIMEOUT_MS".to_string(), "1000".to_string()),
            ]);
            let file = FileConfig::load(path.clone());
            let resolved = crate::config::resolve(&env, &file, None, None, None, false, None);
            let client = Client::new(resolved, file).unwrap();

            let (first, second) = tokio::join!(client.refresh(), client.refresh());
            assert!(
                first && second,
                "both refreshes on one runtime completed (first {first}, second {second})"
            );
            server.await.unwrap();

            let st = state.lock().unwrap();
            assert_eq!(
                st.presentations,
                vec!["r0".to_string(), "r1".to_string()],
                "the second refresh waited for the first and sent the rotated token"
            );
            assert_eq!(client.token(), "a2");
            drop(st);
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }

        /// Two processes reaching the token endpoint at once, which is what a machine running a
        /// hook and an editor and a shell does. The first rotates. The second waits for the lock,
        /// re-reads, finds an access token that is not the one it holds and has an hour of life
        /// ahead of it, and adopts it without sending anything.
        ///
        /// Before the double check this test asserted the other outcome: two presentations, two
        /// rotations, each process ending on its own access token. Nothing was wrong with that,
        /// it just spent a rotation for a token the machine already had, and every rotation is a
        /// window in which a crash between the POST and the save leaves a spent refresh token on
        /// disk. The guarantee the name carries holds either way and is asserted below.
        #[tokio::test]
        async fn the_second_process_adopts_the_first_refresh_rather_than_spending_another() {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = token_server();
            let server = serve(listener, state.clone(), 2);

            let path = fixture_config();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let first_path = path.clone();
            let first_barrier = barrier.clone();
            let first =
                std::thread::spawn(move || refresh_as_a_process(first_path, port, &first_barrier));
            let second_path = path.clone();
            let second =
                std::thread::spawn(move || refresh_as_a_process(second_path, port, &barrier));

            // join on the blocking pool so the server on this runtime keeps answering while
            // both processes run.
            let ((a, token_a), (b, token_b)) = tokio::task::spawn_blocking(move || {
                (first.join().unwrap(), second.join().unwrap())
            })
            .await
            .unwrap();
            assert!(a, "one of the two refreshes failed");
            assert!(b, "one of the two refreshes failed");
            server.abort();

            let st = state.lock().unwrap();
            let mut seen = std::collections::BTreeSet::new();
            for token in &st.presentations {
                assert!(seen.insert(token.clone()), "refresh token {token:?} was presented twice");
            }
            assert_eq!(
                st.presentations,
                vec!["r0".to_string()],
                "the waiter found a fresh token and sent nothing"
            );
            assert_eq!(st.current, "r1", "one rotation happened server-side, not two");

            let back: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(back["oauth"]["refresh_token"], json!("r1"));
            assert_eq!(back["oauth"]["access_token"], json!("a1"));
            assert_eq!(
                [token_a, token_b],
                ["a1".to_string(), "a1".to_string()],
                "both processes ended on the one access token the machine holds"
            );
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }

        /// The double check reads `expires_at`, which no code path has ever depended on. A file
        /// that does not carry one, or carries something no parser accepts, has to mean "refresh"
        /// rather than "skip" or "fail": the alternative is a client that stops refreshing because
        /// somebody hand-edited a date.
        #[tokio::test]
        async fn an_unreadable_expires_at_still_refreshes() {
            for stored in [
                json!({ "refresh_token": "r0", "client_id": "cid", "access_token": "a9" }),
                json!({
                    "refresh_token": "r0",
                    "client_id": "cid",
                    "access_token": "a9",
                    "expires_at": "the day after tomorrow",
                }),
                json!({
                    "refresh_token": "r0",
                    "client_id": "cid",
                    "access_token": "a9",
                    "expires_at": "",
                }),
            ] {
                let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                let port = listener.local_addr().unwrap().port();
                let state = token_server();
                let server = serve(listener, state.clone(), 1);

                // The client starts on "a0" and another process replaces the file underneath it,
                // which is the state the double check has to judge.
                let path = fixture_config_with(
                    json!({ "refresh_token": "r0", "client_id": "cid", "access_token": "a0" }),
                );
                let client = client_on(&path, port);
                std::fs::write(&path, json!({ "oauth": stored }).to_string()).unwrap();

                assert!(client.refresh().await, "the refresh failed for {stored}");
                server.abort();
                assert_eq!(
                    state.lock().unwrap().presentations,
                    vec!["r0".to_string()],
                    "a token endpoint request was expected for {stored}"
                );
                assert_eq!(client.token(), "a1", "the rotation happened for {stored}");
                std::fs::remove_dir_all(path.parent().unwrap()).ok();
            }
        }

        /// The other half of the same judgement: a readable instant far enough out means the
        /// refresh has already happened and this one is waste.
        #[tokio::test]
        async fn a_readable_future_expires_at_skips_the_exchange() {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = token_server();
            let server = serve(listener, state.clone(), 1);

            let path = fixture_config_with(
                json!({ "refresh_token": "r0", "client_id": "cid", "access_token": "a0" }),
            );
            let client = client_on(&path, port);
            std::fs::write(
                &path,
                json!({ "oauth": {
                    "refresh_token": "r1",
                    "client_id": "cid",
                    "access_token": "a1",
                    "expires_at": in_seconds(3600),
                    "expires_in": 3600,
                } })
                .to_string(),
            )
            .unwrap();

            assert!(client.refresh().await);
            server.abort();
            assert!(
                state.lock().unwrap().presentations.is_empty(),
                "nothing should have reached the token endpoint"
            );
            assert_eq!(client.token(), "a1", "the fresh token on disk was adopted");
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }

        /// A token inside its last quarter gets replaced before the request goes out, so the
        /// request carries the new one and nobody spends a round trip on a 401 everybody else is
        /// spending in the same second.
        #[tokio::test]
        async fn a_request_refreshes_before_the_token_expires() {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = token_server();
            let server = serve(listener, state.clone(), 2);

            // Sixty seconds left on an hour-long token. The jitter puts the trigger somewhere
            // between seven and fifteen minutes out, so every draw fires here.
            let path = fixture_config_with(json!({
                "refresh_token": "r0",
                "client_id": "cid",
                "access_token": "a0",
                "expires_at": in_seconds(60),
                "expires_in": 3600,
            }));
            let client = client_on(&path, port);
            let url = format!("http://127.0.0.1:{port}/api/health");
            let res = client.send(reqwest::Method::GET, &url, Payload::None).await.unwrap();
            assert_eq!(res.status().as_u16(), 200);
            server.abort();

            let st = state.lock().unwrap();
            assert_eq!(st.presentations, vec!["r0".to_string()], "the refresh went out first");
            assert_eq!(
                st.api_calls,
                vec!["a1".to_string()],
                "the request carried the refreshed token, and there was no 401 round trip"
            );
            drop(st);
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }

        /// The mirror: a token with most of its life left is left alone. A proactive refresh that
        /// fires too early is the same herd arriving earlier.
        #[tokio::test]
        async fn a_request_leaves_a_token_with_life_left_alone() {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = token_server();
            let server = serve(listener, state.clone(), 2);

            // Fifty minutes left on an hour-long token, past the widest trigger the jitter draws.
            let path = fixture_config_with(json!({
                "refresh_token": "r0",
                "client_id": "cid",
                "access_token": "a0",
                "expires_at": in_seconds(3000),
                "expires_in": 3600,
            }));
            let client = client_on(&path, port);
            let url = format!("http://127.0.0.1:{port}/api/health");
            let res = client.send(reqwest::Method::GET, &url, Payload::None).await.unwrap();
            assert_eq!(res.status().as_u16(), 200);
            server.abort();

            let st = state.lock().unwrap();
            assert!(st.presentations.is_empty(), "no refresh was due");
            assert_eq!(st.api_calls, vec!["a0".to_string()]);
            drop(st);
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }

        /// A short-lived token gets a proportionally short trigger. With a fixed window, a token
        /// that lives five minutes would be refreshed before every request it ever carries.
        #[test]
        fn the_trigger_follows_the_token_lifetime() {
            let path = fixture_config_with(json!({
                "refresh_token": "r0",
                "client_id": "cid",
                "access_token": "a0",
                "expires_at": in_seconds(200),
                "expires_in": 300,
            }));
            let client = client_on(&path, 1);
            assert!(
                !client.refresh_is_due(),
                "200 seconds left on a 300 second token is outside the last quarter"
            );
            std::fs::write(
                &path,
                json!({ "oauth": {
                    "refresh_token": "r0",
                    "client_id": "cid",
                    "access_token": "a0",
                    "expires_at": in_seconds(20),
                    "expires_in": 300,
                } })
                .to_string(),
            )
            .unwrap();
            *client.file.borrow_mut() = FileConfig::load(path.clone());
            assert!(client.refresh_is_due(), "20 seconds left on a 300 second token is due");
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }

        /// The jitter is what keeps two processes that started together from crossing the trigger
        /// in the same second. A draw that always returned the same number would read as working
        /// in every test above and would rebuild the herd on the machine.
        #[test]
        fn the_refresh_lead_is_drawn_per_client() {
            let draws: Vec<f64> = (0..64).map(|_| random_lead()).collect();
            for lead in &draws {
                assert!((0.5..=1.0).contains(lead), "{lead} is outside the window");
            }
            let first = draws[0];
            assert!(
                draws.iter().any(|lead| (lead - first).abs() > f64::EPSILON),
                "64 draws were all identical, so the trigger point is fixed"
            );
        }
    }
}
