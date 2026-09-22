//! Configuration. Every open decision is a setting here, not a code change, and every setting is
//! validated at boot so a deployment error fails loudly instead of at the first tool call.

use serde::Deserialize;
use std::collections::HashMap;

use crate::domain::errors::{DomainError, Result};
use crate::domain::oauth::canonical_resource;
use crate::domain::policy::{NamespaceGrant, SensitivityDefaults};
use crate::domain::types::Sensitivity;

/// What the server accepts on top of static bearer tokens.
///
/// The modes compose rather than exclude. Static tokens from `AUTH_TOKENS` are honoured whenever
/// they are configured, in every mode, because the surfaces that take a bearer header (Claude Code,
/// Hermes, OpenWebUI, the `lumberroom` CLI) and the surfaces that require OAuth (the Claude.ai family,
/// ChatGPT) have to work against one deployment at the same time. An exclusive mode would break
/// the CLI on the day OAuth was switched on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Static bearer tokens only. The Phase 1 deploy path.
    Token,
    /// Static tokens, plus the built-in authorization server (decision 0002).
    Oauth,
    /// Static tokens, plus JWTs from an external issuer such as Logto.
    Oidc,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Oauth => "oauth",
            Self::Oidc => "oidc",
        }
    }

    /// Whether this mode publishes OAuth metadata and answers 401 with a resource pointer.
    pub fn is_oauth_protected(self) -> bool {
        matches!(self, Self::Oauth | Self::Oidc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedProvider {
    Local,
    Openai,
    Hash,
}

/// How the two search arms combine into one ordering.
///
/// The linear blend adds a cosine similarity near 0.7 to a `ts_rank` near 0.26, so the lexical arm
/// supplies candidates and barely reorders them whatever weight it carries. Reciprocal rank fusion
/// throws both magnitudes away and keeps the positions, which puts the arms on one scale by
/// construction. Linear stays the default until a measurement on this store says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fusion {
    /// Weighted sum of similarity and lexical score. What every deployment runs today.
    Linear,
    /// Weighted sum of `1 / (k + rank)` over each arm.
    Rrf,
}

impl Fusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Rrf => "rrf",
        }
    }
}

/// How strictly an OAuth access token's audience is checked when the token is spent.
///
/// RFC 8707 binds a token to the resource it was minted for, and until this existed the server
/// wrote that binding down and compared it to nothing. Enforcement is a setting rather than a
/// constant because of the tokens already in the table: every one of them predates the check and
/// carries a NULL resource, and refusing all of those on the deploy is an outage for every live
/// client at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAudience {
    /// Compare nothing. For a deployment reachable under two names that has not settled which one
    /// its clients discover, where the check refuses a caller who did everything right.
    Off,
    /// A token that names a resource must name this one. A token that names none is admitted.
    Lenient,
    /// A token that names no resource is refused as well.
    Strict,
}

impl ResourceAudience {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lenient => "lenient",
            Self::Strict => "strict",
        }
    }
}

/// The rank-fusion smoothing constant, at the value the literature and the comparison both used.
///
/// It lives here rather than in the adapter so the fallback an unwired repository holds and the
/// value an operator overrides are one number.
pub const DEFAULT_RRF_K: f64 = 60.0;

/// How old an `occurred_at` must be before `memory_write` accepts it. One day.
///
/// Named so the write path's refusal and the boot-time check read one number, and so a test can
/// pin the default without setting a process-wide variable.
pub const DEFAULT_MIN_OCCURRED_AGE_SECS: u64 = 86_400;

/// Widest window the boot check allows. A year.
const MAX_MIN_OCCURRED_AGE_SECS: u64 = 365 * 24 * 60 * 60;

/// Where the key-encryption key comes from.
///
/// The Phase 3 spec assumed OCI Vault because it was written for one deployment target. The product
/// has to come up with `docker compose up` on any host, so this is a choice with an honest threat
/// model per option rather than a single hard requirement. See decision 0004.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KekProvider {
    /// No key. Writes at `private` are refused rather than silently stored in plaintext.
    None,
    File,
    Env,
}

impl KekProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::File => "file",
            Self::Env => "env",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientGrant {
    /// Stable client identifier. Recorded as source_client and on every tool call.
    pub client: String,
    #[serde(default)]
    pub token: Option<String>,
    /// Namespace globs this client may read, each with a sensitivity ceiling. A bare string means a
    /// ceiling of `open`, so every Phase 1 grant stays valid and never silently gains access when
    /// the sensitivity axis lands. Absent means unrestricted; an explicitly empty list means exactly
    /// that, and must never be widened into full access.
    #[serde(default)]
    pub read: Option<Vec<NamespaceGrant>>,
    #[serde(default)]
    pub write: Option<Vec<NamespaceGrant>>,
    /// The registry holds credential locations, so writing to it is an operator action.
    #[serde(default, rename = "registryWrite", alias = "registry_write")]
    pub registry_write: bool,
    /// Asserts the client can decrypt locally. A client without it may hold a sealed ceiling and
    /// still only ever receive ciphertext.
    #[serde(default, rename = "sealedCapable", alias = "sealed_capable")]
    pub sealed_capable: bool,
    /// A model that can silently delete memories is a worse failure than one that hoards them.
    #[serde(default, rename = "mayDelete", alias = "may_delete")]
    pub may_delete: bool,
    /// A client that can post proposals can fill the queue, and a queue the owner stops reading is
    /// an approval gate in name only.
    #[serde(default, rename = "mayIngest", alias = "may_ingest")]
    pub may_ingest: bool,
    /// Whether this client may read facts that no longer hold. Off unless the grant says otherwise,
    /// because a retired fact can be more revealing than the one that replaced it.
    #[serde(default, rename = "mayReadHistory", alias = "may_read_history")]
    pub may_read_history: bool,
}

impl ClientGrant {
    /// Unrestricted means unrestricted at every level: this is the owner's own client.
    pub fn read_grants(&self) -> Vec<NamespaceGrant> {
        self.read.clone().unwrap_or_else(NamespaceGrant::everything)
    }

    pub fn write_grants(&self) -> Vec<NamespaceGrant> {
        self.write.clone().unwrap_or_else(NamespaceGrant::everything)
    }

    /// A grant with no explicit sealed capability still gets it when the grant is unrestricted,
    /// because an unrestricted grant is the operator's own client by definition.
    pub fn effective_sealed_capable(&self) -> bool {
        self.sealed_capable || self.read.is_none()
    }

    pub fn effective_may_delete(&self) -> bool {
        self.may_delete
    }

    /// Unrestricted grants gain sealed capability and never gain this, which is the `may_delete`
    /// reading rather than the `sealed_capable` one. Filling the proposal queue is an action the
    /// owner asks for by name, on his own client too.
    pub fn effective_may_read_history(&self) -> bool {
        self.may_read_history
    }

    pub fn effective_may_ingest(&self) -> bool {
        self.may_ingest
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub host: String,
    pub tenant_id: String,
    pub database_url: String,
    pub run_migrations_on_boot: bool,
    /// The public origin this server is reached at, with no trailing slash. Every externally
    /// visible URL is derived from it: the MCP endpoint, the OAuth issuer, the metadata documents
    /// and the redirect targets. One source, because an issuer that disagrees with the host behind
    /// a reverse proxy is invisible until a real client's discovery fails.
    pub public_url: String,
    pub auth: AuthConfig,
    pub oauth: OauthConfig,
    pub embed: EmbedConfig,
    pub bootstrap: BootstrapConfig,
    pub search: SearchConfig,
    pub policy: PolicyConfig,
    pub crypto: CryptoConfig,
    pub quality: QualityConfig,
    pub ingest: IngestConfig,
    pub cleanup: CleanupConfig,
}

/// The scheduled cleanup pass, which runs inside this process.
///
/// Only the deterministic half. It reads the store, groups duplicates and writes proposals, and it
/// opens no outbound connection to anyone. The judgment half stays in the `lumberroom` client because it calls a
/// provider, and this process holds the KEK: decision 0011 is where that line is drawn, and moving
/// the model call in here would erase it.
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// Seconds between passes. Zero turns the pass off and is the only way to turn it off, so a
    /// deployment that wants nothing running in the background sets it explicitly rather than
    /// discovering a timer it did not know about.
    pub interval_secs: u64,
    /// A namespace glob, or every namespace. Present because a large store may want the pass
    /// narrowed, and absent by default because a pass that covers only part of the store reports a
    /// clean bill of health for the part it never read.
    pub namespace: Option<String>,
    /// How many rows one pass considers. A run that hits this holds its watermark where the
    /// findings stopped and says so in the log, so the next pass picks up the remainder.
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub mode: AuthMode,
    /// The OAuth resource identifier, which is the MCP endpoint itself.
    pub resource_url: String,
    pub grants: Vec<ClientGrant>,
    pub issuer: String,
    pub audience: String,
    pub jwks_uri: String,
    pub required_scopes: Vec<String>,
    /// Claims carrying client identity, in precedence order.
    pub client_claims: Vec<String>,
    /// The `sub` values this server accepts, from `OIDC_ALLOWED_SUBJECTS`.
    ///
    /// Without it, oidc mode authorizes the application and never the person holding it. The grant
    /// is looked up by client id, that id is public, and the issuer signs a token for whoever signs
    /// up, so a stranger who registers at the issuer and runs the code flow against the owner's
    /// application id arrives with a token that validates and picks up the owner's grant.
    /// `validate` requires a non-empty list in oidc mode for that reason: an empty one would mean
    /// everyone.
    ///
    /// `sub`, not `email`. The subject is the issuer's stable identifier for an account, while an
    /// email claim can arrive unverified, can be changed by the account holder, and is absent from
    /// tokens issued without the profile scope.
    pub allowed_subjects: Vec<String>,
}

