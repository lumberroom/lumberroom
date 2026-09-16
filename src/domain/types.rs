//! Domain types. No I/O, and nothing here imports an adapter or a transport.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::policy::NamespaceGrant;

/// Ordered: a grant admits everything at or below its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Open,
    Private,
    Sealed,
}

impl Default for Sensitivity {
    fn default() -> Self {
        Self::Open
    }
}

impl Sensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Private => "private",
            Self::Sealed => "sealed",
        }
    }

    /// Unknown input is not silently treated as `open`: a level the server does not understand is
    /// a configuration error, and defaulting it downward is how a private fact becomes a public one.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "open" => Some(Self::Open),
            "private" => Some(Self::Private),
            "sealed" => Some(Self::Sealed),
            _ => None,
        }
    }

    /// Whether content at this level is stored encrypted. `sealed` lives in its own table and never
    /// reaches this path, so only `private` answers true.
    pub fn is_encrypted(self) -> bool {
        matches!(self, Self::Private)
    }

    /// Whether content at this level may enter the lexical index. A Postgres tsvector is not an
    /// index over the document, it is the document, stemmed, so anything above open stays out.
    pub fn is_lexically_indexed(self) -> bool {
        matches!(self, Self::Open)
    }
}

impl std::fmt::Display for Sensitivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_client: String,
    pub embedding_model: Option<String>,
    pub sensitivity: Sensitivity,
    pub supersedes: Option<String>,
    /// When the fact became true in the world, and when it stopped. Valid time, as distinct from
    /// `created_at`, which is when this store learned it. Both None on a fact with no known date,
    /// which is most of them: a standing preference has no start, and forcing one would be a lie.
    ///
    /// Half-open, [occurred_at, occurred_until). A fact with `occurred_until = T` did not hold at
    /// `T`, which is what lets a predecessor and its successor tile the timeline once rather than
    /// both answering a point query at the changeover.
    /// Skipped when absent. The published wire contract is pinned by a test that counts keys, and
    /// most rows carry no date, so serialising two nulls on every row would change every payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_until: Option<DateTime<Utc>>,
    /// Set when a later write replaced this row. Live reads filter on it being absent.
    pub superseded_by: Option<String>,
    pub superseded_at: Option<DateTime<Utc>>,
    /// Ageing signals. A fact retrieved often is more likely to be the one wanted; a fact never
    /// retrieved in a year probably is not.
    pub access_count: i32,
    pub last_accessed_at: Option<DateTime<Utc>>,
    /// Set when a write restates this fact rather than contradicting it. Repetition is confirmation.
    pub last_confirmed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Memory {
    /// Does this fact hold now? Both clocks, since decision 0017.
    ///
    /// The period half is what stops an expired row being named as the successor in a supersession,
    /// which would retire a live fact into one no live read returns. Distinct from the `expired`
    /// state the console prints: a row a supersession retired does not hold now either, and it is
    /// retired rather than expired.
    pub fn is_live(&self) -> bool {
        self.superseded_by.is_none() && self.occurred_until.is_none_or(|until| until > Utc::now())
    }

    /// Did this fact's period end with nothing replacing it? Decision 0017's state, read off a row.
    pub fn is_expired(&self) -> bool {
        expired(self.superseded_at, self.occurred_until, Utc::now())
    }
}

/// Decision 0017's definition of expired, in one place and with the clock passed in.
///
/// Both halves matter. A row carrying `superseded_at` was retired by a supersession, even where a
/// restore lost the link, so it keeps its replace form. An end still ahead of now has not arrived,
/// so the fact holds and takes a successor like any other row does.
///
/// The service door reads this and `RETIRE_PREDECESSOR_SQL` spells the same test inside the
/// statement. The two disagreed for one release and a row with a future end fell through the gap:
/// the service accepted it, the UPDATE matched nothing, and the caller was told a supersession had
/// happened.
pub fn expired(
    superseded_at: Option<DateTime<Utc>>,
    occurred_until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    superseded_at.is_none() && occurred_until.is_some_and(|until| until <= now)
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub memory: Memory,
    pub score: f64,
    pub similarity: f64,
    /// False when the row came from outside the primary namespace set.
    pub primary: bool,
}

