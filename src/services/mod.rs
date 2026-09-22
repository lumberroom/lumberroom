//! Use cases. One module per thing the system does, depending only on ports.
//!
//! Nothing here imports an adapter except `adapters::auth`, which is pure grant arithmetic over a
//! `Principal`, and `crypto`, which is the key material this layer has to reason about to refuse a
//! private write it cannot honour. Everything else arrives as a port trait object, which is what
//! makes a second storage implementation a matter of construction rather than a rewrite.

pub mod alias;
pub mod archive;
pub mod bootstrap;
pub mod cleanup;
pub mod currency;
pub mod eval;
pub mod export;
pub mod forget;
pub mod graph;
pub mod history;
pub mod ingest;
pub mod recall;
pub mod registry;
pub mod review;
pub mod review_queue;
pub mod search;
pub mod supersession;
pub mod write;

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::crypto::envelope::SealedContent;
use crate::crypto::kek::KeyProvider;
use crate::domain::errors::{DomainError, Result};
use crate::domain::types::{Invocation, Memory, Principal};
use crate::ports::{
    Embedder, MemoryRepository, RegistryRepository, SealedRepository, ToolCallRepository,
};

/// The ciphertext of rows the caller is already holding, so this layer can open what it may read.
///
/// Declared here rather than in `ports` on purpose. `Memory` has no ciphertext field and should not
/// gain one: a read that returned both plaintext and ciphertext in the same struct makes it possible
/// to hand a caller raw bytes in a text field by mistake. The KEK lives in this layer and nowhere
/// else, so this is the one thing the store has to hand up, and the consumer declaring the interface
/// it needs is the direction the dependency rule wants.
///
/// Returns `(row id, ciphertext, kek id)`. The kek id is carried so a rotation is distinguishable
/// from data loss in a log line.
#[async_trait]
pub trait SealedReader: Send + Sync {
    async fn sealed_batch(
        &self,
        tenant: &str,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, SealedContent, Option<String>)>>;
}

/// The stores, as ports.
///
/// Deliberately not `adapters::postgres::Repositories`: a service that names the Postgres adapter
/// has already lost the portability the ports were written for, and this layer also needs the
/// sealed store, which that struct does not carry. The composition root builds this from whatever
/// concrete repositories it constructed.
#[derive(Clone)]
pub struct Repos {
    pub memories: Arc<dyn MemoryRepository>,
    pub registry: Arc<dyn RegistryRepository>,
    pub tool_calls: Arc<dyn ToolCallRepository>,
    /// Optional because a deployment may run before the sealed store exists. Absent means the
    /// digest reports no sealed items rather than failing, and the sealed read path refuses.
    pub sealed: Option<Arc<dyn SealedRepository>>,
    /// Usually the same object as `memories`. Absent means private rows come back unreadable and are
    /// dropped from every result with a log line, which is the safe direction: a missing decryptor
    /// must not turn into a missing check.
    pub ciphertext: Option<Arc<dyn SealedReader>>,
    /// Names that denote the same subject. It sits here rather than being passed to each call
    /// because search consults it on every query, which is what `Repos` is for. The alias service
    /// still takes the repository as an argument, so nothing else has to reach through this.
    pub aliases: Arc<dyn crate::ports::AliasRepository>,
}

/// Everything a tool handler needs. Cloned per request; the expensive parts are behind Arc.
#[derive(Clone)]
pub struct Ctx {
    pub cfg: Arc<Config>,
    pub repos: Repos,
    pub embedder: Arc<dyn Embedder>,
    /// `None` when KEK_PROVIDER=none. A write at `private` is then refused rather than stored in
    /// plaintext, which is the only safe reading of a missing key.
    pub keys: Option<Arc<dyn KeyProvider>>,
    /// Set by the composition root after the boot check compared the live KEK fingerprint against
    /// `kek_state`. Step 4 of the Phase 3 migration order is the one that can strand data: do not
    /// write an encrypted row until a restart has proved the key can be recovered.
    pub kek_verified: bool,
    pub principal: Principal,
    pub invocation: Invocation,
    /// Correlates the calls one surface made inside one conversation. Recorded on every tool call,
    /// and the only way to answer "did this surface read before it answered".
    pub session_id: Option<String>,
}

impl Ctx {
    pub fn tenant(&self) -> &str {
        &self.cfg.tenant_id
    }

    /// Whether an encrypted write can be honoured right now.
    ///
    /// Two conditions, not one. A provider has to exist, and the key it hands out has to have been
    /// recognised at boot. A provider that silently returns a *different* key than the one that
    /// sealed the existing rows would encrypt new rows nobody can later read, and the fingerprint
    /// check at boot is what notices that.
    pub fn can_encrypt(&self) -> bool {
        self.keys.is_some() && (self.kek_verified || !self.cfg.crypto.require_verified_kek)
    }