/// The built-in authorization server.
#[derive(Debug, Clone)]
pub struct OauthConfig {
    /// Argon2 PHC string. The owner's password is never stored, only this.
    pub owner_password_hash: Option<String>,
    /// RFC 7591 self-registration. On by default: registration is not authorization, since a
    /// registered client holds nothing until the owner logs in and consents. Turn it off to require
    /// every client to be issued by hand.
    pub dcr_enabled: bool,
    pub code_ttl_secs: i64,
    pub access_ttl_secs: i64,
    pub refresh_ttl_secs: i64,
    /// How long the owner's browser stays signed in to the consent screen.
    pub session_ttl_secs: i64,
    /// Preselected on the consent screen. The owner still chooses.
    pub default_profile: String,
    pub scopes_supported: Vec<String>,
    /// HMAC key for the consent-screen cookie. Derived from a configured secret so a restart does
    /// not invalidate a login mid-flow.
    pub cookie_secret: String,
    /// Failed password attempts allowed per minute before the login form starts refusing.
    pub login_attempts_per_minute: u32,
    /// RFC 7591 registrations allowed per minute, per caller, before `/oauth/register` answers 429.
    /// Registration is unauthenticated and writes a row, so without a ceiling one caller can fill
    /// the client table at whatever rate the network allows.
    pub registrations_per_minute: u32,
    /// How strictly the audience on an access token is checked. See [`ResourceAudience`].
    ///
    /// `lenient` rather than `strict` by default, and the reason is in the data: every token issued
    /// before this landed has a NULL resource, and the CLI in this repository omits `resource` on
    /// the refresh grant, so it re-mints a NULL one on every rotation. `strict` on the deploy would
    /// log all of them out. The NULLs drain within one `OAUTH_ACCESS_TTL_SECS` once the token
    /// endpoint starts stamping a default, which is what makes `strict` reachable later.
    pub resource_audience: ResourceAudience,
}

#[derive(Debug, Clone)]
pub struct EmbedConfig {
    pub provider: EmbedProvider,
    pub dim: usize,
    pub model: String,
    pub cache_dir: String,
    pub allow_fallback: bool,
}

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub cache_ms: u64,
    pub profile_limit: i64,
    pub project_limit: i64,
    pub recent_limit: i64,
    pub recent_days: i32,
    pub registry_limit: i64,
    /// Hard ceiling on the rendered digest so a large store cannot blow a client's context.
    pub max_chars: usize,
    /// Ceilings differ by surface and at least one is undocumented: Claude Code caps output in
    /// tokens, the Claude.ai family at roughly 150,000 characters, and ChatGPT truncates at a line
    /// boundary at a size nobody has published. The structured payload stays authoritative so a
    /// client that truncates the text block still has the data.
    pub max_chars_by_client: HashMap<String, usize>,
}

impl BootstrapConfig {
    pub fn budget_for(&self, client: &str) -> usize {
        self.max_chars_by_client.get(client).copied().unwrap_or(self.max_chars)
    }
}

#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub default_limit: i64,
    pub max_limit: i64,
    /// When a question is shaped like a join rather than a lookup, and a graph walk is worth its
    /// cost. Both are design targets set from two observations on 25 August 2026, which is not a
    /// calibration; `domain::routing` publishes the signals behind every verdict so a run over real
    /// questions can move them.
    pub graph_route: crate::domain::routing::Thresholds,
    pub vector_weight: f64,
    pub lexical_weight: f64,
    pub include_all_projects: bool,
    pub other_project_penalty: f64,
    /// Recency-and-use boost. Capped low: a fact retrieved often is more likely to be the one
    /// wanted, but never more likely than the fact that actually answers the question.
    ///
    /// The term is additive under [`Fusion::Linear`] and multiplicative under [`Fusion::Rrf`],
    /// where the whole score of a top row is about 1/61 and an additive 0.05 would sort the
    /// results by access count.
    pub usage_weight: f64,
    /// Which of the two blends orders the candidate set.
    pub fusion: Fusion,
    /// The `k` in `1 / (k + rank)`. Larger flattens the difference between the top ranks.
    pub rrf_k: f64,
}

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    /// Namespace-to-default classification, in effect.
    ///
    /// `load` fills this from `SENSITIVITY_DEFAULTS` when that variable carries rules. The
    /// composition root then calls [`Config::apply_sensitivity_defaults`] with the
    /// `sensitivity_default` table, which is what makes an operator editing rows rather than
    /// redeploying work. Never empty once boot is past that call: an empty rule set classifies
    /// everything open and says nothing about having done so.
    pub defaults: SensitivityDefaults,
    /// Whether `defaults` came from the environment. Read once, by the boot-time resolution, to
    /// decide whether the database table gets a say.
    pub defaults_from_env: bool,
    /// The credential tripwire. On by default; a write of credential-shaped content at `open` is
    /// refused with the pattern named.
    pub tripwire: bool,
    /// The longest content one memory may carry. A memory is a fact, not a transcript, and 8000
    /// characters is generous for one. It is a setting rather than a constant because a retrieval
    /// benchmark stores whole chat sessions as single documents, and refusing those would measure
    /// the cap instead of the ranking.
    pub max_content_chars: usize,
    /// Highest level `memory_write` will accept from a tool at all. Sealed content goes through the
    /// client-side path, never through a plaintext tool argument.
    pub max_write_sensitivity: Sensitivity,
    /// How old an `occurred_at` has to be before `memory_write` accepts it, in seconds.
    ///
    /// A valid time inside this window repeats `created_at`, which the store records anyway, so
    /// refusing it loses nothing and the writes it refuses are the ones where the column says
    /// nothing. It bounds the future for free: a date ahead of now is inside every window, which
    /// is why there is one setting here rather than two.
    ///
    /// The ingest fill is exempt through `write::run_observed`. A transcript recorded this morning
    /// carries a same-day `observed_at` that is the real date the owner said the thing.
    pub write_min_occurred_age_secs: u64,
}

#[derive(Debug, Clone)]
pub struct CryptoConfig {
    pub provider: KekProvider,
    pub kek_path: String,
    pub kek_env_var: String,
    pub kek_id: String,
    /// Do not write an encrypted row until a boot has proved the key can be recovered. Step 4 of
    /// the Phase 3 migration order is the one that can strand data.
    pub require_verified_kek: bool,
}