/// A live row close enough to a new write to be worth showing the caller.
///
/// Supersession only works if a model chooses to supersede rather than write afresh, and models
/// overwhelmingly write afresh. Handing back the row it probably meant to replace is the mechanism
/// that makes the choice available at the moment it matters.
#[derive(Debug, Clone, Serialize)]
pub struct ConflictCandidate {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub similarity: f64,
}

/// What `memory_write` answers. The published wire contract is snake_case and pinned by a test: a
/// rename on the domain side once turned every latency into "-ms" with nothing failing.
#[derive(Debug, Clone, Serialize)]
pub struct WriteOutcome {
    pub id: String,
    pub namespace: String,
    pub sensitivity: Sensitivity,
    /// True when the content already existed and no new row was created.
    pub deduplicated: bool,
    /// Set when this write retired an earlier row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded: Option<String>,
    /// The row this write retired kept an open end, so it still reads as holding at every instant.
    /// Two facts dated the same day do this, and a dump gives every line in a day the same date.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub end_left_open: bool,
    /// Live rows near enough to be the fact this write should have replaced.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub possible_conflicts: Vec<ConflictCandidate>,
}

/// The half fuzzy memory cannot answer: how do you know that?
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Provenance {
    pub source_client: String,
    pub conv_id: Option<String>,
    pub confidence: f64,
    pub user_confirmed: bool,
    pub valid_from: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryEntry {
    pub namespace: String,
    pub kind: String,
    pub key: String,
    pub value: serde_json::Value,
    pub provenance: Provenance,
    pub sensitivity: Sensitivity,
    pub version: i32,
    /// Set when the key asked for was an alias. The caller is told which key answered, so a
    /// redirect is visible rather than silent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_from: Option<String>,
}

/// A blob this server cannot read. Keyed by an HMAC of the canonical name computed client-side, so
/// the server cannot enumerate what is stored either.
#[derive(Debug, Clone, Serialize)]
pub struct SealedItem {
    pub namespace: String,
    pub key_hmac: String,
    /// Base64. Opaque here by construction.
    pub ciphertext: String,
    pub alg: String,
    pub source_client: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub client: String,
    pub tool: String,
    pub succeeded: bool,
    /// True when the model chose to call, false when a hook or the operator forced it.
    pub unprompted: bool,
    pub latency_ms: i32,
    /// Correlates the calls one surface made inside one conversation. Phase 2 needs it to answer
    /// "did this surface read before it answered", which per-call rows alone cannot.
    pub session_id: Option<String>,
    pub namespace: Option<String>,
}

/// Who is calling, and what they may reach. Every auth mode produces this and nothing else, which
/// is what lets a per-client denial land without touching the authorization path.
#[derive(Debug, Clone)]
pub struct Principal {
    pub client: String,
    /// Short non-reversible fingerprint, safe to log.
    pub token_id: String,
    pub mode: &'static str,
    pub scopes: Vec<String>,
    /// Namespace globs with a sensitivity ceiling each.
    pub read: Vec<NamespaceGrant>,
    pub write: Vec<NamespaceGrant>,
    /// Registry writes are an operator action, off unless granted.
    pub registry_write: bool,
    /// A property of the client, not of the grant: it asserts the client can decrypt locally. A
    /// client without it may hold a sealed ceiling and still only ever receive ciphertext.
    pub sealed_capable: bool,
    /// A model that can silently delete memories is a worse failure than one that hoards them, so
    /// the delete tool is opt-in per client rather than available by default.
    pub may_delete: bool,
    /// Whether this client may reach the ingest routes at all. Off by default for the same reason
    /// `may_delete` is: a client that can post proposals can fill the queue, and a queue the owner
    /// stops reading approves nothing.
    pub may_ingest: bool,
    /// Whether this client may read facts that no longer hold. Off by default, for the same reason
    /// `may_delete` is: a retired fact can be more revealing than the one that replaced it, and an
    /// old credential location is exactly the shape that gets superseded rather than deleted. A
    /// grant over live rows is not a grant over the history behind them.
    pub may_read_history: bool,
}

impl Principal {
    /// The read globs alone, for paths that reason about namespaces only.
    pub fn read_patterns(&self) -> Vec<String> {
        crate::domain::policy::patterns(&self.read)
    }

    pub fn write_patterns(&self) -> Vec<String> {
        crate::domain::policy::patterns(&self.write)
    }

    /// A principal with no grant at all. Useful in tests and as the shape a denial produces.
    pub fn empty(client: &str) -> Self {
        Self {
            client: client.to_string(),
            token_id: String::new(),
            mode: "none",
            scopes: vec![],
            read: vec![],
            write: vec![],
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
        }
    }
}

/// Where a call came from. Anything without an explicit header is the model deciding on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invocation {
    Model,
    Hook,
    Cli,
    User,
}

