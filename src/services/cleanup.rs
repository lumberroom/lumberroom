//! The periodic pass that reads the store as a whole, and the queue it writes into.
//!
//! Every other part of this system looks at one write at a time. `memory_write` checks the row in
//! front of it for a near-duplicate and flags a conflict; the review queue lists pairs above a
//! threshold. Nothing steps back and asks what the store as a whole now contains, and after a month
//! that is where the damage is: four rows a test harness wrote, an injection probe from a security
//! run, two values for the same nickname, and a preference stated twice in different words. None of
//! those is a bug in any write path.
//!
//! # It proposes and never acts
//!
//! The same rule ingestion follows, for the same reason: a personal memory that silently forgets is
//! worse than one that gets cluttered. Applying goes through `review::supersede` and
//! `review::delete`, which already hold the grant check, the ceiling check and the history rules.
//!
//! # What a model is ever shown
//!
//! Rows at `open`. The candidate query runs at the caller's ceiling so that the deterministic half
//! can see private duplicates, and the band handed out for a model is filtered on the way out:
//! a pair with a side above `open` is skipped before it reaches the list. Decision 0005 draws the
//! same line for the lexical index on the same argument: publishing private content to a second
//! system is publishing it. `assert_model_visible` is a second check at the boundary, and it is
//! deliberate redundancy rather than the mechanism.
//!
//! # Whose rows a run reads
//!
//! The scheduled pass reads the whole tenant and hands nothing to anyone, so it runs over
//! `NamespaceGrant::everything()`. An HTTP caller runs over its own read grant, pushed into the
//! query as a term, and a scope it names has to sit inside that grant. `mayIngest` opens the
//! route; it does not widen what the route may read.
//!
//! # The deterministic pass
//!
//! Exact duplicates and the near-certain cosine band need no model and no network. They run first,
//! they run over every sensitivity the caller holds because nothing they read leaves this machine,
//! and on a store this size they are the majority of the findings.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::Ctx;
use crate::adapters::auth::can_read;
use crate::domain::cleanup::{model_may_see, ApplyRefusal, CleanupKind, Disposition};
use crate::domain::errors::{DomainError, Result};
use crate::domain::namespaces;
use crate::domain::policy::NamespaceGrant;
use crate::domain::types::Sensitivity;
use crate::ports::cleanup::{
    Candidate, CandidateQuery, CleanupRepository, NewMember, NewProposal, Proposal, QueueOutcome,
};

/// Above this, two rows are the same fact and the pass says so without asking anyone.
///
/// A guess, and labelled one. Phase 4 set 0.97 for duplicate collapse on write from the same guess,
/// and nobody has read real pairs with their scores to check it. Every proposal publishes the
/// similarity that produced it precisely so that reading the queue is the calibration.
pub const NEAR_CERTAIN: f64 = 0.97;

/// Below this, two rows are not worth a model's attention.
///
/// The band between here and `NEAR_CERTAIN` is what a model is asked about: close enough to be
/// suspicious, not close enough to act on.
///
/// 0.65, lowered from the Phase 4 spec's 0.85 on 21 August 2026 by the only real measurement anyone
/// has taken of it. The owner's store held one duplicate a person had found by reading, his two
/// statements of the same image-generation preference, and it scores **0.694** against
/// `bge-base-en-v1.5`. At 0.85 the pass never showed it to a model, and never would have.
///
/// The cost of the lower floor, measured on the same store the same day: 19 pairs instead of 1,
/// three calls to `glm-5.3`, 4,967 prompt and 1,109 completion tokens for the whole store. The
/// model called 17 of the 19 unrelated and was right about all 17.
///
/// This is one store, one embedder and one duplicate, so it is a better guess rather than a
/// calibrated number. Reverse it if the queue starts filling with pairs the owner rejects: every
/// proposal carries the similarity that produced it, so the queue itself is the evidence.
pub const WORTH_ASKING: f64 = 0.65;

/// How old an unread row has to be before the pass mentions it.
pub const STALE_DAYS: i64 = 90;

/// What one run did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunReport {
    pub scope: String,
    pub cadence: String,
    /// Rows the run was responsible for: created since the watermark, or all of them on a first run.
    pub anchored: usize,
    pub exact_groups: usize,
    pub near_certain_pairs: usize,
    pub stale_rows: usize,
    pub queued: usize,
    /// Clusters already in the queue, in any state. A second run over the same window reports every
    /// finding here and queues nothing, which is what makes an hourly cadence safe.
    pub already_known: usize,
    pub closed_as_answered: usize,
    /// Pairs in the band a model would be asked about. The deterministic pass counts them and
    /// leaves them; `lumberroom cleanup run` is what takes them to a provider.
    pub for_the_model: usize,
    /// Pairs in the same band with a side above `open`. Counted so a run over a store that is
    /// mostly private does not read as a clean store, and never listed: what makes them
    /// uncountable for the model is the reason they are not here.
    pub withheld_from_model: usize,
    /// The floor actually used, which is not always the one asked for.
    pub min_similarity: f64,
    pub through: Option<DateTime<Utc>>,
    /// A query hit its limit, so this run did not read to the end of the store and its watermark
    /// stayed where the findings did. Reported rather than logged: a silent cap reads as "covered
    /// everything" and the next run has to be the one that says otherwise.
    pub truncated: bool,
}

