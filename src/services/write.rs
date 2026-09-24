//! memory_write(content, namespace, tags?, supersedes?, sensitivity?, occurred_at?) -> WriteOutcome
//!
//! Embed on write, store raw. No extraction and no model call in the write path.
//!
//! This is the densest service in the system because it is the only place where classification,
//! encryption, duplicate collapse and supersession all have to agree about one row. The order of
//! the checks is load-bearing and is written out in the body.
//!
//! Two biases run through the whole file.
//!
//! **Classification only ever goes up.** A caller may raise a write above its namespace default and
//! can never lower it, because a tool that can downgrade classification is a tool that can leak by
//! mistake.
//!
//! **Every duplicate threshold is biased toward storing too much.** Collapsing a correction into
//! its own predecessor destroys data, and it does it silently, which is the part that makes it
//! unacceptable. A store with two rows where one would do is untidy; a store that ate the
//! correction is wrong and nothing in it says so.
//!
//! **Two clocks, never conflated.** `created_at` is when this store learned the fact and the
//! database sets it. `occurred_at` is when the fact held in the world and only a caller can know
//! it. Everything in this file that touches the second one exists to keep it from decaying into a
//! copy of the first.

use chrono::{DateTime, Utc};

use super::Ctx;
use crate::adapters::auth::{assert_writable, can_read, can_write};
use crate::domain::errors::{DomainError, Result};
use crate::domain::types::{ConflictCandidate, Memory, Sensitivity, WriteOutcome};
use crate::domain::{namespaces, policy, tripwire};
use crate::ports::{NeighbourQuery, NewMemory};

/// The default, kept as a name so callers and tests have one. The effective limit is
/// `cfg.policy.max_content_chars`, which reads `WRITE_MAX_CONTENT_CHARS`.
pub const MAX_CONTENT_CHARS: usize = 8000;

/// Whether the near-now fence runs on this write.
///
/// Private, and reached through two entry points rather than through an argument. The MCP layer
/// builds its call from a `WriteArgs` it deserializes from JSON, so anything a model can set has to
/// be a field on that struct; this type cannot be named outside this module and [`run_observed`] is
/// not reachable from `crate::mcp`. That makes the exemption a compiler question rather than a
/// question about how carefully someone read the tool layer. A boolean parameter would answer it
/// the second way.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fence {
    /// A caller relaying what a person said, which is every model-facing surface.
    Apply,
    /// The ingest fill. Its date is a transcript's `observed_at`, metadata rather than a model
    /// reading a date out of text, and a conversation recorded this morning is legitimately
    /// same-day. Fencing it would refuse the one fill that pays for valid time.
    Bypass,
}

/// The model-facing write. `occurred_at` passes the near-now fence or the write is refused.
pub async fn run(
    ctx: &Ctx,
    content: &str,
    namespace: &str,
    tags: Option<Vec<String>>,
    supersedes: Option<&str>,
    sensitivity: Option<&str>,
    occurred_at: Option<DateTime<Utc>>,
) -> Result<WriteOutcome> {
    run_inner(ctx, content, namespace, tags, supersedes, sensitivity, occurred_at, Fence::Apply)
        .await
}

