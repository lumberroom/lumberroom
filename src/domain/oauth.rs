//! The pure half of the built-in OAuth 2.1 authorization server: wire types, PKCE, credential
//! generation, and redirect-URI validation.
//!
//! Decision 0002 chose a built-in authorization server over an external issuer, so every rule an
//! external issuer would have enforced now lives here. Phase 2 spec §2 lists the ways a real MCP
//! client fails against a nearly-correct server, and most of those failures are silent: the client
//! shows a generic error, or no error, and the flow simply never completes. The rules that belong
//! in pure code rather than in a handler are here so they can be tested as rules.
//!
//! No I/O. Nothing here reads a database, a socket or a clock.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::domain::errors::{DomainError, Result};
use crate::domain::policy::NamespaceGrant;
use crate::domain::types::Sensitivity;

/// Scopes carry no policy weight here. Authorization is the `GrantProfile` the owner picks at the
/// consent screen, which is a server-side record a client cannot influence. A scope string exists
/// only because clients display one and RFC 6749 requires it in a token response when it differs
/// from the request.
const DEFAULT_SCOPE: &str = "lumberroom:memory";

// ---- RFC 7591 dynamic client registration ----

/// What a client posts to `/register`, as JSON.
///
/// Phase 2 spec §2 prefers manually issued credentials over dynamic registration, because both
/// Claude and ChatGPT mint a fresh client on every connection and the registrations accumulate.
/// The endpoint exists anyway: refusing it means those surfaces cannot connect at all.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistrationRequest {
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub grant_types: Option<Vec<String>>,
    pub response_types: Option<Vec<String>>,
    pub token_endpoint_auth_method: Option<String>,
    pub scope: Option<String>,
    pub software_id: Option<String>,
    pub software_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistrationResponse {
    pub client_id: String,
    pub client_id_issued_at: i64,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth_method: String,
}

// ---- /authorize ----

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeRequest {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: Option<String>,
    pub state: Option<String>,
    pub scope: Option<String>,
    /// RFC 8707. The audience the resulting token is bound to.
    pub resource: Option<String>,
}

/// Validated form of the above. Constructing one is the only way to proceed.
#[derive(Debug, Clone)]
pub struct AuthorizeIntent {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub state: Option<String>,
    pub scope: String,
    pub resource: Option<String>,
}

impl AuthorizeRequest {
    /// Checks response_type, that the challenge method is S256, and that redirect_uri is one of
    /// the client's registered URIs by EXACT string match.
    ///
    /// redirect_uri goes first on purpose. Every other error could in principle be reported by
    /// redirecting to the client with `error=`, and doing that before the URI is known to be
    /// registered turns this endpoint into an open redirector. Because a caller cannot tell the
    /// failures apart from a `DomainError`, the contract is that any error out of here is rendered
    /// as a page and never redirected.
    pub fn validate(self, registered: &[String]) -> Result<AuthorizeIntent> {
        // Exact string equality, never a prefix or an origin comparison. A registered
        // `https://host/cb` must not admit `https://host/cb/evil` or `https://host/cb?x=1`, and
        // origin matching would admit both.
        if !registered.iter().any(|uri| uri == &self.redirect_uri) {
            return Err(DomainError::validation(
                "redirect_uri does not exactly match a redirect URI registered for this client",
            ));
        }

        if self.response_type != "code" {
            return Err(DomainError::validation(format!(
                "unsupported response_type {:?}. This server issues authorization codes only.",
                self.response_type
            )));
        }

        // RFC 7636 §4.3: when code_challenge_method is omitted the default is `plain`, not S256.
        // Assuming S256 for a client that meant `plain` would verify a challenge that equals the
        // verifier, which is PKCE switched off while looking switched on. So absent means refused.
        // The value is case-sensitive, so `s256` is refused too rather than repaired.
        match self.code_challenge_method.as_deref() {
            Some("S256") => {}
            Some(other) => {
                return Err(DomainError::validation(format!(
                    "unsupported code_challenge_method {other:?}. Use S256."
                )))
            }
            None => {
                return Err(DomainError::validation(
                    "code_challenge_method is required and must be S256. \
                     Omitting it means 'plain' under RFC 7636, which this server refuses.",
                ))
            }
        }

        // An S256 challenge is 32 bytes of base64url without padding, so its length is fixed. The
        // check catches a client that sends a raw verifier while claiming S256.
        if self.code_challenge.len() != 43 || !is_base64url(&self.code_challenge) {
            return Err(DomainError::validation(
                "code_challenge is not a base64url-encoded SHA-256 digest",
            ));
        }

        // RFC 8707 §2: the resource indicator is an absolute URI and carries no fragment. A token
        // bound to a resource the client did not spell out exactly is a token that can be replayed
        // against the wrong audience.
        if let Some(resource) = &self.resource {
            if resource.contains('#') || Url::parse(resource).is_err() {
                return Err(DomainError::validation(
                    "resource must be an absolute URI without a fragment",
                ));
            }
        }

        let scope = match self.scope.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => DEFAULT_SCOPE.to_string(),
        };

        Ok(AuthorizeIntent {
            client_id: self.client_id,
            redirect_uri: self.redirect_uri,
            code_challenge: self.code_challenge,
            state: self.state,
            scope,
            resource: self.resource,
        })
    }
}

// ---- RFC 8707 resource indicators ----

/// The longest resource indicator this server will parse. See [`canonical_resource`].
const MAX_RESOURCE_LEN: usize = 2048;