/// A pair the deterministic pass is not confident enough to call, handed to whoever asks a model.
#[derive(Debug, Clone, Serialize)]
pub struct ModelCandidate {
    pub similarity: f64,
    pub namespace: String,
    pub a_id: String,
    pub a_content: String,
    pub b_id: String,
    pub b_content: String,
}

/// Refuses anything a model must not be shown.
///
/// The candidate query does not filter on `open`: it runs at the caller's ceiling so the
/// deterministic half can group private duplicates. The pass skips a pair with a private side
/// before it reaches this point, and this runs where the rows are handed out so that a future
/// caller building a candidate list some other way still cannot get a private row past it.
fn assert_model_visible(rows: &[&Candidate]) -> Result<()> {
    for r in rows {
        if !model_may_see(r.sensitivity) {
            return Err(DomainError::internal(format!(
                "a row at {} reached the model-visible path. The candidate query is supposed to \
                 make this impossible",
                r.sensitivity.as_str()
            )));
        }
    }
    Ok(())
}

/// One deterministic pass over the whole tenant, for the scheduler.
///
/// Reads nothing out to a network and calls no model. The grant is `NamespaceGrant::everything()`
/// and the ceiling is `Sealed`, which is to say unrestricted: nothing here leaves the machine, so
/// restricting it would only make the pass blind to duplicates among private rows.
///
/// Takes a tenant rather than a `Ctx`, and that is the whole of what it needs. A `Ctx` carries a
/// `Principal`, and a function that takes one implies it does something with the caller's
/// identity. This one reads nothing but the tenant, so the scheduled pass can call it without
/// inventing a principal. A synthetic system identity holding `*` at `sealed` is the kind of thing
/// that exists to satisfy a signature and then gets reused somewhere it decides an answer. The
/// grant here is a query bound, not an identity, and the HTTP path cannot reach this function.
pub async fn run(
    tenant: &str,
    repo: &dyn CleanupRepository,
    scope: Option<&str>,
    cadence: &str,
    limit: i64,
    min_similarity: Option<f64>,
) -> Result<(RunReport, Vec<ModelCandidate>)> {
    let scope_key = scope.unwrap_or("*").to_string();
    pass(
        tenant,
        repo,
        Bounds {
            scope: scope.map(str::to_string),
            scope_key,
            cadence: cadence.to_string(),
            limit,
            min_similarity,
            grant: NamespaceGrant::everything(),
            posted_by: None,
        },
    )
    .await
}

/// The same pass, run by an HTTP caller inside its own grant.
///
/// The scope the caller names has to sit inside what it may read; a scope outside is refused
/// rather than narrowed, because a run that silently did less than it was asked is a run whose
/// report lies. No scope means the caller's grant and nothing wider.
///
/// The watermark is keyed by client as well as by scope. A narrow client reads a subset of the
/// rows the scheduler reads, and a mark it advanced under the scheduler's key would step the
/// scheduler past rows only the scheduler can see.
pub async fn run_as(
    ctx: &Ctx,
    repo: &dyn CleanupRepository,
    scope: Option<&str>,
    cadence: &str,
    limit: i64,
    min_similarity: Option<f64>,
) -> Result<(RunReport, Vec<ModelCandidate>)> {
    let scope = scope.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
    if let Some(glob) = scope.as_deref() {
        if !covers(&ctx.principal.read, glob) {
            // Names the scope the caller chose and nothing else, so the refusal maps nothing.
            return Err(DomainError::forbidden(format!(
                "client {} may not run cleanup over {glob}: the scope is outside its read grant",
                ctx.principal.client
            )));
        }
    }
    let scope_key = format!("{}@{}", scope.as_deref().unwrap_or("*"), ctx.principal.client);
    pass(
        ctx.tenant(),
        repo,
        Bounds {
            scope,
            scope_key,
            cadence: cadence.to_string(),
            limit,
            min_similarity,
            grant: ctx.principal.read.clone(),
            posted_by: Some(ctx.principal.client.clone()),
        },
    )
    .await
}

/// Whether a read grant admits every namespace a scope glob can name, at `open`.
///
/// A glob is covered by an entry when the entry is `*`, or when both are prefix globs and the
/// entry's prefix is a prefix of the scope's, or when the scope is one name the grant admits.
/// `open` is the level asked for because a scope is a namespace question; the ceiling is applied
/// row by row inside the query.
pub fn covers(grant: &[NamespaceGrant], scope: &str) -> bool {
    let scope = scope.trim().to_ascii_lowercase();
    match scope.strip_suffix('*') {
        Some(prefix) => grant.iter().any(|g| {
            let pattern = g.namespace.trim().to_ascii_lowercase();
            match pattern.strip_suffix('*') {
                Some(granted) => prefix.starts_with(granted),
                None => false,
            }
        }),
        None => grant.iter().any(|g| namespaces::matches(&g.namespace, &scope)),
    }
}

/// Whether a grant admits every namespace at every level, which is what a tenant-wide write needs.
fn reads_everything(grant: &[NamespaceGrant]) -> bool {
    grant.iter().any(|g| g.namespace.trim() == "*" && g.max == Sensitivity::Sealed)
}

