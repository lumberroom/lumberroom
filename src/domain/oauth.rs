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
/// http to a non-loopback host, and anything that is not http/https or a private-use scheme.
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
        // A local CLI receives its code on a loopback listener, which cannot hold a certificate,
        // so RFC 8252 §7.3 allows plain http there and only there. Any port, because the port is
        // chosen at run time.
        "http" => {
            if is_loopback(&parsed) {
                Ok(())
            } else {
                Err(DomainError::validation(
                    "plain http is allowed only for a loopback redirect_uri. Use https.",
                ))
            }
        }
        // RFC 8252 §7.1 private-use scheme: a reverse-DNS name the app owns, so it contains a dot.
        // Requiring the dot is also what keeps `javascript:` and `data:` out.
        scheme if scheme.contains('.') => Ok(()),
        scheme => Err(DomainError::validation(format!(
            "redirect_uri scheme {scheme:?} is not supported. Use https, a loopback http URI, or \
             a private-use scheme such as com.example.app:/callback."
        ))),
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

/// The redirect hosts of the MCP clients this server can name on the consent page. Compared whole
/// against the parsed host: `claude.ai.attacker.example` ends with nothing and contains nothing
/// that counts.
const KNOWN_CLIENT_HOSTS: &[&str] = &["claude.ai", "claude.com", "chatgpt.com"];

/// Words in a client name that claim one of those clients, with the spelling the consent page
/// prints. Checked against [`comparable_client_name`], so case and invisible characters do not
/// hide them.
const KNOWN_CLIENT_NAMES: &[(&str, &str)] = &[
    ("claude", "Claude"),
    ("chatgpt", "ChatGPT"),
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
];

/// Where an authorization code goes once the owner presses Allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectDestination {
    /// An http or https URI. `host` comes from the parser, so it is lowercase, an international
    /// name arrives as punycode, and an IPv6 address carries its brackets. Punycode is the point:
    /// a Cyrillic look-alike of a known host prints as `xn--...` instead of as the host it mimics.
    ///
    /// `local` is true for loopback only: the code stays on the owner's machine. `recognised` also
    /// covers the known hosts, which are shared by every user of that service.
    Host { host: String, recognised: bool, local: bool },
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
}

/// Read a redirect URI as the place a code will land, and say whether that place belongs to a
/// client this server can name.
///
/// Loopback counts as recognised: a code sent there stays on the owner's machine. A known host
/// counts only over https, since plain http to a public host is a code anyone on the path can read.
pub fn redirect_destination(uri: &str) -> RedirectDestination {
    let Ok(parsed) = Url::parse(uri) else { return RedirectDestination::Unreadable };
    match parsed.scheme() {
        "https" | "http" => {
            let Some(host) = parsed.host_str() else { return RedirectDestination::Unreadable };
            let local = is_loopback(&parsed);
            let recognised =
                local || (parsed.scheme() == "https" && KNOWN_CLIENT_HOSTS.contains(&host));
            RedirectDestination::Host { host: host.to_string(), recognised, local }
        }
        scheme => RedirectDestination::App { scheme: scheme.to_string() },
    }
}

/// The known client a name claims to be, when it claims one.
///
/// A substring match on purpose. "Claude Desktop" and "my claude helper" both borrow the name, and
/// the consent page only uses the answer to word a warning it shows anyway.
pub fn claimed_known_client(name: &str) -> Option<&'static str> {
    let folded = comparable_client_name(name);
    KNOWN_CLIENT_NAMES.iter().find(|(word, _)| folded.contains(word)).map(|(_, shown)| *shown)
}

/// Characters that change nothing a reader sees: zero-width spaces and joiners, the soft hyphen,
/// the byte-order mark, and the bidi embedding, override and isolate controls. A right-to-left
/// override is the dangerous one, since it can make a name print as a different name.
///
/// The same list as `services::sources::invisible`, which predates this one and cannot be imported
/// from here because domain does not import services.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// A client name as registration stores it: invisible characters removed, control characters
/// turned into spaces, whitespace runs collapsed and trimmed. May come back empty; the caller picks
/// the placeholder.
///
/// Cleaned once at registration rather than at every render, because the name also reaches the log,
/// the clients listing and the digest, and a newline in a log field forges a log line.
pub fn client_name_display(name: &str) -> String {
    let flat: String = name
        .chars()
        .filter(|c| !invisible(*c))
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

/// Redirect rules for a client that registered itself, on top of [`validate_redirect_uri`].
///
/// Two refusals, both about a destination the consent page cannot describe to the owner:
///
/// - An IP address in place of a host name, loopback aside. The owner cannot tell whose machine
///   `203.0.113.7` is, and no MCP client this server knows of registers one. An owner who runs such
///   a client issues it by hand, where this rule does not apply.
/// - This server's own host. No client lives at the authorization server, and a code sent back here
///   lands in its access log. On loopback the comparison is the whole origin, because the CLI
///   listens on the same address as a local server, on another port.
///
/// A private-use scheme passes: it has no host to judge, and the consent page warns about it.
pub fn check_self_registered_redirect(uri: &str, public_url: &str) -> Result<()> {
    let Ok(parsed) = Url::parse(uri) else { return Ok(()) };
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Ok(());
    }

    let ip_host = matches!(parsed.host(), Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_)));
    if ip_host && !is_loopback(&parsed) {
        return Err(DomainError::validation(
            "a self-registered client must name its redirect host, not an IP address. Ask the \
             owner to issue a client for this address.",
        ));
    }

    if let Ok(public) = Url::parse(public_url) {
        let same = if is_loopback(&parsed) {
            parsed.origin() == public.origin()
        } else {
            parsed.host_str().is_some() && parsed.host_str() == public.host_str()
        };
        if same {
            return Err(DomainError::validation(
                "redirect_uri points at this authorization server, which is not a client",
            ));
        }
    }
    Ok(())
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
    fn plain_http_to_anything_other_than_loopback_is_refused() {
        assert!(validate_redirect_uri("http://lumberroom.example/cb").is_err());
        assert!(
            validate_redirect_uri("http://localhost.attacker.example/cb").is_err(),
            "a host that merely contains 'localhost' is not loopback"
        );
        assert!(validate_redirect_uri("http://127.0.0.1.attacker.example/cb").is_err());
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
    fn a_self_registered_client_may_not_send_codes_to_an_ip_address() {
        assert!(check_self_registered_redirect("https://203.0.113.7/cb", PUBLIC).is_err());
        assert!(check_self_registered_redirect("https://[2001:db8::1]/cb", PUBLIC).is_err());
    }

    #[test]
    fn a_self_registered_client_may_not_send_codes_to_this_server() {
        assert!(check_self_registered_redirect("https://lumberroom.example/cb", PUBLIC).is_err());
        assert!(
            check_self_registered_redirect("https://LUMBERROOM.example:8443/x", PUBLIC).is_err()
        );
        assert!(check_self_registered_redirect("https://other.example/cb", PUBLIC).is_ok());
    }

    #[test]
    fn a_loopback_server_still_accepts_a_loopback_cli_on_another_port() {
        let local = "http://127.0.0.1:8798";
        assert!(check_self_registered_redirect("http://127.0.0.1:53682/callback", local).is_ok());
        assert!(check_self_registered_redirect("http://127.0.0.1:8798/oauth/cb", local).is_err());
    }
}
