//! The three documents a hosted MCP client reads before it will show a login screen: RFC 9728
//! protected-resource metadata, RFC 8414 authorization-server metadata, and the 401 challenge that
//! points at the first one.
//!
//! A 200 carrying an error body is silently ignored by the hosted Claude clients, which then fail
//! before offering to authenticate. Claude Code probes for these documents and falls back when they
//! are missing, so a green result from Claude Code proves nothing about the browser surfaces. Treat
//! every field here as load-bearing.
//!
//! Every URL is derived from `cfg.public_url`. An issuer that disagrees with the host behind a
//! reverse proxy is invisible until a real client's discovery fails.

use crate::config::{AuthMode, Config};

/// Endpoint paths of the built-in authorization server. These are advertised here and routed in
/// `src/http`, and the two must agree: a `registration_endpoint` pointing at a path nobody serves
/// is a 404 in the middle of a client's first handshake, with no error the owner ever sees.
const AUTHORIZE_PATH: &str = "/oauth/authorize";
const TOKEN_PATH: &str = "/oauth/token";
const REGISTER_PATH: &str = "/oauth/register";
const REVOKE_PATH: &str = "/oauth/revoke";

/// RFC 9728. Tells a client which authorization server guards this resource.
///
/// Correct in both oauth and oidc mode: `authorization_server()` answers with the built-in issuer
/// in the first and the external issuer in the second, and an empty list in token mode where there
/// is nothing to point at.
pub fn protected_resource_metadata(cfg: &Config) -> serde_json::Value {
    serde_json::json!({
        "resource": cfg.auth.resource_url,
        "authorization_servers": cfg.authorization_server().map(|s| vec![s]).unwrap_or_default(),
        "bearer_methods_supported": ["header"],
        "scopes_supported": scopes_supported(cfg),
    })
}