/// What one pass runs over. Built by `run` for the scheduler and by `run_as` for a caller, and
/// the only way into `pass`.
struct Bounds {
    scope: Option<String>,
    scope_key: String,
    cadence: String,
    limit: i64,
    min_similarity: Option<f64>,
    grant: Vec<NamespaceGrant>,
    posted_by: Option<String>,
}

async fn pass(
    tenant: &str,
    repo: &dyn CleanupRepository,
    bounds: Bounds,
) -> Result<(RunReport, Vec<ModelCandidate>)> {
    let Bounds { scope, scope_key, cadence, limit, min_similarity, grant, posted_by } = bounds;
    let cadence = cadence.as_str();
    // The floor a pair has to clear to be worth a model's attention. `WORTH_ASKING` is a guess and
    // the store says so: on 21 August 2026 the owner's two statements of the same image-generation
    // preference scored 0.694 against this embedder, so the shipped floor of 0.85 would never have
    // shown a model the one duplicate anyone had noticed by reading. Clamped below `NEAR_CERTAIN`,
    // because a floor at or above it leaves the model nothing to decide.
    let floor = min_similarity.unwrap_or(WORTH_ASKING).clamp(0.0, NEAR_CERTAIN - 0.001);
    let mut report = RunReport {
        scope: scope.clone().unwrap_or_else(|| "*".to_string()),
        cadence: cadence.to_string(),
        ..Default::default()
    };

    // Closing first. A finding the store has already answered should not be counted as work still
    // outstanding in the same report that goes on to queue new ones. The sweep is tenant-wide and
    // touches findings in every namespace at every level, so only a caller that reads all of that
    // runs it: `*` at open would let a client obsolete findings over rows it cannot see. The
    // scheduler covers everyone else within the hour.
    if reads_everything(&grant) {
        report.closed_as_answered = repo.close_answered(tenant).await?.len();
    }

    let mark = repo.watermark(tenant, &scope_key, cadence).await?;
    let query = CandidateQuery {
        namespace: scope,
        max_sensitivity: Sensitivity::Sealed,
        grant,
        since: mark.map(|m| m.through),
        limit,
    };

    // The newest row in scope, whether or not it produced a finding. A quiet run still read the
    // store and its mark still has to move, or the window never narrows and every later run
    // re-reads the same rows.
    let scope_newest = repo.newest_in_scope(tenant, &query).await?;

    // The newest row a finding actually touched. Used instead of `scope_newest` when a query hit
    // its limit, because a truncated run did not read to the end of the store and a mark set past
    // what it read skips the remainder silently.
    let mut seen_newest: Option<DateTime<Utc>> = None;
    let mut truncated = false;
    let note = |c: &Candidate, newest: &mut Option<DateTime<Utc>>| {
        if newest.is_none_or(|n| c.created_at > n) {
            *newest = Some(c.created_at);
        }
    };

    // Exact duplicates. No judgement, so no threshold and no model.
    let groups = repo.exact_duplicates(tenant, &query).await?;
    truncated |= groups.iter().map(Vec::len).sum::<usize>() as i64 >= limit;
    // Rows an exact group has already claimed. Two rows with the same normalised text also sit at
    // a cosine of 1.0, so without this the pair arrives twice: once as `exact` and once as
    // `paraphrase`, with different cluster keys, and the owner reads two findings about one pair.
    // Exact is the stronger claim, so it wins.
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for group in groups {
        report.exact_groups += 1;
        for c in &group {
            note(c, &mut seen_newest);
        }
        report.anchored += group.len();
        claimed.extend(group.iter().map(|c| c.id.clone()));
        // The oldest survives. It carries the access count and the created_at that anything else in
        // the store may already refer to, and keeping the newest would make a re-statement look
        // like the origin of the fact.
        let keep = &group[0];
        let members = group
            .iter()
            .map(|c| NewMember {
                memory_id: c.id.clone(),
                disposition: if c.id == keep.id { Disposition::Keep } else { Disposition::Retire },
                seen_content: c.content.clone(),
            })
            .collect();
        let outcome = queue_checked(
            tenant,
            repo,
            NewProposal {
                kind: CleanupKind::Exact,
                namespace: keep.namespace.clone(),
                keep_id: Some(keep.id.clone()),
                rationale: format!(
                    "These {} rows hold the same text once case and spacing are normalised. \
                         The oldest survives, because it carries the reads and the date anything \
                         else refers to.",
                    group.len()
                ),
                produced_by: "exact".to_string(),
                similarity: Some(1.0),
                posted_by: posted_by.clone(),
                members,
            },
        )
        .await?;
        tally(&outcome.0, &mut report);
    }

    // The near-certain band, then the band worth a model's attention. One query at the lower bound
    // serves both, because the pass has to look at everything above 0.85 either way.
    let pairs = repo.similar_pairs(tenant, &query, floor).await?;
    truncated |= pairs.len() as i64 >= limit;
    let mut for_model = Vec::new();
    for pair in pairs {
        note(&pair.older, &mut seen_newest);
        note(&pair.newer, &mut seen_newest);
        if claimed.contains(&pair.older.id) && claimed.contains(&pair.newer.id) {
            continue;
        }
        if pair.similarity >= NEAR_CERTAIN {
            report.near_certain_pairs += 1;
            let outcome = queue_checked(
                tenant,
                repo,
                NewProposal {
                    kind: CleanupKind::Paraphrase,
                    namespace: pair.older.namespace.clone(),
                    keep_id: Some(pair.newer.id.clone()),
                    // No score in the sentence. `similarity` carries it as its own field, and a
                    // person reading the rationale has no scale for a cosine.
                    rationale: "These two say the same thing. The newer one survives, because a \
                                restatement is usually a correction."
                        .to_string(),
                    produced_by: "cosine".to_string(),
                    similarity: Some(pair.similarity),
                    posted_by: posted_by.clone(),
                    members: vec![
                        NewMember {
                            memory_id: pair.newer.id.clone(),
                            disposition: Disposition::Keep,
                            seen_content: pair.newer.content.clone(),
                        },
                        NewMember {
                            memory_id: pair.older.id.clone(),
                            disposition: Disposition::Retire,
                            seen_content: pair.older.content.clone(),
                        },
                    ],
                },
            )
            .await?;
            tally(&outcome.0, &mut report);
            continue;
        }
        // Below the certain band. A model decides whether these are the same fact, whether they
        // contradict, or whether a cosine simply put two unrelated sentences near each other.
        //
        // Skipped, and counted, when a side is above `open`. The query ran at the full ceiling so
        // the deterministic half could see private duplicates; this band leaves the machine. The
        // skip sits before the assert on purpose: an assert that fired here turned the exact floor
        // at which a private pair entered the band into an answer the caller could read off a
        // 500, one probe at a time.
        if !model_may_see(pair.older.sensitivity) || !model_may_see(pair.newer.sensitivity) {
            report.withheld_from_model += 1;
            continue;
        }
        assert_model_visible(&[&pair.older, &pair.newer])?;
        for_model.push(ModelCandidate {
            similarity: pair.similarity,
            namespace: pair.older.namespace.clone(),
            a_id: pair.older.id.clone(),
            a_content: pair.older.content.clone(),
            b_id: pair.newer.id.clone(),
            b_content: pair.newer.content.clone(),
        });
    }
    report.for_the_model = for_model.len();
    report.min_similarity = floor;

    // Staleness, daily only. An hourly pass that re-reports the same 90-day-old row twenty-four
    // times a day is a pass whose report nobody opens.
    if cadence == "daily" {
        let unread = repo.unread(tenant, &query, STALE_DAYS).await?;
        truncated |= unread.len() as i64 >= limit;
        for row in unread {
            report.stale_rows += 1;
            let age = (Utc::now() - row.created_at).num_days();
            let outcome = queue_checked(
                tenant,
                repo,
                NewProposal {
                    kind: CleanupKind::Stale,
                    namespace: row.namespace.clone(),
                    keep_id: None,
                    rationale: format!(
                        "Nothing has read this in the {age} days since it was written, and \
                             nothing has confirmed it."
                    ),
                    produced_by: "unread".to_string(),
                    similarity: None,
                    posted_by: posted_by.clone(),
                    members: vec![NewMember {
                        memory_id: row.id.clone(),
                        disposition: Disposition::Retire,
                        seen_content: row.content.clone(),
                    }],
                },
            )
            .await?;
            tally(&outcome.0, &mut report);
        }
    }

    // Advanced to what this run read, never to now(). A row written while the run was in flight has
    // to be picked up by the next one, and a mark set to the clock skips it with nothing to show
    // for it.
    let through = if truncated { seen_newest } else { scope_newest.or(seen_newest) };
    if let Some(through) = through {
        repo.advance(tenant, &scope_key, cadence, through).await?;
        report.through = Some(through);
    }
    report.truncated = truncated;
    Ok((report, for_model))
}