#[derive(Debug, Clone)]
pub struct QualityConfig {
    /// At or above: collapse into the existing row. Starts at 0.97 and must not stay a guess; the
    /// calibration procedure is in the Phase 4 spec §2.
    pub dedupe_threshold: f64,
    /// At or above, and below the dedupe threshold: store, and hand the caller the older row as a
    /// possible conflict.
    pub conflict_threshold: f64,
    /// How many conflict candidates a write returns.
    pub conflict_limit: i64,
    /// Largest readable namespace, in live embedded rows, that the conflicts self-join will scan.
    /// The join is O(n squared) per namespace: a dev-container probe timed 2.48s at 1,400 rows,
    /// 13.33s at 3,000 and 34.96s at 5,000, so past this the queue refuses instead of hanging.
    pub conflict_scan_max: i64,
    /// A live row never retrieved and older than this appears in `lumberroom review --stale`.
    pub stale_days: i32,
    /// Highest sensitivity the Obsidian export may include. Private content in a vault synced to a
    /// third party defeats the encryption it was given.
    pub export_max_sensitivity: Sensitivity,
    /// What one uploaded archive may decompress to before the reader gives up.
    ///
    /// A bomb reaches this process before any parsing does, so the ceiling belongs to the
    /// deployment rather than to the crate that enforces it.
    pub archive_max_decompressed_bytes: u64,
}

/// Transcript ingestion. Both numbers belong to the anti-loop check and to nothing else.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// How far back an emission still counts as the cause of an echo. Ninety days: a fact the store
    /// handed out last year and the owner restated this morning is a restatement, not a loop.
    pub emission_window_days: i64,
    /// Clock slack between the store handing content out and a transcript recording it. The two
    /// clocks are the server's and the laptop's, so an emission a few minutes after the span it
    /// caused is still the cause.
    pub emission_slack_secs: f64,
}

impl IngestConfig {
    pub fn emission_window_secs(&self) -> f64 {
        (self.emission_window_days * 86_400) as f64
    }
}

impl Config {
    pub fn mode_str(&self) -> &'static str {
        self.auth.mode.as_str()
    }

    /// RFC 9728 metadata location, at the origin.
    pub fn protected_resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource", self.public_url)
    }

    /// The issuer the built-in authorization server advertises, which is this origin.
    pub fn builtin_issuer(&self) -> String {
        self.public_url.clone()
    }

    /// Settle the classification table at boot, given the rows the database holds.
    ///
    /// Called from the composition root once the pool exists, next to the KEK check, because both
    /// are one-off boot questions about what this store already holds. Returns the source label so
    /// the caller can log it.
    pub fn apply_sensitivity_defaults(
        &mut self,
        from_table: Vec<(String, Sensitivity)>,
    ) -> &'static str {
        let from_env = self.policy.defaults_from_env.then(|| self.policy.defaults.clone());
        let (defaults, source) = resolve_sensitivity_defaults(from_env, from_table);
        self.policy.defaults = defaults;
        source
    }

    /// Whichever authorization server is in play, for the metadata document and the 401 challenge.
    pub fn authorization_server(&self) -> Option<String> {
        match self.auth.mode {
            AuthMode::Oauth => Some(self.builtin_issuer()),
            AuthMode::Oidc if !self.auth.issuer.is_empty() => Some(self.auth.issuer.clone()),
            _ => None,
        }
    }
}

fn env(key: &str, fallback: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => fallback.to_string(),
    }
}

fn env_opt(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn env_num<T: std::str::FromStr>(key: &str, fallback: T) -> Result<T> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v
            .parse()
            .map_err(|_| DomainError::validation(format!("{key} is not a valid number: {v:?}"))),
        _ => Ok(fallback),
    }
}

fn env_bool(key: &str, fallback: bool) -> bool {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => {
            matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        }
        _ => fallback,
    }
}

fn env_list(key: &str, fallback: &[&str]) -> Vec<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => {
            v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        }
        _ => fallback.iter().map(|s| s.to_string()).collect(),
    }
}

/// `OAUTH_RESOURCE_AUDIENCE`, refused at boot when it is none of the three.
///
/// A free function for `parse_fusion`'s reason: the refusal can be tested without two tests racing
/// on one process environment.
fn parse_resource_audience(raw: &str) -> Result<ResourceAudience> {
    match raw.trim() {
        "lenient" | "" => Ok(ResourceAudience::Lenient),
        "strict" => Ok(ResourceAudience::Strict),
        "off" => Ok(ResourceAudience::Off),
        other => Err(DomainError::validation(format!(
            "OAUTH_RESOURCE_AUDIENCE must be off|lenient|strict, got {other:?}"
        ))),
    }
}

/// `SEARCH_FUSION`, refused at boot when it is neither spelling.
///
/// A free function rather than a match inside `load`, so the refusal can be tested without two
/// tests racing on one process environment.
fn parse_fusion(raw: &str) -> Result<Fusion> {
    match raw.trim() {
        "linear" | "" => Ok(Fusion::Linear),
        "rrf" => Ok(Fusion::Rrf),
        other => {
            Err(DomainError::validation(format!("SEARCH_FUSION must be linear|rrf, got {other:?}")))
        }
    }
}

/// `WRITE_MIN_OCCURRED_AGE_SECS`, refused at boot at both ends.
///
/// A free function for `parse_fusion`'s reason: the refusal is testable without two tests racing
/// on one process environment.
fn check_min_occurred_age(secs: u64) -> Result<()> {
    if secs == 0 {
        return Err(DomainError::validation(
            "WRITE_MIN_OCCURRED_AGE_SECS must be at least 1. Zero accepts a valid time of now, \
             which is the write the fence exists to refuse: it stores the arrival clock in the \
             column that was added to stop conflating the two.",
        ));
    }
    if secs > MAX_MIN_OCCURRED_AGE_SECS {
        return Err(DomainError::validation(format!(
            "WRITE_MIN_OCCURRED_AGE_SECS is {secs}, above the {MAX_MIN_OCCURRED_AGE_SECS} second \
             ceiling of one year. A window that wide refuses a date the owner stated months ago, \
             and the refusal asks the caller to omit the field, so the fence would throw away \
             true dates instead of redundant ones."
        )));
    }
    Ok(())
}

fn env_sensitivity(key: &str, fallback: Sensitivity) -> Result<Sensitivity> {
    match env_opt(key) {
        Some(v) => Sensitivity::parse(&v).ok_or_else(|| {
            DomainError::validation(format!("{key} must be open|private|sealed, got {v:?}"))
        }),
        None => Ok(fallback),
    }
}

/// Per-client digest budgets, as `client=chars` pairs or JSON.
fn parse_client_budgets(raw: &str) -> Result<HashMap<String, usize>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(HashMap::new());
    }
    if raw.starts_with('{') {
        return serde_json::from_str(raw).map_err(|e| {
            DomainError::validation(format!("BOOTSTRAP_MAX_CHARS_BY_CLIENT is not valid JSON: {e}"))
        });
    }
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (client, chars) = pair.split_once('=').ok_or_else(|| {
                DomainError::validation(format!(
                    "BOOTSTRAP_MAX_CHARS_BY_CLIENT entry {pair:?} must look like client=chars"
                ))
            })?;
            let n: usize = chars.trim().parse().map_err(|_| {
                DomainError::validation(format!(
                    "BOOTSTRAP_MAX_CHARS_BY_CLIENT entry {pair:?} has a non-numeric budget"
                ))
            })?;
            Ok((client.trim().to_string(), n))
        })
        .collect()
}

/// Grants come from AUTH_TOKENS as JSON, or as compact `client:token` pairs.
pub fn parse_grants(inline: &str, file: &str) -> Result<Vec<ClientGrant>> {
    let mut raw = inline.trim().to_string();
    if raw.is_empty() && !file.is_empty() {
        raw = std::fs::read_to_string(file)
            .map_err(|e| {
                DomainError::validation(format!("cannot read AUTH_TOKENS_FILE {file}: {e}"))
            })?
            .trim()
            .to_string();
    }
    if raw.is_empty() {
        return Ok(vec![]);
    }

    if raw.starts_with('[') || raw.starts_with('{') {
        let parsed: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| DomainError::validation(format!("AUTH_TOKENS is not valid JSON: {e}")))?;
        let list = match parsed {
            serde_json::Value::Array(a) => a,
            other => vec![other],
        };
        let mut grants = Vec::with_capacity(list.len());
        for (i, entry) in list.into_iter().enumerate() {
            let g: ClientGrant = serde_json::from_value(entry).map_err(|e| {
                DomainError::validation(format!("AUTH_TOKENS[{i}] is not a valid grant: {e}"))
            })?;
            if g.client.is_empty() {
                return Err(DomainError::validation(format!(
                    "AUTH_TOKENS[{i}] needs a non-empty \"client\""
                )));
            }
            grants.push(g);
        }
        return Ok(grants);
    }

    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once(':') {
            Some((client, token)) if !client.is_empty() && !token.is_empty() => Ok(ClientGrant {
                client: client.to_string(),
                token: Some(token.to_string()),
                read: None,
                write: None,
                registry_write: false,
                sealed_capable: false,
                may_delete: false,
                may_ingest: false,
                may_read_history: false,
            }),
            _ => Err(DomainError::validation(format!(
                "AUTH_TOKENS entry {pair:?} must look like client:token"
            ))),
        })
        .collect()
}

