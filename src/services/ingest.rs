//! Transcript ingestion, server side. The queue, the approval path, the watermarks and the
//! anti-loop check.
//!
//! **Approval is the only path into the store, and it is one call.** `approve` hands the proposal
//! to `services::write::run`, which is what keeps the credentials-namespace refusal, the
//! classification table and the ceiling check, the grant check, the credential tripwire,
//! exact-duplicate collapse, the dedupe bands with their numeric, identifier and negation guards,
//! and supersession validation in one place. A handler that inserted a row itself would be a second
//! write path with none of those seven checks, and the difference would show up as a stored
//! credential rather than as a failing test.
//!
//! A refused write is not an error the caller has to handle. The proposal stays at `proposed` with
//! `last_error` set, and the owner reads the refusal in the queue.
//!
//! The repository arrives as an argument rather than on `Ctx`. Ingestion has no MCP tool behind it
//! and every caller is an admin route that already holds the port, so the tool path carries no
//! field it never reads.
//!
//! # `mayIngest` opens the routes and widens nothing
//!
//! Every read of the queue goes through the caller's grant, pushed into the query as a term, and a
//! fact is only accepted for a namespace the caller may read at the level the fact would be
//! written at. A client granted ingestion and `project:*` at `open` fills the queue for its
//! projects and sees that much of it; it cannot propose into `global`, cannot list the owner's
//! refusals elsewhere, and cannot block a fingerprint in a namespace it was never given.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::Ctx;
use crate::adapters::auth::{can_read, can_write};
use crate::crypto::Digester;
use crate::domain::errors::{DomainError, Result};
use crate::domain::namespaces;
use crate::domain::tripwire;
use crate::domain::types::Sensitivity;
use crate::ports::ingest::{
    EmissionHit, EmissionProbe, IngestRepository, NewProposal, NewRun, Proposal, ProposalFilter,
    ProposalSource, ProposalState, ProposalUpsert, ReadGrant, RunRecord, RunTotals, Watermark,
    WatermarkAdvance,
};

/// The speaker that can auto-approve, and the only one that may carry a quote.
pub const SPEAKER_OWNER_TYPED: &str = "owner_typed";

/// The most probes one emission check answers.
///
/// The lookup answers one bit per probe and nothing rate-limits the admin routes, so the bound on
/// how fast a caller can test guesses against the store is this number times the request rate.
/// The CLI posts facts a hundred at a time; the check is sized to match rather than to the body
/// cap, which fits thousands.
pub const MAX_EMISSION_PROBES: usize = 200;

/// The rule name a fact is refused under when its namespace sits outside the poster's grant. Rides
/// the tripwire's refusal shape so a client that counts refusals by rule keeps working.
pub const REFUSAL_OUTSIDE_GRANT: &str = "namespace_outside_grant";

/// The rule name for a namespace the store would never accept, refused before it reaches the
/// queue rather than at approval.
pub const REFUSAL_INVALID_NAMESPACE: &str = "invalid_namespace";

/// The rule name a fact is refused under when its supersession target does not exist or sits
/// outside the poster's grant. One name for both, on purpose: a proposal row that names a memory
/// holds a foreign key to it, so an insert that failed only on an unknown id would tell a
/// mayIngest-only client which uuids are real, one probe at a time.
pub const REFUSAL_SUPERSEDES_TARGET: &str = "supersedes_not_writable";

/// The rule name a fact is refused under when it is longer than `WRITE_MAX_CONTENT_CHARS`. The
/// approval would refuse it, and a queue row nobody can approve is one the owner reads for nothing.
pub const REFUSAL_TOO_LONG: &str = "content_too_long";

/// The caller's grant and the classification table, for the query to apply.
pub fn reader(ctx: &Ctx) -> ReadGrant {
    ReadGrant {
        grant: ctx.principal.read.clone(),
        levels: ctx.cfg.policy.defaults.rules().to_vec(),
    }
}

/// One normaliser, or the hashes never meet. `recall_emission.content_sha256` and
/// `ingest_proposal.fingerprint` are `Digester::digest` of the same input, and the substring check
/// below runs under the same normalisation so a quote that passes it is the one its fingerprint
/// was computed under.
pub use crate::crypto::digest::normalise;

/// The identity of a fact, and the join between a proposal and what the store already handed out.
/// `crypto::Digester::digest`, keyed under the KEK; see that module for why it is not a plain hash.
pub async fn fingerprint(ctx: &Ctx, text: &str) -> Result<String> {
    Ok(Digester::from_provider(ctx.keys.as_ref()).await?.digest(text))
}

/// Whether a fact may write itself.
///
/// Two conditions, and the second is the one that matters. A model asserting "the owner said this"
/// is not evidence, so the normalised content has to be a substring of the normalised span it came
/// from. The model gets to select and trim a sentence the owner typed. It does not get to
/// paraphrase one and call it a quote.
///
/// No span text means no auto-approval. A caller that omits it has not made the claim checkable,
/// and an unverified claim queues.
pub fn qualifies_for_auto(speaker: &str, content: &str, span_text: Option<&str>) -> bool {
    if speaker != SPEAKER_OWNER_TYPED {
        return false;
    }
    match span_text {
        Some(span) => normalise(span).contains(&normalise(content)),
        None => false,
    }
}