/// Queue a cluster, after making sure supersession will accept it.
///
/// The pass chooses its survivor on other grounds: the deterministic half by `created_at`, the
/// model half by which wording reads better. Supersession validates on **valid time**, and refuses
/// a replacement that became true before the fact it ends. Those two can disagree, and when they do
/// the queue holds a finding the owner reads, tries, and cannot act on.
///
/// Reconciled here rather than at apply, so what the queue shows is what applying does. Valid time
/// wins, because supersession is a claim about which fact holds later and valid time is the store's
/// record of exactly that. Which wording reads better is a different question and it does not
/// decide this one.
///
/// A cluster whose members carry no valid time at all is queued unchanged: the guard only fires on
/// two known dates in the wrong order.
pub async fn queue_checked(
    tenant: &str,
    repo: &dyn CleanupRepository,
    mut p: NewProposal,
) -> Result<(QueueOutcome, String)> {
    if !p.kind.has_keep() || p.members.len() < 2 {
        return repo.queue(tenant, p).await;
    }
    let ids: Vec<String> = p.members.iter().map(|m| m.memory_id.clone()).collect();
    let times: std::collections::HashMap<String, Option<chrono::DateTime<Utc>>> =
        repo.valid_times(tenant, &ids).await?.into_iter().collect();

    // The member that became true last, among those that say when they did.
    let latest = p
        .members
        .iter()
        .filter_map(|m| {
            times.get(&m.memory_id).copied().flatten().map(|t| (t, m.memory_id.clone()))
        })
        .max_by_key(|(t, _)| *t);

    let Some((latest_at, latest_id)) = latest else {
        return repo.queue(tenant, p).await;
    };
    let keep_at = p.keep_id.as_ref().and_then(|k| times.get(k).copied().flatten());
    // Only when the chosen survivor is strictly earlier than another member. Equal dates and a
    // survivor with no date of its own both leave the choice alone.
    if keep_at.is_some_and(|k| k < latest_at) && p.keep_id.as_deref() != Some(latest_id.as_str()) {
        p.rationale = format!(
            "{} The survivor is the one that became true later, which is what supersession records; \
             the other reading would have produced a proposal supersession refuses.",
            p.rationale.trim_end()
        );
        for m in &mut p.members {
            m.disposition =
                if m.memory_id == latest_id { Disposition::Keep } else { Disposition::Retire };
        }
        p.keep_id = Some(latest_id);
    }
    repo.queue(tenant, p).await
}