    /// The refusal, worded for the operator reading a client's error rather than for the model.
    ///
    /// `Unavailable` rather than `Validation`: nothing the caller sends fixes this, and retrying
    /// after the operator configures a key does work. Never a plaintext fallback.
    pub fn assert_can_encrypt(&self) -> Result<()> {
        if self.can_encrypt() {
            return Ok(());
        }
        Err(DomainError::unavailable(if self.keys.is_none() {
            "this content classifies as private and no encryption key is configured. \
             Set KEK_PROVIDER and KEK_PATH, or write it to a namespace that defaults to open. \
             Storing it in plaintext is not an option this server takes."
        } else {
            "this content classifies as private and the encryption key was not verified at boot. \
             Check the server log for the KEK fingerprint mismatch before writing private content."
        }))
    }

    /// The sealed store, or a refusal naming what is missing.
    pub fn sealed_store(&self) -> Result<&Arc<dyn SealedRepository>> {
        self.repos
            .sealed
            .as_ref()
            .ok_or_else(|| DomainError::unavailable("the sealed store is not configured"))
    }
}

/// Whether this grant covers the whole store: the pattern `*`, at `sealed`.
///
/// The bar a tenant-wide number has to clear before it may be published. `Staleness` and the
/// per-client call counts are computed over every row and every call in the tenant, so they carry no
/// namespace and no ceiling for anything downstream to filter on. Either the caller's grant already
/// reaches every row that went into the number, or the number tells it the size and the shape of a
/// store it may not read. A narrow token gets its own rows and no total.
///
/// Expressed as `admits` over the literal namespace `"*"` because `namespaces::matches` lets only the
/// pattern `*` match that name: a grant of `user:*` at sealed fails this, which is the direction to
/// fail in. No new grant flag, so no deployment has to edit `AUTH_TOKENS` and the owner's own client
/// keeps the reports it already had.
pub(crate) fn reads_whole_store(principal: &Principal) -> bool {
    crate::domain::policy::admits(&principal.read, "*", crate::domain::types::Sensitivity::Sealed)
}

/// The `sealed` level: blobs this server cannot read, by construction.
///
/// Inline here rather than in `services/sealed.rs` because no file was allocated to it on this
/// track, and leaving the last row of the Phase 3 §1 enforcement table uncovered was the worse
/// option. Move it to its own file when one exists; nothing outside this module depends on where it
/// lives.
pub mod sealed {
    //! Keyed by an HMAC of the canonical name computed client-side, so the server cannot enumerate
    //! what is stored either. Not searchable, retrievable only by exact key. That is the level's
    //! whole point and pretending otherwise would be the failure mode.
    //!
    //! Every caller receives ciphertext, including the ones that can decrypt it, because the server
    //! holds no key. `sealed_capable` therefore does not gate the bytes; it reports whether the bytes
    //! are of any use to this client, so a browser surface is told plainly rather than being handed
    //! base64 and left to guess.

    use base64::Engine as _;
    use serde::Serialize;

    use super::Ctx;
    use crate::adapters::auth::{can_read, can_write};
    use crate::domain::errors::{DomainError, Result};
    use crate::domain::namespaces;
    use crate::domain::types::{SealedItem, Sensitivity};

    /// Ciphertext ceiling, in encoded characters. A sealed item is a credential or a key, not a
    /// document, and an unbounded blob column reachable by a tool is a denial of service.
    const MAX_CIPHERTEXT_CHARS: usize = 64 * 1024;

    #[derive(Debug, Serialize)]
    pub struct SealedPut {
        pub namespace: String,
        pub key_hmac: String,
        pub alg: String,
        pub bytes: usize,
    }

    #[derive(Debug, Serialize)]
    pub struct SealedGet {
        pub found: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub item: Option<SealedItem>,
        /// False when this client cannot decrypt what it just received. The bytes are the same
        /// either way; this is the honest label on them.
        pub decryptable: bool,
        pub searched: Vec<String>,
    }