/// One extracted fact on its way into the queue.
#[derive(Debug, Clone)]
pub struct FactInput {
    pub content: String,
    pub namespace: String,
    pub tags: Vec<String>,
    pub supersedes: Option<Uuid>,
    pub speaker: String,
    /// The verbatim owner span, kept only for `owner_typed`. A quote on any other speaker is a
    /// claim about the owner that nothing checked.
    pub quote: Option<String>,
    /// The frozen span this fact was drawn from. The server checks the substring claim against it
    /// rather than trusting the extractor, which is why `auto` is never a request field.
    pub span_text: Option<String>,
    pub source: ProposalSource,
}

/// What happened to one fact.
///
/// Serialised tagged, so a report reads `{"outcome":"confirmed","memory_id":...}` rather than a
/// bare variant name. The wire contract is snake_case throughout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FactOutcome {
    /// A new row in the queue. `auto` says whether the approval pass will write it without asking.
    Proposed { id: Uuid, auto: bool },
    /// A fingerprint already in the queue gained a source row and nothing else.
    Reinforced { id: Uuid },
    /// The owner already answered this question. The fingerprint stays blocked.
    Blocked { id: Uuid },
    /// The store handed this content out before the transcript recorded it, so it is an echo. The
    /// memory is confirmed and no proposal exists.
    Confirmed { memory_id: Uuid },
    /// The tripwire, the poster's grant or the length cap refused it before a row could exist.
    ///
    /// `detail` carries the write path's own refusal for the length cap, which holds two numbers
    /// and no content. The tripwire's refusal never fills it: a finding's detail stays in this
    /// process. Skipped when absent, so a client that reads `rule` alone sees the old shape.
    Refused {
        rule: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PostReport {
    pub outcomes: Vec<FactOutcome>,
    pub proposals_new: i32,
    pub proposals_reinforced: i32,
    pub confirmations: i32,
    pub refused: i32,
    pub blocked: i32,
}

/// Post a batch of extracted facts.
///
/// The order is the spec's, and each step exists because the one after it would otherwise store
/// something it should not. The tripwire runs before a proposal exists, so a credential never
/// reaches a table at all. The emission check runs before the insert, so content the store itself
/// emitted becomes a confirmation rather than a fact the owner is asked to re-approve. The insert
/// is idempotent on the fingerprint, so the 808th sighting of a preference is a source row.
///
/// The CLI runs the same emission check through its own route before posting. This one is the
/// authoritative one: a client that skipped its check changes nothing here.
pub async fn post(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    extractor: &str,
    facts: Vec<FactInput>,
) -> Result<PostReport> {
    let mut report = PostReport::default();
    if facts.is_empty() {
        return Ok(report);
    }

    // One key read for the batch. The digest is what the emission lookup below joins on, so it
    // has to be the same keyed function search and bootstrap recorded with.
    let digester = Digester::from_provider(ctx.keys.as_ref()).await?;

    // The grant and the tripwire first, on every fact, before anything is looked up on the fact's
    // behalf. A proposal that never exists cannot be approved by mistake later.
    let mut screened: Vec<(FactInput, String)> = Vec::with_capacity(facts.len());
    for mut fact in facts {
        // The grant's bar is read at the level the fact would be written at: a poster that could
        // not see the row it is asking for cannot see the proposal either, and a queue entry its
        // own poster cannot read is one nobody accountable for it can read.
        // Reserved here as well as on the write path. A proposal accepted months later would
        // otherwise lay down a fresh stranded row long after the boot warning was read once.
        let Ok(namespace) = namespaces::normalize(&fact.namespace)
            .and_then(|ns| namespaces::check_writable(&ns).map(|()| ns))
        else {
            // One bad namespace refuses one fact, not the batch. The write path would refuse it
            // at approval anyway; refusing here keeps it out of the queue.
            report
                .outcomes
                .push(FactOutcome::Refused { rule: REFUSAL_INVALID_NAMESPACE, detail: None });
            report.refused += 1;
            continue;
        };
        fact.namespace = namespace;
        let level = ctx.cfg.policy.defaults.for_namespace(&fact.namespace);
        if !can_read(&ctx.principal, &fact.namespace, level) {
            tracing::info!(
                namespace = %fact.namespace,
                client = %ctx.principal.client,
                "an ingested fact named a namespace outside the poster's grant"
            );
            report
                .outcomes
                .push(FactOutcome::Refused { rule: REFUSAL_OUTSIDE_GRANT, detail: None });
            report.refused += 1;
            continue;
        }
        if ctx.cfg.policy.tripwire {
            // The quote is scanned beside the content because the quote is stored. An extractor
            // handed a clean sentence and a span holding a credential can put the sentence in
            // `content` and the credential in `quote`, and the row keeps it: `ingest show` prints
            // it and a rejection does not remove it. `span_text` is compared and discarded, so it
            // never needs this.
            let subject = match fact.quote.as_deref() {
                Some(quote) => format!("{}\n{quote}", fact.content),
                None => fact.content.clone(),
            };
            if let Some(finding) = tripwire::scan(&subject) {
                // The finding's detail is safe by contract and its matched text is not carried at
                // all. Only the rule name travels, here and into the log.
                tracing::warn!(
                    rule = %finding.rule,
                    namespace = %fact.namespace,
                    "credential tripwire refused an ingested fact before it became a proposal"
                );
                report.outcomes.push(FactOutcome::Refused { rule: finding.rule, detail: None });
                report.refused += 1;
                continue;
            }
        }
        // The approval writes through the configured cap, so the post checks the same number.
        // After the tripwire, so a long credential reports as a credential.
        if let Some(refusal) =
            super::write::length_refusal(&fact.content, ctx.cfg.policy.max_content_chars)
        {
            report
                .outcomes
                .push(FactOutcome::Refused { rule: REFUSAL_TOO_LONG, detail: Some(refusal) });
            report.refused += 1;
            continue;
        }
        if let Some(target) = fact.supersedes {
            // Checked here and again at approval. Approval runs the full rule (head of chain,
            // valid time); this pass exists because the proposal row references the target from
            // the moment it is queued, and a row that pins a memory the poster cannot write is a
            // row that should never have existed.
            if !supersedes_target_writable(ctx, target).await? {
                tracing::info!(
                    client = %ctx.principal.client,
                    "an ingested fact named a supersession target outside the poster's grant"
                );
                report
                    .outcomes
                    .push(FactOutcome::Refused { rule: REFUSAL_SUPERSEDES_TARGET, detail: None });
                report.refused += 1;
                continue;
            }
        }
        let hash = digester.digest(&fact.content);
        screened.push((fact, hash));
    }

    let hits = emission_hits(ctx, repo, &screened).await?;

    for (fact, hash) in screened {
        // The lookup already applied the grant to the row each emission names. Checked again here
        // because this is where a row gets touched on the caller's say-so, and a stamp on a row
        // the caller cannot read is the one effect of this path that outlives the request.
        let hit = hits
            .iter()
            .find(|h| h.content_sha256 == hash)
            .filter(|h| can_read(&ctx.principal, &h.namespace, h.sensitivity));
        if let Some(hit) = hit {
            // The store emitted this content before the transcript recorded it, so the transcript
            // is quoting the store back at itself. Repetition is confirmation, and confirm can
            // neither create a row nor change one's content: it is the same metadata touch the
            // exact-duplicate path in write::run already performs.
            confirm(ctx, hit.memory_id).await;
            report.outcomes.push(FactOutcome::Confirmed { memory_id: hit.memory_id });
            report.confirmations += 1;
            continue;
        }

        // Two conditions on the claim and one on the claimant. The span arrived in the same
        // request as the content, so the substring check binds an honest extractor and nobody
        // else; what binds a poster is whether it could have written the row itself. A client
        // without write on the namespace gets a proposal the owner reads, never a badge that says
        // the server vouched for it.
        let level = ctx.cfg.policy.defaults.for_namespace(&fact.namespace);
        let auto = qualifies_for_auto(&fact.speaker, &fact.content, fact.span_text.as_deref())
            && can_write(&ctx.principal, &fact.namespace, level);
        let quote = match fact.speaker == SPEAKER_OWNER_TYPED {
            true => fact.quote.clone(),
            false => None,
        };

        let upsert = repo
            .insert_proposal(
                ctx.tenant(),
                NewProposal {
                    fingerprint: hash,
                    content: fact.content.clone(),
                    namespace: fact.namespace.clone(),
                    tags: fact.tags.clone(),
                    supersedes: fact.supersedes,
                    speaker: fact.speaker.clone(),
                    quote,
                    auto,
                    extractor: extractor.to_string(),
                    posted_by: ctx.principal.client.clone(),
                    source: fact.source.clone(),
                },
            )
            .await?;

        match upsert {
            ProposalUpsert::Created(p) => {
                report.outcomes.push(FactOutcome::Proposed { id: p.id, auto: p.auto });
                report.proposals_new += 1;
            }
            ProposalUpsert::Existing(p) => match ProposalState::parse(&p.state) {
                Some(ProposalState::Written) => {
                    // An exact restatement of a fact already written. The same thing
                    // `memory_write` does on an exact duplicate, and it counts as reinforcement.
                    if let Some(memory_id) = p.memory_id {
                        confirm(ctx, memory_id).await;
                    }
                    report.outcomes.push(FactOutcome::Reinforced { id: p.id });
                    report.proposals_reinforced += 1;
                }
                Some(ProposalState::Rejected) => {
                    // A queue that re-asks a question the owner already answered is a queue the
                    // owner stops opening. The source row landed; the row stays rejected.
                    report.outcomes.push(FactOutcome::Blocked { id: p.id });
                    report.blocked += 1;
                }
                _ => {
                    report.outcomes.push(FactOutcome::Reinforced { id: p.id });
                    report.proposals_reinforced += 1;
                }
            },
        }
    }

    Ok(report)
}

/// The tripwire as a service, because the CLI cannot call it in process.
///
/// Rule names only. The finding's detail is written to exclude the matched text, and even so it
/// stays here: this answer travels back to a client that is about to write it into a run report.
pub fn scan(texts: &[String]) -> Vec<Option<&'static str>> {
    texts.iter().map(|t| tripwire::scan(t).map(|f| f.rule)).collect()
}