/// Queue a cluster an HTTP caller posted, after checking it names rows the caller may read.
///
/// The posted shape is the shape the pass produces, and a client holding the route can fill it in
/// by hand: one `stale` member naming any memory id, a rationale copied from the deterministic
/// pass, and the queue shows the owner a finding that looks like the server's own. So nothing here
/// is taken on trust. Every member is resolved; the caller must be able to read each one at the
/// level it is stored at; the members must all live in the namespace the item claims; the text the
/// caller says it saw must be the text the row holds; a kind that names a survivor must name one of
/// its own `keep` members; and a kind that deletes needs the delete capability, because applying
/// it is a hard delete.
///
/// One message covers "missing" and "not yours", the way `forget::by_id` phrases it, so a refusal
/// does not say whether the id exists.
pub async fn queue_posted(
    ctx: &Ctx,
    repo: &dyn CleanupRepository,
    mut p: NewProposal,
) -> Result<(QueueOutcome, String)> {
    if p.members.is_empty() {
        return Err(DomainError::validation("a cleanup proposal names at least one row"));
    }
    if p.kind.has_keep() && p.members.len() < 2 {
        return Err(DomainError::validation(format!(
            "a {} proposal names at least two rows: one survives, the rest retire into it",
            p.kind
        )));
    }
    let namespace = namespaces::normalize(&p.namespace)?;

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &p.members {
        let not_yours = || {
            DomainError::not_found(format!(
                "memory {} does not exist or is not yours to propose against",
                m.memory_id
            ))
        };
        let uuid = uuid::Uuid::parse_str(m.memory_id.trim()).map_err(|_| {
            DomainError::validation(format!("{:?} is not a memory id", m.memory_id))
        })?;
        if !seen.insert(uuid.to_string()) {
            return Err(DomainError::validation(format!(
                "memory {uuid} is named twice in one proposal"
            )));
        }
        let row = ctx.repos.memories.find_by_id(ctx.tenant(), uuid).await?;
        let Some(row) = row.filter(|r| can_read(&ctx.principal, &r.namespace, r.sensitivity))
        else {
            return Err(not_yours());
        };
        if row.namespace != namespace {
            // The caller can read the row, so naming its namespace back maps nothing.
            return Err(DomainError::validation(format!(
                "memory {uuid} lives in {}, not in {namespace}",
                row.namespace
            )));
        }
        // Compared here so the queue shows what applying would do. A row the caller read one way
        // and the store holds another is refused at apply anyway, and a row whose text the server
        // cannot read has nothing to compare. Empty content is the second case.
        if row.content.is_empty() || row.content != m.seen_content {
            return Err(DomainError::conflict(format!(
                "memory {uuid} does not hold the text this proposal says it saw"
            )));
        }
    }

    // After the members, so a caller without the capability learns nothing about rows it cannot
    // read: those answered not_found above, the same as a row that does not exist.
    if p.kind.deletes() && !ctx.principal.may_delete {
        return Err(DomainError::forbidden(format!(
            "client {} may not propose a {} cleanup: applying one deletes, and the grant carries \
             no may_delete",
            ctx.principal.client, p.kind
        )));
    }

    if p.kind.has_keep() {
        let keep = p.keep_id.as_deref().map(str::trim).ok_or_else(|| {
            DomainError::validation(format!("a {} proposal names its survivor in keep_id", p.kind))
        })?;
        let keeps: Vec<&NewMember> =
            p.members.iter().filter(|m| m.disposition == Disposition::Keep).collect();
        if keeps.len() != 1 || keeps[0].memory_id.trim() != keep {
            return Err(DomainError::validation(
                "keep_id names exactly one member, and that member's disposition is keep",
            ));
        }
    } else if p.members.iter().any(|m| m.disposition == Disposition::Keep) && p.kind.deletes() {
        return Err(DomainError::validation(format!(
            "a {} proposal keeps nothing; every member retires",
            p.kind
        )));
    }

    p.namespace = namespace;
    p.posted_by = Some(ctx.principal.client.clone());
    queue_checked(ctx.tenant(), repo, p).await
}

fn tally(outcome: &QueueOutcome, report: &mut RunReport) {
    match outcome {
        QueueOutcome::Queued => report.queued += 1,
        QueueOutcome::AlreadyKnown => report.already_known += 1,
    }
}

