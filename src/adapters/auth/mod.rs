//! Authentication. Every mode resolves to one `Principal` and nothing else, which is what lets a
//! per-client namespace denial land without touching the authorization path.
//!
//! The modes compose. Static bearer tokens are honoured whenever they are configured, and the
//! selected mode adds a second credential type on top: the built-in authorization server, or an
//! external issuer's JWTs. Making them exclusive would have meant that switching a deployment to
//! OAuth broke the `lumberroom` CLI and the three surfaces that only ever send a bearer header.

mod chain;
mod metadata;
mod oidc;
mod opaque;
mod token;

pub use chain::ChainAuthenticator;
pub use metadata::{authorization_server_metadata, protected_resource_metadata, www_authenticate};
pub use oidc::OidcAuthenticator;
pub use opaque::OpaqueTokenAuthenticator;
pub use token::{fingerprint, TokenAuthenticator};

use async_trait::async_trait;
use std::sync::Arc;

use crate::config::{AuthMode, Config};
use crate::domain::errors::{DomainError, Result};
use crate::domain::policy::{self, NamespaceCeiling};
use crate::domain::types::{Principal, Sensitivity};
use crate::ports::OauthStore;

#[async_trait]
pub trait Authenticator: Send + Sync {
    fn mode(&self) -> &'static str;
    async fn authenticate(&self, authorization: Option<&str>) -> Result<Principal>;
}

/// Build the chain for this configuration. Static tokens go first because they are the cheapest
/// check and the most common caller; the mode-specific authenticator follows.
pub fn create(cfg: &Config, oauth: Option<Arc<dyn OauthStore>>) -> Result<Arc<dyn Authenticator>> {
    let mut links: Vec<Arc<dyn Authenticator>> = Vec::new();

    if cfg.auth.grants.iter().any(|g| g.token.is_some()) {
        links.push(Arc::new(TokenAuthenticator::new(&cfg.auth.grants)));
    }

    match cfg.auth.mode {
        AuthMode::Token => {}
        AuthMode::Oauth => {
            let store = oauth.ok_or_else(|| {
                DomainError::internal("AUTH_MODE=oauth needs the authorization server store")
            })?;
            links.push(Arc::new(OpaqueTokenAuthenticator::new(
                store,
                &cfg.auth.resource_url,
                cfg.oauth.resource_audience,
            )));
        }
        AuthMode::Oidc => links.push(Arc::new(OidcAuthenticator::new(cfg)?)),
    }

    if links.is_empty() {
        return Err(DomainError::validation(
            "no credential source configured: set AUTH_TOKENS, or AUTH_MODE=oauth|oidc",
        ));
    }

    Ok(Arc::new(ChainAuthenticator::new(links, cfg.auth.mode.as_str())))
}

/// Reads narrow silently to the intersection of asked-for and granted, carrying each ceiling
/// through so the sensitivity filter runs in the query.
pub fn filter_readable(principal: &Principal, requested: &[String]) -> Vec<NamespaceCeiling> {
    policy::resolve(&principal.read, requested)
}

/// The readable namespaces alone, for the paths that only need names.
pub fn readable_names(principal: &Principal, requested: &[String]) -> Vec<String> {
    filter_readable(principal, requested).into_iter().map(|c| c.namespace).collect()
}

/// Writes fail loudly. A silently dropped write looks like a memory that forgets.
pub fn assert_writable(
    principal: &Principal,
    namespace: &str,
    sensitivity: Sensitivity,
) -> Result<()> {
    if policy::admits(&principal.write, namespace, sensitivity) {
        return Ok(());
    }
    // The message names the namespace but never the ceiling of some other namespace, because an
    // error that enumerates the grant is an error that maps it.
    Err(DomainError::forbidden(match policy::ceiling(&principal.write, namespace) {
        Some(max) => format!(
            "client {} may write to {namespace} only up to {max}, not {sensitivity}",
            principal.client
        ),
        None => format!("client {} may not write to {namespace}", principal.client),
    }))
}