/// The emission check as a read-only service, for `--dry-run` and the report.
///
/// The same query the post path runs, so a dry run cannot disagree with what a post will do.
/// Capped at `MAX_EMISSION_PROBES` per call: this is the one route that answers a yes or no about
/// content the caller chose, and the cap is what bounds how fast guesses can be tested. The post
/// path batches internally instead, because its probes are facts it is about to queue anyway.
pub async fn check_emissions(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    probes: &[EmissionProbe],
) -> Result<Vec<EmissionHit>> {
    if probes.len() > MAX_EMISSION_PROBES {
        return Err(DomainError::validation(format!(
            "at most {MAX_EMISSION_PROBES} probes per check, got {}",
            probes.len()
        )));
    }
    lookup_emissions(ctx, repo, probes).await
}

/// Whether each probe is an echo, in probe order. What the HTTP route answers, and all it answers:
/// a hit carries the id and namespace of a row, and those belong to the row's readers rather than
/// to whoever guessed its text.
pub fn echoes(probes: &[EmissionProbe], hits: &[EmissionHit]) -> Vec<bool> {
    probes.iter().map(|p| hits.iter().any(|h| h.content_sha256 == p.content_sha256)).collect()
}

async fn lookup_emissions(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    probes: &[EmissionProbe],
) -> Result<Vec<EmissionHit>> {
    repo.emissions_matching(
        ctx.tenant(),
        probes,
        ctx.cfg.ingest.emission_slack_secs,
        ctx.cfg.ingest.emission_window_secs(),
        &ctx.principal.read,
    )
    .await
}