/// `pattern=level` pairs, overriding the seeded defaults when set.
pub fn parse_sensitivity_defaults(raw: &str) -> Result<SensitivityDefaults> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(SensitivityDefaults::default());
    }
    let mut rules = Vec::new();
    for pair in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (pattern, level) = pair.split_once('=').ok_or_else(|| {
            DomainError::validation(format!(
                "SENSITIVITY_DEFAULTS entry {pair:?} must look like namespace=level"
            ))
        })?;
        let level = Sensitivity::parse(level).ok_or_else(|| {
            DomainError::validation(format!(
                "SENSITIVITY_DEFAULTS entry {pair:?} has an unknown level"
            ))
        })?;
        rules.push((pattern.trim().to_string(), level));
    }
    Ok(SensitivityDefaults::new(rules))
}

/// Which classification table the store actually runs on. The one place the precedence is decided.
///
/// `SENSITIVITY_DEFAULTS` first, because an operator who sets an environment variable means it.
/// Then the `sensitivity_default` table, which migration 004 seeds and which the operator is meant
/// to edit with SQL rather than a redeploy. Then [`SensitivityDefaults::seeded`].
///
/// The last step is the one that matters. An empty rule set is not a policy, it is the absence of
/// one, and it classifies every namespace open without a word in any log. That is precisely how
/// `personal:finance` came to be stored at open on a default install: `SENSITIVITY_DEFAULTS` is
/// unset, `parse_sensitivity_defaults("")` returned an empty set, and nothing ever read the table.
///
/// `from_env` is `None` when the variable is absent *or* empty. docker-compose.yml passes
/// `SENSITIVITY_DEFAULTS: ${SENSITIVITY_DEFAULTS:-}`, so the variable is present and empty in every
/// compose deployment. Treating that as "the operator chose no rules" would reinstate the empty
/// table this function exists to prevent.
///
/// Returns the effective rules and a label naming where they came from, for the boot log. An
/// operator who cannot tell what their store classifies as private has no way to trust it.
pub fn resolve_sensitivity_defaults(
    from_env: Option<SensitivityDefaults>,
    from_table: Vec<(String, Sensitivity)>,
) -> (SensitivityDefaults, &'static str) {
    if let Some(rules) = from_env {
        if !rules.is_empty() {
            return (rules, "SENSITIVITY_DEFAULTS");
        }
    }
    if !from_table.is_empty() {
        return (SensitivityDefaults::new(from_table), "sensitivity_default table");
    }
    (SensitivityDefaults::seeded(), "built-in seeded defaults")
}

pub fn load() -> Result<Config> {
    // Absent and empty are the same answer: no rules were chosen here. See
    // `resolve_sensitivity_defaults` for why the empty case cannot be allowed to win.
    let sensitivity_defaults_env = match env_opt("SENSITIVITY_DEFAULTS") {
        Some(raw) => Some(parse_sensitivity_defaults(&raw)?),
        None => None,
    };

    let mode = match env("AUTH_MODE", "token").as_str() {
        "token" => AuthMode::Token,
        "oauth" => AuthMode::Oauth,
        "oidc" => AuthMode::Oidc,
        other => {
            return Err(DomainError::validation(format!(
                "AUTH_MODE must be \"token\", \"oauth\" or \"oidc\", got {other:?}"
            )))
        }
    };

    let provider = match env("EMBED_PROVIDER", "local").as_str() {
        "local" => EmbedProvider::Local,
        "openai" => EmbedProvider::Openai,
        "hash" => EmbedProvider::Hash,
        other => {
            return Err(DomainError::validation(format!(
                "EMBED_PROVIDER must be local|openai|hash, got {other:?}"
            )))
        }
    };

    let kek_provider = match env("KEK_PROVIDER", "none").as_str() {
        "none" | "" => KekProvider::None,
        "file" => KekProvider::File,
        "env" => KekProvider::Env,
        other => {
            return Err(DomainError::validation(format!(
                "KEK_PROVIDER must be none|file|env, got {other:?}"
            )))
        }
    };

    // One public origin, everything derived. MCP_RESOURCE_URL stays readable for deployments that
    // already set it, and PUBLIC_URL wins when both are present.
    let public_url = match env_opt("PUBLIC_URL") {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => {
            let resource = env("MCP_RESOURCE_URL", "http://127.0.0.1:8787/mcp");
            resource.trim_end_matches('/').trim_end_matches("/mcp").to_string()
        }
    };
    let resource_url = match env_opt("MCP_RESOURCE_URL") {
        Some(u) if env_opt("PUBLIC_URL").is_none() => u,
        _ => format!("{public_url}/mcp"),
    };

    let issuer = env("OIDC_ISSUER", "").trim_end_matches('/').to_string();
    let jwks_default =
        if issuer.is_empty() { String::new() } else { format!("{issuer}/oidc/jwks") };

    // Parsed once. Two reads of `SEARCH_FUSION` would agree today and drift the first time
    // somebody edits one of them, and the two things it decides are the ranking and whether the
    // router's cosine thresholds mean anything at all.
    let fusion = parse_fusion(&env("SEARCH_FUSION", "linear"))?;

    let cfg = Config {
        public_url: public_url.clone(),
        port: env_num("PORT", 8787u16)?,
        host: env("HOST", "0.0.0.0"),
        tenant_id: env("TENANT_ID", "me"),
        database_url: env(
            "DATABASE_URL",
            "postgres://lumberroom:lumberroom@127.0.0.1:5432/lumberroom",
        ),
        run_migrations_on_boot: env_bool("RUN_MIGRATIONS_ON_BOOT", true),
        auth: AuthConfig {
            mode,
            audience: env("OIDC_AUDIENCE", &resource_url),
            jwks_uri: env("OIDC_JWKS_URI", &jwks_default),
            resource_url,
            grants: parse_grants(&env("AUTH_TOKENS", ""), &env("AUTH_TOKENS_FILE", ""))?,
            issuer,
            required_scopes: env_list("OIDC_REQUIRED_SCOPES", &[]),
            client_claims: env_list("OIDC_CLIENT_CLAIM", &["client_id", "azp", "sub"]),
            allowed_subjects: env_list("OIDC_ALLOWED_SUBJECTS", &[]),
        },
        oauth: OauthConfig {
            owner_password_hash: env_opt("OWNER_PASSWORD_HASH"),
            dcr_enabled: env_bool("OAUTH_DCR_ENABLED", true),
            code_ttl_secs: env_num("OAUTH_CODE_TTL_SECS", 120i64)?,
            access_ttl_secs: env_num("OAUTH_ACCESS_TTL_SECS", 3600i64)?,
            refresh_ttl_secs: env_num("OAUTH_REFRESH_TTL_SECS", 60 * 60 * 24 * 60i64)?,
            session_ttl_secs: env_num("OAUTH_SESSION_TTL_SECS", 900i64)?,
            default_profile: env("OAUTH_DEFAULT_PROFILE", "standard"),
            scopes_supported: env_list("OAUTH_SCOPES", &["memory.read", "memory.write"]),
            cookie_secret: env("OAUTH_COOKIE_SECRET", ""),
            login_attempts_per_minute: env_num("OAUTH_LOGIN_ATTEMPTS_PER_MINUTE", 5u32)?,
            registrations_per_minute: env_num("OAUTH_REGISTRATIONS_PER_MINUTE", 5u32)?,
            resource_audience: parse_resource_audience(&env("OAUTH_RESOURCE_AUDIENCE", ""))?,
        },
        embed: EmbedConfig {
            provider,
            dim: env_num("EMBED_DIM", 768usize)?,
            model: env("EMBED_MODEL", "Xenova/bge-base-en-v1.5"),
            cache_dir: env("MODEL_CACHE_DIR", "/models"),
            allow_fallback: env_bool("EMBED_ALLOW_FALLBACK", false),
        },
        bootstrap: BootstrapConfig {
            cache_ms: env_num("BOOTSTRAP_CACHE_MS", 30_000u64)?,
            profile_limit: env_num("BOOTSTRAP_PROFILE_LIMIT", 12i64)?,
            project_limit: env_num("BOOTSTRAP_PROJECT_LIMIT", 10i64)?,
            recent_limit: env_num("BOOTSTRAP_RECENT_LIMIT", 8i64)?,
            recent_days: env_num("BOOTSTRAP_RECENT_DAYS", 14i32)?,
            registry_limit: env_num("BOOTSTRAP_REGISTRY_LIMIT", 25i64)?,
            max_chars: env_num("BOOTSTRAP_MAX_CHARS", 6000usize)?,
            max_chars_by_client: parse_client_budgets(&env("BOOTSTRAP_MAX_CHARS_BY_CLIENT", ""))?,
        },
        search: SearchConfig {
            default_limit: env_num("SEARCH_DEFAULT_LIMIT", 8i64)?,
            max_limit: env_num("SEARCH_MAX_LIMIT", 50i64)?,
            vector_weight: env_num("SEARCH_VECTOR_WEIGHT", 1.0f64)?,
            lexical_weight: env_num("SEARCH_LEXICAL_WEIGHT", 0.35f64)?,
            include_all_projects: env_bool("SEARCH_INCLUDE_ALL_PROJECTS", true),
            other_project_penalty: env_num("SEARCH_OTHER_PROJECT_PENALTY", 0.85f64)?,
            usage_weight: env_num("SEARCH_USAGE_WEIGHT", 0.05f64)?,
            graph_route: crate::domain::routing::Thresholds {
                // Follows the blend rather than being set beside it. The thresholds are cosine
                // values, and under rank fusion they would call every question weak and flat.
                scale: match parse_fusion(&env("SEARCH_FUSION", "linear"))? {
                    Fusion::Linear => crate::domain::routing::Scale::Cosine,
                    Fusion::Rrf => crate::domain::routing::Scale::Ranked,
                },
                max_top: env_num("GRAPH_ROUTE_MAX_TOP", 0.65f64)?,
                max_spread: env_num("GRAPH_ROUTE_MAX_SPREAD", 0.08f64)?,
            },
            fusion: parse_fusion(&env("SEARCH_FUSION", "linear"))?,
            rrf_k: env_num("SEARCH_RRF_K", DEFAULT_RRF_K)?,
        },
        policy: PolicyConfig {
            // Parsed here so a malformed pair fails at boot rather than at the first write, but the
            // effective table is not settled until the composition root has read the database.
            defaults: sensitivity_defaults_env.clone().unwrap_or_default(),
            defaults_from_env: sensitivity_defaults_env.is_some(),
            tripwire: env_bool("SENSITIVITY_TRIPWIRE", true),
            max_write_sensitivity: env_sensitivity("MAX_WRITE_SENSITIVITY", Sensitivity::Private)?,
            // A memory is a fact, not a transcript, and 8000 characters is generous for one. It
            // moves for one reason: a retrieval benchmark stores whole chat sessions as single
            // documents, and refusing those would measure the cap rather than the ranking.
            max_content_chars: env_num("WRITE_MAX_CONTENT_CHARS", 8000usize)?,
            write_min_occurred_age_secs: env_num(
                "WRITE_MIN_OCCURRED_AGE_SECS",
                DEFAULT_MIN_OCCURRED_AGE_SECS,
            )?,
        },
        crypto: CryptoConfig {
            provider: kek_provider,
            kek_path: env("KEK_PATH", "/run/secrets/lumberroom-kek"),
            kek_env_var: env("KEK_ENV_VAR", "LUMBERROOM_KEK"),
            kek_id: env("KEK_ID", "kek-1"),
            require_verified_kek: env_bool("REQUIRE_VERIFIED_KEK", true),
        },
        cleanup: CleanupConfig {
            interval_secs: env_num("CLEANUP_INTERVAL_SECS", 3600u64)?,
            namespace: env_opt("CLEANUP_NAMESPACE"),
            limit: env_num("CLEANUP_LIMIT", 500i64)?,
        },
        quality: QualityConfig {
            dedupe_threshold: env_num("DEDUPE_THRESHOLD", 0.97f64)?,
            conflict_threshold: env_num("CONFLICT_THRESHOLD", 0.90f64)?,
            conflict_limit: env_num("CONFLICT_LIMIT", 3i64)?,
            conflict_scan_max: env_num("CONFLICT_SCAN_MAX", 2_000i64)?,
            stale_days: env_num("STALE_DAYS", 365i32)?,
            export_max_sensitivity: env_sensitivity("EXPORT_MAX_SENSITIVITY", Sensitivity::Open)?,
            archive_max_decompressed_bytes: env_num(
                "ARCHIVE_MAX_DECOMPRESSED_BYTES",
                2u64 * 1024 * 1024 * 1024,
            )?,
        },
        ingest: IngestConfig {
            emission_window_days: env_num("INGEST_EMISSION_WINDOW_DAYS", 90i64)?,
            emission_slack_secs: env_num("INGEST_EMISSION_SLACK_SECS", 300.0f64)?,
        },
    };

    validate(&cfg)?;
    Ok(cfg)
}