/// The ingest fill's write, exempt from the near-now fence and identical in every other check.
///
/// `pub(super)` is the exemption's whole enforcement: `crate::services::ingest` reaches it and
/// `crate::mcp` does not. Its one caller is `services::ingest::approve`, which passes
/// `min(observed_at)` over the proposal's sources, or `None` when every source lacks one. Passing
/// `Some(Utc::now())` here would write the approval clock into a valid-time column, which is the
/// conflation the column exists to end.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_observed(
    ctx: &Ctx,
    content: &str,
    namespace: &str,
    tags: Option<Vec<String>>,
    supersedes: Option<&str>,
    sensitivity: Option<&str>,
    occurred_at: Option<DateTime<Utc>>,
) -> Result<WriteOutcome> {
    run_inner(ctx, content, namespace, tags, supersedes, sensitivity, occurred_at, Fence::Bypass)
        .await
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    ctx: &Ctx,
    content: &str,
    namespace: &str,
    tags: Option<Vec<String>>,
    supersedes: Option<&str>,
    sensitivity: Option<&str>,
    occurred_at: Option<DateTime<Utc>>,
    fence: Fence,
) -> Result<WriteOutcome> {
    let content = content.trim();
    if content.is_empty() {
        return Err(DomainError::validation("content cannot be empty"));
    }
    let max_chars = ctx.cfg.policy.max_content_chars;
    if content.chars().count() > max_chars {
        return Err(DomainError::validation(format!(
            "content is {} chars, limit is {max_chars}. Write the durable fact, not the transcript.",
            content.chars().count()
        )));
    }

    let namespace = namespaces::normalize(namespace)?;
    // The personal namespace is reserved before anything is resolved on the write's behalf. A
    // write to `user:<anything else>` validates, stores, and is then read by nothing.
    namespaces::check_writable(&namespace)?;

    // (a0) A credentials namespace never takes plaintext, whatever the classification table says.
    //
    // Checked before the level is resolved, and deliberately not from policy. The ceiling check in
    // (b) already refuses this on a default install, but it reads an operator-editable table: an
    // operator who replaces SENSITIVITY_DEFAULTS with rules of their own drops the `credentials:*`
    // row with it, and the same call would then store a credential at open and stem it into the
    // lexical index. The shape is what the caller said they were writing, so the shape decides.
    if namespaces::requires_client_sealing(&namespace) {
        return Err(DomainError::validation(format!(
            "{namespace} holds client-encrypted items and memory_write takes plaintext. \
             Use `lumberroom seal <key> --namespace {namespace}`, which encrypts on the client and stores \
             the ciphertext by key. This server holds no key for it and never will."
        )));
    }

    // (a) Resolve the level. The namespace default is a floor, the request may raise it.
    let requested = parse_sensitivity(sensitivity)?;
    let resolved = ctx.cfg.policy.defaults.resolve_for_write(&namespace, requested);

    // (b) Sealed never arrives as a plaintext tool argument: the client encrypts it and stores it
    // through the sealed path. Refusing here rather than at the insert keeps the plaintext from
    // travelling any further into the process than it already has.
    if resolved > ctx.cfg.policy.max_write_sensitivity {
        return Err(DomainError::validation(format!(
            "content for {namespace} classifies as {resolved} and memory_write accepts up to {}. \
             Content at sealed is encrypted by the client and stored by key, never sent as a \
             plaintext tool argument: use `lumberroom seal <key> --namespace {namespace}` instead.",
            ctx.cfg.policy.max_write_sensitivity
        )));
    }

    // (c) The grant, at the resolved level, before anything else touches the content.
    assert_writable(&ctx.principal, &namespace, resolved)?;

    // Refuse an encrypted write we cannot honour, before spending an embedding on it. The
    // alternative to refusing is storing private content in plaintext, which is not a trade this
    // server makes.
    if resolved.is_encrypted() {
        ctx.assert_can_encrypt()?;
    }

    // (d) The tripwire. A backstop for the case where namespace inference was wrong in the
    // expensive direction, and only at open, because that is the only level where a credential
    // would be stored in the clear.
    if ctx.cfg.policy.tripwire && resolved == Sensitivity::Open {
        if let Some(finding) = tripwire::scan(content) {
            // The finding carries the matched text so the operator can read it in a log. It must
            // not travel back to the caller: echoing a secret into an error message puts it in
            // whatever transcript, log or bug report the error lands in next.
            tracing::warn!(
                client = %ctx.principal.client,
                namespace = %namespace,
                rule = %finding.rule,
                "credential tripwire refused a write at open"
            );
            // The advice has to be advice that works. Pointing at `credentials:*` would send the
            // caller straight into the (a0) refusal, and an error that routes into another error
            // costs the operator the hour this message was written to save.
            return Err(DomainError::validation(format!(
                "this content matches the {} pattern and will not be stored at open. \
                 Store it at sealed with `lumberroom seal <key> --namespace credentials:<slug>`, which \
                 encrypts on the client, or write the durable fact about it rather than the secret \
                 itself. The matched text is not repeated here on purpose.",
                finding.rule
            )));
        }
    }

    // (d1) The near-now fence, rule D1. Refuse an `occurred_at` this store would learn nothing
    // from, and tell the caller to drop the field.
    //
    // Last of the refusals rather than first, and the tripwire is the reason. A write carrying a
    // credential and a near-now date has one message worth sending, and it is the tripwire's:
    // stopped on the date first, the caller fixes a timestamp and comes back with the secret still
    // in the content, having spent a round trip on the smaller of its two problems. The grant runs
    // ahead of it for the same reason read the other way. A client that may not write here hears
    // that, and never that its date was the one thing in the way.
    //
    // Before (e) and (h), which both query stored rows. A write about to be refused costs no read.
    if fence == Fence::Apply {
        fence_occurred_at(
            occurred_at,
            ctx.cfg.policy.write_min_occurred_age_secs,
            Utc::now(),
            content,
        )?;
    }

    let tags = clean_tags(tags);

    // (h) supersedes, validated before any write happens so a bad target costs one round trip
    // rather than an orphan row.
    let supersedes_id = match supersedes {
        Some(raw) => {
            let (id, target) = validate_supersedes_target(ctx, raw).await?;
            // Rule S4. Checked on both entry points, because the ingest path is where the
            // out-of-order approvals live.
            refuse_inversion(occurred_at, target.occurred_at, raw)?;
            Some(id)
        }
        None => None,
    };

    // (e) Exact-duplicate collapse. Agents restate the same fact across sessions, and this keeps
    // the digest from filling with the same sentence eight times.
    //
    // Plaintext only, for two reasons: an encrypted row has no content column to compare against,
    // and a private plaintext sent as a query parameter would appear in the statement log of a
    // database that is otherwise holding only ciphertext for that row.
    //
    // And only when the caller may read the row it would collapse into. Answering
    // `deduplicated: true` with an existing id is a yes to "does this exact sentence exist here",
    // and a write-only grant is not a grant to ask that. The neighbour path below already applies
    // the read ceiling; this is the same rule on the exact-match path. A write-only client stores
    // a second copy, which is the side of the trade this file's header commits to.
    if supersedes_id.is_none()
        && resolved == Sensitivity::Open
        && can_read(&ctx.principal, &namespace, Sensitivity::Open)
    {
        if let Some(existing) =
            ctx.repos.memories.find_exact(ctx.tenant(), &namespace, content).await?
        {
            if existing.sensitivity == resolved {
                // Repetition is confirmation. A restatement is evidence the fact is still true,
                // which is exactly what the review queue needs to tell a live fact from a stale one.
                confirm(ctx, &existing.id).await;
                return Ok(WriteOutcome {
                    id: existing.id,
                    namespace,
                    sensitivity: existing.sensitivity,
                    deduplicated: true,
                    superseded: None,
                    end_left_open: false,
                    possible_conflicts: vec![],
                });
            }
        }
    }

    // An encrypted row's embedding is stored in the clear, and with the real model that leaks the
    // gist, which is the documented trade. The hash embedder leaks more than the gist: it is an
    // unsalted bucket count over the row's own tokens, so a dump holder can test words against it
    // one at a time. While the server is degraded onto it, a private write is refused rather than
    // stored beside a sketch of itself.
    if resolved.is_encrypted() {
        refuse_sketch_of_private_content(ctx)?;
    }

    let mut vectors = ctx.embedder.embed_documents(vec![content.to_string()]).await?;
    let embedding =
        vectors.pop().ok_or_else(|| DomainError::internal("embedder returned no vector"))?;

    // (f) The dedupe bands. One query at the lower threshold; the bands are split here.
    let mut possible_conflicts = Vec::new();
    if supersedes_id.is_none() {
        let neighbours = neighbours(ctx, &namespace, &embedding).await?;
        for (candidate, row) in resolve_candidates(ctx, neighbours).await? {
            let above_dedupe = candidate.similarity >= ctx.cfg.quality.dedupe_threshold;
            let block = collapse_block(content, &candidate.content);

            if above_dedupe && block.is_none() {
                if let Some(id) =
                    collapse_target(ctx, &candidate, content, resolved, row.as_ref()).await?
                {
                    confirm(ctx, &id.to_string()).await;
                    return Ok(WriteOutcome {
                        id: id.to_string(),
                        namespace,
                        sensitivity: resolved,
                        deduplicated: true,
                        superseded: None,
                        end_left_open: false,
                        possible_conflicts: vec![],
                    });
                }
            }

            if let Some(reason) = block {
                if above_dedupe {
                    // The interesting log line in this file. A pair this similar that the guard
                    // refused is either a correction or a calibration problem, and both are worth
                    // being able to find later.
                    tracing::info!(
                        namespace = %namespace,
                        similarity = candidate.similarity,
                        reason = reason.as_str(),
                        existing = %candidate.id,
                        "declined to collapse a near-identical write"
                    );
                }
            }
            possible_conflicts.push(candidate);
        }
    }

    // (g) Encryption. The row id comes first because it is bound into both AES-GCM layers as
    // associated data: without it, anyone with database write access could move one row's
    // ciphertext onto another row and have the server decrypt it under the wrong namespace and the
    // wrong grant.
    //
    // The embedding above was computed over the plaintext and is stored in the clear, because
    // search has to work. That leaks the gist of a private row to anyone holding the database, and
    // it is a defended, documented trade rather than an oversight
    // (docs/research/encryption-and-sensitivity.md).
    let sealed = match resolved.is_encrypted() {
        true => Some(seal(ctx, content).await?),
        false => None,
    };

    let written = ctx
        .repos
        .memories
        .insert(NewMemory {
            // Whatever survived the fence, unaltered. The write path never derives a date: an
            // absent `occurred_at` stays absent rather than becoming `now()`, because a row whose
            // valid time was invented is worse than a row with none (rule N1).
            occurred_at,
            tenant_id: ctx.cfg.tenant_id.clone(),
            id: sealed.as_ref().map(|(id, _)| *id),
            namespace: namespace.clone(),
            // Empty rather than the plaintext for an encrypted row. The port says this field is
            // ignored when `sealed` is present; sending nothing means a repository that ignores
            // that contract fails its CHECK constraint instead of quietly storing the plaintext.
            content: match &sealed {
                Some(_) => String::new(),
                None => content.to_string(),
            },
            embedding,
            tags,
            supersedes: supersedes_id,
            source_client: ctx.principal.client.clone(),
            embedding_model: ctx.embedder.id(),
            sensitivity: resolved,
            sealed: sealed.map(|(_, s)| s),
        })
        .await?;

    let new_id = uuid::Uuid::parse_str(&written.id)
        .map_err(|_| DomainError::internal("repository returned an id that is not a uuid"))?;

    let mut end_left_open = false;
    let superseded = match supersedes_id {
        Some(old) => {
            // The forward link is already on the new row from the insert; this writes the reverse
            // link and the timestamp on the old one. A failure here leaves a live new row and an
            // un-retired old one, so the error names the row that was written: a caller that
            // retries blindly would otherwise create a second copy.
            let done =
                ctx.repos.memories.supersede(ctx.tenant(), old, new_id).await.map_err(|e| {
                    DomainError::new(
                        e.kind,
                        format!(
                            "memory {new_id} was written but {old} was not retired: {}",
                            e.client_message()
                        ),
                    )
                })?;
            end_left_open = done.end_left_open;
            Some(old.to_string())
        }
        None => None,
    };

    // A fact written now must appear in the next bootstrap, not after the cache expires.
    super::bootstrap::clear_cache();

    Ok(WriteOutcome {
        id: written.id,
        namespace,
        sensitivity: resolved,
        deduplicated: false,
        superseded,
        end_left_open,
        possible_conflicts,
    })
}