/// The canonical form of a resource indicator, for comparing two of them.
///
/// String equality is what this server used to do with a resource, and it is wrong in four ways a
/// real client hits. `HTTPS://Host/mcp` and `https://host/mcp` are one URI. So are
/// `https://host:443/mcp` and `https://host/mcp`. So are `https://host/mcp` and `https://host/mcp/`
/// once you accept that a resource indicator names a deployment rather than a document. Every one
/// of those reads as a different audience under `==`, and the rejection it produces tells the
/// operator nothing about which of the two strings to change.
///
/// `Url::parse` does the first three: it lowercases the scheme and the host, drops a default port,
/// and resolves dot segments. It does NOT normalise percent-encoding, so `%2f` and `%2F` stay
/// different and `%63` never becomes `c`. That half of RFC 3986 §6.2.2 is missing here, and it is
/// missing in the closed direction: the two spellings read as two audiences and the token is
/// refused. The trailing slash
/// is this function's own rule and it is a deliberate departure: RFC 3986 does not make `/mcp` and
/// `/mcp/` equivalent, and for a document they are not. For an audience the difference identifies
/// nothing, and refusing on it is an outage nobody can read off the error.
///
/// `None` means "not an absolute URI", or "carries a fragment", which RFC 8707 §2 forbids. Callers
/// compare two `Some` values, so an unparseable resource equals nothing at all and the failure is
/// closed rather than open.
pub fn canonical_resource(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains('#') {
        return None;
    }
    // A ceiling, because this parses a value a client chose and a token then carries for its whole
    // life: the authenticator canonicalises the stored resource on every request that token makes.
    // Measured on url 2.5.8, a 1MB resource parses in about 55ms and 2MB in about 109ms, and the
    // request body limit admits both. No real resource indicator is anywhere near 2048 bytes.
    if raw.len() > MAX_RESOURCE_LEN {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    // `urn:` and `mailto:` are absolute URIs with no path to normalise, and `set_path` on one
    // panics rather than failing. Their serialisation is already canonical enough.
    if url.cannot_be_a_base() {
        return Some(url.into());
    }
    let mut url = url;
    let trimmed = url.path().trim_end_matches('/').to_string();
    url.set_path(&trimmed);
    Some(url.into())
}

/// Whether a token bound to `bound` may be spent at a server that serves `served`.
///
/// Both sides go through [`canonical_resource`], so a value neither side can parse matches nothing.
pub fn resource_matches(bound: &str, served: &str) -> bool {
    match (canonical_resource(bound), canonical_resource(served)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

// ---- /token ----

/// What arrives at `/token`. Phase 2 spec §2: this endpoint must accept form encoding while
/// `/register` takes JSON. A stack wired for JSON only returns 415 here while registration
/// succeeds, which reads as almost-working.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<String>,
    pub resource: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub scope: String,
}

/// The OAuth error shape. Clients read 'error' and nothing else, so it has to be one of the
/// registered codes rather than free text.
///
/// The registered codes for these endpoints are `invalid_request`, `invalid_client`,
/// `invalid_grant`, `unauthorized_client`, `unsupported_grant_type`, `invalid_scope` (RFC 6749
/// §5.2), `access_denied`, `unsupported_response_type`, `server_error`,
/// `temporarily_unavailable` (§4.1.2.1), and `invalid_target` (RFC 8707 §2.2). Anything else is
/// invisible to a client.
#[derive(Debug, Clone, Serialize)]
pub struct OauthError {
    pub error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

impl OauthError {
    pub fn new(error: &'static str, description: impl Into<String>) -> Self {
        Self { error, error_description: Some(description.into()) }
    }

    /// 400 for most, 401 for invalid_client.
    ///
    /// This type covers protocol failures the caller could fix, so it never produces a 5xx. A
    /// fault of ours is a `DomainError::internal` and does not travel as an OAuth error body.
    pub fn http_status(&self) -> u16 {
        match self.error {
            "invalid_client" => 401,
            _ => 400,
        }
    }
}

// ---- the grant the owner assigns at the consent screen ----

/// What the owner picks once, per client, at the consent screen.
///
/// The profile is the boundary, not the scope string the client asked for. Phase 2 spec §3 is
/// blunt about why: a grant that cannot tell two clients apart is decoration, and the only signal
/// that is actually a boundary is the credential the server issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantProfile {
    Full,
    Standard,
    Narrow,
}

impl GrantProfile {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "standard" => Some(Self::Standard),
            "narrow" => Some(Self::Narrow),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Standard => "standard",
            Self::Narrow => "narrow",
        }
    }

    /// One line the owner reads on the consent screen before choosing.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Full => {
                "Every namespace at every level, including sealed items. Can delete memories, \
                 change the registry and post ingested proposals."
            }
            Self::Standard => {
                "Your own notes, shared facts and project notes. Nothing private or sealed, no \
                 deletes, no registry changes."
            }
            Self::Narrow => {
                "Your own notes and shared facts. No project notes, nothing private or sealed, no \
                 deletes, no registry changes."
            }
        }
    }

    pub fn read(self) -> Vec<NamespaceGrant> {
        self.namespaces()
    }

    /// Read and write are the same set at every profile. Phase 2 spec §3 starts grants symmetric
    /// and the asymmetry that matters is the sensitivity ceiling, which both axes already carry.
    pub fn write(self) -> Vec<NamespaceGrant> {
        self.namespaces()
    }

    fn namespaces(self) -> Vec<NamespaceGrant> {
        match self {
            Self::Full => vec![NamespaceGrant::new("*", Sensitivity::Sealed)],
            Self::Standard => vec![
                NamespaceGrant::open("user:me"),
                NamespaceGrant::open("global"),
                NamespaceGrant::open("project:*"),
            ],
            Self::Narrow => {
                vec![NamespaceGrant::open("user:me"), NamespaceGrant::open("global")]
            }
        }
    }

    /// Holding a sealed ceiling and being able to decrypt a sealed item are separate. The flag is
    /// the second one, so a profile cannot reach sealed content by being handed a wider glob.
    pub fn sealed_capable(self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn may_delete(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Posting proposals is an operator action, so only the profile that already carries delete and
    /// registry writes carries it. A hosted client filling the queue is the failure this gate
    /// exists for.
    pub fn may_ingest(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Registry writes are an operator action. A model that can rewrite `services.postgres.endpoint`
    /// can redirect the operator, so this stays off outside the owner's own client.
    pub fn registry_write(self) -> bool {
        matches!(self, Self::Full)
    }
}

// ---- credentials ----

/// CSPRNG bytes, base64url without padding. Used for client ids, codes, access and refresh
/// tokens. Panic-free: an error from the OS RNG is an error, not a weaker token.
pub fn random_token(bytes: usize) -> Result<String> {
    // Bounded because both ends are bugs with no visible symptom. `bytes == 0` returns an empty
    // string that hashes and compares like any other token, and a caller asking for kilobytes has
    // confused bytes with bits.
    if !(16..=64).contains(&bytes) {
        return Err(DomainError::internal(format!(
            "token length {bytes} is outside the supported 16..=64 bytes"
        )));
    }
    let mut buf = vec![0u8; bytes];
    // The error is formatted into the message rather than attached as a source: `getrandom::Error`
    // implements `std::error::Error` only under its `std` feature, and depending on another crate
    // in the tree to switch that on is a build that breaks for an unrelated reason.
    getrandom::fill(&mut buf).map_err(|e| {
        DomainError::internal(format!("could not read randomness from the OS: {e}"))
    })?;
    Ok(URL_SAFE_NO_PAD.encode(&buf))
}

/// SHA-256, lowercase hex. Codes and tokens are stored only as this.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Constant-time comparison of two hashes.
///
/// Unequal lengths compare false without a timing signal worth having: both inputs are
/// fixed-length hex from `hash_token`, so a length difference means a caller passed the wrong
/// kind of string, not an attacker probing.
pub fn hashes_match(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// RFC 7636 S256: BASE64URL(SHA256(ASCII(verifier))) == challenge, compared in constant time.
/// Rejects a verifier outside the 43..=128 character range and a non-S256 method.
pub fn verify_pkce_s256(challenge: &str, verifier: &str) -> bool {
    if !is_valid_verifier(verifier) {
        return false;
    }
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    bool::from(computed.as_bytes().ct_eq(challenge.as_bytes()))
}

/// RFC 7636 §4.1: 43 to 128 characters drawn from the unreserved set. Checking the alphabet first
/// makes the length check byte-safe, since every accepted character is one ASCII byte.
fn is_valid_verifier(verifier: &str) -> bool {
    verifier
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~')
        && (43..=128).contains(&verifier.len())
}

fn is_base64url(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// How many redirect URIs one client may register. The list is walked on every authorize.
pub const MAX_REDIRECT_URIS: usize = 8;
/// Per URI. Browsers cap a URL near this, and every registered URI is stored and compared on each
/// authorize, so a longer one is a storage cost with no client that could use it.
pub const MAX_REDIRECT_URI: usize = 2048;

/// The whole list as one check: at least one, at most `MAX_REDIRECT_URIS`, none longer than
/// `MAX_REDIRECT_URI`, each structurally valid. `/oauth/register` and the console's client form
/// both store what passes here, so the two cannot disagree about what a redirect list may hold.
pub fn validate_redirect_uris(uris: &[String]) -> Result<()> {
    if uris.is_empty() {
        return Err(DomainError::validation("redirect_uris must hold at least one URI"));
    }
    if uris.len() > MAX_REDIRECT_URIS {
        return Err(DomainError::validation(format!("at most {MAX_REDIRECT_URIS} redirect URIs")));
    }
    for uri in uris {
        if uri.len() > MAX_REDIRECT_URI {
            return Err(DomainError::validation(format!(
                "a redirect URI is longer than {MAX_REDIRECT_URI} characters"
            )));
        }
        validate_redirect_uri(uri)
            .map_err(|e| DomainError::validation(format!("{uri}: {}", e.client_message())))?;
    }
    Ok(())
}

/// Structural check at registration time. Must reject: a non-absolute URI, a fragment, plain
/// http to a host that can sit outside the owner's network, and anything that is not http/https
/// or a private-use scheme.
///
/// Plain http passes for loopback, a private or link-local IP literal, and a LAN-only name (see
/// [`is_lan_only`]). RFC 8252 §7.3 allows http for loopback alone, and this departs from it on
/// purpose: a stock OpenWebUI on a home network serves `http://192.168.1.10:3000` and registers
/// that as its callback, and the owner ruled on 29 September 2026 that it must connect. Two things
/// hold the risk down. PKCE with S256 is mandatory here, so a code read off the wire is useless
/// without the verifier, which never leaves the client. And each allowed host resolves or routes
/// only on the network the owner's browser sits on, so a remote attacker cannot stand up a
/// listener that receives the redirect. What stays exposed is someone already on that network:
/// they can read the code in transit, or register a client whose redirect points at their own
/// machine and receive the code outright if the owner presses Allow. The consent page says both.
///
/// This validates, it does not normalise. The stored string is whatever the client registered,
/// because `/authorize` compares byte for byte and a URI that was lowercased or given a
/// trailing slash at registration would stop matching what the client sends.
pub fn validate_redirect_uri(uri: &str) -> Result<()> {
    // Checked on the raw string rather than through the parser. RFC 6749 §3.1.2 forbids a
    // fragment, and an empty one (`...cb#`) parses to `Some("")`, which a parser check that asks
    // whether a fragment exists can read either way.
    if uri.contains('#') {
        return Err(DomainError::validation("redirect_uri must not contain a fragment"));
    }

    let parsed = Url::parse(uri).map_err(|e| {
        DomainError::validation(format!("redirect_uri must be an absolute URI: {e}"))
    })?;

    // Credentials in a redirect URI end up in browser history and in the authorization request
    // that is logged next to it.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(DomainError::validation("redirect_uri must not carry userinfo"));
    }

    match parsed.scheme() {
        "https" => {
            if parsed.host().is_none() {
                return Err(DomainError::validation("https redirect_uri needs a host"));
            }
            Ok(())
        }
        // A local CLI receives its code on a loopback listener, and a LAN service such as
        // OpenWebUI rarely holds a certificate. Any port, because a CLI picks its port at run
        // time. The doc comment above carries why the LAN half is safe enough.
        "http" => {
            if is_loopback(&parsed) || is_lan_only(&parsed) {
                Ok(())
            } else {
                Err(DomainError::validation(
                    "plain http is allowed only for a loopback, private IP or LAN-only \
                     redirect_uri: localhost, 127.0.0.1, [::1], a private or link-local IP address, \
                     a single-label name such as nas, or a name under .local, .lan, .internal or \
                     .home.arpa. Use https.",
                ))
            }
        }
        // RFC 8252 §7.1 private-use scheme: a reverse-DNS name the app owns, so it contains a dot.
        // Requiring the dot is also what keeps `javascript:` and `data:` out.
        scheme if scheme.contains('.') => Ok(()),
        scheme => Err(DomainError::validation(format!(
            "redirect_uri scheme {scheme:?} is not supported. Use https, a loopback or LAN http \
             URI, or a private-use scheme such as com.example.app:/callback."
        ))),
    }
}

/// Names the public DNS does not answer for: `.local` belongs to mDNS (RFC 6762), `.home.arpa` to
/// home networks (RFC 8375), and ICANN reserved `.internal` for private use in 2024. `.lan` has no
/// reservation behind it. Home routers hand it out by default and ICANN has never delegated it; if
/// that ever changes, drop it from this list.
const LAN_SUFFIXES: &[&str] = &[".local", ".lan", ".internal", ".home.arpa"];

/// A host that only the owner's own network can answer for: a private or link-local IP literal,
/// a single-label name, or a name under [`LAN_SUFFIXES`].
///
/// Suffix match on the whole label, so `nas.local.example.com` is public. The parser has already
/// lowercased the host. One trailing dot is tolerated on a suffixed name, since `nas.local.` names
/// the same host. A single label with a trailing dot is refused: `nas.` is an absolute name, a
/// top-level domain the resolver asks the internet about, while a bare `nas` goes through the
/// owner's own search domains.
fn is_lan_only(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => private_v4(ip),
        Some(url::Host::Ipv6(ip)) => private_v6(ip),
        Some(url::Host::Domain(d)) => {
            if !d.contains('.') {
                return !d.is_empty();
            }
            let d = d.strip_suffix('.').unwrap_or(d);
            LAN_SUFFIXES.iter().any(|s| {
                d.strip_suffix(s).is_some_and(|name| !name.is_empty() && !name.ends_with('.'))
            })
        }
        None => false,
    }
}

/// Exact host match, never a suffix or a substring: `localhost.attacker.example` contains
/// "localhost" and is not loopback.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        // Covers the whole of 127.0.0.0/8, which is what a CLI may bind.
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

// ---- who a redirect URI hands the code to ----

/// One MCP client service this server can name on the consent page: the redirect hosts it sends
/// codes to, and the words in a client name that claim it, with the spelling the page prints.
///
/// A new service is one entry. A brand belongs to exactly one service, so the page can tell
/// "Claude" at chatgpt.com apart from "Claude" at claude.ai: both hosts are recognised, and only
/// one of them is Claude's.
struct KnownService {
    /// Printed on the consent page as the owner of these hosts.
    shown: &'static str,
    /// Compared whole against the parsed host: `claude.ai.attacker.example` ends with nothing and
    /// contains nothing that counts.
    hosts: &'static [&'static str],
    /// Checked against [`comparable_client_name`], so case and invisible characters do not hide
    /// them.
    names: &'static [(&'static str, &'static str)],
}

const KNOWN_SERVICES: &[KnownService] = &[
    KnownService {
        shown: "Claude",
        hosts: &["claude.ai", "claude.com"],
        names: &[("claude", "Claude"), ("anthropic", "Anthropic")],
    },
    KnownService {
        shown: "ChatGPT",
        hosts: &["chatgpt.com"],
        names: &[("chatgpt", "ChatGPT"), ("openai", "OpenAI")],
    },
];

fn service_at(host: &str) -> Option<&'static KnownService> {
    KNOWN_SERVICES.iter().find(|service| service.hosts.contains(&host))
}