    pub async fn put(
        ctx: &Ctx,
        namespace: &str,
        key_hmac: &str,
        ciphertext_b64: &str,
        alg: &str,
    ) -> Result<SealedPut> {
        let namespace = namespaces::normalize(namespace)?;
        // Writing here needs the sealed ceiling, the same as any other write at that level.
        if !can_write(&ctx.principal, &namespace, Sensitivity::Sealed) {
            return Err(DomainError::forbidden(format!(
                "client {} may not write sealed items to {namespace}",
                ctx.principal.client
            )));
        }

        let key_hmac = key_hmac.trim();
        if key_hmac.is_empty() {
            return Err(DomainError::validation("key_hmac cannot be empty"));
        }
        let alg = alg.trim();
        if alg.is_empty() {
            return Err(DomainError::validation("alg cannot be empty: record what sealed this"));
        }
        if ciphertext_b64.len() > MAX_CIPHERTEXT_CHARS {
            return Err(DomainError::validation(format!(
                "ciphertext is {} characters, limit is {MAX_CIPHERTEXT_CHARS}",
                ciphertext_b64.len()
            )));
        }
        // Decoded here so a malformed blob fails at the door rather than at the client that later
        // tries to open it. The bytes stay opaque; only the encoding is checked.
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(ciphertext_b64.trim())
            .map_err(|_| DomainError::validation("ciphertext must be standard base64"))?;
        if bytes.is_empty() {
            return Err(DomainError::validation("ciphertext cannot be empty"));
        }

        ctx.sealed_store()?
            .put(ctx.tenant(), &namespace, key_hmac, &bytes, alg, &ctx.principal.client)
            .await?;
        super::bootstrap::clear_cache();

        Ok(SealedPut {
            namespace,
            key_hmac: key_hmac.to_string(),
            alg: alg.to_string(),
            bytes: bytes.len(),
        })
    }

    pub async fn get(
        ctx: &Ctx,
        key_hmac: &str,
        requested: Option<Vec<String>>,
    ) -> Result<SealedGet> {
        let key_hmac = key_hmac.trim();
        if key_hmac.is_empty() {
            return Err(DomainError::validation("key_hmac cannot be empty"));
        }

        // Only namespaces whose ceiling reaches sealed. A namespace granted at open is not a
        // namespace this client may pull sealed items out of, and narrowing silently is what the
        // read paths do everywhere else.
        //
        // With no list given, the grant's own concrete namespaces are used and its globs are not.
        // A glob is a pattern, not a place: the store matches this key exactly, on
        // (tenant, namespace, key_hmac), so handing it `*` would look up a namespace literally
        // called `*` and report that nothing is stored. There is nothing to enumerate against
        // either, because a sealed item lives in a namespace that need hold no memory rows at all.
        // Captured before the match consumes `requested`, so the refusal below can tell "you named
        // namespaces and none of them admit a sealed read" from "you named none and your grant has
        // no concrete namespace to look in". Those are different problems with different fixes.
        let named = requested.as_ref().is_some_and(|list| !list.is_empty());
        let asked = match requested {
            Some(list) if !list.is_empty() => {
                let mut out =
                    list.iter().map(|n| namespaces::normalize(n)).collect::<Result<Vec<_>>>()?;
                namespaces::dedupe(&mut out);
                out
            }
            _ => crate::domain::policy::patterns(&ctx.principal.read)
                .into_iter()
                .filter(|p| !p.contains('*'))
                .collect(),
        };
        let searched: Vec<String> = asked
            .into_iter()
            .filter(|ns| can_read(&ctx.principal, ns, Sensitivity::Sealed))
            .collect();
        let decryptable = ctx.principal.sealed_capable;
        if searched.is_empty() {
            // The old text said "name the namespace" in both cases, including the case where the
            // namespace had been named and the sensitivity ceiling was what refused it. The refusal
            // was right and the explanation sent the operator looking at their own request.
            //
            // Neither branch enumerates what this client may reach. An error that maps the grant is
            // a way to discover the grant by probing, which is its own problem.
            return Err(if named {
                DomainError::forbidden(format!(
                    "client {} has no namespace among the ones named whose ceiling reaches sealed. \
                     A sealed read needs a ceiling of sealed on the namespace holding the item, and \
                     a grant at open or private does not admit it however the item was named.",
                    ctx.principal.client
                ))
            } else {
                DomainError::validation(
                    "name the namespace to read a sealed item from. Retrieval is by exact key and \
                     the exact key includes the namespace, so there is no set of namespaces to \
                     search.",
                )
            });
        }

        let item = ctx.sealed_store()?.get(ctx.tenant(), &searched, key_hmac).await?;
        Ok(SealedGet { found: item.is_some(), item, decryptable, searched })
    }
}