/// How the hash embedder's `id()` begins. The adapter formats it as `hash-v1-<dim>`, and this
/// layer cannot name the adapter, so the prefix is the contract; `tests/integration.rs` pins the
/// two against each other.
pub const HASH_EMBEDDER_ID_PREFIX: &str = "hash-v1-";

/// Refuse a private write while the embedder in use is the hash sketch and the operator did not
/// choose it.
///
/// The fallback case only. `EMBED_ALLOW_FALLBACK` swaps the sketch in when the model fails to
/// load, `/readyz` already reports that window as degraded, and an outage on private writes for
/// its duration costs less than a token-membership oracle stored for good. `EMBED_PROVIDER=hash`
/// is the operator's own choice and the test suite's, and it goes through; the config documents
/// that setting as never for production.
///
/// The embedder is picked once at boot and nothing reloads it, so the way out of this refusal is
/// a restart, and the message says so. Telling the caller to retry would have it retry forever.
fn refuse_sketch_of_private_content(ctx: &Ctx) -> Result<()> {
    let sketch = ctx.embedder.id().starts_with(HASH_EMBEDDER_ID_PREFIX);
    let chosen = ctx.cfg.embed.provider == crate::config::EmbedProvider::Hash;
    if !sketch || chosen {
        return Ok(());
    }
    Err(DomainError::unavailable(
        "this content classifies as private and the server is running on the fallback hash \
         embedder because the embedding model failed to load at boot. Its vector is a token \
         sketch of the content, which would sit in the clear beside the ciphertext. The embedder \
         is chosen once at startup: restart the server with the model reachable (/readyz reports \
         degraded_embedder until then), or write it to a namespace that defaults to open.",
    ))
}