impl Invocation {
    pub fn parse(header: Option<&str>) -> Self {
        match header.map(|h| h.trim().to_ascii_lowercase()).as_deref() {
            Some("hook") => Self::Hook,
            Some("cli") => Self::Cli,
            Some("user") => Self::User,
            _ => Self::Model,
        }
    }

    /// PRD §7: the flag that matters. Only the model deciding counts as unprompted.
    pub fn is_unprompted(self) -> bool {
        matches!(self, Self::Model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusts_the_three_declared_non_model_callers() {
        assert_eq!(Invocation::parse(Some("hook")), Invocation::Hook);
        assert_eq!(Invocation::parse(Some("cli")), Invocation::Cli);
        assert_eq!(Invocation::parse(Some("user")), Invocation::User);
    }

    #[test]
    fn normalises_case_and_whitespace() {
        assert_eq!(Invocation::parse(Some(" HOOK ")), Invocation::Hook);
    }

    #[test]
    fn treats_missing_or_unknown_as_the_model_deciding() {
        assert_eq!(Invocation::parse(None), Invocation::Model);
        assert_eq!(Invocation::parse(Some("")), Invocation::Model);
        assert_eq!(Invocation::parse(Some("something-else")), Invocation::Model);
    }

    #[test]
    fn counts_only_model_calls_as_unprompted() {
        assert!(Invocation::parse(None).is_unprompted());
        assert!(!Invocation::parse(Some("hook")).is_unprompted());
    }

    #[test]
    fn sensitivity_orders_open_below_private_below_sealed() {
        assert!(Sensitivity::Open < Sensitivity::Private);
        assert!(Sensitivity::Private < Sensitivity::Sealed);
    }

    #[test]
    fn sensitivity_round_trips_through_its_wire_form() {
        for level in [Sensitivity::Open, Sensitivity::Private, Sensitivity::Sealed] {
            assert_eq!(Sensitivity::parse(level.as_str()), Some(level));
        }
    }

    #[test]
    fn an_unrecognised_level_is_an_error_rather_than_open() {
        assert_eq!(Sensitivity::parse("secret"), None);
        assert_eq!(Sensitivity::parse(""), None);
    }

    #[test]
    fn only_open_content_reaches_the_lexical_index() {
        assert!(Sensitivity::Open.is_lexically_indexed());
        assert!(!Sensitivity::Private.is_lexically_indexed());
        assert!(!Sensitivity::Sealed.is_lexically_indexed());
    }

    #[test]
    fn an_empty_principal_can_reach_nothing() {
        let p = Principal::empty("nobody");
        assert!(p.read_patterns().is_empty());
        assert!(p.write_patterns().is_empty());
        assert!(!p.registry_write);
        assert!(!p.may_delete);
        assert!(!p.sealed_capable);
    }
}