/// Record that a tool handed this content out, one row at a time.
///
/// Hashed through the same normaliser as a proposal's fingerprint, which is the only reason the two
/// can ever meet. `context_bootstrap` and `memory_search` do not come through here: they hand whole
/// result sets to `MemoryRepository::record_emissions`, which is batched and fire and forget, so a
/// read never waits on the record and never fails because of it. This path stays for a caller that
/// holds one row and can afford to wait for the answer.
pub async fn record_emission(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    content: &str,
    memory_id: Uuid,
    tool: &str,
) -> Result<()> {
    repo.record_emission(
        ctx.tenant(),
        &fingerprint(ctx, content).await?,
        memory_id,
        tool,
        ctx.session_id.as_deref(),
    )
    .await
}

/// What an approval did.
///
/// A refusal is a value rather than an `Err`, because one refused row must not stop a batch of
/// three hundred. The refusal is already on the proposal by the time this returns.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApproveOutcome {
    pub id: Uuid,
    pub memory_id: Option<Uuid>,
    /// `write::run` collapsed the content into an existing row. That is a success, and the report
    /// says `deduplicated`.
    pub deduplicated: bool,
    pub refused: Option<String>,
}

/// The earliest moment any source stated this fact. `approve` fills `occurred_at` from it.
///
/// Earliest rather than latest: a restatement is confirmation and `last_confirmed_at` already
/// carries that. A proposal whose sources all hold a NULL `observed_at` returns `None`, and the
/// `None` has to survive to the column. Substituting `now()` would stamp the approval clock onto a
/// valid-time column, on the exact rows valid time exists for.
pub fn earliest_observation(sources: &[ProposalSource]) -> Option<DateTime<Utc>> {
    sources.iter().filter_map(|s| s.observed_at).min()
}

/// Order a batch oldest fact first, undated proposals last.
///
/// The ordering is a correctness property rather than a tidier report. Supersession ends the
/// predecessor's period at the successor's start, so approving a July proposal after the August one
/// it supersedes hides the August row and serves July as current truth. Nothing in this tree
/// un-supersedes a row, so that damage stays. Sorting by the same `min(observed_at)` the fill uses
/// removes the trigger for almost every pair in a transcript backfill, where both proposals came
/// out of the same week of files.
///
/// Undated proposals go last. An unknown date cannot be shown to precede a known one, and last is
/// where it costs least: an undated successor ends its predecessor's period at its own
/// `created_at`, which is now, and now sits after every dated row in the batch. The sort is stable,
/// so ties and the undated tail keep the order the caller gave.
pub fn approval_order(keyed: &[(Uuid, Option<DateTime<Utc>>)]) -> Vec<Uuid> {
    let mut keyed = keyed.to_vec();
    keyed.sort_by_key(|(_, observed)| (observed.is_none(), *observed));
    keyed.into_iter().map(|(id, _)| id).collect()
}