/// Split out of `validate` so a test can reach it without building a whole `Config`.
fn validate_cleanup(c: &CleanupConfig) -> Result<()> {
    // A pass every few seconds walks the store faster than the store changes and writes nothing new
    // each time, which costs the database and tells nobody anything. Sixty seconds is well under
    // any cadence worth running and still refuses the typo that sets this to 5.
    if c.interval_secs != 0 && c.interval_secs < 60 {
        return Err(DomainError::validation(format!(
            "CLEANUP_INTERVAL_SECS is {}, which walks the store faster than it changes. Use 0 to \
             turn the pass off, or at least 60.",
            c.interval_secs
        )));
    }
    if c.limit < 1 {
        return Err(DomainError::validation(
            "CLEANUP_LIMIT must be at least 1. Zero reads nothing and reports a clean store.",
        ));
    }
    Ok(())
}

/// What `AUTH_MODE=oidc` needs before the server will answer with an external issuer's tokens.
/// Split out so a test can reach it without assembling a whole `Config`.
fn validate_oidc(auth: &AuthConfig) -> Result<()> {
    // Fail closed. An earlier build granted everything when no grants were configured, which meant
    // any token the issuer signed held full access.
    if auth.grants.is_empty() {
        return Err(DomainError::validation(
            "AUTH_MODE=oidc needs at least one client grant in AUTH_TOKENS. Without one, every \
             token the issuer signs would be refused; with an unchecked default it would hold \
             full access.",
        ));
    }
    if auth.issuer.is_empty() {
        return Err(DomainError::validation("AUTH_MODE=oidc needs OIDC_ISSUER"));
    }
    if auth.jwks_uri.is_empty() {
        return Err(DomainError::validation(
            "AUTH_MODE=oidc needs OIDC_JWKS_URI (or a derivable OIDC_ISSUER)",
        ));
    }
    // The one check that authorizes a person rather than an application. Everything above is about
    // which client the token names, and a client id is public: it sits in `~/.claude.json` and in
    // the address bar of every sign-in. Anyone who can create an account at the issuer can run the
    // code flow against it, so without a subject list the grant belongs to the issuer's whole user
    // base.
    if auth.allowed_subjects.is_empty() {
        return Err(DomainError::validation(
            "AUTH_MODE=oidc needs OIDC_ALLOWED_SUBJECTS. Grants are keyed on the client id, which \
             is public, so any account at the issuer could present a valid token for it. List the \
             `sub` claim of the accounts allowed in, comma separated. Find yours in the issuer's \
             user detail page, or decode the `sub` of a token it already issued you.",
        ));
    }
    // Defence in depth rather than the control: a scope carried by the issuer's default role admits
    // the same stranger, which is why the subject list above is what actually decides.
    if auth.required_scopes.is_empty() {
        return Err(DomainError::validation(
            "AUTH_MODE=oidc needs OIDC_REQUIRED_SCOPES. Without one, a token minted for any \
             audience-matching purpose reaches the memory tools. Create a scope on the API \
             resource, grant it to a role only you hold, and name it here.",
        ));
    }
    Ok(())
}