/// Every known brand a name claims, each with the service that owns it, in table order.
fn claimed_brands(name: &str) -> Vec<(&'static KnownService, &'static str)> {
    let folded = comparable_client_name(name);
    KNOWN_SERVICES
        .iter()
        .flat_map(|service| service.names.iter().map(move |name| (service, name)))
        .filter(|(_, (word, _))| folded.contains(word))
        .map(|(service, (_, shown))| (service, *shown))
        .collect()
}

/// Where an authorization code goes once the owner presses Allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectDestination {
    /// An http or https URI. `host` comes from the parser, so it is lowercase, an international
    /// name arrives as punycode, and an IPv6 address carries its brackets. Punycode is the point:
    /// a Cyrillic look-alike of a known host prints as `xn--...` instead of as the host it mimics.
    ///
    /// `local` is true for loopback only: the code stays on the owner's machine. `recognised` also
    /// covers the known hosts, which are shared by every user of that service. `plain_http` marks a
    /// code that crosses the wire unencrypted; off loopback, registration allows that only on a LAN.
    Host { host: String, recognised: bool, local: bool, plain_http: bool },
    /// A private-use scheme. The code goes to whichever app on the device claimed the scheme, and
    /// any app can claim any scheme, so this server cannot say which one that is.
    App { scheme: String },
    /// Not a URI. Registration refuses these, so reaching one means a row from before that check.
    Unreadable,
}

impl RedirectDestination {
    pub fn recognised(&self) -> bool {
        matches!(self, RedirectDestination::Host { recognised: true, .. })
    }

    /// True only when the code lands on the owner's own machine. A recognised public host is
    /// multi-tenant, so it says which service gets the code and not whose account there.
    pub fn is_local(&self) -> bool {
        matches!(self, RedirectDestination::Host { local: true, .. })
    }

    /// The service that owns a recognised public host, as the consent page prints it. None for
    /// loopback, which belongs to the owner, and for anything unrecognised.
    pub fn service(&self) -> Option<&'static str> {
        match self {
            RedirectDestination::Host { host, recognised: true, local: false, .. } => {
                service_at(host).map(|service| service.shown)
            }
            _ => None,
        }
    }

    pub fn is_plain_http(&self) -> bool {
        matches!(self, RedirectDestination::Host { plain_http: true, .. })
    }
}

/// Read a redirect URI as the place a code will land, and say whether that place belongs to a
/// client this server can name.
///
/// Loopback counts as recognised: a code sent there stays on the owner's machine. A known host
/// counts only over https, since plain http to a public host is a code anyone on the path can read.
/// A plain-http LAN host is never recognised, so the consent page warns about it.
pub fn redirect_destination(uri: &str) -> RedirectDestination {
    let Ok(parsed) = Url::parse(uri) else { return RedirectDestination::Unreadable };
    match parsed.scheme() {
        "https" | "http" => {
            let Some(host) = parsed.host_str() else { return RedirectDestination::Unreadable };
            let local = is_loopback(&parsed);
            let plain_http = parsed.scheme() == "http";
            let recognised = local || (!plain_http && service_at(host).is_some());
            RedirectDestination::Host { host: host.to_string(), recognised, local, plain_http }
        }
        scheme => RedirectDestination::App { scheme: scheme.to_string() },
    }
}

/// A known brand the name claims that the destination does not belong to, when there is one.
///
/// Loopback answers None: the code stays on the owner's machine, and Claude Code and other local
/// clients register under their own names there. Every other destination answers with the first
/// claimed brand its service does not own, so "Claude for ChatGPT" mismatches on both services'
/// hosts. Picking one brand from such a name and checking only that one would let the order of
/// [`KNOWN_SERVICES`] decide whether the owner sees the alarm.
pub fn mismatched_claim(name: &str, destination: &RedirectDestination) -> Option<&'static str> {
    if destination.is_local() {
        return None;
    }
    let owner = destination.service();
    claimed_brands(name)
        .into_iter()
        .find(|(service, _)| owner != Some(service.shown))
        .map(|(_, shown)| shown)
}

/// The known client a name claims to be, when it claims one.
///
/// A substring match on purpose. "Claude Desktop" and "my claude helper" both borrow the name. The
/// consent page asks [`mismatched_claim`] instead, which checks every brand a name claims against
/// the destination; this answers with the first brand in table order and nothing more.
pub fn claimed_known_client(name: &str) -> Option<&'static str> {
    claimed_brands(name).first().map(|(_, shown)| *shown)
}

/// Characters that change nothing a reader sees: zero-width spaces and joiners, the soft hyphen,
/// the byte-order mark, the word joiner and invisible operators, variation selectors, tag
/// characters, the Hangul fillers, and the bidi embedding, override and isolate controls. A
/// right-to-left override is the dangerous one, since it can make a name print as a different name.
/// The fillers and tag characters render blank, so they pad a name or spell text no reader sees.
///
/// The one list. `services::sources` compares and prints writer names through it, so a character
/// added here disappears from the consent page, the clients listing and the digest together.
pub fn invisible_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{E0000}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