/// The queue, filtered to what this caller may read.
///
/// The grant goes into the query: a proposal comes back only when every member row is readable at
/// the level it is stored at, and the limit counts what the caller gets. The queue is an operator
/// surface, and "operator surface" is not a grant: `services::review` makes the same argument in
/// the same words, and for the same reason. The check repeated here is the second line, the way
/// `search` repeats its ceiling check.
pub async fn list(
    ctx: &Ctx,
    repo: &dyn CleanupRepository,
    state: Option<&str>,
    limit: i64,
) -> Result<Vec<Proposal>> {
    let rows = repo.list(ctx.tenant(), state, limit.clamp(1, 200), &ctx.principal.read).await?;
    Ok(rows.into_iter().filter(|p| readable(ctx, p)).collect())
}

/// One proposal, or `None` for an id outside the grant as much as for one that does not exist.
pub async fn get(ctx: &Ctx, repo: &dyn CleanupRepository, id: &str) -> Result<Option<Proposal>> {
    Ok(repo.get(ctx.tenant(), id, &ctx.principal.read).await?.filter(|p| readable(ctx, p)))
}

/// The second line behind the query's visibility rule. A proposal that reaches here with a member
/// the caller cannot read is a repository that got the SQL wrong, and it is logged as such.
fn readable(ctx: &Ctx, p: &Proposal) -> bool {
    let ok = can_read(&ctx.principal, &p.namespace, Sensitivity::Open)
        && p.members.iter().all(|m| match (&m.namespace, m.sensitivity) {
            (Some(ns), Some(level)) => can_read(&ctx.principal, ns, level),
            _ => true,
        });
    if !ok {
        tracing::error!(
            proposal = %p.id,
            client = %ctx.principal.client,
            "repository returned a cleanup proposal outside the caller's grant; dropped"
        );
    }
    ok
}

/// What applying a proposal did.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub id: String,
    pub kind: CleanupKind,
    /// Rows superseded into the survivor.
    pub retired: Vec<String>,
    /// Rows deleted, which only a `stale` proposal produces.
    pub deleted: Vec<String>,
    pub kept: Option<String>,
}

/// Carry out a proposal, one member at a time, through the paths that already hold the checks.
///
/// Refuses rather than adapting when the store has moved. A proposal describes a cluster as it
/// stood when a pass read it, and a cluster that has changed since is a different question.
pub async fn apply(ctx: &Ctx, repo: &dyn CleanupRepository, id: &str) -> Result<Applied> {
    let Some(p) = get(ctx, repo, id).await? else {
        return Err(DomainError::not_found(format!("no cleanup proposal {id}")));
    };
    if p.state != "proposed" {
        return Err(refusal(ApplyRefusal::NotProposed(p.state.clone())));
    }
    if !p.kind.has_keep() && !p.kind.deletes() {
        return Err(refusal(ApplyRefusal::NothingToApply(p.kind)));
    }

    // Every member checked before anything is written. Half an applied proposal leaves the store in
    // a state no report describes.
    for m in &p.members {
        let Some(current) = m.current_content.as_deref() else {
            return Err(refusal(ApplyRefusal::MemberMissing(m.memory_id.clone())));
        };
        if current != m.seen_content {
            return Err(refusal(ApplyRefusal::MemberChanged(m.memory_id.clone())));
        }
        if m.superseded_by.is_some() {
            return Err(refusal(ApplyRefusal::MemberRetired(m.memory_id.clone())));
        }
    }

    // The survivor has to be one of the cluster's own `keep` members. A posted proposal could
    // otherwise name any readable row as `keep_id` and retire its members into it.
    if p.kind.has_keep() {
        let named = p.keep_id.as_deref();
        let keeps_it = p
            .members
            .iter()
            .any(|m| m.disposition == Disposition::Keep && Some(m.memory_id.as_str()) == named);
        if !keeps_it {
            return Err(DomainError::conflict(format!(
                "proposal {} names a survivor that is not one of its own keep members",
                p.id
            )));
        }
    }

    let mut applied = Applied {
        id: p.id.clone(),
        kind: p.kind,
        retired: Vec::new(),
        deleted: Vec::new(),
        kept: p.keep_id.clone(),
    };

    for m in p.members.iter().filter(|m| m.disposition == Disposition::Retire) {
        if p.kind.deletes() {
            super::review::delete(ctx, &m.memory_id, Some("cleanup: unread and unconfirmed"))
                .await?;
            applied.deleted.push(m.memory_id.clone());
        } else {
            let keep = p.keep_id.as_deref().ok_or_else(|| {
                DomainError::internal("a proposal that retires rows has no survivor to retire into")
            })?;
            super::review::supersede(ctx, &m.memory_id, keep).await?;
            applied.retired.push(m.memory_id.clone());
        }
    }

    if !repo.decide(ctx.tenant(), &p.id, "applied", None).await? {
        // The rows are already moved. Saying so is better than a silent disagreement between the
        // store and the queue.
        return Err(DomainError::conflict(format!(
            "the memories were changed but proposal {} could not be marked applied. Something else \
             decided it at the same moment",
            p.id
        )));
    }
    Ok(applied)
}