/// An unrecognised level is a refusal, never a silent `open`: defaulting a level the server does
/// not understand downward is how a private fact becomes a public one.
fn parse_sensitivity(raw: Option<&str>) -> Result<Option<Sensitivity>> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    Sensitivity::parse(s).map(Some).ok_or_else(|| {
        DomainError::validation(format!("sensitivity {s:?} is not one of open, private, sealed"))
    })
}

/// The one spelling a tag filter also runs through, so a filter matches what this stored.
fn clean_tags(tags: Option<Vec<String>>) -> Vec<String> {
    crate::domain::tags::normalise(tags.unwrap_or_default())
}

/// Live rows in this namespace close enough to be worth a decision, bounded by what the caller may
/// read there.
///
/// A caller with no read ceiling in a namespace it may write to gets no neighbours and therefore no
/// collapse, so a write-only client stores a second copy rather than being handed the id of a row
/// it cannot see. That is the storing-too-much side of the trade, which is the side to be on.
async fn neighbours(
    ctx: &Ctx,
    namespace: &str,
    embedding: &[f32],
) -> Result<Vec<ConflictCandidate>> {
    let Some(ceiling) = policy::ceiling(&ctx.principal.read, namespace) else {
        return Ok(vec![]);
    };
    ctx.repos
        .memories
        .neighbours(NeighbourQuery {
            tenant_id: ctx.cfg.tenant_id.clone(),
            namespace: namespace.to_string(),
            embedding: embedding.to_vec(),
            min_similarity: ctx.cfg.quality.conflict_threshold,
            limit: ctx.cfg.quality.conflict_limit,
            max_sensitivity: ceiling,
        })
        .await
}

/// Fill in the text of every candidate before any of them is judged.
///
/// A private neighbour arrives from the store with an empty `content`, because the repository holds
/// no key. Running the numeric guard against an empty string passes for any text without digits or
/// negations, which would collapse a write into a row nobody compared it to. The guard is the last
/// thing standing between a correction and its own predecessor, so it never runs against a blank.
///
/// A candidate that cannot be resolved is dropped rather than reported: a conflict candidate with no
/// text tells the model nothing and cannot be acted on.
///
/// The extra round trip only happens for encrypted neighbours. A plaintext store pays nothing.
async fn resolve_candidates(
    ctx: &Ctx,
    candidates: Vec<ConflictCandidate>,
) -> Result<Vec<(ConflictCandidate, Option<crate::domain::types::Memory>)>> {
    let mut out = Vec::with_capacity(candidates.len());
    for mut candidate in candidates {
        if !candidate.content.is_empty() {
            out.push((candidate, None));
            continue;
        }
        let Ok(id) = uuid::Uuid::parse_str(&candidate.id) else { continue };
        let Some(mut row) = ctx.repos.memories.find_by_id(ctx.tenant(), id).await? else {
            continue;
        };
        let _ = super::decrypt(ctx, vec![&mut row]).await;
        if row.content.is_empty() {
            tracing::warn!(id = %id, "neighbour has no readable content; not offering it");
            continue;
        }
        candidate.content = row.content.clone();
        out.push((candidate, Some(row)));
    }
    Ok(out)
}

/// The id to collapse into, or `None` when the candidate turned out not to be a legal target.
///
/// Levels must match. A private write collapsed into an open row would silently store the caller's
/// private content at open, and an open write collapsed into a private row would report an id whose
/// content the next reader may not be able to see.
///
/// The guard runs again here, against the stored row rather than against the neighbour query's copy
/// of it. This is the last check before a row is thrown away, and it is worth paying for twice.
async fn collapse_target(
    ctx: &Ctx,
    candidate: &ConflictCandidate,
    content: &str,
    resolved: Sensitivity,
    prefetched: Option<&crate::domain::types::Memory>,
) -> Result<Option<uuid::Uuid>> {
    let Ok(id) = uuid::Uuid::parse_str(&candidate.id) else {
        return Ok(None);
    };
    let fetched = match prefetched {
        Some(_) => None,
        None => {
            let Some(mut row) = ctx.repos.memories.find_by_id(ctx.tenant(), id).await? else {
                return Ok(None);
            };
            let _ = super::decrypt(ctx, vec![&mut row]).await;
            Some(row)
        }
    };
    let existing = prefetched.unwrap_or_else(|| fetched.as_ref().expect("one of the two is set"));

    if existing.sensitivity != resolved || !existing.is_live() {
        return Ok(None);
    }
    // Never collapse into text this server could not read.
    if existing.content.is_empty() {
        return Ok(None);
    }
    if collapse_block(content, &existing.content).is_some() {
        return Ok(None);
    }
    Ok(Some(id))
}