/// The checks on one static bearer token. Split out so a test can reach them without assembling a
/// whole `Config`.
fn validate_static_token(client: &str, token: &str) -> Result<()> {
    // `.env.example` ships a placeholder that clears the length check below. A server that boots
    // with it accepts a credential printed in a public repository, so the placeholder is refused
    // by name before anything else is measured.
    if token.contains("CHANGE_ME") {
        return Err(DomainError::validation(format!(
            "Token for client {client} is the placeholder from .env.example. Generate one with: \
             openssl rand -hex 32"
        )));
    }
    if token.len() < 24 {
        return Err(DomainError::validation(format!(
            "Token for client {client} is shorter than 24 chars. Generate one with: openssl rand -hex 32"
        )));
    }
    Ok(())
}

fn validate(cfg: &Config) -> Result<()> {
    validate_cleanup(&cfg.cleanup)?;

    // Every mode honours static tokens, so a deployment with no token and no other credential
    // source can authenticate nobody. That is a configuration error rather than a lockout to
    // discover at the first tool call.
    let has_tokens = cfg.auth.grants.iter().any(|g| g.token.is_some());
    if !has_tokens && cfg.auth.mode == AuthMode::Token {
        return Err(DomainError::validation(
            "AUTH_MODE=token needs at least one grant with a token. Set AUTH_TOKENS.",
        ));
    }

    for g in &cfg.auth.grants {
        if let Some(t) = &g.token {
            validate_static_token(&g.client, t)?;
        }
    }

    if cfg.auth.mode == AuthMode::Oidc {
        validate_oidc(&cfg.auth)?;
    }

    if cfg.auth.mode == AuthMode::Oauth {
        // The consent screen is the only thing standing between a self-registered client and the
        // owner's memory. Without a password there is nothing to consent with.
        if cfg.oauth.owner_password_hash.is_none() {
            return Err(DomainError::validation(
                "AUTH_MODE=oauth needs OWNER_PASSWORD_HASH. Generate one with: lumberroom-server hash-password",
            ));
        }
        if cfg.oauth.cookie_secret.len() < 32 {
            return Err(DomainError::validation(
                "AUTH_MODE=oauth needs OAUTH_COOKIE_SECRET of at least 32 chars. Generate one \
                 with: openssl rand -hex 32",
            ));
        }
        if !cfg.public_url.starts_with("https://")
            && !cfg.public_url.starts_with("http://127.0.0.1")
            && !cfg.public_url.starts_with("http://localhost")
        {
            return Err(DomainError::validation(format!(
                "PUBLIC_URL must be https for a reachable deployment, got {}. Browser MCP clients \
                 reject anything else, and an owner password would cross the network in the clear.",
                cfg.public_url
            )));
        }
        if cfg.oauth.code_ttl_secs < 10 || cfg.oauth.code_ttl_secs > 600 {
            return Err(DomainError::validation(
                "OAUTH_CODE_TTL_SECS should be between 10 and 600",
            ));
        }
        // The audience check compares a token's resource against this string. A string that is not
        // an absolute URI canonicalises to nothing and therefore matches nothing, so it refuses
        // every token rather than none. Refusing to boot is the cheap version of that failure; the
        // expensive version is every client at once reporting an invalid bearer token.
        if cfg.oauth.resource_audience != ResourceAudience::Off
            && canonical_resource(&cfg.auth.resource_url).is_none()
        {
            return Err(DomainError::validation(format!(
                "OAUTH_RESOURCE_AUDIENCE is {} and the resource this server serves reads as {:?}, \
                 which is not an absolute URI without a fragment. Every token would be \
                 refused. Fix PUBLIC_URL or MCP_RESOURCE_URL, or set \
                 OAUTH_RESOURCE_AUDIENCE=off.",
                cfg.oauth.resource_audience.as_str(),
                cfg.auth.resource_url
            )));
        }
    }

    if cfg.embed.dim < 8 {
        return Err(DomainError::validation("EMBED_DIM looks wrong"));
    }

    if cfg.quality.conflict_threshold > cfg.quality.dedupe_threshold {
        return Err(DomainError::validation(
            "CONFLICT_THRESHOLD must be at or below DEDUPE_THRESHOLD, otherwise the band that \
             produces conflict candidates is empty and corrections silently collapse into the row \
             they were correcting.",
        ));
    }

    // Under a hundred rows a namespace the self-join answers in milliseconds, so a bound below
    // that only refuses stores that were never at risk.
    if cfg.quality.conflict_scan_max < 100 {
        return Err(DomainError::validation(
            "CONFLICT_SCAN_MAX must be at least 100. Below that the bound refuses namespaces the \
             conflicts query answers in milliseconds, which removes the review queue's conflict \
             source on a store that never needed protecting.",
        ));
    }

    // Zero here reads as "no limit" to somebody skimming and means "refuse every archive" to the
    // reader, which turns a bomb defence into an outage of its own.
    if cfg.quality.archive_max_decompressed_bytes == 0 {
        return Err(DomainError::validation(
            "ARCHIVE_MAX_DECOMPRESSED_BYTES must be above zero. It is the ceiling an uploaded \
             archive may decompress to, and zero refuses every import rather than lifting the \
             limit.",
        ));
    }

    // A window of zero switches the anti-loop layer off without saying so, and a negative slack
    // makes the direction test read backwards. Both are the kind of setting nobody re-reads once
    // the server starts.
    if cfg.ingest.emission_window_days < 1 {
        return Err(DomainError::validation(
            "INGEST_EMISSION_WINDOW_DAYS must be at least 1. Zero switches off the check that stops \
             the store re-ingesting what it handed out.",
        ));
    }
    if cfg.ingest.emission_slack_secs < 0.0 {
        return Err(DomainError::validation(
            "INGEST_EMISSION_SLACK_SECS cannot be negative: it is the clock difference between the \
             server and the laptop, and a negative value reverses the echo test.",
        ));
    }

    // A NaN threshold parses as a number and then loses every comparison, so `top < max_top` is
    // false for every question and the router silently never walks: a switched-off feature that
    // reports itself as switched on. Both are cosine values, so anything outside 0..=1 is a
    // threshold that can never be crossed in one direction or is always crossed in the other.
    for (name, value) in [
        ("GRAPH_ROUTE_MAX_TOP", cfg.search.graph_route.max_top),
        ("GRAPH_ROUTE_MAX_SPREAD", cfg.search.graph_route.max_spread),
    ] {
        if !(value.is_finite() && (0.0..=1.0).contains(&value)) {
            return Err(DomainError::validation(format!(
                "{name} must be a finite number between 0 and 1, got {value}. It is compared \
                 against cosine scores."
            )));
        }
    }

    // `k` divides every rank-fused score. Zero puts a rank-1 row at infinity, a negative k flips
    // the ordering, and `SEARCH_RRF_K=NaN` parses as a number and then sorts above every real
    // score in Postgres, which leaves search returning rows in insertion order with no error.
    if !(cfg.search.rrf_k.is_finite() && cfg.search.rrf_k > 0.0) {
        return Err(DomainError::validation(format!(
            "SEARCH_RRF_K must be a finite number above zero, got {}. The standard value is {}.",
            cfg.search.rrf_k, DEFAULT_RRF_K
        )));
    }

    check_min_occurred_age(cfg.policy.write_min_occurred_age_secs)?;

    if cfg.policy.max_write_sensitivity == Sensitivity::Sealed {
        return Err(DomainError::validation(
            "MAX_WRITE_SENSITIVITY cannot be sealed. Sealed content is encrypted by the client and \
             never arrives as a plaintext tool argument.",
        ));
    }

    Ok(())
}