/// Approve one proposal. The only path from the queue into the store.
///
/// Everything a model's `memory_write` goes through, this goes through, because it is the same
/// call. The proposal supplies content, namespace, tags and its supersession target, and no
/// sensitivity override: a proposal that could choose its own level would be a way to write a
/// private fact at open.
pub async fn approve(ctx: &Ctx, repo: &dyn IngestRepository, id: Uuid) -> Result<ApproveOutcome> {
    let proposal = repo
        .proposal(ctx.tenant(), id, &reader(ctx))
        .await?
        .ok_or_else(|| DomainError::not_found(format!("proposal {id} does not exist")))?;

    match ProposalState::parse(&proposal.state) {
        Some(ProposalState::Written) => {
            // Approving twice is a person pressing the key twice, not an error. Report the row that
            // already exists rather than writing a second one.
            return Ok(ApproveOutcome {
                id,
                memory_id: proposal.memory_id,
                deduplicated: true,
                refused: None,
            });
        }
        Some(ProposalState::Rejected) => {
            return Err(DomainError::conflict(format!(
                "proposal {id} was rejected. `lumberroom ingest unreject {id}` returns it to the queue."
            )))
        }
        Some(ProposalState::Proposed) => {}
        None => {
            return Err(DomainError::internal(format!(
                "proposal {id} holds an unknown state {:?}",
                proposal.state
            )))
        }
    }

    // The fill, and what it means. `occurred_at` takes the earliest moment a source was recorded
    // stating this fact. That is when the owner said it. It is not when the fact became true: a
    // July transcript reading "we moved in June" stores July, and June is nowhere in the data.
    // The value is an upper bound and the tightest one this store holds, since a fact is true no
    // later than the first time somebody states it. Read it as the instant the fact began and every
    // retrospective sentence in the queue comes out misdated, which is the failure valid time was
    // added to fix, one level up.
    let occurred_at = earliest_observation(&repo.proposal_sources(ctx.tenant(), id).await?);

    let supersedes = proposal.supersedes.map(|s| s.to_string());
    // `run_observed` rather than `run`, and the difference is the near-now fence. That fence
    // refuses a valid time inside a day of now, because a model passing one is almost always
    // stamping today onto a fact it merely heard today. A transcript span carries a real
    // observation, and a conversation from this morning is exactly the case the fence would
    // refuse, so approval takes the entry point the tool layer cannot reach. Every other check in
    // the write path still runs.
    let written = super::write::run_observed(
        ctx,
        &proposal.content,
        &proposal.namespace,
        Some(proposal.tags.clone()),
        supersedes.as_deref(),
        None,
        occurred_at,
    )
    .await;

    match written {
        Ok(outcome) => {
            let memory_id = Uuid::parse_str(&outcome.id)
                .map_err(|_| DomainError::internal("write returned an id that is not a uuid"))?;
            repo.mark_written(ctx.tenant(), id, memory_id).await?;
            Ok(ApproveOutcome {
                id,
                memory_id: Some(memory_id),
                deduplicated: outcome.deduplicated,
                refused: None,
            })
        }
        Err(e) => {
            // The tripwire, the ceiling, a missing KEK or a superseded target. The row stays at
            // proposed and carries the reason, so the owner reads a refusal instead of finding a
            // row that stopped moving. `client_message` is the text written for a person and it
            // never repeats a matched secret.
            let message = e.client_message().to_string();
            repo.mark_error(ctx.tenant(), id, &message).await?;
            tracing::info!(proposal = %id, "approval refused by the write path");
            Ok(ApproveOutcome { id, memory_id: None, deduplicated: false, refused: Some(message) })
        }
    }
}

/// Approve several proposals, carrying on past a refusal.
///
/// A backfill queue runs to hundreds of rows, and a batch that stopped at the first refusal would
/// leave the owner approving the rest one at a time.
///
/// The outcomes come back in approval order rather than the caller's, and each one names its id.
pub async fn approve_all(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    ids: &[Uuid],
) -> Result<Vec<ApproveOutcome>> {
    // Order first. `approval_order` carries why a batch approved in the caller's order can hide a
    // fact behind an older one. The cost is one indexed read per row against the same source rows
    // `approve` folds again, and the batch has no other defence.
    //
    // A read that fails here fails the whole call, unlike a refusal further down. Every key is read
    // before the first approval, so nothing has been written yet, and approving in an order this
    // loop could not establish is the outcome the ordering exists to prevent.
    let mut keyed = Vec::with_capacity(ids.len());
    for id in ids {
        // An id that names nothing has no sources and sorts undated. `approve` reports it as a
        // refusal below, which is where a vanished proposal belongs.
        let observed = earliest_observation(&repo.proposal_sources(ctx.tenant(), *id).await?);
        keyed.push((*id, observed));
    }

    let mut out = Vec::with_capacity(ids.len());
    for id in approval_order(&keyed) {
        match approve(ctx, repo, id).await {
            Ok(outcome) => out.push(outcome),
            // A proposal that vanished or was rejected between the list and the approval is a
            // refusal of that row, not of the batch.
            Err(e) => out.push(ApproveOutcome {
                id,
                memory_id: None,
                deduplicated: false,
                refused: Some(e.client_message().to_string()),
            }),
        }
    }
    Ok(out)
}

/// The auto-approval pass at the end of a run: every queued proposal this run created whose
/// `auto` the server set itself.
///
/// The filter is on the stored column, never on anything the client sends. `auto` was decided at
/// insert against the frozen span and is never recomputed, so a row the owner is reading cannot
/// gain the right to write itself while he reads it.
///
/// The batch order comes from `approve_all`, and this path needs it most: `list_proposals` returns
/// newest first, so the ids below arrive in the one order that lets a newer fact be superseded by
/// an older one.
pub async fn approve_auto(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    run_id: Uuid,
) -> Result<Vec<ApproveOutcome>> {
    let queued = repo
        .list_proposals(
            ctx.tenant(),
            ProposalFilter {
                state: Some(ProposalState::Proposed.as_str().to_string()),
                run_id: Some(run_id),
                auto: Some(true),
                limit: 1000,
                reader: reader(ctx),
                ..Default::default()
            },
        )
        .await?;
    let ids: Vec<Uuid> = queued.iter().map(|p| p.id).collect();
    approve_all(ctx, repo, &ids).await
}