/// Generate the row id, then seal the content under it.
///
/// `assert_can_encrypt` has already run, so a missing provider here is a programming error rather
/// than a configuration one and says so.
async fn seal(ctx: &Ctx, content: &str) -> Result<(uuid::Uuid, crate::crypto::SealedContent)> {
    let provider = ctx
        .keys
        .as_ref()
        .ok_or_else(|| DomainError::internal("reached the encryption path with no key provider"))?;
    // Fetched per write rather than cached in the process. The file provider reads sixty-four bytes
    // and the env provider reads a variable, so the cost is nothing next to an embedding, and a KEK
    // held in a long-lived cache is a KEK that survives a rotation it should not have survived.
    let kek = provider.kek().await?;
    let id = uuid::Uuid::new_v4();
    let sealed = crate::crypto::envelope::seal(&kek, id, content)?;
    Ok((id, sealed))
}

/// Repetition is confirmation, and it is never worth failing a write over.
async fn confirm(ctx: &Ctx, id: &str) {
    let Ok(uuid) = uuid::Uuid::parse_str(id) else { return };
    if let Err(e) = ctx.repos.memories.confirm(ctx.tenant(), uuid).await {
        tracing::warn!(id, error = %e.log_message(), "could not record a confirmation");
    }
}

/// Retiring a fact is a destructive write, so it needs the same grant as writing one, plus the read
/// grant: a client that cannot see a row has no business deciding it is out of date.
pub(super) async fn validate_supersedes(ctx: &Ctx, raw: &str) -> Result<uuid::Uuid> {
    validate_supersedes_target(ctx, raw).await.map(|(id, _)| id)
}

/// The same validation, handing back the row it validated.
///
/// Rule S4 needs the predecessor's `occurred_at`, and this function already fetched the row to
/// check the grant. Returning it costs nothing; a second `find_by_id` in the caller would be a
/// round trip spent re-reading what this one threw away.
async fn validate_supersedes_target(ctx: &Ctx, raw: &str) -> Result<(uuid::Uuid, Memory)> {
    let id = uuid::Uuid::parse_str(raw)
        .map_err(|_| DomainError::validation(format!("supersedes id {raw:?} is not a uuid")))?;
    let target = ctx.repos.memories.find_by_id(ctx.tenant(), id).await?;

    // One message for "does not exist" and for "not yours". The refusal deliberately does not name
    // the target's namespace: doing so would tell a client that a namespace it cannot read exists.
    let target = match target {
        Some(t)
            if can_read(&ctx.principal, &t.namespace, t.sensitivity)
                && can_write(&ctx.principal, &t.namespace, t.sensitivity) =>
        {
            t
        }
        _ => {
            return Err(DomainError::validation(format!(
                "supersedes id {raw} does not exist or is not writable"
            )))
        }
    };

    // A closed period has no successor to write. Decision 0017 made `occurred_until` reachable
    // without a supersession, and a supersession over an expired row would end a period that has
    // already ended and hand `forget`'s revive an end it did not write.
    // `domain::types::expired` holds the definition, and `RETIRE_PREDECESSOR_SQL` transcribes the
    // same test into the UPDATE. A guard here that the statement does not share lets a row pass one
    // door and stall at the other, which is how a supersession got reported that never landed.
    if target.is_expired() {
        return Err(DomainError::conflict(format!(
            "memory {raw} expired, so its period is already closed and it takes no successor. \
             Write the new fact on its own, or bring the old one back first."
        )));
    }

    if target.superseded_by.is_some() {
        // Naming the live head is the whole point of the error: the caller retries against it in
        // the same turn instead of writing a third row beside the two that already disagree.
        let head = ctx.repos.memories.supersession_head(ctx.tenant(), id).await?;
        return Err(DomainError::conflict(match head {
            Some(h) => format!(
                "memory {raw} was already superseded. The live row is {}; retry with supersedes \
                 set to it if this write replaces that.",
                h.id
            ),
            None => format!("memory {raw} was already superseded"),
        }));
    }

    Ok((id, target))
}

// -- valid time -------------------------------------------------------------------------------
//
// Two refusals, both pure, both citing the rule they implement from docs/specs/phase-7-valid-time.md.
// Pure because the interesting cases are boundaries, and a boundary is worth a test that needs no
// database.

/// Rule D1. Refuse an `occurred_at` inside the fence, and tell the caller to omit the field.
///
/// A valid time this close to now repeats `created_at`, which every row carries already, so the
/// refusal destroys no information and the writes it stops are the ones where the column would say
/// nothing. It refuses a future date too: a date ahead of now is inside every window, which is why
/// there is one bound here and one message rather than two of each.
///
/// **The message never offers an older date as the fix, and that wording is the fence.** A model
/// told to send an older date sends one it invented. An invented date lands outside the window,
/// passes, and afterwards nothing can tell it from a date the owner stated, which turns a refusal
/// the owner can see into corruption nobody can find. Omission is recoverable. A fabricated
/// timestamp is not.
fn fence_occurred_at(
    occurred_at: Option<DateTime<Utc>>,
    min_age_secs: u64,
    now: DateTime<Utc>,
    content: &str,
) -> Result<()> {
    let Some(stated) = occurred_at else { return Ok(()) };
    // `try_from` rather than `as`: config refuses anything above a year, and a value that wrapped
    // negative here would widen the fence into accepting everything.
    let required = i64::try_from(min_age_secs).unwrap_or(i64::MAX);
    if now.signed_duration_since(stated).num_seconds() >= required {
        return Ok(());
    }
    // The one way through, and it exists because the fence was costing real dates. An agent
    // recording an event the day it happens could never date it, so a whole namespace of live
    // event-writing carried NULL on every row: 0 of 175 in one measured case, while the ingest
    // fill, which bypasses this, ran at 100%.
    //
    // The exemption answers the objection in the paragraph above rather than dodging it. The worry
    // is that a date nobody can check reads afterwards exactly like a date the owner stated. A date
    // written verbatim in the content is checkable forever, by anyone, against the row itself. It
    // is corroboration stored beside the claim, which is the one thing an invented timestamp can
    // never have.
    //
    // The future stays shut. A date ahead of now is refused whatever the text says, because content
    // asserting a future date is a plan rather than a record, and a future `occurred_at` reads live
    // and never reads as-of.
    if stated <= now && crate::domain::dates::states(content, stated.date_naive()) {
        return Ok(());
    }
    Err(DomainError::validation(format!(
        "occurred_at {} is inside the last {min_age_secs} seconds. Omit occurred_at. It records \
         when a fact became true in the world, and this store already records when it learned the \
         fact, so a date this recent stores one clock twice. Set it only when the user stated a \
         time that has passed, like \"since March\" or \"we moved in June\". Sending some other \
         date in place of omitting it stores a guess.",
        stated.to_rfc3339()
    )))
}