/// The one line an operator needs to know what their store classifies as private, and the warning
/// that follows from it.
///
/// Called after the effective table is settled, not from `validate`: at load time the only rules in
/// hand are whatever the environment supplied, which on a default install is nothing, so the check
/// looked at an empty set and never fired.
pub fn log_effective_policy(cfg: &Config, source: &str) {
    let rules = cfg
        .policy
        .defaults
        .rules()
        .iter()
        .map(|(pattern, level)| format!("{pattern}={level}"))
        .collect::<Vec<_>>()
        .join(",");
    tracing::info!(source, rules = %rules, "classification table");

    if cfg.crypto.provider == KekProvider::None
        && cfg.policy.defaults.rules().iter().any(|(_, l)| *l == Sensitivity::Private)
    {
        tracing::warn!(
            "a namespace defaults to private but KEK_PROVIDER=none, so writes to it will be \
             refused rather than stored unencrypted"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cleanup settings that pass, so each test below changes exactly one thing.
    fn valid() -> CleanupConfig {
        CleanupConfig { interval_secs: 3600, namespace: None, limit: 500 }
    }

    #[test]
    fn a_cleanup_interval_under_a_minute_is_refused() {
        // A pass every few seconds walks the store faster than it changes and writes nothing new
        // each time. Refused at boot rather than discovered in a log a week later.
        let mut c = valid();
        c.interval_secs = 59;
        let e = validate_cleanup(&c).unwrap_err();
        assert!(e.client_message().contains("CLEANUP_INTERVAL_SECS"), "{}", e.client_message());
        assert!(
            e.client_message().contains("Use 0"),
            "the refusal should name the way to turn it off: {}",
            e.client_message()
        );
    }

    #[test]
    fn zero_turns_the_cleanup_pass_off_and_is_not_an_error() {
        let mut c = valid();
        c.interval_secs = 0;
        assert!(validate_cleanup(&c).is_ok());
    }

    #[test]
    fn a_minute_and_an_hour_are_both_accepted() {
        for secs in [60u64, 3600] {
            let mut c = valid();
            c.interval_secs = secs;
            assert!(validate_cleanup(&c).is_ok(), "{secs} should be accepted");
        }
    }

    #[test]
    fn a_cleanup_limit_of_zero_is_refused() {
        // Zero reads nothing and reports a clean store, which is the one answer this system must
        // never give about content it holds.
        let mut c = valid();
        c.limit = 0;
        assert!(validate_cleanup(&c).is_err());
    }

    /// The grant line `.env.example` ships, token included. Copying the file to `.env` without
    /// editing it must fail at boot rather than run with a credential anyone can read.
    const EXAMPLE_GRANT: &str = r#"[{"client":"claude-code-mac","token":"CHANGE_ME_openssl_rand_hex_32","read":[{"namespace":"*","max":"sealed"}],"write":[{"namespace":"*","max":"sealed"}],"sealedCapable":true,"mayDelete":false,"mayIngest":false,"registryWrite":true}]"#;

    #[test]
    fn the_placeholder_token_from_env_example_does_not_boot() {
        let g = parse_grants(EXAMPLE_GRANT, "").unwrap();
        let token = g[0].token.as_deref().unwrap();
        assert!(token.len() >= 24, "the placeholder clears the length check, so it needs its own");
        let e = validate_static_token(&g[0].client, token).unwrap_err();
        assert!(e.client_message().contains("placeholder"), "{}", e.client_message());
        assert!(
            e.client_message().contains("openssl rand -hex 32"),
            "the refusal should name the fix: {}",
            e.client_message()
        );
    }

    #[test]
    fn a_generated_hex_token_passes() {
        let token = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert_eq!(token.len(), 64);
        assert!(validate_static_token("claude-code-mac", token).is_ok());
    }

    #[test]
    fn a_short_token_is_still_refused() {
        let e = validate_static_token("c", "tooshort").unwrap_err();
        assert!(e.client_message().contains("shorter than 24"), "{}", e.client_message());
    }

    #[test]
    fn reads_compact_client_token_pairs() {
        let g =
            parse_grants("mac:abcdefghijklmnopqrstuvwxyz,browser:abcdefghijklmnopqrstuvwxyz", "")
                .unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].client, "mac");
        assert_eq!(g[0].read_grants(), NamespaceGrant::everything());
    }

    #[test]
    fn reads_json_with_per_client_grants() {
        let g = parse_grants(
            r#"[{"client":"browser","token":"t","read":["user:me","global"],"write":["global"]}]"#,
            "",
        )
        .unwrap();
        assert_eq!(g[0].read_grants().len(), 2);
        assert_eq!(g[0].read_grants()[0].max, Sensitivity::Open);
        assert_eq!(g[0].write_grants()[0].namespace, "global");
    }

    #[test]
    fn reads_a_grant_carrying_sensitivity_ceilings() {
        let g = parse_grants(
            r#"[{"client":"mac","token":"t","read":[{"namespace":"*","max":"sealed"}],"write":[{"namespace":"user:me","max":"private"}],"sealedCapable":true,"mayDelete":true}]"#,
            "",
        )
        .unwrap();
        assert_eq!(g[0].read_grants()[0].max, Sensitivity::Sealed);
        assert_eq!(g[0].write_grants()[0].max, Sensitivity::Private);
        assert!(g[0].sealed_capable);
        assert!(g[0].may_delete);
    }

    #[test]
    fn an_absent_grant_list_means_unrestricted_at_every_level() {
        let g = parse_grants(r#"[{"client":"c","token":"t"}]"#, "").unwrap();
        assert_eq!(g[0].read_grants(), NamespaceGrant::everything());
        assert!(g[0].effective_sealed_capable());
    }

    #[test]
    fn an_explicitly_empty_list_means_no_access() {
        let g = parse_grants(r#"[{"client":"c","token":"t","read":[],"write":[]}]"#, "").unwrap();
        assert!(g[0].read_grants().is_empty(), "empty must not widen to full access");
        assert!(g[0].write_grants().is_empty());
    }

    #[test]
    fn a_phase_one_grant_stays_valid_and_gains_nothing() {
        let g =
            parse_grants(r#"[{"client":"chatgpt","token":"t","read":["user:me"]}]"#, "").unwrap();
        assert_eq!(
            g[0].read_grants()[0].max,
            Sensitivity::Open,
            "a grant written before the sensitivity axis must not silently gain access"
        );
        assert!(!g[0].effective_sealed_capable());
    }

    #[test]
    fn withholds_registry_write_and_delete_unless_asked_for() {
        let g = parse_grants(r#"[{"client":"c","token":"t"}]"#, "").unwrap();
        assert!(!g[0].registry_write);
        assert!(!g[0].may_delete);
    }

    #[test]
    fn rejects_a_malformed_pair() {
        assert!(parse_grants("nocolon", "").is_err());
    }

    #[test]
    fn rejects_json_without_a_client() {
        assert!(parse_grants(r#"[{"token":"t"}]"#, "").is_err());
    }

    #[test]
    fn reads_per_client_digest_budgets_in_both_forms() {
        let b = parse_client_budgets("chatgpt=3000, claude-code-mac=20000").unwrap();
        assert_eq!(b.get("chatgpt"), Some(&3000));
        let b = parse_client_budgets(r#"{"chatgpt":3000}"#).unwrap();
        assert_eq!(b.get("chatgpt"), Some(&3000));
    }

    #[test]
    fn falls_back_to_the_global_digest_budget_for_an_unlisted_client() {
        let cfg = BootstrapConfig {
            cache_ms: 0,
            profile_limit: 1,
            project_limit: 1,
            recent_limit: 1,
            recent_days: 1,
            registry_limit: 1,
            max_chars: 6000,
            max_chars_by_client: parse_client_budgets("chatgpt=3000").unwrap(),
        };
        assert_eq!(cfg.budget_for("chatgpt"), 3000);
        assert_eq!(cfg.budget_for("hermes"), 6000);
    }

    #[test]
    fn reads_sensitivity_defaults_from_a_pair_list() {
        let d = parse_sensitivity_defaults("*=open,credentials:*=sealed").unwrap();
        assert_eq!(d.for_namespace("credentials:aws"), Sensitivity::Sealed);
        assert_eq!(d.for_namespace("global"), Sensitivity::Open);
    }

    /// The regression the whole precedence function exists for. `SENSITIVITY_DEFAULTS` unset used to
    /// produce an EMPTY rule set, which classifies every namespace open and says nothing about it,
    /// and the `sensitivity_default` table migration 004 seeds was never read at all.
    #[test]
    fn an_empty_rule_set_is_never_the_effective_policy() {
        let (rules, source) = resolve_sensitivity_defaults(None, vec![]);
        assert!(!rules.is_empty(), "an empty table silently classifies everything open");
        assert_eq!(source, "built-in seeded defaults");
        assert_eq!(rules.for_namespace("personal:finance"), Sensitivity::Private);
        assert_eq!(rules.for_namespace("credentials:aws"), Sensitivity::Sealed);
    }

    #[test]
    fn the_database_table_wins_when_the_environment_says_nothing() {
        let table = vec![("project:vault".to_string(), Sensitivity::Private)];
        let (rules, source) = resolve_sensitivity_defaults(None, table);
        assert_eq!(source, "sensitivity_default table");
        assert_eq!(rules.for_namespace("project:vault"), Sensitivity::Private);
    }

    #[test]
    fn the_environment_wins_over_the_table_because_setting_it_means_it() {
        let from_env = parse_sensitivity_defaults("project:vault=open").unwrap();
        let table = vec![("project:vault".to_string(), Sensitivity::Sealed)];
        let (rules, source) = resolve_sensitivity_defaults(Some(from_env), table);
        assert_eq!(source, "SENSITIVITY_DEFAULTS");
        assert_eq!(rules.for_namespace("project:vault"), Sensitivity::Open);
    }

    /// docker-compose.yml passes `SENSITIVITY_DEFAULTS: ${SENSITIVITY_DEFAULTS:-}`, so the variable
    /// is present and empty in every compose deployment. Letting that take the environment branch
    /// would reinstate the empty rule set this resolution exists to prevent.
    #[test]
    fn an_environment_variable_set_to_nothing_does_not_beat_the_table() {
        let from_env = parse_sensitivity_defaults("").unwrap();
        let table = vec![("personal:health".to_string(), Sensitivity::Private)];
        let (rules, source) = resolve_sensitivity_defaults(Some(from_env), table);
        assert_eq!(source, "sensitivity_default table");
        assert_eq!(rules.for_namespace("personal:health"), Sensitivity::Private);
    }

    #[test]
    fn rejects_an_unknown_sensitivity_level_rather_than_defaulting_it_open() {
        assert!(parse_sensitivity_defaults("global=secret").is_err());
    }

    /// An unset or empty `SEARCH_FUSION` has to leave the shipped ranking alone. Every existing
    /// deployment is in that state.
    #[test]
    fn the_blend_stays_linear_until_an_operator_asks_for_rank_fusion() {
        assert_eq!(parse_fusion("linear").unwrap(), Fusion::Linear);
        assert_eq!(parse_fusion("").unwrap(), Fusion::Linear);
        assert_eq!(parse_fusion("rrf").unwrap(), Fusion::Rrf);
        assert_eq!(parse_fusion(" rrf ").unwrap(), Fusion::Rrf);
    }

    /// A typo must stop the boot rather than pick a ranking. Silently falling back to linear would
    /// make an operator's rank-fusion rollout look like a result.
    #[test]
    fn refuses_a_fusion_it_does_not_implement_and_names_both_spellings() {
        let err = parse_fusion("reciprocal").unwrap_err().to_string();
        assert!(err.contains("linear|rrf"), "the message has to name what to write: {err}");
        assert!(parse_fusion("RRF").is_err(), "the other enums here match lowercase only");
    }

    #[test]
    fn the_audience_check_is_lenient_until_an_operator_asks_for_more() {
        assert_eq!(parse_resource_audience("").unwrap(), ResourceAudience::Lenient);
        assert_eq!(parse_resource_audience("lenient").unwrap(), ResourceAudience::Lenient);
        assert_eq!(parse_resource_audience(" strict ").unwrap(), ResourceAudience::Strict);
        assert_eq!(parse_resource_audience("off").unwrap(), ResourceAudience::Off);
    }

    /// An unrecognised value must not fall back to a default. A typo that silently reads as
    /// `lenient` is an operator who believes the audience is enforced strictly and is wrong.
    #[test]
    fn refuses_an_audience_setting_it_does_not_implement_and_names_all_three() {
        let err = parse_resource_audience("enforced").unwrap_err().to_string();
        assert!(err.contains("off"), "{err}");
        assert!(err.contains("lenient"), "{err}");
        assert!(err.contains("strict"), "{err}");
        assert!(
            parse_resource_audience("Strict").is_err(),
            "the other enums here match lowercase only"
        );
    }

    #[test]
    fn the_audience_names_round_trip() {
        for a in [ResourceAudience::Off, ResourceAudience::Lenient, ResourceAudience::Strict] {
            assert_eq!(parse_resource_audience(a.as_str()).unwrap(), a);
        }
    }

    #[test]
    fn the_fusion_names_round_trip() {
        assert_eq!(parse_fusion(Fusion::Linear.as_str()).unwrap(), Fusion::Linear);
        assert_eq!(parse_fusion(Fusion::Rrf.as_str()).unwrap(), Fusion::Rrf);
    }

    /// The shipped default has to pass its own check, and a day is the number the tool description
    /// and the runbook both quote.
    #[test]
    fn the_default_occurred_age_fence_is_one_day_and_boots() {
        assert_eq!(DEFAULT_MIN_OCCURRED_AGE_SECS, 86_400);
        assert!(check_min_occurred_age(DEFAULT_MIN_OCCURRED_AGE_SECS).is_ok());
        assert!(check_min_occurred_age(1).is_ok(), "a one second window is an operator's escape");
        assert!(check_min_occurred_age(MAX_MIN_OCCURRED_AGE_SECS).is_ok());
    }

    #[test]
    fn refuses_a_fence_of_zero_rather_than_accepting_a_valid_time_of_now() {
        let err = check_min_occurred_age(0).unwrap_err().to_string();
        assert!(err.contains("at least 1"), "the message has to say what to write: {err}");
    }

    /// The refusal that costs data if it is missing. A fence wider than the dates people state
    /// turns "since March" into a refusal telling the caller to drop the date.
    #[test]
    fn refuses_a_fence_wider_than_a_year() {
        assert!(check_min_occurred_age(MAX_MIN_OCCURRED_AGE_SECS + 1).is_err());
        let err = check_min_occurred_age(10 * MAX_MIN_OCCURRED_AGE_SECS).unwrap_err().to_string();
        assert!(err.contains("one year"), "{err}");
    }

    /// An oidc deployment that boots, so each test below changes exactly one thing.
    fn oidc_auth() -> AuthConfig {
        AuthConfig {
            mode: AuthMode::Oidc,
            resource_url: "https://memory.example.com/mcp".into(),
            grants: vec![ClientGrant {
                client: "app-id".into(),
                token: None,
                read: None,
                write: None,
                registry_write: false,
                sealed_capable: false,
                may_delete: false,
                may_ingest: false,
                may_read_history: false,
            }],
            issuer: "https://auth.example.com/oidc".into(),
            audience: "https://memory.example.com/mcp".into(),
            jwks_uri: "https://auth.example.com/oidc/jwks".into(),
            required_scopes: vec!["memory.rw".into()],
            client_claims: vec!["client_id".into()],
            allowed_subjects: vec!["usr_1a2b3c".into()],
        }
    }

    #[test]
    fn the_shipped_oidc_shape_boots() {
        assert!(validate_oidc(&oidc_auth()).is_ok());
    }

    /// The flaw this refusal exists for: a grant is keyed on the client id, and a client id is
    /// public. Without a subject list, anyone who can sign up at the issuer holds the owner's grant.
    #[test]
    fn oidc_mode_refuses_to_boot_without_a_subject_allowlist() {
        let mut auth = oidc_auth();
        auth.allowed_subjects = vec![];
        let err = validate_oidc(&auth).unwrap_err();
        let msg = err.client_message();
        assert!(
            msg.contains("OIDC_ALLOWED_SUBJECTS"),
            "the refusal has to name the setting: {msg}"
        );
        assert!(msg.contains("sub"), "and where to find the value: {msg}");
    }

    #[test]
    fn oidc_mode_refuses_to_boot_without_a_required_scope() {
        let mut auth = oidc_auth();
        auth.required_scopes = vec![];
        let err = validate_oidc(&auth).unwrap_err();
        assert!(err.client_message().contains("OIDC_REQUIRED_SCOPES"));
    }

    #[test]
    fn oidc_mode_still_refuses_an_empty_grant_list_and_a_missing_issuer() {
        let mut no_grants = oidc_auth();
        no_grants.grants = vec![];
        assert!(validate_oidc(&no_grants).is_err());

        let mut no_issuer = oidc_auth();
        no_issuer.issuer = String::new();
        assert!(validate_oidc(&no_issuer).is_err());

        let mut no_jwks = oidc_auth();
        no_jwks.jwks_uri = String::new();
        assert!(validate_oidc(&no_jwks).is_err());
    }
}