/// The queue as this caller may read it. The grant replaces whatever the filter carried: a caller
/// does not get to name its own reader.
pub async fn list(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    filter: ProposalFilter,
) -> Result<Vec<Proposal>> {
    let filter = ProposalFilter { reader: reader(ctx), ..filter };
    repo.list_proposals(ctx.tenant(), filter).await
}

/// One proposal with every source that stated it, which is what makes "have I already counted this"
/// an exact answer rather than a similarity guess.
///
/// An id outside the grant answers the same `not_found` as one that does not exist. The sources
/// name transcript files on disk, and they are read only once the proposal itself has passed.
pub async fn show(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    id: Uuid,
) -> Result<(Proposal, Vec<ProposalSource>)> {
    let proposal = visible(ctx, repo, id).await?;
    let sources = repo.proposal_sources(ctx.tenant(), id).await?;
    Ok((proposal, sources))
}

/// The proposal, if this caller may read it. One error for "missing" and for "not yours".
async fn visible(ctx: &Ctx, repo: &dyn IngestRepository, id: Uuid) -> Result<Proposal> {
    repo.proposal(ctx.tenant(), id, &reader(ctx))
        .await?
        .ok_or_else(|| DomainError::not_found(format!("proposal {id} does not exist")))
}

/// The strongest speaker across a proposal's sources, with the quote that came with it.
///
/// Computed on read and never written onto the parent row. The parent's speaker is frozen at first
/// insert, and this is how `show` still tells the owner that the fact he is looking at was also
/// typed by him somewhere.
pub fn strongest_speaker(sources: &[ProposalSource]) -> Option<&ProposalSource> {
    sources.iter().max_by_key(|s| speaker_rank(&s.speaker))
}

fn speaker_rank(speaker: &str) -> u8 {
    match speaker {
        SPEAKER_OWNER_TYPED => 5,
        "main_model" => 4,
        "subagent" => 3,
        "tool_returned" => 2,
        "hook_injected" => 1,
        _ => 0,
    }
}

/// Reject a proposal. The reason is logged rather than stored: the schema has no column for it, and
/// inventing one here would put the queue's shape out of step with the spec.
///
/// Inside the grant only. A rejection blocks its fingerprint for good, so a client that could
/// reject in a namespace it cannot read could keep a fact out of that namespace forever.
pub async fn reject(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    id: Uuid,
    reason: Option<&str>,
) -> Result<bool> {
    let proposal = visible(ctx, repo, id).await?;
    let done = repo.reject(ctx.tenant(), id).await?;
    if done {
        tracing::info!(proposal = %id, reason = reason.unwrap_or(""), "proposal rejected");
        // A rejection keeps the fingerprint and needs nothing else: the Blocked branch of `post`
        // reads state alone, and `unreject` returns a row the owner then re-reads from its
        // sources. For a namespace that would have sealed the fact, leaving the sentence in the
        // queue table stores in the clear the one thing the owner just refused to store at all.
        // Migration 000018's trigger covers the written and forgotten cases; it cannot see this
        // one, because the level of a namespace is resolved here and not in SQL.
        if ctx.cfg.policy.defaults.for_namespace(&proposal.namespace) > Sensitivity::Open {
            repo.clear_text(ctx.tenant(), id).await?;
        }
    }
    Ok(done)
}

pub async fn unreject(ctx: &Ctx, repo: &dyn IngestRepository, id: Uuid) -> Result<bool> {
    visible(ctx, repo, id).await?;
    repo.unreject(ctx.tenant(), id).await
}

/// One file, as `submit` sees it after merging the extractor's output.
#[derive(Debug, Clone)]
pub struct FileAdvance {
    pub file_path: String,
    pub session_id: Option<String>,
    pub is_sidechain: bool,
    /// The byte ceiling this run froze at plan time. Nothing beyond it was read.
    pub plan_ceiling: i64,
    /// Hash of bytes `[0, plan_ceiling)`, so a file rewritten in place is caught next run.
    pub prefix_sha256: String,
    pub entries_seen: i64,
    /// The first byte of every span from this file that came back missing or failed. Empty means
    /// every span was extracted, or that the file produced no span at all.
    pub unextracted_from: Vec<i64>,
}

/// A file whose watermark stopped short, and by how much.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HeldBack {
    pub file: String,
    pub held_at: i64,
    pub ceiling: i64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct WatermarkReport {
    /// File and the offset that is stored now, which is not always the offset this run asked for.
    pub advanced: Vec<(String, i64)>,
    pub held_back: Vec<HeldBack>,
}