/// A client name as registration stores it: invisible characters removed, control characters
/// turned into spaces, whitespace runs collapsed and trimmed. May come back empty; the caller picks
/// the placeholder.
///
/// Cleaned at registration because the name also reaches the log, the clients listing and the
/// digest, and a newline in a log field forges a log line. Cleaned again at render, because a row
/// stored before this list grew still holds whatever it held.
pub fn client_name_display(name: &str) -> String {
    let flat: String = name
        .chars()
        .filter(|c| !invisible_char(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The form a name is compared in: cleaned, NFKC, lowercased.
///
/// Look-alike letters from other scripts, a Cyrillic "а" for a Latin "a", survive this; NFKC does
/// not map across scripts. The consent page does not lean on the name for that reason: the redirect
/// host decides whether it warns, and the name only sharpens the wording.
fn comparable_client_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    client_name_display(name).nfkc().collect::<String>().to_lowercase()
}

/// The longest client name stored, in bytes of UTF-8 after cleaning. Registration and the owner's
/// label share it.
pub const MAX_CLIENT_NAME: usize = 200;

/// True when a name, cleaned and lowercased, ends in a parenthesis that opens with `(added ` or
/// reads `(not approved)`. Those are the words `services::sources` appends to tell duplicates
/// apart, so a name ending in one could pass for another client's disambiguated label.
pub fn claims_a_stamp(name: &str) -> bool {
    let _ = name;
    todo!("E1")
}

/// What the owner typed, as it is stored. Cleaned by `client_name_display`, then, in this order:
/// empty, or equal to the cleaned registered name, answers `Ok(None)`, which clears the label;
/// longer than `MAX_CLIENT_NAME` bytes answers a validation error; a name `claims_a_stamp` accepts
/// answers a validation error; anything else is the label.
pub fn owner_label(typed: &str, registered: &str) -> Result<Option<String>> {
    let _ = (typed, registered);
    todo!("E1")
}

/// Redirect rules for a client that registered itself, on top of [`validate_redirect_uri`].
///
/// Two refusals, both about a destination the consent page cannot describe to the owner:
///
/// - A globally routable IP address in place of a host name. The owner cannot tell whose machine
///   `203.0.113.7` is, and no MCP client this server knows of registers one. Loopback and private
///   ranges pass (RFC 1918, 100.64.0.0/10 as Tailscale uses it, link-local, IPv6 ULA and
///   link-local): an attacker cannot receive a code on the owner's own network, and a self-hosted
///   owner reaches a client there by LAN address. The consent page still prints the address as the
///   host and warns, since an IP is never a recognised client host.
/// - This server's own origin. No client lives at the authorization server, and a code sent back
///   here lands in its access log. The comparison is the whole origin (scheme, host, port with the
///   scheme default filled in), never the host alone: a self-hosted owner runs OpenWebUI on the
///   same LAN address or `.local` name as this server, on another port, and the CLI does the same
///   on loopback. [`origin_key`] folds the aliases a host can hide behind: an IPv4-mapped IPv6
///   host, a trailing dot, and `localhost` against a loopback address. Without that,
///   `[::ffff:192.168.1.10]` would slip past a server at `192.168.1.10`, and `127.0.0.1:8787` past
///   one at `localhost:8787`.
///
/// A private-use scheme passes: it has no host to judge, and the consent page warns about it.
pub fn check_self_registered_redirect(uri: &str, public_url: &str) -> Result<()> {
    let Ok(parsed) = Url::parse(uri) else { return Ok(()) };
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Ok(());
    }

    let public_ip = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => !private_v4(ip),
        Some(url::Host::Ipv6(ip)) => !private_v6(ip),
        _ => false,
    };
    if public_ip && !is_loopback(&parsed) {
        return Err(DomainError::validation(
            "a self-registered client must name its redirect host, not a public IP address. Ask \
             the owner to issue a client for this address.",
        ));
    }

    if let Ok(public) = Url::parse(public_url) {
        let same = origin_key(&parsed).is_some() && origin_key(&parsed) == origin_key(&public);
        if same {
            return Err(DomainError::validation(
                "redirect_uri points at this authorization server, which is not a client",
            ));
        }
    }
    Ok(())
}

/// Scheme, host and port, with the scheme's default port filled in. `None` for a URL with no host
/// or no known port, which matches nothing.
///
/// The host is folded so an alias of this server compares equal to it: an IPv4-mapped IPv6 host
/// becomes IPv4, one trailing dot comes off a domain (`lumberroom.example.` is the same DNS name),
/// and `localhost` with every loopback address becomes one key. The last errs wide: a server on
/// `127.0.0.1:8787` also refuses `127.0.0.2:8787`, which a server bound to all interfaces answers
/// anyway, and no real client registers it.
fn origin_key(url: &Url) -> Option<(String, url::Host<String>, u16)> {
    const LOOPBACK: &str = "localhost";
    let host = match url.host()? {
        url::Host::Ipv6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) if v4.is_loopback() => url::Host::Domain(LOOPBACK.to_string()),
            Some(v4) => url::Host::Ipv4(v4),
            None if ip.is_loopback() => url::Host::Domain(LOOPBACK.to_string()),
            None => url::Host::Ipv6(ip),
        },
        url::Host::Ipv4(ip) if ip.is_loopback() => url::Host::Domain(LOOPBACK.to_string()),
        url::Host::Ipv4(ip) => url::Host::Ipv4(ip),
        url::Host::Domain(d) => {
            let d = d.strip_suffix('.').unwrap_or(d);
            url::Host::Domain(d.to_ascii_lowercase())
        }
    };
    Some((url.scheme().to_string(), host, url.port_or_known_default()?))
}

/// Private IPv4 space: RFC 1918, link-local, and 100.64.0.0/10 (shared address space, which
/// Tailscale uses). `Ipv4Addr::is_shared` is unstable, so the CGNAT range is spelled out.
fn private_v4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_link_local() || (o[0] == 100 && (o[1] & 0xC0) == 64)
}