/// RFC 8414, describing the BUILT-IN authorization server only. In oidc mode the external issuer
/// publishes its own document and this one is not served.
///
/// `code_challenge_methods_supported` advertises S256 and nothing else on purpose. Newer clients
/// refuse to start a flow when the array is absent, and older ones will happily downgrade to
/// `plain` when it is offered, which removes the only binding between the code and the client that
/// asked for it.
pub fn authorization_server_metadata(cfg: &Config) -> serde_json::Value {
    let base = &cfg.public_url;
    serde_json::json!({
        // The issuer is the origin with no path and no trailing slash, so this document lives at
        // the origin's /.well-known/oauth-authorization-server. Clients compare the issuer they
        // discovered against this string exactly.
        "issuer": cfg.builtin_issuer(),
        "authorization_endpoint": format!("{base}{AUTHORIZE_PATH}"),
        "token_endpoint": format!("{base}{TOKEN_PATH}"),
        "registration_endpoint": format!("{base}{REGISTER_PATH}"),
        "revocation_endpoint": format!("{base}{REVOKE_PATH}"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        // "none" is what a public client with PKCE sends. client_secret_post covers the clients
        // the owner issues by hand. client_secret_basic is left out: it carries the secret in a
        // header that proxies log.
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
        "scopes_supported": cfg.oauth.scopes_supported,
    })
}

/// The 401 challenge. `error` is an RFC 6750 error code such as `invalid_token`, or empty when the
/// caller sent no credential at all: RFC 6750 §3 wants a bare challenge in that case, and a client
/// that has never authenticated reads `error="invalid_token"` as "the token I hold is bad" and can
/// loop refreshing a token it does not have.
///
/// The `resource_metadata` pointer is what makes an MCP client discover where to authenticate. It
/// is omitted in token mode, where no metadata document is served and a pointer would resolve to a
/// 404.
pub fn www_authenticate(cfg: &Config, error: &str) -> String {
    let mut params: Vec<String> = Vec::new();

    let code = sanitise_error_code(error);
    if !code.is_empty() {
        params.push(format!("error=\"{code}\""));
    }
    if cfg.auth.mode.is_oauth_protected() {
        params.push(format!("resource_metadata=\"{}\"", cfg.protected_resource_metadata_url()));
    }

    if params.is_empty() {
        return "Bearer".to_string();
    }
    format!("Bearer {}", params.join(", "))
}

/// Header values cannot carry a quote or a control character without breaking the challenge into
/// something the client parses differently than intended. Callers pass literals today; this keeps
/// that true even if one day the code comes from further away.
fn sanitise_error_code(error: &str) -> String {
    error
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect()
}

/// What the resource says it accepts. In oauth mode that is whatever the built-in server issues;
/// in the other modes it is what an external token has to carry to be admitted.
fn scopes_supported(cfg: &Config) -> Vec<String> {
    match cfg.auth.mode {
        AuthMode::Oauth if !cfg.oauth.scopes_supported.is_empty() => {
            cfg.oauth.scopes_supported.clone()
        }
        _ => cfg.auth.required_scopes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Built by hand rather than through `config::load`, which reads the environment: two unit
    // tests mutating process environment variables in parallel is a flake nobody can reproduce.
    fn cfg(mode: AuthMode) -> Config {
        use crate::config::*;
        Config {
            cleanup: crate::config::CleanupConfig { interval_secs: 0, namespace: None, limit: 500 },
            port: 8787,
            host: "0.0.0.0".into(),
            tenant_id: "me".into(),
            database_url: String::new(),
            run_migrations_on_boot: false,
            public_url: "https://lumberroom.example.com".into(),
            auth: AuthConfig {
                mode,
                resource_url: "https://lumberroom.example.com/mcp".into(),
                grants: vec![],
                issuer: "https://id.example.com".into(),
                audience: "https://lumberroom.example.com/mcp".into(),
                jwks_uri: "https://id.example.com/oidc/jwks".into(),
                required_scopes: vec!["memory.read".into()],
                client_claims: vec!["client_id".into()],
                allowed_subjects: vec!["usr_test".into()],
            },
            oauth: OauthConfig {
                owner_password_hash: None,
                dcr_enabled: true,
                code_ttl_secs: 120,
                access_ttl_secs: 3600,
                refresh_ttl_secs: 86_400,
                session_ttl_secs: 900,
                default_profile: "standard".into(),
                scopes_supported: vec!["memory.read".into(), "memory.write".into()],
                cookie_secret: String::new(),
                login_attempts_per_minute: 5,
                registrations_per_minute: 5,
                resource_audience: ResourceAudience::Lenient,
            },
            embed: EmbedConfig {
                provider: EmbedProvider::Hash,
                dim: 768,
                model: "test".into(),
                cache_dir: String::new(),
                allow_fallback: true,
            },
            bootstrap: BootstrapConfig {
                cache_ms: 0,
                profile_limit: 1,
                project_limit: 1,
                recent_limit: 1,
                recent_days: 1,
                registry_limit: 1,
                max_chars: 100,
                max_chars_by_client: std::collections::HashMap::new(),
            },
            search: SearchConfig {
                graph_route: crate::domain::routing::Thresholds::default(),
                default_limit: 8,
                max_limit: 50,
                vector_weight: 1.0,
                lexical_weight: 0.35,
                include_all_projects: true,
                other_project_penalty: 0.85,
                usage_weight: 0.05,
                fusion: crate::config::Fusion::Linear,
                rrf_k: 60.0,
            },
            policy: PolicyConfig {
                defaults: crate::domain::policy::SensitivityDefaults::default(),
                defaults_from_env: false,
                tripwire: true,
                max_write_sensitivity: crate::domain::types::Sensitivity::Private,
                max_content_chars: 8000,
                write_min_occurred_age_secs: crate::config::DEFAULT_MIN_OCCURRED_AGE_SECS,
            },
            crypto: CryptoConfig {
                provider: KekProvider::None,
                kek_path: String::new(),
                kek_env_var: String::new(),
                kek_id: "kek-1".into(),
                require_verified_kek: true,
            },
            quality: QualityConfig {
                dedupe_threshold: 0.97,
                conflict_threshold: 0.90,
                conflict_limit: 3,
                conflict_scan_max: 2_000,
                stale_days: 365,
                export_max_sensitivity: crate::domain::types::Sensitivity::Open,
                archive_max_decompressed_bytes: 2 * 1024 * 1024 * 1024,
            },
            ingest: IngestConfig { emission_window_days: 90, emission_slack_secs: 300.0 },
        }
    }

    #[test]
    fn the_resource_document_points_at_the_builtin_server_in_oauth_mode() {
        let doc = protected_resource_metadata(&cfg(AuthMode::Oauth));
        assert_eq!(doc["resource"], "https://lumberroom.example.com/mcp");
        assert_eq!(doc["authorization_servers"][0], "https://lumberroom.example.com");
        assert_eq!(doc["bearer_methods_supported"][0], "header");
        assert_eq!(doc["scopes_supported"][1], "memory.write");
    }

    #[test]
    fn the_resource_document_points_at_the_external_issuer_in_oidc_mode() {
        let doc = protected_resource_metadata(&cfg(AuthMode::Oidc));
        assert_eq!(doc["authorization_servers"][0], "https://id.example.com");
        assert_eq!(doc["scopes_supported"][0], "memory.read");
    }

    #[test]
    fn the_resource_document_names_no_server_in_token_mode() {
        let doc = protected_resource_metadata(&cfg(AuthMode::Token));
        assert_eq!(doc["authorization_servers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn every_authorization_server_url_derives_from_the_public_origin() {
        let doc = authorization_server_metadata(&cfg(AuthMode::Oauth));
        assert_eq!(doc["issuer"], "https://lumberroom.example.com");
        assert_eq!(doc["authorization_endpoint"], "https://lumberroom.example.com/oauth/authorize");
        assert_eq!(doc["token_endpoint"], "https://lumberroom.example.com/oauth/token");
        assert_eq!(doc["registration_endpoint"], "https://lumberroom.example.com/oauth/register");
        assert_eq!(doc["revocation_endpoint"], "https://lumberroom.example.com/oauth/revoke");
    }

    #[test]
    fn the_authorization_server_advertises_s256_and_no_other_challenge_method() {
        let doc = authorization_server_metadata(&cfg(AuthMode::Oauth));
        let methods = doc["code_challenge_methods_supported"].as_array().unwrap();
        assert_eq!(methods.len(), 1, "offering plain lets a client downgrade PKCE away");
        assert_eq!(methods[0], "S256");
    }

    #[test]
    fn the_authorization_server_advertises_only_the_two_flows_it_implements() {
        let doc = authorization_server_metadata(&cfg(AuthMode::Oauth));
        assert_eq!(doc["response_types_supported"].as_array().unwrap().len(), 1);
        assert_eq!(doc["response_types_supported"][0], "code");
        let grants = doc["grant_types_supported"].as_array().unwrap();
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0], "authorization_code");
        assert_eq!(grants[1], "refresh_token");
        let auth_methods = doc["token_endpoint_auth_methods_supported"].as_array().unwrap();
        assert_eq!(auth_methods.len(), 2);
        assert_eq!(auth_methods[0], "none");
        assert_eq!(auth_methods[1], "client_secret_post");
    }

    #[test]
    fn the_challenge_carries_the_metadata_pointer_in_both_oauth_modes() {
        let expected = "Bearer error=\"invalid_token\", \
             resource_metadata=\"https://lumberroom.example.com/.well-known/oauth-protected-resource\"";
        assert_eq!(www_authenticate(&cfg(AuthMode::Oidc), "invalid_token"), expected);
        assert_eq!(www_authenticate(&cfg(AuthMode::Oauth), "invalid_token"), expected);
    }

    #[test]
    fn a_caller_that_sent_no_credential_gets_a_challenge_without_an_error_code() {
        let h = www_authenticate(&cfg(AuthMode::Oauth), "");
        assert!(!h.contains("error="), "a client with no token must not be told its token is bad");
        assert!(h.contains("resource_metadata="));
    }

    #[test]
    fn the_challenge_omits_the_pointer_in_token_mode() {
        assert_eq!(www_authenticate(&cfg(AuthMode::Token), ""), "Bearer");
        assert_eq!(
            www_authenticate(&cfg(AuthMode::Token), "invalid_token"),
            "Bearer error=\"invalid_token\""
        );
    }

    #[test]
    fn an_error_code_can_never_break_out_of_its_quotes() {
        let h = www_authenticate(&cfg(AuthMode::Oauth), "bad\", scope=\"*");
        assert!(h.starts_with("Bearer error=\"badscope\","), "got {h}");
    }
}