/// Rule S4, as review resolution D3 settled it. Refuse a successor that became true before the row
/// it retires.
///
/// Supersession ends the predecessor's period at the successor's start, so an inverted pair writes
/// an `occurred_until` earlier than the predecessor's `occurred_at`: a period that ends before it
/// begins. The database CHECK would reject it, and the version that gets stored says the current
/// fact stopped holding a month before it started. Approving a July proposal after an August one
/// is that case, and the queue is full of them.
///
/// Strict only. Equal dates tile the timeline once and proceed. An undated side leaves nothing to
/// compare, so it proceeds too, which is the case rule N1 covers.
fn refuse_inversion(
    successor: Option<DateTime<Utc>>,
    predecessor: Option<DateTime<Utc>>,
    target: &str,
) -> Result<()> {
    let (Some(new_at), Some(old_at)) = (successor, predecessor) else { return Ok(()) };
    if new_at >= old_at {
        return Ok(());
    }
    Err(DomainError::validation(format!(
        "this write has occurred_at {} and supersedes memory {target}, which became true at {}. \
         A replacement cannot start before the fact it replaces. If this is the older fact, write \
         it without supersedes and it will be stored beside the newer one. If the dates are the \
         wrong way round, send them the other way.",
        new_at.to_rfc3339(),
        old_at.to_rfc3339()
    )))
}

// -- the numeric guard ------------------------------------------------------------------------
//
// This is the part of Phase 4 §2 that matters more than the thresholds do. "The port is 8787" and
// "The port is 8080" are near-identical to an embedding model and are the exact case supersession
// exists for. Pure, and tested as rules, because it is the last thing standing between a
// correction and its own predecessor.

/// Why two texts must not be collapsed into one row, however high the similarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapseBlock {
    /// The digit runs differ, in order. A port, a version, a date, an amount.
    Digits,
    /// An identifier-shaped token differs: a host, a path, an address, an acronym, a dotted key.
    Identifiers,
    /// One text carries a negation the other lacks. Embeddings handle negation poorly, so a
    /// retraction scores as a restatement of the thing it retracts.
    Negation,
}

impl CollapseBlock {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Digits => "digits-differ",
            Self::Identifiers => "identifiers-differ",
            Self::Negation => "negation-differs",
        }
    }
}

/// `None` when the two texts may be collapsed, otherwise the reason they may not.
pub fn collapse_block(a: &str, b: &str) -> Option<CollapseBlock> {
    if digit_runs(a) != digit_runs(b) {
        return Some(CollapseBlock::Digits);
    }
    if identifiers(a) != identifiers(b) {
        return Some(CollapseBlock::Identifiers);
    }
    if negation_count(a) != negation_count(b) {
        return Some(CollapseBlock::Negation);
    }
    None
}

/// Maximal runs of digits, in the order they appear. Order matters: "port 8080, host 10" and
/// "port 10, host 8080" are different claims.
fn digit_runs(text: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

/// Tokens that name a thing rather than describe it, lowercased, in order.
///
/// The shapes are deliberately broad. A false positive costs one extra row when two texts differ in
/// a hyphenated word; a false negative merges a correction into the fact it corrects. Those are not
/// the same size of mistake.
fn identifiers(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(trim_edges)
        .filter(|t| identifier_shaped(t))
        .map(|t| t.to_lowercase())
        .collect()
}

fn identifier_shaped(token: &str) -> bool {
    if token.len() < 2 {
        return false;
    }
    // A path, a url, an email, a handle.
    if token.contains('@') || token.contains('/') || token.contains('\\') {
        return true;
    }
    // A dotted key, a host:port, a snake_case name. Hyphens are excluded: English prose is full of
    // them and they carry no identity on their own.
    if inner_separator(token) {
        return true;
    }
    // A token mixing letters and digits: v2, 8080ms, sha256.
    if token.chars().any(|c| c.is_ascii_digit()) && token.chars().any(|c| c.is_alphabetic()) {
        return true;
    }
    // An acronym. AWS and GCP are the same shape to an embedding model and are not the same cloud.
    token.chars().all(|c| c.is_uppercase() || c == '-') && token.chars().any(char::is_alphabetic)
}

/// True when `.`, `:` or `_` sits between two non-space characters, which is what makes a token a
/// name rather than a sentence that lost its spacing.
fn inner_separator(token: &str) -> bool {
    let chars: Vec<char> = token.chars().collect();
    chars.windows(3).any(|w| {
        matches!(w[1], '.' | ':' | '_') && w[0].is_alphanumeric() && w[2].is_alphanumeric()
    })
}

/// Sentence punctuation only. Interior punctuation is what makes an identifier an identifier, so it
/// stays.
fn trim_edges(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '.' | ','
                | ';'
                | ':'
                | '!'
                | '?'
                | '"'
                | '\''
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '“'
                | '”'
                | '‘'
                | '’'
        )
    })
}