/// Fill in the plaintext of private rows, and report the ids of the rows that would not open.
///
/// A private row arrives from the store with an empty `content` and its ciphertext left behind. This
/// is the one place in the service layer that turns it back into text.
///
/// **A row that will not open is never an error.** It is one row, and the caller asked a question
/// that the other rows answer. Every failure is logged with the row id and the kek id, because a
/// sudden run of them means the KEK changed and that is the only signal there will be. The caller
/// drops the returned ids from its result.
pub(crate) async fn decrypt(ctx: &Ctx, rows: Vec<&mut Memory>) -> Vec<String> {
    let wanted: Vec<uuid::Uuid> = rows
        .iter()
        .filter(|m| m.sensitivity.is_encrypted() && m.content.is_empty())
        .filter_map(|m| uuid::Uuid::parse_str(&m.id).ok())
        .collect();
    if wanted.is_empty() {
        return vec![];
    }
    let unreadable = || -> Vec<String> { wanted.iter().map(|id| id.to_string()).collect() };

    let Some(reader) = ctx.repos.ciphertext.as_ref() else {
        tracing::error!(
            rows = wanted.len(),
            "private rows were requested but no ciphertext reader is wired; dropping them"
        );
        return unreadable();
    };
    let Some(provider) = ctx.keys.as_ref() else {
        tracing::error!(
            rows = wanted.len(),
            "private rows are stored but no key provider is configured; dropping them"
        );
        return unreadable();
    };

    let kek = match provider.kek().await {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e.log_message(), "cannot read the KEK; dropping private rows");
            return unreadable();
        }
    };
    let batch = match reader.sealed_batch(ctx.tenant(), &wanted).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e.log_message(), "cannot read ciphertext; dropping private rows");
            return unreadable();
        }
    };

    let mut by_id: HashMap<uuid::Uuid, (SealedContent, Option<String>)> =
        batch.into_iter().map(|(id, sealed, kek_id)| (id, (sealed, kek_id))).collect();

    // The same row arrives more than once: the digest hands over profile, project and recent in one
    // call, and one memory belongs to several of those sections. Opening it once and reusing the
    // plaintext keeps the second copy from consuming a ciphertext that is no longer in `by_id` and
    // being reported as data loss, which dropped the row from every section including the one that
    // had already decrypted it. It also costs one AES pass per row rather than one per copy.
    let mut opened: HashMap<uuid::Uuid, String> = HashMap::new();

    let mut failed = Vec::new();
    for row in rows {
        if !(row.sensitivity.is_encrypted() && row.content.is_empty()) {
            continue;
        }
        let Ok(id) = uuid::Uuid::parse_str(&row.id) else {
            failed.push(row.id.clone());
            continue;
        };
        if let Some(plaintext) = opened.get(&id) {
            row.content = plaintext.clone();
            continue;
        }
        let Some((sealed, kek_id)) = by_id.remove(&id) else {
            // A private row with no ciphertext behind it. The CHECK constraint in migration 008
            // forbids that shape, so this means the row was written before encryption was turned on.
            tracing::error!(id = %id, "private row has no ciphertext; dropping it");
            failed.push(row.id.clone());
            continue;
        };
        match crate::crypto::envelope::open(&kek, id, &sealed) {
            Ok(plaintext) => {
                opened.insert(id, plaintext.clone());
                row.content = plaintext;
            }
            Err(_) => {
                // `open` already logged the stage. This line adds which key the row claims, which is
                // what turns "one bad row" into "the KEK was rotated".
                tracing::error!(
                    id = %id,
                    kek_id = kek_id.as_deref().unwrap_or("-"),
                    current_kek = %provider.kek_id(),
                    "private row did not open; dropping it from the result"
                );
                failed.push(row.id.clone());
            }
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy::NamespaceGrant;
    use crate::domain::types::Sensitivity;

    fn with_read(read: Vec<NamespaceGrant>) -> Principal {
        Principal {
            client: "browser".into(),
            token_id: "test".into(),
            mode: "token",
            scopes: vec![],
            read,
            write: vec![],
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
        }
    }

    #[test]
    fn the_owners_own_grant_reads_the_whole_store() {
        assert!(reads_whole_store(&with_read(NamespaceGrant::everything())));
    }

    #[test]
    fn a_glob_below_sealed_does_not_read_the_whole_store() {
        assert!(!reads_whole_store(&with_read(vec![NamespaceGrant::open("*")])));
        assert!(!reads_whole_store(&with_read(vec![NamespaceGrant::new(
            "*",
            Sensitivity::Private
        )])));
    }

    #[test]
    fn a_prefix_glob_at_sealed_does_not_read_the_whole_store() {
        // The trap this pins: `user:*` covers every namespace a deployment happens to hold today and
        // covers nothing the next write creates, so it is not the whole store.
        assert!(!reads_whole_store(&with_read(vec![NamespaceGrant::new(
            "user:*",
            Sensitivity::Sealed
        )])));
        assert!(!reads_whole_store(&with_read(vec![NamespaceGrant::new(
            "user:me",
            Sensitivity::Sealed
        )])));
    }

    #[test]
    fn an_empty_grant_reads_nothing() {
        assert!(!reads_whole_store(&with_read(vec![])));
    }
}
