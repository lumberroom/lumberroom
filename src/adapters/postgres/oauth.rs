//! Postgres implementation of `OauthStore`, against migration 007.
//!
//! Two statements in here carry the whole replay-detection story and both must stay single
//! statements: `consume_code` and `rotate_refresh` mark and read in one `UPDATE ... WHERE
//! consumed_at IS NULL ... RETURNING`, so two concurrent exchanges of one credential cannot both
//! succeed. A read followed by a write would let both win under load, which turns a detectable leak
//! into a silent one.
//!
//! When that UPDATE returns nothing, a second SELECT classifies why. That one is not atomic and
//! does not need to be: the credential was already refused by then, and the only question left is
//! which error to report.

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use crate::domain::errors::Result;
use crate::domain::policy::NamespaceGrant;
use crate::ports::{
    AccessTokenRecord, ClientGrantUpdate, CodeOutcome, NewAccessToken, NewAuthCode, NewOauthClient,
    NewRefreshToken, OauthClientRecord, OauthStore, RefreshOutcome,
};

pub struct PgOauthStore {
    pool: PgPool,
}

impl PgOauthStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn grants(value: serde_json::Value) -> Vec<NamespaceGrant> {
    // A grant column that will not parse must not read as "everything". It reads as nothing, which
    // fails the client closed and shows up as a refused request rather than a widened one.
    serde_json::from_value(value).unwrap_or_default()
}

fn client_from_row(r: &sqlx::postgres::PgRow) -> OauthClientRecord {
    OauthClientRecord {
        client_id: r.get("client_id"),
        secret_hash: r.get("secret_hash"),
        client_name: r.get("client_name"),
        redirect_uris: r.get("redirect_uris"),
        grant_types: r.get("grant_types"),
        registered_via: r.get("registered_via"),
        software_id: r.get("software_id"),
        read: grants(r.get::<serde_json::Value, _>("grant_read")),
        write: grants(r.get::<serde_json::Value, _>("grant_write")),
        registry_write: r.get("registry_write"),
        sealed_capable: r.get("sealed_capable"),
        may_delete: r.get("may_delete"),
        may_ingest: r.get("may_ingest"),
        may_read_history: r.get("may_read_history"),
        consented_at: r.get("consented_at"),
        profile: r.get("profile"),
        created_at: r.get("created_at"),
        last_used_at: r.get("last_used_at"),
        revoked_at: r.get("revoked_at"),
    }
}

#[async_trait]
impl OauthStore for PgOauthStore {
    async fn register_client(&self, c: NewOauthClient) -> Result<()> {
        // No grant columns are set. Registration is not authorization: the row exists with the
        // default empty grant and no consent, and only the consent screen changes that.
        sqlx::query(
            "INSERT INTO oauth_client
               (client_id, secret_hash, client_name, redirect_uris, grant_types,
                software_id, software_version, registered_via)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&c.client_id)
        .bind(&c.secret_hash)
        .bind(&c.client_name)
        .bind(&c.redirect_uris)
        .bind(&c.grant_types)
        .bind(&c.software_id)
        .bind(&c.software_version)
        .bind(&c.registered_via)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_client(&self, client_id: &str) -> Result<Option<OauthClientRecord>> {
        // The column list is spelled out rather than `*`, in every statement: the row reader indexes
        // by name, and a `SELECT *` silently changes shape when a later migration adds a column.
        // sqlx 0.9 accepts only a `&'static str` here, so there is no shared column constant to
        // interpolate and each statement carries its own list.
        let row = sqlx::query(
            "SELECT client_id, secret_hash, client_name, redirect_uris, grant_types,
                    software_id, software_version, registered_via, grant_read, grant_write,
                    registry_write, sealed_capable, may_delete, may_ingest, may_read_history, consented_at, profile,
                    created_at, last_used_at, revoked_at
               FROM oauth_client
              WHERE client_id = $1",
        )
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(client_from_row))
    }