/// Move the watermarks at the end of a run. The one place this pipeline can lose data.
///
/// A file advances to the first byte of the **earliest** span that landed in a missing or failed
/// chunk, and to `plan_ceiling` only when every span came back. Advancing past every span that
/// happened to be extracted is the failure with no recovery: chunks cap at 40 spans, so a
/// substantial file spreads across many of them, and killing extraction at chunk 400 leaves a file
/// with spans in chunks 398 to 405 partly extracted. Advancing that file to the ceiling means the
/// bytes behind chunks 401 to 405 were never extracted and are never planned again.
///
/// **A file with no unextracted span still advances.** Those bytes were read, classified and
/// excluded, so nothing is left to extract. Most of the corpus is this case, and a rule that held
/// it back would stall the watermark on almost every file forever.
///
/// The advance stays monotonic in the repository, so a hold-back is a smaller advance rather than a
/// rewind: an overlapping run that already extracted those bytes keeps its progress.
pub async fn advance_watermarks(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    run_id: Uuid,
    files: &[FileAdvance],
) -> Result<WatermarkReport> {
    let mut report = WatermarkReport::default();

    for file in files {
        let earliest = file.unextracted_from.iter().copied().filter(|b| *b >= 0).min();
        let target = match earliest {
            Some(byte) => byte.min(file.plan_ceiling),
            None => file.plan_ceiling,
        };

        let stored = repo
            .advance_watermark(
                ctx.tenant(),
                WatermarkAdvance {
                    file_path: file.file_path.clone(),
                    session_id: file.session_id.clone(),
                    is_sidechain: file.is_sidechain,
                    byte_offset: target,
                    prefix_sha256: file.prefix_sha256.clone(),
                    entries_seen: file.entries_seen,
                    run_id,
                },
            )
            .await?;

        report.advanced.push((file.file_path.clone(), stored));
        if target < file.plan_ceiling {
            // Named, counted and printed. The owner learns that bytes are pending from the report
            // rather than from a proposal that never arrives.
            report.held_back.push(HeldBack {
                file: file.file_path.clone(),
                held_at: target,
                ceiling: file.plan_ceiling,
            });
        }
    }

    Ok(report)
}

pub async fn watermarks(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    skipped_only: bool,
) -> Result<Vec<Watermark>> {
    repo.watermarks(ctx.tenant(), skipped_only).await
}

pub async fn unskip(ctx: &Ctx, repo: &dyn IngestRepository, file_path: &str) -> Result<bool> {
    repo.clear_skip(ctx.tenant(), file_path).await
}

/// Stamp the files this run created itself, so the next run does not ingest its own output.
pub async fn skip_artifacts(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    run_id: Uuid,
    files: &[String],
    reason: &str,
) -> Result<()> {
    for file in files {
        repo.set_skip(ctx.tenant(), file, reason, run_id).await?;
    }
    Ok(())
}

pub async fn open_run(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    extractor: &str,
    scope: serde_json::Value,
) -> Result<Uuid> {
    let run = NewRun { id: Uuid::new_v4(), scope, extractor: extractor.to_string() };
    repo.open_run(ctx.tenant(), run).await
}

pub async fn close_run(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    id: Uuid,
    totals: RunTotals,
) -> Result<()> {
    repo.close_run(ctx.tenant(), id, totals).await
}

pub async fn run_report(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    id: Uuid,
) -> Result<Option<RunRecord>> {
    repo.run(ctx.tenant(), id).await
}

/// Build the probes and ask in batches of the cap. A per-fact query would turn a batch of three
/// hundred into three hundred round trips for a check that hits almost never.
async fn emission_hits(
    ctx: &Ctx,
    repo: &dyn IngestRepository,
    screened: &[(FactInput, String)],
) -> Result<Vec<EmissionHit>> {
    let probes: Vec<EmissionProbe> = screened
        .iter()
        .map(|(fact, hash)| EmissionProbe {
            content_sha256: hash.clone(),
            // A span with no timestamp is checked against now, which is the strictest reading
            // available: an emission after this moment cannot have caused it. The same default is
            // wrong in `earliest_observation`, where a missing timestamp means no known valid time
            // and `now()` would write the approval clock into `occurred_at`.
            observed_at: fact.source.observed_at.unwrap_or_else(Utc::now),
        })
        .collect();
    let mut hits = Vec::new();
    for batch in probes.chunks(MAX_EMISSION_PROBES) {
        hits.extend(lookup_emissions(ctx, repo, batch).await?);
    }
    Ok(hits)
}

/// Whether the poster could retire this row itself: read and write on it at its stored level.
///
/// A missing row and a row outside the grant answer the same `false`. The refusal never names the
/// target's namespace, for the reason `write::validate_supersedes` gives: saying so would tell a
/// client that a namespace it cannot read exists.
async fn supersedes_target_writable(ctx: &Ctx, target: Uuid) -> Result<bool> {
    Ok(ctx.repos.memories.find_by_id(ctx.tenant(), target).await?.is_some_and(|t| {
        can_read(&ctx.principal, &t.namespace, t.sensitivity)
            && can_write(&ctx.principal, &t.namespace, t.sensitivity)
    }))
}