/// Settle a contradiction by naming which member holds.
///
/// A contradiction names no survivor, so `apply` refuses it: deciding which of two conflicting
/// facts is true is the owner's call and a pass that also picked would be writing the fact. This is
/// how the owner says it, once he has.
///
/// The console's other route to this is worse and it is worth naming, because it looks right. The
/// fact page offers Replace, which composes a **new** memory superseding the one you are looking
/// at. Used on a contradiction it leaves three rows: the new one, the row it retired, and the other
/// original still live. Superseding one existing row into the other is a different operation and
/// this is it.
pub async fn resolve(
    ctx: &Ctx,
    repo: &dyn CleanupRepository,
    id: &str,
    keep_id: &str,
) -> Result<Applied> {
    let Some(p) = get(ctx, repo, id).await? else {
        return Err(DomainError::not_found(format!("no cleanup proposal {id}")));
    };
    if p.state != "proposed" {
        return Err(refusal(ApplyRefusal::NotProposed(p.state.clone())));
    }
    if p.kind.has_keep() {
        return Err(DomainError::validation(format!(
            "a {} proposal already names its survivor. Apply it rather than resolving it",
            p.kind
        )));
    }
    if !p.members.iter().any(|m| m.memory_id == keep_id) {
        return Err(DomainError::validation(format!(
            "{keep_id} is not one of the rows this finding is about"
        )));
    }

    // Every member checked before one is written, the same as `apply`. Half a resolved
    // contradiction leaves the store in a state no report describes.
    for m in &p.members {
        let Some(current) = m.current_content.as_deref() else {
            return Err(refusal(ApplyRefusal::MemberMissing(m.memory_id.clone())));
        };
        if current != m.seen_content {
            return Err(refusal(ApplyRefusal::MemberChanged(m.memory_id.clone())));
        }
        if m.superseded_by.is_some() {
            return Err(refusal(ApplyRefusal::MemberRetired(m.memory_id.clone())));
        }
    }

    let mut applied = Applied {
        id: p.id.clone(),
        kind: p.kind,
        retired: Vec::new(),
        deleted: Vec::new(),
        kept: Some(keep_id.to_string()),
    };
    for m in p.members.iter().filter(|m| m.memory_id != keep_id) {
        super::review::supersede(ctx, &m.memory_id, keep_id).await?;
        applied.retired.push(m.memory_id.clone());
    }

    if !repo.decide(ctx.tenant(), &p.id, "applied", None).await? {
        return Err(DomainError::conflict(format!(
            "the memories were changed but proposal {} could not be marked applied. Something else \
             decided it at the same moment",
            p.id
        )));
    }
    Ok(applied)
}

pub async fn reject(
    ctx: &Ctx,
    repo: &dyn CleanupRepository,
    id: &str,
    reason: Option<&str>,
) -> Result<()> {
    if get(ctx, repo, id).await?.is_none() {
        return Err(DomainError::not_found(format!("no cleanup proposal {id}")));
    }
    if !repo.decide(ctx.tenant(), id, "rejected", reason).await? {
        return Err(DomainError::conflict(format!("cleanup proposal {id} was already decided")));
    }
    Ok(())
}

/// Put a refused finding back in the queue.
///
/// A rejection is permanent by design: `queue` treats a cluster it already holds as known in every
/// state, which is what makes an hourly pass safe to run hourly. The cost lands when the pass was
/// what was wrong. The cluster key is the same one the fixed pass computes, so the replacement
/// finding is swallowed as already known and the owner reads a queue that says nothing. Until this
/// existed the way out was a DELETE typed into psql.
pub async fn unreject(ctx: &Ctx, repo: &dyn CleanupRepository, id: &str) -> Result<()> {
    let Some(p) = get(ctx, repo, id).await? else {
        return Err(DomainError::not_found(format!("no cleanup proposal {id}")));
    };
    if !repo.unreject(ctx.tenant(), id).await? {
        return Err(DomainError::conflict(format!(
            "cleanup proposal {id} is {}, and only a rejected one returns to the queue",
            p.state
        )));
    }
    Ok(())
}

/// A refusal a person reads, at the status that says the store moved rather than that they erred.
fn refusal(r: ApplyRefusal) -> DomainError {
    match r {
        ApplyRefusal::NothingToApply(_) => DomainError::validation(r.to_string()),
        other => DomainError::conflict(other.to_string()),
    }
}

