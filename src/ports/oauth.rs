//! Storage for the built-in authorization server (decision 0002).
//!
//! Access tokens are opaque and stored hashed, so the two operations that matter are a hash lookup
//! on every request and a single UPDATE to revoke. Codes and refresh tokens are single use, and the
//! two "consume" operations below must be atomic: a replayed code or a replayed refresh token is
//! the signal that a credential leaked, and detecting it requires the check and the mark to be one
//! statement rather than a read followed by a write.

use async_trait::async_trait;

use crate::domain::errors::Result;
use crate::domain::policy::NamespaceGrant;

#[derive(Debug, Clone)]
pub struct NewOauthClient {
    pub client_id: String,
    pub secret_hash: Option<String>,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub software_id: Option<String>,
    pub software_version: Option<String>,
    /// "dcr" for RFC 7591 self-registration, "manual" for a credential the owner issued.
    pub registered_via: String,
}

#[derive(Debug, Clone)]
pub struct OauthClientRecord {
    pub client_id: String,
    pub secret_hash: Option<String>,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub registered_via: String,
    /// Self-declared at registration, so it is a hint and never an identity. The consent screen
    /// shows it because the owner deciding whether to trust a client wants everything the client
    /// claimed about itself, including the parts that could be a lie.
    pub software_id: Option<String>,
    pub read: Vec<NamespaceGrant>,
    pub write: Vec<NamespaceGrant>,
    pub registry_write: bool,
    pub sealed_capable: bool,
    pub may_delete: bool,
    /// Whether this client may reach the ingest routes. Off unless the owner granted it, so a
    /// client consented to before the capability existed keeps exactly the reach it had.
    pub may_ingest: bool,
    pub may_read_history: bool,
    /// None until the owner has approved this client. Registration is not authorization: a
    /// self-registered client exists and holds nothing.
    pub consented_at: Option<chrono::DateTime<chrono::Utc>>,
    pub profile: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl OauthClientRecord {
    pub fn is_live(&self) -> bool {
        self.revoked_at.is_none()
    }

    pub fn has_consent(&self) -> bool {
        self.consented_at.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct NewAuthCode {
    pub code_hash: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scope: String,
    pub resource: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// The result of trying to spend a code. Distinguishing "already spent" from "unknown" matters:
/// the first means a code leaked and every token issued from it has to die.
#[derive(Debug, Clone)]
pub enum CodeOutcome {
    Fresh(NewAuthCode),
    AlreadyConsumed { client_id: String },
    Expired,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct NewAccessToken {
    pub token_hash: String,
    pub client_id: String,
    pub scope: String,
    pub resource: Option<String>,
    pub family_id: uuid::Uuid,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct AccessTokenRecord {
    pub token_hash: String,
    pub client_id: String,
    pub scope: String,
    pub resource: Option<String>,
    pub family_id: uuid::Uuid,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone)]
pub struct NewRefreshToken {
    pub token_hash: String,
    pub client_id: String,
    pub family_id: uuid::Uuid,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub enum RefreshOutcome {
    /// Consumed successfully. Issue a successor in the same family.
    Rotated {
        client_id: String,
        family_id: uuid::Uuid,
    },
    /// Presented after it was already spent, which means it leaked. The caller must kill the family.
    Replayed {
        family_id: uuid::Uuid,
    },
    Expired,
    Revoked,
    Unknown,
}

/// The grant the owner assigns at the consent screen, applied without a restart.
#[derive(Debug, Clone)]
pub struct ClientGrantUpdate {
    pub profile: Option<String>,
    pub read: Vec<NamespaceGrant>,
    pub write: Vec<NamespaceGrant>,
    pub registry_write: bool,
    pub sealed_capable: bool,
    pub may_delete: bool,
    pub may_ingest: bool,
    pub may_read_history: bool,
}

#[async_trait]
pub trait OauthStore: Send + Sync {
    async fn register_client(&self, c: NewOauthClient) -> Result<()>;
    async fn find_client(&self, client_id: &str) -> Result<Option<OauthClientRecord>>;
    async fn list_clients(&self, include_revoked: bool) -> Result<Vec<OauthClientRecord>>;
    /// Records consent and the grant in one write, so a consented client always has a grant.
    async fn set_client_grant(&self, client_id: &str, g: ClientGrantUpdate) -> Result<()>;
    /// Revokes the client and every token it holds.
    async fn revoke_client(&self, client_id: &str) -> Result<bool>;
    fn touch_client(&self, client_id: &str);

    async fn insert_code(&self, c: NewAuthCode) -> Result<()>;
    /// Atomic: marks the code consumed and returns what it was, in one statement.
    async fn consume_code(&self, code_hash: &str) -> Result<CodeOutcome>;

    async fn insert_token(&self, t: NewAccessToken) -> Result<()>;
    /// The hot path. One indexed lookup per authenticated request.
    async fn find_token(&self, token_hash: &str) -> Result<Option<AccessTokenRecord>>;
    async fn revoke_token(&self, token_hash: &str) -> Result<bool>;

    async fn insert_refresh(&self, r: NewRefreshToken) -> Result<()>;
    /// Atomic, for the same reason as `consume_code`.
    async fn rotate_refresh(&self, token_hash: &str) -> Result<RefreshOutcome>;
    /// The client a live refresh token was issued to, read without spending it. `None` for a token
    /// that is unknown, spent, revoked or expired. `rotate` asks this first so a caller presenting
    /// another client's live token is refused before the spend; checking afterwards burned the
    /// legitimate client's token. A spent token answers `None` so it still reaches
    /// `rotate_refresh`, whose replay verdict revokes the family whoever presented it.
    async fn refresh_owner(&self, token_hash: &str) -> Result<Option<String>>;
    /// Kills every access and refresh token descended from one authorization.
    async fn revoke_family(&self, family_id: uuid::Uuid) -> Result<()>;

    /// Expired codes and tokens accumulate forever otherwise. Called on a timer, not on the path.
    async fn purge_expired(&self) -> Result<u64>;
}