/// Private IPv6 space: unique local fc00::/7 and link-local fe80::/10, both spelled out because
/// the std predicates for them are unstable. An IPv4-mapped address takes the IPv4 verdict, so
/// `::ffff:203.0.113.7` cannot pass as private.
fn private_v6(ip: std::net::Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return private_v4(v4);
    }
    let first = ip.segments()[0];
    (first & 0xFE00) == 0xFC00 || (first & 0xFFC0) == 0xFE80
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B.
    const RFC_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn authorize(redirect_uri: &str) -> AuthorizeRequest {
        AuthorizeRequest {
            response_type: "code".into(),
            client_id: "abc".into(),
            redirect_uri: redirect_uri.into(),
            code_challenge: RFC_CHALLENGE.into(),
            code_challenge_method: Some("S256".into()),
            state: Some("xyz".into()),
            scope: None,
            resource: None,
        }
    }

    #[test]
    fn the_rfc_7636_appendix_b_vector_verifies() {
        assert!(verify_pkce_s256(RFC_CHALLENGE, RFC_VERIFIER));
    }

    #[test]
    fn a_verifier_that_is_not_the_one_behind_the_challenge_is_refused() {
        let mut wrong = RFC_VERIFIER.to_string();
        wrong.replace_range(0..1, "e");
        assert!(!verify_pkce_s256(RFC_CHALLENGE, &wrong));
    }

    #[test]
    fn a_challenge_that_is_the_verifier_itself_is_refused() {
        // What accepting a `plain` challenge under an S256 label would look like.
        assert!(!verify_pkce_s256(RFC_VERIFIER, RFC_VERIFIER));
    }

    #[test]
    fn a_verifier_at_either_length_bound_verifies() {
        assert!(verify_pkce_s256("ZtNPunH49FD35FWYhT5Tv8I7vRKQJ8uxMaL0_9eHjNA", &"a".repeat(43)));
        assert!(verify_pkce_s256("cK4cUwf1JQ1cueQHQrqWE_zfm42ett05MzBEOy1e_70", &"b".repeat(128)));
    }

    #[test]
    fn a_verifier_shorter_than_43_characters_is_refused_before_it_is_hashed() {
        let short = "a".repeat(42);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(short.as_bytes()));
        assert!(
            !verify_pkce_s256(&challenge, &short),
            "a correct hash of a short verifier still fails"
        );
    }

    #[test]
    fn a_verifier_longer_than_128_characters_is_refused() {
        let long = "a".repeat(129);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(long.as_bytes()));
        assert!(!verify_pkce_s256(&challenge, &long));
    }

    #[test]
    fn a_verifier_outside_the_unreserved_alphabet_is_refused() {
        let bad = format!("{}+/", "a".repeat(41));
        assert_eq!(bad.len(), 43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(bad.as_bytes()));
        assert!(!verify_pkce_s256(&challenge, &bad));
    }

    #[test]
    fn an_empty_verifier_is_refused() {
        assert!(!verify_pkce_s256("", ""));
    }

    #[test]
    fn hash_token_is_lowercase_hex_sha256() {
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hashes_match_only_on_equal_strings() {
        assert!(hashes_match(&hash_token("code"), &hash_token("code")));
        assert!(!hashes_match(&hash_token("code"), &hash_token("code ")));
        assert!(!hashes_match(&hash_token("code"), "short"));
        assert!(!hashes_match("", &hash_token("code")));
    }

    #[test]
    fn a_random_token_is_base64url_without_padding_and_never_repeats() {
        let a = random_token(32).unwrap();
        let b = random_token(32).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43, "32 bytes is 43 unpadded base64url characters");
        assert!(is_base64url(&a), "{a} must be url-safe and unpadded");
        assert!(!a.contains('='));
    }

    #[test]
    fn a_token_length_outside_the_supported_range_is_an_error_not_a_weak_token() {
        assert!(random_token(0).is_err());
        assert!(random_token(8).is_err());
        assert!(random_token(4096).is_err());
    }

    #[test]
    fn a_valid_authorize_request_becomes_an_intent() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let intent = authorize("https://lumberroom.example/cb").validate(&registered).unwrap();
        assert_eq!(intent.client_id, "abc");
        assert_eq!(intent.state.as_deref(), Some("xyz"));
        assert_eq!(intent.scope, DEFAULT_SCOPE, "an absent scope gets the default");
    }

    #[test]
    fn a_requested_scope_and_resource_pass_through() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let mut req = authorize("https://lumberroom.example/cb");
        req.scope = Some("  lumberroom:memory  ".into());
        req.resource = Some("https://lumberroom.example/mcp".into());
        let intent = req.validate(&registered).unwrap();
        assert_eq!(intent.scope, "lumberroom:memory");
        assert_eq!(intent.resource.as_deref(), Some("https://lumberroom.example/mcp"));
    }

    #[test]
    fn a_resource_with_a_fragment_or_no_scheme_is_refused() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let mut req = authorize("https://lumberroom.example/cb");
        req.resource = Some("https://lumberroom.example/mcp#a".into());
        assert!(req.validate(&registered).is_err());

        let mut req = authorize("https://lumberroom.example/cb");
        req.resource = Some("/mcp".into());
        assert!(req.validate(&registered).is_err());
    }

    #[test]
    fn the_scheme_of_a_resource_is_compared_without_regard_to_case() {
        assert!(resource_matches("HTTPS://host.example/mcp", "https://host.example/mcp"));
    }

    #[test]
    fn the_host_of_a_resource_is_compared_without_regard_to_case() {
        assert!(resource_matches("https://HOST.example/mcp", "https://host.example/mcp"));
    }

    #[test]
    fn a_default_port_names_the_same_audience_as_no_port_at_all() {
        assert!(resource_matches("https://host.example:443/mcp", "https://host.example/mcp"));
        assert!(resource_matches("http://host.example:80/mcp", "http://host.example/mcp"));
    }

    #[test]
    fn a_non_default_port_is_a_different_audience() {
        // A second deployment on 8443 behind the same name is the case this protects. Dropping
        // every port because 443 is dropped would hand it the other one's tokens.
        assert!(!resource_matches("https://host.example:8443/mcp", "https://host.example/mcp"));
        assert!(!resource_matches("https://host.example:8443/mcp", "https://host.example:443/mcp"));
    }

    #[test]
    fn a_trailing_slash_does_not_change_the_audience() {
        assert!(resource_matches("https://host.example/mcp/", "https://host.example/mcp"));
        assert!(resource_matches("https://host.example/mcp//", "https://host.example/mcp"));
        // The root case runs the path through `set_path("")`, which is the branch most likely to
        // disagree with itself, so the rule is pinned as a relation and not as a literal string.
        assert!(resource_matches("https://host.example/", "https://host.example"));
    }

    #[test]
    fn the_path_of_a_resource_is_compared_case_sensitively() {
        // RFC 3986 §6.2.2 lowercases the scheme and the host and nothing else. Folding the path
        // too would admit an audience the operator never configured.
        assert!(!resource_matches("https://host.example/MCP", "https://host.example/mcp"));
    }

    #[test]
    fn a_query_string_is_part_of_the_audience() {
        assert!(!resource_matches("https://host.example/mcp?a=1", "https://host.example/mcp"));
        assert!(resource_matches("https://host.example/mcp?a=1", "https://host.example/mcp?a=1"));
    }

    #[test]
    fn a_resource_carrying_a_fragment_is_refused() {
        assert_eq!(canonical_resource("https://host.example/mcp#a"), None);
        assert_eq!(canonical_resource("https://host.example/mcp#"), None);
    }

    #[test]
    fn a_relative_reference_is_not_a_resource_indicator() {
        assert_eq!(canonical_resource("/mcp"), None);
        assert_eq!(canonical_resource("mcp"), None);
        assert_eq!(canonical_resource("host.example/mcp"), None);
    }

    #[test]
    fn an_empty_or_blank_resource_is_refused_and_surrounding_space_is_ignored() {
        assert_eq!(canonical_resource(""), None);
        assert_eq!(canonical_resource("   "), None);
        assert_eq!(canonical_resource("\t\n"), None);
        assert!(resource_matches("  https://host.example/mcp  ", "https://host.example/mcp"));
    }

    #[test]
    fn an_unparseable_resource_matches_nothing_including_an_identical_copy_of_itself() {
        // Two identical garbage strings must not authenticate each other. Raw `==` says yes here,
        // and that open failure is the reason both sides go through the parser first.
        assert!(!resource_matches("not a resource", "not a resource"));
        assert!(!resource_matches("", ""));
        assert!(!resource_matches("https://host.example/mcp#a", "https://host.example/mcp#a"));
        assert!(!resource_matches("not a resource", "https://host.example/mcp"));
        assert!(!resource_matches("https://host.example/mcp", "not a resource"));
    }

    #[test]
    fn a_urn_resource_canonicalises_without_the_path_munging_panicking() {
        // `set_path` panics on a cannot-be-a-base URI, so the guard ahead of it is load-bearing.
        let urn = canonical_resource("urn:example:lumberroom");
        assert_eq!(urn.as_deref(), Some("urn:example:lumberroom"));
        assert!(resource_matches("urn:example:lumberroom", "urn:example:lumberroom"));
        assert!(!resource_matches("urn:example:lumberroom", "https://host.example/mcp"));
    }

    #[test]
    fn a_percent_encoded_path_segment_canonicalises_to_one_stable_value() {
        let once = canonical_resource("https://host.example/m%2Fcp").expect("an absolute URI");
        let again = canonical_resource(&once).expect("a canonical value stays parseable");
        assert_eq!(once, again, "a stored resource has to survive being read back and compared");
        assert!(resource_matches("https://host.example/m%2Fcp", "https://host.example/m%2Fcp"));
        // %2F is an encoded slash inside one segment rather than a separator, so the deployment
        // that serves /m/cp is a different audience.
        assert!(!resource_matches("https://host.example/m%2Fcp", "https://host.example/m/cp"));
    }

    #[test]
    fn only_the_code_response_type_is_accepted() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let mut req = authorize("https://lumberroom.example/cb");
        req.response_type = "token".into();
        assert!(req.validate(&registered).is_err());
    }

    #[test]
    fn an_omitted_code_challenge_method_is_refused_rather_than_read_as_s256() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let mut req = authorize("https://lumberroom.example/cb");
        req.code_challenge_method = None;
        let err = req.validate(&registered).unwrap_err();
        assert!(
            err.client_message().contains("S256"),
            "RFC 7636 defaults an omitted method to plain, so silence is not consent"
        );
    }

    #[test]
    fn the_plain_challenge_method_is_refused() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let mut req = authorize("https://lumberroom.example/cb");
        req.code_challenge_method = Some("plain".into());
        assert!(req.validate(&registered).is_err());
    }

    #[test]
    fn the_challenge_method_is_compared_case_sensitively() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let mut req = authorize("https://lumberroom.example/cb");
        req.code_challenge_method = Some("s256".into());
        assert!(req.validate(&registered).is_err());
    }

    #[test]
    fn a_challenge_that_is_not_a_base64url_digest_is_refused() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        let mut req = authorize("https://lumberroom.example/cb");
        req.code_challenge = "too-short".into();
        assert!(req.validate(&registered).is_err());

        let mut req = authorize("https://lumberroom.example/cb");
        req.code_challenge = format!("{}+/", "a".repeat(41));
        assert!(req.validate(&registered).is_err());
    }

    #[test]
    fn a_trailing_slash_is_a_different_redirect_uri() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        assert!(authorize("https://lumberroom.example/cb/").validate(&registered).is_err());
    }

    #[test]
    fn a_query_string_makes_a_redirect_uri_stop_matching() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        assert!(authorize("https://lumberroom.example/cb?next=x").validate(&registered).is_err());
    }

    #[test]
    fn a_registered_uri_is_never_treated_as_a_prefix() {
        let registered = vec!["https://lumberroom.example/cb".to_string()];
        assert!(authorize("https://lumberroom.example/cb/evil").validate(&registered).is_err());
        assert!(authorize("https://lumberroom.example.evil/cb").validate(&registered).is_err());
    }

    #[test]
    fn a_client_with_no_registered_uris_can_authorize_nothing() {
        assert!(authorize("https://lumberroom.example/cb").validate(&[]).is_err());
    }

    #[test]
    fn one_of_several_registered_uris_is_enough() {
        let registered = vec![
            "https://lumberroom.example/cb".to_string(),
            "http://127.0.0.1:7000/callback".to_string(),
        ];
        assert!(authorize("http://127.0.0.1:7000/callback").validate(&registered).is_ok());
    }

    #[test]
    fn the_redirect_uri_is_checked_before_anything_else() {
        // Otherwise the handler is tempted to report the other failure by redirecting to an
        // address nobody has vouched for.
        let mut req = authorize("https://attacker.example/cb");
        req.response_type = "token".into();
        req.code_challenge_method = None;
        let err = req.validate(&["https://lumberroom.example/cb".to_string()]).unwrap_err();
        assert!(err.client_message().contains("redirect_uri"));
    }

    #[test]
    fn an_https_redirect_uri_is_accepted() {
        assert!(validate_redirect_uri("https://lumberroom.example/oauth/callback").is_ok());
        assert!(validate_redirect_uri("https://lumberroom.example/cb?x=1").is_ok());
    }

    #[test]
    fn a_loopback_redirect_uri_may_use_plain_http_on_any_port() {
        assert!(validate_redirect_uri("http://127.0.0.1:53219/callback").is_ok());
        assert!(validate_redirect_uri("http://127.0.0.1/callback").is_ok());
        assert!(validate_redirect_uri("http://localhost:8080/cb").is_ok());
        assert!(validate_redirect_uri("http://[::1]:8080/cb").is_ok());
    }

    #[test]
    fn plain_http_to_a_public_host_is_refused() {
        for bad in [
            "http://lumberroom.example/cb",
            "http://example.com/cb",
            "http://8.8.8.8/cb",
            "http://203.0.113.7:3000/cb",
            "http://[2001:db8::1]/cb",
            "http://[::ffff:8.8.8.8]/cb",
            // A LAN suffix in the middle of a public name is a public name.
            "http://nas.local.example.com/cb",
            "http://nas.lan.example/cb",
            "http://localhost.attacker.example/cb",
            "http://127.0.0.1.attacker.example/cb",
            // A trailing dot makes a single label an absolute name: a top-level domain, which the
            // victim's resolver looks up on the internet rather than on the local network.
            "http://nas./cb",
            // The edges of the private ranges are public.
            "http://172.32.0.1/cb",
            "http://100.128.0.1/cb",
            "http://[fe00::1]/cb",
        ] {
            assert!(validate_redirect_uri(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plain_http_to_a_private_address_or_a_lan_only_name_is_accepted() {
        for ok in [
            "http://192.168.1.10:3000/cb",
            "http://10.0.0.5/cb",
            "http://172.16.0.1/cb",
            "http://100.64.0.1/cb",
            "http://169.254.10.20/cb",
            "http://[fd00::1]:3000/cb",
            "http://[fe80::1]/cb",
            "http://[::ffff:192.168.1.10]:3000/cb",
            "http://nas.local:3000/cb",
            "http://NAS.Local/cb",
            "http://nas.local./cb",
            "http://nas:3000/cb",
            "http://printer.lan/cb",
            "http://svc.internal/cb",
            "http://box.home.arpa/cb",
        ] {
            assert!(validate_redirect_uri(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn a_lan_suffix_with_no_name_in_front_of_it_is_refused() {
        assert!(validate_redirect_uri("http://home.arpa/cb").is_err());
    }

    #[test]
    fn the_plain_http_refusal_names_what_plain_http_may_reach() {
        let e = validate_redirect_uri("http://example.com/cb").unwrap_err();
        let message = e.client_message();
        for part in ["loopback", "private IP", ".local", ".home.arpa", "https"] {
            assert!(message.contains(part), "{part} missing from {message}");
        }
    }

    #[test]
    fn a_fragment_is_refused_even_when_it_is_empty() {
        assert!(validate_redirect_uri("https://lumberroom.example/cb#frag").is_err());
        assert!(validate_redirect_uri("https://lumberroom.example/cb#").is_err());
    }

    #[test]
    fn a_relative_uri_is_refused() {
        assert!(validate_redirect_uri("/callback").is_err());
        assert!(validate_redirect_uri("lumberroom.example/cb").is_err());
        assert!(validate_redirect_uri("").is_err());
    }

    #[test]
    fn a_private_use_scheme_is_accepted_and_a_bare_word_scheme_is_not() {
        assert!(validate_redirect_uri("com.example.app:/oauth2redirect").is_ok());
        assert!(validate_redirect_uri("com.example.app://callback").is_ok());
        assert!(validate_redirect_uri("javascript:alert(1)").is_err());
        assert!(validate_redirect_uri("data:text/html,x").is_err());
        assert!(validate_redirect_uri("ftp://lumberroom.example/cb").is_err());
    }

    #[test]
    fn userinfo_in_a_redirect_uri_is_refused() {
        assert!(validate_redirect_uri("https://user@lumberroom.example/cb").is_err());
        assert!(validate_redirect_uri("https://user:pw@lumberroom.example/cb").is_err());
    }

    #[test]
    fn the_redirect_list_is_capped_in_count_and_length_wherever_it_is_validated() {
        let ok: Vec<String> = vec!["https://lumberroom.example/cb".into()];
        assert!(validate_redirect_uris(&ok).is_ok());
        assert!(validate_redirect_uris(&[]).is_err(), "an empty list registers nothing");
        let many: Vec<String> =
            (0..=MAX_REDIRECT_URIS).map(|i| format!("https://lumberroom.example/cb{i}")).collect();
        assert!(validate_redirect_uris(&many).is_err(), "one over the count cap");
        let long = vec![format!("https://lumberroom.example/{}", "a".repeat(MAX_REDIRECT_URI))];
        assert!(validate_redirect_uris(&long).is_err(), "one over the length cap");
        let bad = vec!["https://lumberroom.example/cb#frag".to_string()];
        let e = validate_redirect_uris(&bad).unwrap_err();
        assert!(
            e.client_message().contains("#frag"),
            "the refusal names the URI: {}",
            e.client_message()
        );
    }

    #[test]
    fn invalid_client_is_the_only_401() {
        assert_eq!(OauthError::new("invalid_client", "no").http_status(), 401);
        assert_eq!(OauthError::new("invalid_grant", "no").http_status(), 400);
        assert_eq!(OauthError::new("invalid_request", "no").http_status(), 400);
        assert_eq!(OauthError::new("server_error", "no").http_status(), 400);
    }

    #[test]
    fn an_error_without_a_description_serialises_to_the_code_alone() {
        let e = OauthError { error: "invalid_grant", error_description: None };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"error":"invalid_grant"}"#);
    }

    #[test]
    fn the_full_profile_reaches_everything_at_every_level() {
        let p = GrantProfile::Full;
        let expected = vec![NamespaceGrant::new("*", Sensitivity::Sealed)];
        assert_eq!(p.read(), expected);
        assert_eq!(p.write(), expected);
        assert!(p.sealed_capable());
        assert!(p.may_delete());
        assert!(p.may_ingest());
        assert!(p.registry_write());
    }

    #[test]
    fn the_standard_profile_adds_projects_and_stops_at_open() {
        let p = GrantProfile::Standard;
        let expected = vec![
            NamespaceGrant::new("user:me", Sensitivity::Open),
            NamespaceGrant::new("global", Sensitivity::Open),
            NamespaceGrant::new("project:*", Sensitivity::Open),
        ];
        assert_eq!(p.read(), expected);
        assert_eq!(p.write(), expected);
        assert!(!p.sealed_capable());
        assert!(!p.may_delete());
        assert!(!p.registry_write());
    }

    #[test]
    fn the_narrow_profile_sees_only_the_owner_and_shared_facts() {
        let p = GrantProfile::Narrow;
        let expected = vec![
            NamespaceGrant::new("user:me", Sensitivity::Open),
            NamespaceGrant::new("global", Sensitivity::Open),
        ];
        assert_eq!(p.read(), expected);
        assert_eq!(p.write(), expected);
        assert!(!p.sealed_capable());
        assert!(!p.may_delete());
        assert!(!p.registry_write());
    }

    #[test]
    fn only_the_full_profile_carries_the_capability_flags() {
        for p in [GrantProfile::Standard, GrantProfile::Narrow] {
            assert!(!p.sealed_capable(), "{}", p.as_str());
            assert!(!p.may_delete(), "{}", p.as_str());
            assert!(!p.may_ingest(), "{}", p.as_str());
            assert!(!p.registry_write(), "{}", p.as_str());
        }
    }

    #[test]
    fn a_profile_round_trips_through_its_string_form() {
        for p in [GrantProfile::Full, GrantProfile::Standard, GrantProfile::Narrow] {
            assert_eq!(GrantProfile::parse(p.as_str()), Some(p));
            assert_eq!(GrantProfile::parse(&format!(" {} ", p.as_str().to_uppercase())), Some(p));
            assert!(!p.describe().is_empty());
        }
        assert_eq!(GrantProfile::parse("admin"), None);
        assert_eq!(GrantProfile::parse(""), None);
    }

    #[test]
    fn a_profile_serialises_as_the_same_lowercase_word_it_parses() {
        assert_eq!(serde_json::to_string(&GrantProfile::Narrow).unwrap(), r#""narrow""#);
        assert_eq!(serde_json::from_str::<GrantProfile>(r#""full""#).unwrap(), GrantProfile::Full);
    }

    // ---- who a redirect URI hands the code to ----

    fn host(uri: &str) -> (String, bool) {
        match redirect_destination(uri) {
            RedirectDestination::Host { host, recognised, .. } => (host, recognised),
            other => panic!("expected a host for {uri}, got {other:?}"),
        }
    }

    #[test]
    fn the_redirect_hosts_of_known_mcp_clients_are_recognised() {
        assert_eq!(
            host("https://claude.ai/api/mcp/auth_callback"),
            ("claude.ai".to_string(), true)
        );
        assert!(host("https://claude.com/api/mcp/auth_callback").1);
        assert!(host("https://chatgpt.com/connector_platform_oauth_redirect").1);
    }

    #[test]
    fn a_known_host_is_matched_whole_and_never_as_a_suffix_or_prefix() {
        assert_eq!(
            host("https://claude.ai.attacker.example/cb"),
            ("claude.ai.attacker.example".to_string(), false)
        );
        assert!(!host("https://evilclaude.ai/cb").1);
        assert!(!host("https://www.claude.ai/cb").1);
        assert!(!host("https://chatgpt.com.attacker.example/cb").1);
        // A trailing dot names the same zone in DNS and a different string here. Unrecognised is
        // the side to fail on.
        assert!(!host("https://claude.ai./cb").1);
    }

    #[test]
    fn a_known_host_is_recognised_whatever_case_the_client_wrote_it_in() {
        assert_eq!(host("https://CLAUDE.AI/cb"), ("claude.ai".to_string(), true));
    }

    #[test]
    fn a_known_host_over_plain_http_is_not_recognised() {
        assert!(!host("http://claude.ai/cb").1);
    }

    #[test]
    fn an_ip_address_host_is_shown_as_the_address_and_not_recognised() {
        assert_eq!(host("https://203.0.113.7/cb"), ("203.0.113.7".to_string(), false));
    }

    #[test]
    fn a_loopback_redirect_is_recognised() {
        assert_eq!(host("http://127.0.0.1:53682/callback"), ("127.0.0.1".to_string(), true));
        assert!(host("http://localhost:9000/cb").1);
        assert!(host("http://[::1]:9000/cb").1);
        assert!(!host("http://localhost.attacker.example/cb").1);
    }

    #[test]
    fn only_loopback_keeps_the_code_on_the_owners_machine() {
        let local = |uri| match redirect_destination(uri) {
            RedirectDestination::Host { local, .. } => local,
            other => panic!("expected a host, got {other:?}"),
        };
        assert!(local("http://127.0.0.1:53682/callback"));
        assert!(local("http://localhost:9000/cb"));
        assert!(local("http://[::1]:9000/cb"));
        assert!(!local("https://claude.ai/api/mcp/auth_callback"));
        assert!(!local("http://localhost.attacker.example/cb"));
    }

    #[test]
    fn a_plain_http_lan_redirect_is_neither_recognised_nor_local() {
        let lan = redirect_destination("http://192.168.1.10:3000/oauth/callback");
        assert_eq!(host("http://192.168.1.10:3000/oauth/callback"), ("192.168.1.10".into(), false));
        assert!(!lan.is_local());
        assert!(lan.is_plain_http());
        assert!(!redirect_destination("http://nas.local:3000/cb").recognised());
    }

    #[test]
    fn only_an_http_redirect_is_plain_http() {
        assert!(redirect_destination("http://127.0.0.1:53682/callback").is_plain_http());
        assert!(redirect_destination("HTTP://nas.local/cb").is_plain_http());
        assert!(!redirect_destination("https://nas.local/cb").is_plain_http());
        assert!(!redirect_destination("com.example.app:/cb").is_plain_http());
        assert!(!RedirectDestination::Unreadable.is_plain_http());
    }

    #[test]
    fn an_international_host_is_shown_in_its_ascii_form() {
        // Punycode is what stops a look-alike host from reading as the real one.
        assert!(host("https://cl\u{0430}ude.ai/cb").0.starts_with("xn--"));
        assert!(!host("https://cl\u{0430}ude.ai/cb").1);
    }

    #[test]
    fn a_private_use_scheme_names_the_app_and_is_not_recognised() {
        assert_eq!(
            redirect_destination("com.example.app:/oauth2redirect"),
            RedirectDestination::App { scheme: "com.example.app".to_string() }
        );
        assert!(!redirect_destination("com.example.app:/oauth2redirect").recognised());
    }

    #[test]
    fn an_unparseable_redirect_is_not_recognised() {
        assert_eq!(redirect_destination("not a uri"), RedirectDestination::Unreadable);
        assert!(!RedirectDestination::Unreadable.recognised());
    }

    #[test]
    fn a_name_that_claims_a_known_client_is_caught_in_any_case_and_inside_other_words() {
        assert_eq!(claimed_known_client("Claude"), Some("Claude"));
        assert_eq!(claimed_known_client("my CLAUDE helper"), Some("Claude"));
        assert_eq!(claimed_known_client("ChatGPT connector"), Some("ChatGPT"));
        assert_eq!(claimed_known_client("OpenAI tools"), Some("OpenAI"));
        assert_eq!(claimed_known_client("by anthropic"), Some("Anthropic"));
        assert_eq!(claimed_known_client("Codex CLI"), None);
    }

    #[test]
    fn invisible_and_compatibility_characters_do_not_hide_a_claimed_name() {
        assert_eq!(claimed_known_client("Cl\u{200B}au\u{202E}de"), Some("Claude"));
        // Fullwidth letters fold to ASCII under NFKC.
        assert_eq!(claimed_known_client("\u{FF23}laude"), Some("Claude"));
    }

    #[test]
    fn a_client_name_loses_controls_and_invisible_characters_and_keeps_its_words() {
        assert_eq!(client_name_display("  My\u{202E} app\n\tv2\u{200B} "), "My app v2");
        assert_eq!(client_name_display("\u{200B}\u{FEFF}\r\n"), "");
        assert_eq!(client_name_display("Zed"), "Zed");
    }

    #[test]
    fn a_client_name_loses_selectors_tags_fillers_and_invisible_operators() {
        for (label, c) in [
            ("variation selector", '\u{FE0F}'),
            ("variation selector supplement", '\u{E0100}'),
            ("tag character", '\u{E0041}'),
            ("tag cancel", '\u{E007F}'),
            ("hangul choseong filler", '\u{115F}'),
            ("hangul jungseong filler", '\u{1160}'),
            ("hangul filler", '\u{3164}'),
            ("halfwidth hangul filler", '\u{FFA0}'),
            ("mongolian vowel separator", '\u{180E}'),
            ("word joiner", '\u{2060}'),
            ("invisible plus", '\u{2064}'),
        ] {
            let name = format!("Cl{c}aude");
            assert_eq!(client_name_display(&name), "Claude", "{label}");
            assert_eq!(claimed_known_client(&name), Some("Claude"), "{label}");
        }
    }

    // ---- redirect rules that apply only to a self-registered client ----

    const PUBLIC: &str = "https://lumberroom.example";

    #[test]
    fn a_self_registered_client_may_use_a_named_https_host_a_loopback_or_an_app_scheme() {
        for ok in [
            "https://claude.ai/api/mcp/auth_callback",
            "https://tool.example/cb",
            "http://127.0.0.1:53682/callback",
            "http://localhost:9000/cb",
            "http://[::1]:9000/cb",
            "com.example.app:/oauth2redirect",
        ] {
            assert!(check_self_registered_redirect(ok, PUBLIC).is_ok(), "{ok}");
        }
    }

    #[test]
    fn a_self_registered_client_may_not_send_codes_to_a_public_ip_address() {
        assert!(check_self_registered_redirect("https://203.0.113.7/cb", PUBLIC).is_err());
        assert!(check_self_registered_redirect("https://8.8.8.8/cb", PUBLIC).is_err());
        assert!(check_self_registered_redirect("https://[2001:db8::1]/cb", PUBLIC).is_err());
        assert!(check_self_registered_redirect("https://[2606:4700::1111]/cb", PUBLIC).is_err());
    }

    #[test]
    fn a_self_registered_client_may_send_codes_to_a_private_ip_address() {
        for ok in [
            "https://10.0.0.5/cb",
            "https://172.16.0.1/cb",
            "https://172.31.255.254/cb",
            "https://192.168.1.10/cb",
            "https://100.64.0.1/cb",
            "https://100.127.255.254/cb",
            "https://169.254.10.20/cb",
            "https://[fc00::1]/cb",
            "https://[fd12:3456::1]/cb",
            "https://[fe80::1]/cb",
            "https://[::ffff:192.168.1.10]/cb",
        ] {
            assert!(check_self_registered_redirect(ok, PUBLIC).is_ok(), "{ok}");
        }
    }

    #[test]
    fn the_edges_of_the_private_ranges_stay_public() {
        for bad in [
            "https://172.15.255.255/cb",
            "https://172.32.0.1/cb",
            "https://100.63.255.255/cb",
            "https://100.128.0.1/cb",
            "https://11.0.0.1/cb",
            "https://[fbff::1]/cb",
            "https://[fe00::1]/cb",
            "https://[fec0::1]/cb",
        ] {
            assert!(check_self_registered_redirect(bad, PUBLIC).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_ipv4_mapped_public_address_is_refused() {
        assert!(check_self_registered_redirect("https://[::ffff:203.0.113.7]/cb", PUBLIC).is_err());
        assert!(check_self_registered_redirect("https://[::ffff:8.8.8.8]/cb", PUBLIC).is_err());
    }

    #[test]
    fn a_self_registered_client_may_not_send_codes_to_this_server() {
        assert!(check_self_registered_redirect("https://lumberroom.example/cb", PUBLIC).is_err());
        assert!(check_self_registered_redirect("https://LUMBERROOM.example/x", PUBLIC).is_err());
        assert!(check_self_registered_redirect("https://other.example/cb", PUBLIC).is_ok());
    }

    #[test]
    fn the_default_https_port_counts_as_the_same_origin_written_or_not() {
        assert!(
            check_self_registered_redirect("https://lumberroom.example:443/cb", PUBLIC).is_err()
        );
        let explicit = "https://lumberroom.example:443";
        assert!(check_self_registered_redirect("https://lumberroom.example/cb", explicit).is_err());
    }

    #[test]
    fn another_scheme_or_port_on_this_servers_host_is_another_origin() {
        assert!(check_self_registered_redirect("https://lumberroom.example:8443/x", PUBLIC).is_ok());
        assert!(check_self_registered_redirect("http://lumberroom.example/x", PUBLIC).is_ok());
    }

    /// Both checks in the order `/oauth/register` runs them.
    fn dcr(uri: &str, public_url: &str) -> Result<()> {
        validate_redirect_uri(uri).and_then(|()| check_self_registered_redirect(uri, public_url))
    }

    /// A stock OpenWebUI serves plain http on :3000 and derives its callback from the URL it is
    /// reached at, so it registers an http LAN redirect. The owner ruled on 29 September 2026 that
    /// this must register.
    #[test]
    fn dcr_accepts_a_plain_http_lan_redirect_for_a_lan_server() {
        let lan = "https://192.168.1.10:8787";
        for uri in [
            "http://192.168.1.10:3000/cb",
            "http://10.0.0.5/cb",
            "http://nas.local:3000/cb",
            "http://nas:3000/cb",
            "http://[fd00::1]:3000/cb",
        ] {
            assert!(dcr(uri, lan).is_ok(), "{uri}");
        }
    }

    #[test]
    fn dcr_refuses_a_plain_http_redirect_to_a_public_host() {
        let lan = "https://192.168.1.10:8787";
        for uri in [
            "http://example.com/cb",
            "http://8.8.8.8/cb",
            "http://nas.local.example.com/cb",
            "http://localhost.attacker.example/cb",
        ] {
            assert!(dcr(uri, lan).is_err(), "{uri}");
        }
    }

    #[test]
    fn a_plain_http_lan_redirect_at_this_servers_own_origin_is_refused() {
        let lan = "http://192.168.1.10:8787";
        assert!(dcr("http://192.168.1.10:8787/cb", lan).is_err());
        assert!(dcr("http://192.168.1.10:3000/cb", lan).is_ok());
    }

    #[test]
    fn a_trailing_dot_on_this_servers_host_is_still_this_server() {
        assert!(check_self_registered_redirect("https://lumberroom.example./cb", PUBLIC).is_err());
        let dotted = "https://lumberroom.example.";
        assert!(check_self_registered_redirect("https://lumberroom.example/cb", dotted).is_err());
    }

    #[test]
    fn localhost_and_a_loopback_address_are_one_origin() {
        let local = "http://localhost:8787";
        for uri in [
            "http://127.0.0.1:8787/cb",
            "http://127.0.0.2:8787/cb",
            "http://[::1]:8787/cb",
            "http://localhost.:8787/cb",
        ] {
            assert!(check_self_registered_redirect(uri, local).is_err(), "{uri}");
        }
        let numeric = "http://127.0.0.1:8787";
        assert!(check_self_registered_redirect("http://localhost:8787/cb", numeric).is_err());
        assert!(check_self_registered_redirect("http://localhost:53682/cb", numeric).is_ok());
    }

    #[test]
    fn a_lan_client_on_this_servers_ip_and_another_port_is_accepted() {
        let lan = "https://192.168.1.10:8787";
        let openwebui = "https://192.168.1.10:3000/oauth/callback";
        assert!(check_self_registered_redirect(openwebui, lan).is_ok());
        assert!(check_self_registered_redirect("https://192.168.1.10:8787/cb", lan).is_err());
    }

    #[test]
    fn a_local_hostname_client_on_another_port_is_accepted() {
        let nas = "https://nas.local:8787";
        assert!(
            check_self_registered_redirect("https://nas.local:3000/oauth/callback", nas).is_ok()
        );
        assert!(
            check_self_registered_redirect("https://nas.local:8787/oauth/callback", nas).is_err()
        );
    }

    #[test]
    fn an_ipv4_mapped_form_of_this_servers_address_is_the_same_origin() {
        let lan = "https://192.168.1.10:8787";
        let mapped = "https://[::ffff:192.168.1.10]:8787/cb";
        assert!(check_self_registered_redirect(mapped, lan).is_err());
        let mapped_server = "https://[::ffff:192.168.1.10]:8787";
        assert!(
            check_self_registered_redirect("https://192.168.1.10:8787/cb", mapped_server).is_err()
        );
        assert!(
            check_self_registered_redirect("https://[::ffff:192.168.1.10]:3000/cb", lan).is_ok()
        );
    }

    #[test]
    fn a_loopback_server_still_accepts_a_loopback_cli_on_another_port() {
        let local = "http://127.0.0.1:8798";
        assert!(check_self_registered_redirect("http://127.0.0.1:53682/callback", local).is_ok());
        assert!(check_self_registered_redirect("http://127.0.0.1:8798/oauth/cb", local).is_err());
    }

    // ---- which service a claimed name belongs to ----

    const CLAUDE_AI: &str = "https://claude.ai/api/mcp/auth_callback";
    const CLAUDE_COM: &str = "https://claude.com/api/mcp/auth_callback";
    const CHATGPT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";

    fn claim(name: &str, uri: &str) -> Option<&'static str> {
        mismatched_claim(name, &redirect_destination(uri))
    }

    #[test]
    fn each_recognised_public_host_names_the_service_that_owns_it() {
        assert_eq!(redirect_destination(CLAUDE_AI).service(), Some("Claude"));
        assert_eq!(redirect_destination(CLAUDE_COM).service(), Some("Claude"));
        assert_eq!(redirect_destination(CHATGPT).service(), Some("ChatGPT"));
    }

    #[test]
    fn loopback_plain_http_apps_and_unknown_hosts_belong_to_no_service() {
        for uri in [
            "http://127.0.0.1:53682/callback",
            "http://claude.ai/api/mcp/auth_callback",
            "https://claude.ai.attacker.example/cb",
            "com.anthropic.claude:/cb",
            "not a uri",
        ] {
            assert_eq!(redirect_destination(uri).service(), None, "{uri}");
        }
    }

    #[test]
    fn a_brand_claimed_at_its_own_service_is_no_mismatch() {
        assert_eq!(claim("Claude", CLAUDE_AI), None);
        assert_eq!(claim("Claude", CLAUDE_COM), None);
        assert_eq!(claim("Anthropic connector", CLAUDE_AI), None);
        assert_eq!(claim("ChatGPT", CHATGPT), None);
        assert_eq!(claim("OpenAI tools", CHATGPT), None);
    }

    #[test]
    fn a_brand_claimed_at_another_services_host_is_a_mismatch() {
        assert_eq!(claim("Claude", CHATGPT), Some("Claude"));
        assert_eq!(claim("by anthropic", CHATGPT), Some("Anthropic"));
        assert_eq!(claim("ChatGPT", CLAUDE_AI), Some("ChatGPT"));
        assert_eq!(claim("OpenAI tools", CLAUDE_COM), Some("OpenAI"));
    }

    #[test]
    fn a_name_that_claims_two_services_mismatches_on_either_host() {
        assert_eq!(claim("Claude for ChatGPT", CHATGPT), Some("Claude"));
        assert_eq!(claim("Claude for ChatGPT", CLAUDE_AI), Some("ChatGPT"));
    }

    #[test]
    fn a_name_that_claims_no_brand_is_no_mismatch_anywhere() {
        for uri in [CLAUDE_AI, CHATGPT, "https://zed.example/cb", "com.example.zed:/cb"] {
            assert_eq!(claim("Zed", uri), None, "{uri}");
        }
    }

    #[test]
    fn a_brand_claimed_at_an_unknown_host_or_an_app_is_a_mismatch() {
        assert_eq!(claim("Claude", "https://attacker.example/cb"), Some("Claude"));
        assert_eq!(claim("Claude", "http://claude.ai/api/mcp/auth_callback"), Some("Claude"));
        assert_eq!(claim("ChatGPT", "com.openai.chat:/cb"), Some("ChatGPT"));
    }

    #[test]
    fn a_brand_claimed_at_loopback_is_no_mismatch() {
        assert_eq!(claim("Claude Code", "http://127.0.0.1:53682/callback"), None);
        assert_eq!(claim("ChatGPT", "http://localhost:8080/cb"), None);
    }

    #[test]
    fn case_spacing_suffixes_and_invisible_characters_do_not_hide_a_crossed_brand() {
        for name in
            ["CLAUDE", "  claude   code ", "Claude (2)", "Cl\u{200B}au\u{202E}de", "\u{FF23}laude"]
        {
            assert_eq!(claim(name, CHATGPT), Some("Claude"), "{name:?}");
            assert_eq!(claim(name, CLAUDE_AI), None, "{name:?}");
        }
    }
}