/// Repetition is confirmation, and it is never worth failing an ingest over.
async fn confirm(ctx: &Ctx, memory_id: Uuid) {
    if let Err(e) = ctx.repos.memories.confirm(ctx.tenant(), memory_id).await {
        let reason = e.log_message();
        tracing::warn!(id = %memory_id, error = %reason, "could not record a confirmation");
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn the_normaliser_ignores_case_spacing_and_terminal_punctuation() {
        assert_eq!(normalise("  The Port  is 8787. "), "the port is 8787");
        assert_eq!(normalise("the port is 8787"), normalise("The\tport\nis 8787!"));
    }

    #[test]
    fn interior_punctuation_survives_because_it_carries_identity() {
        assert_eq!(normalise("db.internal:5432 is the host"), "db.internal:5432 is the host");
    }

    #[test]
    fn only_an_owner_typed_span_containing_the_fact_auto_approves() {
        let span = "I always run Postgres on port 5433 on the dev box, never 5432.";
        assert!(qualifies_for_auto(
            SPEAKER_OWNER_TYPED,
            "I always run Postgres on port 5433 on the dev box",
            Some(span)
        ));
    }

    #[test]
    fn a_paraphrase_of_an_owner_span_queues() {
        let span = "I always run Postgres on port 5433 on the dev box.";
        assert!(!qualifies_for_auto(
            SPEAKER_OWNER_TYPED,
            "the dev box runs Postgres on 5433",
            Some(span)
        ));
    }

    #[test]
    fn a_model_speaker_never_auto_approves_however_exact_the_quote() {
        let span = "the dev box runs Postgres on 5433";
        assert!(!qualifies_for_auto("main_model", "the dev box runs Postgres on 5433", Some(span)));
        assert!(!qualifies_for_auto("subagent", "the dev box runs Postgres on 5433", Some(span)));
    }

    #[test]
    fn a_missing_span_queues_rather_than_trusting_the_claim() {
        assert!(!qualifies_for_auto(SPEAKER_OWNER_TYPED, "the dev box runs Postgres", None));
    }

    #[test]
    fn echoes_answer_one_bit_per_probe_in_probe_order() {
        let probe =
            |hash: &str| EmissionProbe { content_sha256: hash.into(), observed_at: Utc::now() };
        let hit = |hash: &str| EmissionHit {
            content_sha256: hash.into(),
            memory_id: Uuid::nil(),
            namespace: "user:me".into(),
            sensitivity: crate::domain::types::Sensitivity::Open,
            tool: "memory_search".into(),
            first_emitted_at: Utc::now(),
        };
        let probes = vec![probe("a"), probe("b"), probe("a"), probe("c")];
        let hits = vec![hit("a"), hit("c")];
        assert_eq!(echoes(&probes, &hits), vec![true, false, true, true]);
        assert_eq!(echoes(&[], &hits), Vec::<bool>::new());
    }

    #[test]
    fn the_probe_cap_is_a_small_multiple_of_the_post_batch() {
        // The CLI posts a hundred facts per request. A cap below that would make the authoritative
        // path batch every post; a cap in the thousands would hand a guesser the body limit.
        assert!(MAX_EMISSION_PROBES >= 100);
        assert!(MAX_EMISSION_PROBES <= 500);
    }

    #[test]
    fn the_strongest_speaker_across_sources_is_the_owner_when_one_source_is_his() {
        let sources = vec![source("main_model"), source("owner_typed"), source("subagent")];
        assert_eq!(strongest_speaker(&sources).map(|s| s.speaker.as_str()), Some("owner_typed"));
    }

    #[test]
    fn the_fill_takes_the_earliest_observation_across_the_sources() {
        let sources = vec![
            dated(Some(day(8, 14))),
            dated(None),
            dated(Some(day(7, 2))),
            dated(Some(day(8, 1))),
        ];
        assert_eq!(earliest_observation(&sources), Some(day(7, 2)));
    }

    #[test]
    fn a_proposal_with_no_dated_source_fills_nothing_rather_than_now() {
        assert_eq!(earliest_observation(&[dated(None), dated(None)]), None);
        assert_eq!(earliest_observation(&[]), None);
    }

    #[test]
    fn the_batch_approves_the_oldest_fact_first() {
        let august = Uuid::from_u128(1);
        let july = Uuid::from_u128(2);
        let june = Uuid::from_u128(3);
        let keyed =
            vec![(august, Some(day(8, 14))), (july, Some(day(7, 2))), (june, Some(day(6, 30)))];
        assert_eq!(approval_order(&keyed), vec![june, july, august]);
    }

    #[test]
    fn undated_proposals_go_last_and_keep_the_order_they_arrived_in() {
        let first_undated = Uuid::from_u128(1);
        let dated_row = Uuid::from_u128(2);
        let second_undated = Uuid::from_u128(3);
        let keyed =
            vec![(first_undated, None), (dated_row, Some(day(7, 2))), (second_undated, None)];
        assert_eq!(approval_order(&keyed), vec![dated_row, first_undated, second_undated]);
    }

    #[test]
    fn two_proposals_stated_at_the_same_moment_keep_the_order_they_arrived_in() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let keyed = vec![(first, Some(day(7, 2))), (second, Some(day(7, 2)))];
        assert_eq!(approval_order(&keyed), vec![first, second]);
    }

    fn day(month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, month, day, 9, 0, 0).unwrap()
    }

    fn dated(observed_at: Option<DateTime<Utc>>) -> ProposalSource {
        ProposalSource { observed_at, ..source("main_model") }
    }

    fn source(speaker: &str) -> ProposalSource {
        ProposalSource {
            source_key: format!("/tmp/a.jsonl#{speaker}"),
            file_path: "/tmp/a.jsonl".into(),
            session_id: None,
            is_sidechain: false,
            entry_uuid: None,
            speaker: speaker.into(),
            observed_at: None,
            run_id: Uuid::nil(),
        }
    }
}