/// Non-throwing form, for paths that must not reveal why a target was refused.
pub fn can_write(principal: &Principal, namespace: &str, sensitivity: Sensitivity) -> bool {
    policy::admits(&principal.write, namespace, sensitivity)
}

/// Non-throwing read check at a level, for the registry precedence walk and the sealed path.
pub fn can_read(principal: &Principal, namespace: &str, sensitivity: Sensitivity) -> bool {
    policy::admits(&principal.read, namespace, sensitivity)
}

/// The highest level this caller may read anywhere. Used to bound a query that spans namespaces.
pub fn max_readable(principal: &Principal) -> Sensitivity {
    principal.read.iter().map(|g| g.max).max().unwrap_or(Sensitivity::Open)
}

pub fn bearer(header: Option<&str>) -> Result<&str> {
    let h = header.ok_or_else(|| DomainError::forbidden("missing Authorization header"))?;
    let trimmed = h.trim();
    let (scheme, token) = trimmed
        .split_once(char::is_whitespace)
        .ok_or_else(|| DomainError::forbidden("Authorization header must be \"Bearer <token>\""))?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return Err(DomainError::forbidden("Authorization header must be \"Bearer <token>\""));
    }
    Ok(token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy::NamespaceGrant;

    fn principal(read: Vec<NamespaceGrant>, write: Vec<NamespaceGrant>) -> Principal {
        Principal {
            client: "browser".into(),
            token_id: "x".into(),
            mode: "token",
            scopes: vec![],
            read,
            write,
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
        }
    }

    fn open(names: &[&str]) -> Vec<NamespaceGrant> {
        names.iter().map(|n| NamespaceGrant::open(*n)).collect()
    }

    #[test]
    fn narrows_reads_silently() {
        let p = principal(open(&["user:me", "global"]), open(&["global"]));
        let requested = vec!["user:me".into(), "global".into(), "project:secret".into()];
        assert_eq!(readable_names(&p, &requested), vec!["user:me", "global"]);
    }

    #[test]
    fn denies_a_write_outside_the_grant_loudly() {
        let p = principal(open(&["user:me", "global"]), open(&["global"]));
        assert!(assert_writable(&p, "user:me", Sensitivity::Open).is_err());
        assert!(assert_writable(&p, "global", Sensitivity::Open).is_ok());
    }

    #[test]
    fn denies_a_write_above_the_ceiling_even_inside_the_namespace() {
        let p = principal(open(&["*"]), open(&["user:me"]));
        assert!(assert_writable(&p, "user:me", Sensitivity::Open).is_ok());
        let err = assert_writable(&p, "user:me", Sensitivity::Private).unwrap_err();
        assert_eq!(err.kind.http_status(), 403);
        assert!(err.client_message().contains("only up to open"));
    }

    #[test]
    fn a_denied_write_is_forbidden_not_unauthorized() {
        let p = principal(open(&["*"]), open(&["global"]));
        let err = assert_writable(&p, "project:x", Sensitivity::Open).unwrap_err();
        assert_eq!(err.kind.http_status(), 403);
    }

    #[test]
    fn reports_the_highest_level_the_caller_may_read_anywhere() {
        let p = principal(
            vec![
                NamespaceGrant::open("global"),
                NamespaceGrant::new("user:me", Sensitivity::Private),
            ],
            vec![],
        );
        assert_eq!(max_readable(&p), Sensitivity::Private);
        assert_eq!(max_readable(&principal(vec![], vec![])), Sensitivity::Open);
    }

    #[test]
    fn parses_a_bearer_header_case_insensitively() {
        assert_eq!(bearer(Some("Bearer abc")).unwrap(), "abc");
        assert_eq!(bearer(Some("bearer  abc  ")).unwrap(), "abc");
    }

    #[test]
    fn rejects_a_missing_or_non_bearer_header() {
        assert!(bearer(None).is_err());
        assert!(bearer(Some("Basic abc")).is_err());
        assert!(bearer(Some("Bearer")).is_err());
        assert!(bearer(Some("Bearer   ")).is_err());
    }
}