/// Explicit negations, normalised so that "does not" and "doesn't" count the same.
///
/// Token list rather than a prefix rule. Stripping "un" or "non" would read "under", "unit" and
/// "none" as negations of words that do not exist, which is a guess dressed as a rule.
const NEGATIONS: &[&str] = &[
    "not", "no", "none", "never", "neither", "nor", "cannot", "without", "nothing", "nobody",
    "nowhere", "lacks", "lacking", "absent", "removed", "unset",
];

fn negation_count(text: &str) -> usize {
    text.split_whitespace()
        .map(|t| trim_edges(t).to_lowercase().replace(['\u{2019}', '\u{02bc}'], "'"))
        .filter(|t| NEGATIONS.contains(&t.as_str()) || t.ends_with("n't"))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restatement_of_the_same_sentence_may_collapse() {
        assert_eq!(collapse_block("Dana prefers TypeScript", "Dana prefers TypeScript"), None);
        assert_eq!(
            collapse_block("Dana prefers TypeScript.", "dana prefers typescript"),
            None,
            "casing and sentence punctuation are not identity"
        );
    }

    #[test]
    fn two_texts_whose_digits_differ_never_collapse() {
        assert_eq!(
            collapse_block("The port is 8787", "The port is 8080"),
            Some(CollapseBlock::Digits),
            "this is the case supersession exists for"
        );
    }

    #[test]
    fn digit_order_is_part_of_the_claim() {
        assert_eq!(
            collapse_block("db on 5432, cache on 6379", "db on 6379, cache on 5432"),
            Some(CollapseBlock::Digits)
        );
    }

    #[test]
    fn adding_a_number_to_a_sentence_blocks_a_collapse() {
        assert_eq!(
            collapse_block("The server restarts nightly", "The server restarts nightly at 3"),
            Some(CollapseBlock::Digits)
        );
    }

    #[test]
    fn two_texts_whose_hosts_differ_never_collapse() {
        assert_eq!(
            collapse_block("Postgres runs on db.internal", "Postgres runs on db.external"),
            Some(CollapseBlock::Identifiers)
        );
        assert_eq!(
            collapse_block("The store runs on AWS", "The store runs on GCP"),
            Some(CollapseBlock::Identifiers)
        );
        assert_eq!(
            collapse_block("Config lives in /etc/lumberroom", "Config lives in /opt/lumberroom"),
            Some(CollapseBlock::Identifiers)
        );
        assert_eq!(
            collapse_block("Mail goes to a@b.com", "Mail goes to c@b.com"),
            Some(CollapseBlock::Identifiers)
        );
    }

    #[test]
    fn a_negation_on_one_side_only_never_collapses() {
        assert_eq!(
            collapse_block("The build uses Docker", "The build does not use Docker"),
            Some(CollapseBlock::Negation)
        );
        assert_eq!(
            collapse_block("Deploys are automatic", "Deploys are never automatic"),
            Some(CollapseBlock::Negation)
        );
    }

    #[test]
    fn a_contraction_and_its_long_form_are_the_same_negation() {
        assert_eq!(
            negation_count("the build doesn't use Docker"),
            negation_count("the build does not use Docker")
        );
    }

    #[test]
    fn ordinary_prose_words_are_not_identifiers() {
        // "well-known" is hyphenated and "a" is one character; neither names a thing.
        assert!(identifiers("a well-known preference for tabs").is_empty());
    }

    #[test]
    fn plain_paraphrase_with_no_numbers_or_names_may_collapse() {
        assert_eq!(
            collapse_block("Dana prefers tabs over spaces", "Dana prefers tabs to spaces"),
            None
        );
    }

    #[test]
    fn the_guard_has_nothing_to_compare_against_an_empty_text() {
        // Why `collapse_target` refuses a row whose content this server could not read: against an
        // empty string every rule here passes, so the guard would wave a private write through into
        // a row nobody compared it to.
        assert_eq!(collapse_block("Dana prefers tabs", ""), None);
    }

    #[test]
    fn tags_are_lowercased_trimmed_and_deduplicated_in_order() {
        let tags = clean_tags(Some(vec![
            " Preference ".into(),
            "preference".into(),
            "".into(),
            "Tooling".into(),
        ]));
        assert_eq!(tags, vec!["preference", "tooling"]);
    }

    // -- the near-now fence, rule D1 ------------------------------------------------------------

    const DAY: i64 = 86_400;

    fn ago(secs: i64) -> Option<DateTime<Utc>> {
        Some(Utc::now() - chrono::Duration::seconds(secs))
    }

    #[test]
    fn a_write_carrying_no_date_is_never_the_fence_s_business() {
        assert!(
            fence_occurred_at(None, DAY as u64, Utc::now(), "a fact with no date in it").is_ok()
        );
    }

    #[test]
    fn a_date_the_owner_stated_months_ago_passes() {
        assert!(fence_occurred_at(
            ago(90 * DAY),
            DAY as u64,
            Utc::now(),
            "a fact with no date in it"
        )
        .is_ok());
        assert!(fence_occurred_at(
            ago(DAY + 1),
            DAY as u64,
            Utc::now(),
            "a fact with no date in it"
        )
        .is_ok());
        // The boundary. Exactly one window old is old enough, so an operator reading the setting
        // as "at least this old" reads it right.
        let now = Utc::now();
        let exactly = now - chrono::Duration::seconds(DAY);
        assert!(
            fence_occurred_at(Some(exactly), DAY as u64, now, "a fact with no date in it").is_ok()
        );
    }

    #[test]
    fn a_date_of_now_is_refused_because_it_repeats_created_at() {
        assert!(
            fence_occurred_at(ago(0), DAY as u64, Utc::now(), "a fact with no date in it").is_err()
        );
        assert!(fence_occurred_at(ago(3600), DAY as u64, Utc::now(), "a fact with no date in it")
            .is_err());
    }

    /// One bound covers both ends. `WRITE_MAX_FUTURE_OCCURRED_SECS` was cut for this reason: a date
    /// ahead of now is inside every window, so a second setting would refuse the same writes with a
    /// second message.
    #[test]
    fn a_future_date_is_refused_by_the_same_bound() {
        let now = Utc::now();
        let next_year = now + chrono::Duration::seconds(365 * DAY);
        assert!(fence_occurred_at(Some(next_year), DAY as u64, now, "a fact with no date in it")
            .is_err());
    }

    /// The wording is the fence. A caller told to send an older date sends one it made up, which
    /// clears the window, stores clean, and cannot be told afterwards from a date the owner
    /// actually stated. This test fails if anybody rewords the refusal into suggesting a backdate.
    #[test]
    fn the_refusal_asks_for_omission_and_never_for_an_older_date() {
        let err = fence_occurred_at(ago(60), DAY as u64, Utc::now(), "a fact with no date in it")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Omit occurred_at"), "the fix has to be stated: {err}");
        assert!(!err.contains("older"), "an older date is not the fix: {err}");
        assert!(!err.contains("earlier"), "an earlier date is not the fix: {err}");
    }

    /// The fence cost a whole namespace its dates. An agent recording an event on the day it happens
    /// could never date it, so `project:investing` carried 0 dated rows out of 175 from one client
    /// while the ingest fill, which bypasses the fence, ran at 100%. A date the content itself
    /// states is checkable against the row forever, which is what an invented timestamp never is.
    #[test]
    fn a_same_day_date_passes_when_the_content_states_it_and_is_refused_when_it_does_not() {
        let now = "2026-08-19T18:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let today = Some("2026-08-19T00:00:00Z".parse::<DateTime<Utc>>().unwrap());

        for form in [
            "the regulator approved it on 19 August 2026",
            "the regulator approved it on 2026-08-19",
            "the regulator approved it on August 19, 2026",
            "the regulator approved it on 19 Aug 2026",
        ] {
            assert!(
                fence_occurred_at(today, DAY as u64, now, form).is_ok(),
                "content states the day: {form}"
            );
        }

        // The same instant, with the day absent from the text, is the case the fence was built for.
        assert!(fence_occurred_at(today, DAY as u64, now, "the regulator approved it").is_err());
        // A different day in the text does not vouch for this one.
        assert!(fence_occurred_at(today, DAY as u64, now, "approved on 3 March 2026").is_err());
    }

    /// The exemption does not open the future, whatever the sentence claims. A future `occurred_at`
    /// reads live and never reads as-of, so the row would answer one query and not the other.
    #[test]
    fn content_naming_a_future_day_still_cannot_date_a_row_ahead_of_now() {
        let now = "2026-08-19T18:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let ahead = Some("2027-03-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert!(
            fence_occurred_at(ahead, DAY as u64, now, "the decision is due 12 March 2027").is_err()
        );
    }

    /// The exemption is a private enum behind a `pub(super)` function, so a model cannot reach it
    /// by setting a field: `WriteArgs` is deserialized from the caller's JSON and `crate::mcp` is
    /// not under `crate::services`. This reads the tool layer as text to keep it that way, the way
    /// the adapter's tests read SQL as text. If it fails, the tool layer grew a path around the
    /// fence; move the call back to `write::run` rather than editing this assertion.
    #[test]
    fn the_ingest_exemption_is_not_reachable_from_the_tool_layer() {
        let mcp = include_str!("../mcp/mod.rs");
        assert!(!mcp.contains("run_observed"), "the MCP layer must not name the unfenced entry");
        assert!(mcp.contains("write::run("), "the MCP layer writes through the fenced entry");
    }

    // -- the ordering guard, rule S4 ------------------------------------------------------------

    #[test]
    fn a_successor_older_than_the_row_it_retires_is_refused() {
        let july = ago(45 * DAY);
        let august = ago(5 * DAY);
        let err = refuse_inversion(july, august, "a1b2").unwrap_err().to_string();
        assert!(err.contains("a1b2"), "the refusal names the target: {err}");
        assert!(
            err.contains(july.unwrap().to_rfc3339().as_str())
                && err.contains(august.unwrap().to_rfc3339().as_str()),
            "both dates, so the owner can see which way round they are: {err}"
        );
    }

    #[test]
    fn a_normal_change_and_a_simultaneous_one_both_proceed() {
        assert!(refuse_inversion(ago(5 * DAY), ago(45 * DAY), "id").is_ok());
        let same = ago(DAY);
        assert!(refuse_inversion(same, same, "id").is_ok(), "equal dates tile the timeline once");
    }

    /// Rule N1. An undated side leaves nothing to compare, and refusing on a missing date would
    /// stop supersession working the way it does today for every row written before valid time.
    #[test]
    fn an_undated_side_never_trips_the_ordering_guard() {
        assert!(refuse_inversion(None, ago(45 * DAY), "id").is_ok());
        assert!(refuse_inversion(ago(45 * DAY), None, "id").is_ok());
        assert!(refuse_inversion(None, None, "id").is_ok());
    }

    #[test]
    fn an_unrecognised_sensitivity_is_refused_rather_than_treated_as_open() {
        assert!(parse_sensitivity(Some("secret")).is_err());
        assert_eq!(parse_sensitivity(Some("  ")).unwrap(), None);
        assert_eq!(parse_sensitivity(None).unwrap(), None);
        assert_eq!(parse_sensitivity(Some("private")).unwrap(), Some(Sensitivity::Private));
    }
}