/// How far back a cadence looks when it has no watermark.
///
/// A first run with no mark reads everything, which is right once. This is what a caller uses to
/// bound a manual run over a window it names.
pub fn default_window(cadence: &str) -> Duration {
    match cadence {
        "hourly" => Duration::hours(1),
        _ => Duration::days(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn candidate(id: &str, sensitivity: Sensitivity) -> Candidate {
        Candidate {
            id: id.to_string(),
            namespace: "user:me".to_string(),
            sensitivity,
            content: "a fact".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
            access_count: 0,
            occurred_at: None,
        }
    }

    #[test]
    fn an_open_row_reaches_the_model_path() {
        let c = candidate("a", Sensitivity::Open);
        assert!(assert_model_visible(&[&c]).is_ok());
    }

    #[test]
    fn a_private_row_never_reaches_the_model_path() {
        // The query filters this out already. This is the check that still refuses when a future
        // caller assembles a candidate list some other way.
        let c = candidate("a", Sensitivity::Private);
        let err = assert_model_visible(&[&c]).unwrap_err();
        assert_eq!(err.kind, crate::domain::errors::Kind::Internal);
    }

    #[test]
    fn a_sealed_row_never_reaches_the_model_path() {
        let c = candidate("a", Sensitivity::Sealed);
        assert!(assert_model_visible(&[&c]).is_err());
    }

    #[test]
    fn one_private_row_in_a_pair_refuses_the_whole_pair() {
        let open = candidate("a", Sensitivity::Open);
        let private = candidate("b", Sensitivity::Private);
        assert!(assert_model_visible(&[&open, &private]).is_err());
    }

    #[test]
    fn the_bands_do_not_overlap_and_leave_a_gap_for_the_model() {
        assert!(WORTH_ASKING < NEAR_CERTAIN, "a model would never be asked about anything");
    }

    #[test]
    fn a_floor_at_or_above_the_certain_band_is_clamped_below_it() {
        // A caller passing 0.99 means "only near-identical pairs", and the near-identical ones are
        // already handled without a model. Left alone it asks the model about an empty list and
        // reports that as a clean store.
        let clamp = |v: f64| v.clamp(0.0, NEAR_CERTAIN - 0.001);
        assert!(clamp(0.99) < NEAR_CERTAIN);
        assert!(clamp(1.0) < NEAR_CERTAIN);
        assert_eq!(clamp(0.70), 0.70);
        assert_eq!(clamp(-1.0), 0.0);
    }

    #[test]
    fn a_contradiction_cannot_be_applied() {
        let e = refusal(ApplyRefusal::NothingToApply(CleanupKind::Contradiction));
        assert_eq!(e.kind, crate::domain::errors::Kind::Validation);
        assert!(e.client_message().contains("names no survivor"));
    }

    #[test]
    fn a_store_that_moved_reads_as_a_conflict_rather_than_a_bad_request() {
        // The difference matters to whoever is reading the error: nothing they sent was wrong.
        for r in [
            ApplyRefusal::MemberMissing("a".into()),
            ApplyRefusal::MemberChanged("a".into()),
            ApplyRefusal::MemberRetired("a".into()),
            ApplyRefusal::NotProposed("applied".into()),
        ] {
            assert_eq!(refusal(r).kind, crate::domain::errors::Kind::Conflict);
        }
    }

    #[test]
    fn a_scope_is_covered_only_by_a_grant_that_reaches_all_of_it() {
        let grant = vec![NamespaceGrant::open("project:*"), NamespaceGrant::open("user:me")];
        assert!(covers(&grant, "project:*"));
        assert!(covers(&grant, "project:lumberroom*"));
        assert!(covers(&grant, "project:lumberroom"));
        assert!(covers(&grant, "user:me"));
        assert!(!covers(&grant, "*"), "a prefix grant does not cover the whole store");
        assert!(!covers(&grant, "personal:*"));
        assert!(!covers(&grant, "global"));
        // An exact grant covers its one name and never a glob over it.
        assert!(!covers(&grant, "user:*"));
    }

    #[test]
    fn the_whole_store_grant_covers_every_scope_and_an_empty_grant_covers_none() {
        for scope in ["*", "project:*", "global", "personal:finance"] {
            assert!(covers(&NamespaceGrant::everything(), scope), "{scope}");
            assert!(!covers(&[], scope), "{scope}");
        }
    }

    #[test]
    fn the_tenant_wide_sweep_needs_every_namespace_at_sealed_and_not_just_the_wildcard() {
        assert!(reads_everything(&NamespaceGrant::everything()));
        assert!(
            !reads_everything(&[NamespaceGrant::open("*")]),
            "* at open cannot see what it would close"
        );
        assert!(!reads_everything(&[NamespaceGrant::new("project:*", Sensitivity::Sealed)]));
        assert!(!reads_everything(&[]));
    }

    #[test]
    fn a_private_side_is_withheld_from_the_model_band_rather_than_refused() {
        // The decision `pass` makes before the assert, isolated. The assert still refuses, and
        // the skip in front of it is what turns a 500 into a count.
        let open = candidate("a", Sensitivity::Open);
        let private = candidate("b", Sensitivity::Private);
        let hidden = !model_may_see(open.sensitivity) || !model_may_see(private.sensitivity);
        assert!(hidden);
        assert!(assert_model_visible(&[&open, &private]).is_err());
    }

    #[test]
    fn hourly_looks_back_an_hour_and_daily_a_day() {
        assert_eq!(default_window("hourly"), Duration::hours(1));
        assert_eq!(default_window("daily"), Duration::days(1));
    }

    #[test]
    fn a_queued_cluster_and_a_known_one_are_counted_apart() {
        // The report's whole claim about an hourly cadence being safe is that a second run over the
        // same window queues nothing, so these two counters must not merge.
        let mut r = RunReport::default();
        tally(&QueueOutcome::Queued, &mut r);
        tally(&QueueOutcome::AlreadyKnown, &mut r);
        tally(&QueueOutcome::AlreadyKnown, &mut r);
        assert_eq!((r.queued, r.already_known), (1, 2));
    }
}