    async fn list_clients(&self, include_revoked: bool) -> Result<Vec<OauthClientRecord>> {
        // One statement per branch, both literal. The predicate is part of the query rather than
        // data, and writing both out is what keeps it that way.
        let rows = if include_revoked {
            sqlx::query(
                "SELECT client_id, secret_hash, client_name, redirect_uris, grant_types,
                        software_id, software_version, registered_via, grant_read, grant_write,
                        registry_write, sealed_capable, may_delete, may_ingest, may_read_history, consented_at, profile,
                        created_at, last_used_at, revoked_at
                   FROM oauth_client
                  ORDER BY created_at DESC",
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT client_id, secret_hash, client_name, redirect_uris, grant_types,
                        software_id, software_version, registered_via, grant_read, grant_write,
                        registry_write, sealed_capable, may_delete, may_ingest, may_read_history, consented_at, profile,
                        created_at, last_used_at, revoked_at
                   FROM oauth_client
                  WHERE revoked_at IS NULL
                  ORDER BY created_at DESC",
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.iter().map(client_from_row).collect())
    }

    async fn set_client_grant(&self, client_id: &str, g: ClientGrantUpdate) -> Result<()> {
        // consented_at and the grant move together. A client that holds a grant has been consented
        // to by definition, so there is no window where one is set and the other is not.
        sqlx::query(
            "UPDATE oauth_client
                SET profile = $2,
                    grant_read = $3,
                    grant_write = $4,
                    registry_write = $5,
                    sealed_capable = $6,
                    may_delete = $7,
                    may_ingest = $8,
                    may_read_history = $9,
                    consented_at = now()
              WHERE client_id = $1",
        )
        .bind(client_id)
        .bind(&g.profile)
        .bind(serde_json::to_value(&g.read).unwrap_or_else(|_| serde_json::json!([])))
        .bind(serde_json::to_value(&g.write).unwrap_or_else(|_| serde_json::json!([])))
        .bind(g.registry_write)
        .bind(g.sealed_capable)
        .bind(g.may_delete)
        .bind(g.may_ingest)
        .bind(g.may_read_history)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn revoke_client(&self, client_id: &str) -> Result<bool> {
        // One transaction: a client marked revoked while its tokens still work is worse than either
        // outcome alone, because the client list says the access is gone.
        let mut tx = self.pool.begin().await?;

        let revoked = sqlx::query(
            "UPDATE oauth_client SET revoked_at = now()
              WHERE client_id = $1 AND revoked_at IS NULL
              RETURNING client_id",
        )
        .bind(client_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();

        sqlx::query(
            "UPDATE oauth_token SET revoked_at = now()
              WHERE client_id = $1 AND revoked_at IS NULL",
        )
        .bind(client_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE oauth_refresh SET revoked_at = now()
              WHERE client_id = $1 AND revoked_at IS NULL",
        )
        .bind(client_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(revoked)
    }

    fn touch_client(&self, client_id: &str) {
        // Fire and forget, off the request path. `last_used_at` answers "is this client still in
        // use", and paying a round trip on every authenticated call to keep it exact would spend the
        // bootstrap latency budget on a field nothing enforces. A lost touch is acceptable; a slower
        // tool call is not.
        let pool = self.pool.clone();
        let client_id = client_id.to_string();
        tokio::spawn(async move {
            let r =
                sqlx::query("UPDATE oauth_client SET last_used_at = now() WHERE client_id = $1")
                    .bind(&client_id)
                    .execute(&pool)
                    .await;
            if let Err(e) = r {
                tracing::debug!(error = %e, client = %client_id, "could not record last_used_at");
            }
        });
    }

    async fn insert_code(&self, c: NewAuthCode) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_code
               (code_hash, client_id, redirect_uri, code_challenge, scope, resource, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&c.code_hash)
        .bind(&c.client_id)
        .bind(&c.redirect_uri)
        .bind(&c.code_challenge)
        .bind(&c.scope)
        .bind(&c.resource)
        .bind(c.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn consume_code(&self, code_hash: &str) -> Result<CodeOutcome> {
        // The whole check in one statement. Two clients racing on one code both reach here, one
        // UPDATE matches `consumed_at IS NULL` and the other matches nothing, so exactly one gets a
        // token and the loser is reported as a replay.
        let row = sqlx::query(
            "UPDATE oauth_code SET consumed_at = now()
              WHERE code_hash = $1 AND consumed_at IS NULL AND expires_at > now()
              RETURNING client_id, redirect_uri, code_challenge, scope, resource, expires_at",
        )
        .bind(code_hash)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            return Ok(CodeOutcome::Fresh(NewAuthCode {
                code_hash: code_hash.to_string(),
                client_id: r.get("client_id"),
                redirect_uri: r.get("redirect_uri"),
                code_challenge: r.get("code_challenge"),
                scope: r.get("scope"),
                resource: r.get("resource"),
                expires_at: r.get("expires_at"),
            }));
        }

        // Classification only. The code is already refused, so this read has nothing left to race
        // with.
        let row = sqlx::query(
            "SELECT client_id, consumed_at, expires_at <= now() AS expired
               FROM oauth_code WHERE code_hash = $1",
        )
        .bind(code_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(match row {
            // Consumed is checked before expired on purpose. A code that is both was spent and then
            // aged out, and "someone presented this twice" outranks "it was old" because only the
            // first calls for killing the tokens it produced.
            Some(r)
                if r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("consumed_at").is_some() =>
            {
                CodeOutcome::AlreadyConsumed { client_id: r.get("client_id") }
            }
            Some(r) if r.get::<bool, _>("expired") => CodeOutcome::Expired,
            // Present, unconsumed and unexpired, yet the UPDATE missed it: another request took it
            // between the two statements. That is a replay from this caller's point of view.
            Some(r) => CodeOutcome::AlreadyConsumed { client_id: r.get("client_id") },
            None => CodeOutcome::Unknown,
        })
    }

    async fn insert_token(&self, t: NewAccessToken) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_token (token_hash, client_id, scope, resource, family_id, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&t.token_hash)
        .bind(&t.client_id)
        .bind(&t.scope)
        .bind(&t.resource)
        .bind(t.family_id)
        .bind(t.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_token(&self, token_hash: &str) -> Result<Option<AccessTokenRecord>> {
        // The hot path: one primary-key lookup per authenticated request. Expiry and revocation
        // travel back with the row rather than being filtered here, because the caller has to tell
        // "expired" from "never existed" to answer with the right challenge.
        let row = sqlx::query(
            "SELECT token_hash, client_id, scope, resource, family_id, expires_at, revoked_at
               FROM oauth_token WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| AccessTokenRecord {
            token_hash: r.get("token_hash"),
            client_id: r.get("client_id"),
            scope: r.get("scope"),
            resource: r.get("resource"),
            family_id: r.get("family_id"),
            expires_at: r.get("expires_at"),
            revoked_at: r.get("revoked_at"),
        }))
    }

    async fn revoke_token(&self, token_hash: &str) -> Result<bool> {
        let row = sqlx::query(
            "UPDATE oauth_token SET revoked_at = now()
              WHERE token_hash = $1 AND revoked_at IS NULL
              RETURNING token_hash",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn insert_refresh(&self, r: NewRefreshToken) -> Result<()> {
        sqlx::query(
            "INSERT INTO oauth_refresh (token_hash, client_id, family_id, expires_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&r.token_hash)
        .bind(&r.client_id)
        .bind(r.family_id)
        .bind(r.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn rotate_refresh(&self, token_hash: &str) -> Result<RefreshOutcome> {
        // Same shape as consume_code and for the same reason: rotation is only a defence if the
        // second presentation of one refresh token cannot also succeed.
        let row = sqlx::query(
            "UPDATE oauth_refresh SET consumed_at = now()
              WHERE token_hash = $1
                AND consumed_at IS NULL
                AND revoked_at IS NULL
                AND expires_at > now()
              RETURNING client_id, family_id",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(r) = row {
            return Ok(RefreshOutcome::Rotated {
                client_id: r.get("client_id"),
                family_id: r.get("family_id"),
            });
        }

        let row = sqlx::query(
            "SELECT family_id, consumed_at, revoked_at, expires_at <= now() AS expired
               FROM oauth_refresh WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;

        let Some(r) = row else { return Ok(RefreshOutcome::Unknown) };
        let family_id: uuid::Uuid = r.get("family_id");
        let consumed = r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("consumed_at").is_some();
        let revoked = r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("revoked_at").is_some();

        // Replay outranks both other verdicts. A consumed token that was later revoked or has since
        // expired still means a token that was spent came back, and that is the case the family kill
        // exists for.
        Ok(if consumed {
            RefreshOutcome::Replayed { family_id }
        } else if revoked {
            RefreshOutcome::Revoked
        } else if r.get::<bool, _>("expired") {
            RefreshOutcome::Expired
        } else {
            // Unconsumed, live, unexpired, and the UPDATE still missed it: a concurrent request took
            // it first, which is the same situation as a replay.
            RefreshOutcome::Replayed { family_id }
        })
    }

    async fn refresh_owner(&self, token_hash: &str) -> Result<Option<String>> {
        // The same liveness filter as the UPDATE in `rotate_refresh`. A spent, revoked or expired
        // token answers None and falls through to `rotate_refresh`, whose replay verdict has to
        // revoke the family whoever presented it.
        let row = sqlx::query(
            "SELECT client_id FROM oauth_refresh
              WHERE token_hash = $1
                AND consumed_at IS NULL
                AND revoked_at IS NULL
                AND expires_at > now()",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get("client_id")))
    }

    async fn revoke_family(&self, family_id: uuid::Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE oauth_token SET revoked_at = now()
              WHERE family_id = $1 AND revoked_at IS NULL",
        )
        .bind(family_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE oauth_refresh SET revoked_at = now()
              WHERE family_id = $1 AND revoked_at IS NULL",
        )
        .bind(family_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn purge_expired(&self) -> Result<u64> {
        // Four statements, and the grace period differs in each. A spent credential has to stay
        // readable for a while after it dies, because deleting it turns a replay into a code this
        // server never issued and the replay is the signal worth having. Codes live for minutes, so
        // a day of history costs nothing; a refresh family matters for as long as the family does.
        // The fourth clears self-registered clients the owner never consented to: registration is
        // unauthenticated, so those rows are the one thing here a stranger can create.
        let codes =
            sqlx::query("DELETE FROM oauth_code WHERE expires_at < now() - interval '1 day'")
                .execute(&self.pool)
                .await?
                .rows_affected();

        let tokens =
            sqlx::query("DELETE FROM oauth_token WHERE expires_at < now() - interval '1 day'")
                .execute(&self.pool)
                .await?
                .rows_affected();

        let refresh =
            sqlx::query("DELETE FROM oauth_refresh WHERE expires_at < now() - interval '30 days'")
                .execute(&self.pool)
                .await?
                .rows_affected();

        // A day is longer than any client's registration-to-consent gap, and a client the owner
        // revoked is kept as the record of that decision. Manual clients are never touched.
        let unconsented = sqlx::query(
            "DELETE FROM oauth_client \
             WHERE registered_via = 'dcr' \
               AND consented_at IS NULL \
               AND revoked_at IS NULL \
               AND created_at < now() - interval '24 hours'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(codes + tokens + refresh + unconsented)
    }
}
