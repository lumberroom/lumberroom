//! Deleting, Phase 3 §4. `lumberroom forget <id>`, `lumberroom forget --query "..."`, and `memory_forget`.
//!
//! Phase 1 had no delete path at all, and this is the phase where sensitive content becomes
//! possible, so the two belong together.
//!
//! # Why every caller needs the delete flag
//!
//! The spec calls the CLI path unrestricted and the MCP path opt-in. This module requires
//! `may_delete` from both, because the thing that separates them is a request header the caller
//! chooses. `X-Invocation: cli` costs a model nothing to send, so treating it as an authorisation
//! boundary would make the MCP restriction decorative. The operator's own grant carries
//! `"mayDelete": true` and that is a one-line config change with a visible reason.
//!
//! # Why a query delete has a similarity floor
//!
//! A vector search always returns its nearest rows, however far away they are. Deleting the top N
//! of a fuzzy search would let "forget the old port number" take out the deploy notes sitting
//! beside it. Candidates below the floor are not offered at all, and the dry run prints exactly
//! what is in scope before anything goes.
//!
//! # What happens to the chain
//!
//! A row that retired another row cannot simply go: the retired row points at it. The first
//! version cleared that pointer, which revived the retired row, in whatever namespace it sat and
//! at whatever level, for a caller who might hold neither. A private fact the owner had corrected
//! came back to life because a client with an open ceiling deleted the correction.
//!
//! Now the store is asked who points at the row, and the service decides under the caller's own
//! grant. A doomed row with a successor has its predecessors spliced onto that successor, so they
//! stay retired and the chain reads as one row shorter. A doomed row at the head of its chain
//! revives its predecessors, and only the ones the caller could have deleted themselves; any other
//! predecessor blocks the delete with the same message an unknown id gets, because naming the
//! namespace would map a grant the caller was refused.

use serde::Serialize;

use super::Ctx;
use crate::adapters::auth::{can_read, can_write, filter_readable};
use crate::domain::errors::{DomainError, Result};
use crate::domain::namespaces;
use crate::domain::types::{Memory, Principal, Sensitivity};
use crate::ports::memory::{ChainLink, ChainNeighbours, DeleteOutcome, DeletePlan};
use crate::ports::{SearchQuery, Weights};

/// How much of a row is quoted back in a dry run. Enough to recognise the fact, short enough that a
/// confirmation prompt stays readable.
const PREVIEW_CHARS: usize = 140;

/// The most rows one query delete may take. A delete that removes forty rows because the phrasing
/// was loose is the failure this bound exists for; the caller can raise it per call, up to this.
const MAX_QUERY_DELETES: i64 = 25;

#[derive(Debug, Clone, Serialize)]
pub struct Doomed {
    pub id: String,
    pub namespace: String,
    pub sensitivity: Sensitivity,
    pub source_client: String,
    pub created_at: String,
    /// Truncated. The caller has already been checked against this row's ceiling.
    pub preview: String,
    /// Similarity to the query, on the query path only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForgetOutcome {
    /// True when nothing was deleted and this is the list of what would be.
    pub dry_run: bool,
    pub count: usize,
    pub rows: Vec<Doomed>,
    /// What deletion means for the levels in this batch, one line each.
    pub consequences: Vec<String>,
    /// Rows a deleted row had retired that are live again, because the deleted row was the head
    /// of its chain and the caller's grant covered each of them.
    pub revived: Vec<String>,
    /// Rows a deleted row had retired that now point at the deleted row's successor instead.
    pub spliced: Vec<String>,
    /// Rows that matched and were left alone: deleting one would have revived a row this caller
    /// may not reach. On the by-id path this is a refusal rather than a list.
    pub blocked: Vec<String>,
    /// Rendered for a confirmation prompt.
    pub text: String,
}

/// The chain edits a delete will make, or why it cannot be made.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    Apply(DeletePlan),
    /// A predecessor outside the caller's grant would come back to life.
    Blocked,
}

/// Decide the chain edits for one row under this caller's grant. Pure, so the boundary cases can
/// be pinned without a database.
///
/// With a successor, every predecessor is spliced onto it whatever its namespace: the row stays
/// retired, which is policy-neutral, and the alternative leaves two live versions of one fact.
/// Without one, a predecessor comes back to life, and that is a write at the predecessor's own
/// level the caller has to hold both grants for.
fn plan_for(principal: &Principal, neighbours: &ChainNeighbours) -> Plan {
    let Some((_, successor)) = neighbours.row else {
        return Plan::Apply(DeletePlan::default());
    };
    if let Some(successor) = successor {
        return Plan::Apply(DeletePlan { revive: vec![], splice_to: Some(successor) });
    }
    let mut revive = Vec::with_capacity(neighbours.predecessors.len());
    for link in &neighbours.predecessors {
        if !reaches(principal, link) {
            return Plan::Blocked;
        }
        revive.push(link.id);
    }
    Plan::Apply(DeletePlan { revive, splice_to: None })
}

/// The same two grants `deletable` needs, at the neighbour's own namespace and level.
fn reaches(principal: &Principal, link: &ChainLink) -> bool {
    can_read(principal, &link.namespace, link.sensitivity)
        && can_write(principal, &link.namespace, link.sensitivity)
}

async fn plan(ctx: &Ctx, id: uuid::Uuid) -> Result<Plan> {
    let neighbours = ctx.repos.memories.chain_neighbours(ctx.tenant(), id).await?;
    Ok(plan_for(&ctx.principal, &neighbours))
}

/// The one refusal for "no such row", "not yours" and "would revive a row that is not yours".
fn not_yours(id: &str) -> DomainError {
    DomainError::not_found(format!("memory {id} does not exist or is not yours to delete"))
}

pub async fn by_id(
    ctx: &Ctx,
    id: &str,
    reason: Option<&str>,
    dry_run: bool,
) -> Result<ForgetOutcome> {
    assert_may_delete(ctx)?;

    let uuid = uuid::Uuid::parse_str(id.trim())
        .map_err(|_| DomainError::validation(format!("{id:?} is not a uuid")))?;
    let row = ctx.repos.memories.find_by_id(ctx.tenant(), uuid).await?;

    // One message for "does not exist" and for "not yours", because naming a namespace the caller
    // cannot read tells it that namespace exists.
    let mut row = match row {
        Some(m) if deletable(ctx, &m) => m,
        _ => return Err(not_yours(id)),
    };
    // Planned on the dry run too, so a preview that says "would delete" is not followed by a
    // refusal on the real call.
    let plan = match plan(ctx, uuid).await? {
        Plan::Apply(p) => p,
        Plan::Blocked => return Err(not_yours(id)),
    };

    // Decrypt for the preview only, and ignore a failure: the dry run then says the row could not
    // be read, which is information, and the delete goes ahead either way.
    let _ = super::decrypt(ctx, vec![&mut row]).await;

    let doomed = vec![to_doomed(&row, None)];
    if dry_run {
        return Ok(outcome(true, doomed, Edits::planned(&plan), vec![]));
    }

    let edits = match ctx.repos.memories.delete(ctx.tenant(), uuid, &plan).await? {
        // Lost a race with another delete. Nothing is wrong with the store, and the caller wanted
        // the row gone, which it is.
        DeleteOutcome::Missing => return Ok(outcome(false, vec![], Edits::default(), vec![])),
        DeleteOutcome::Deleted(e) => e,
    };
    let edits = Edits::from_chain(&edits);
    record(ctx, &doomed, &edits, reason);
    super::bootstrap::clear_cache();
    Ok(outcome(false, doomed, edits, vec![]))
}

#[allow(clippy::too_many_arguments)]
pub async fn by_query(
    ctx: &Ctx,
    query: &str,
    requested: Option<Vec<String>>,
    limit: Option<i64>,
    min_similarity: Option<f64>,
    reason: Option<&str>,
    dry_run: bool,
) -> Result<ForgetOutcome> {
    assert_may_delete(ctx)?;

    let query = query.trim();
    if query.is_empty() {
        return Err(DomainError::validation("query cannot be empty"));
    }
    let limit = limit.unwrap_or(MAX_QUERY_DELETES).clamp(1, MAX_QUERY_DELETES);
    let floor = min_similarity.unwrap_or(ctx.cfg.quality.conflict_threshold);

    let asked = match requested {
        Some(list) if !list.is_empty() => {
            let mut out =
                list.iter().map(|n| namespaces::normalize(n)).collect::<Result<Vec<_>>>()?;
            namespaces::dedupe(&mut out);
            out
        }
        _ => namespaces::default_read_namespaces(None)?,
    };
    let primary = filter_readable(&ctx.principal, &asked);
    if primary.is_empty() {
        return Ok(outcome(dry_run, vec![], Edits::default(), vec![]));
    }

    let embedding = ctx.embedder.embed_query(query).await?;
    let mut hits = ctx
        .repos
        .memories
        .search(SearchQuery {
            tenant_id: ctx.cfg.tenant_id.clone(),
            primary,
            secondary: vec![],
            embedding,
            text: query.to_string(),
            limit,
            // Forgetting acts on what holds now. Deleting a fact as it stood in March would remove
            // the row that still stands today.
            as_of: None,
            weights: Weights {
                vector: ctx.cfg.search.vector_weight,
                lexical: ctx.cfg.search.lexical_weight,
                secondary_penalty: ctx.cfg.search.other_project_penalty,
                // No usage boost on a delete. A row being popular is not a reason to delete it, and
                // it is certainly not a reason to prefer it over the row the caller described.
                usage: 0.0,
            },
            include_superseded: true,
            tags: vec![],
        })
        .await?;

    let _ = super::decrypt(ctx, hits.iter_mut().map(|h| &mut h.memory).collect()).await;

    // Superseded rows are included on purpose: forgetting a fact means forgetting the corrections
    // of it too, and a caller cleaning up after a mistake would otherwise be left with history
    // pointing at nothing.
    //
    // Retired rows reach this process only because the caller may delete them, which `deletable`
    // checks per row below. They are never returned as search results: a preview of a row the
    // caller is about to delete is a different thing from a history read.
    let candidates: Vec<Doomed> = hits
        .iter()
        .filter(|h| h.similarity >= floor)
        .filter(|h| deletable(ctx, &h.memory))
        .map(|h| to_doomed(&h.memory, Some(h.similarity)))
        .collect();

    // Each row is planned right before it goes, never all at once up front. Two rows of one chain
    // can both match, and deleting the first changes what the second's neighbours are: a plan made
    // before that would splice onto a row that no longer exists. The dry run plans every row
    // against the store as it stands, which is the preview it can honestly give.
    let mut gone = Vec::with_capacity(candidates.len());
    let mut blocked = Vec::new();
    let mut edits = Edits::default();
    let mut failure = None;
    for row in candidates {
        let Ok(uuid) = uuid::Uuid::parse_str(&row.id) else { continue };
        let plan = match plan(ctx, uuid).await? {
            Plan::Apply(p) => p,
            Plan::Blocked => {
                blocked.push(row.id);
                continue;
            }
        };
        if dry_run {
            edits.extend(&Edits::planned(&plan));
            gone.push(row);
            continue;
        }
        match ctx.repos.memories.delete(ctx.tenant(), uuid, &plan).await {
            Ok(DeleteOutcome::Deleted(e)) => {
                edits.extend(&Edits::from_chain(&e));
                gone.push(row);
            }
            Ok(DeleteOutcome::Missing) => {}
            // Partial success is reported as partial success. Rolling the whole batch back would
            // mean claiming rows still exist that do not, and the rows already gone are recorded
            // below before the error goes back, so an audit line is never lost to a later row.
            Err(e) => {
                tracing::error!(id = %row.id, error = %e.log_message(), "delete failed mid-batch");
                failure = Some(e);
                break;
            }
        }
    }
    if dry_run {
        return Ok(outcome(true, gone, edits, blocked));
    }
    record(ctx, &gone, &edits, reason);
    super::bootstrap::clear_cache();
    if let Some(e) = failure {
        return Err(e);
    }
    Ok(outcome(false, gone, edits, blocked))
}

/// A sealed item, by the client-computed HMAC of its name.
///
/// The server never held a key for this and cannot check what it is deleting, so there is nothing
/// to preview and nothing to recover. That is the level working as specified, not a gap.
pub async fn sealed_item(
    ctx: &Ctx,
    namespace: &str,
    key_hmac: &str,
    reason: Option<&str>,
    dry_run: bool,
) -> Result<ForgetOutcome> {
    assert_may_delete(ctx)?;

    let namespace = namespaces::normalize(namespace)?;
    if !can_write(&ctx.principal, &namespace, Sensitivity::Sealed) {
        return Err(DomainError::forbidden(format!(
            "client {} may not delete sealed items in {namespace}",
            ctx.principal.client
        )));
    }
    let key_hmac = key_hmac.trim();
    if key_hmac.is_empty() {
        return Err(DomainError::validation("key_hmac cannot be empty"));
    }

    let doomed = vec![Doomed {
        id: key_hmac.to_string(),
        namespace: namespace.clone(),
        sensitivity: Sensitivity::Sealed,
        source_client: String::new(),
        created_at: String::new(),
        preview: "(sealed; this server holds no key)".to_string(),
        similarity: None,
    }];
    if dry_run {
        return Ok(outcome(true, doomed, Edits::default(), vec![]));
    }

    let removed = ctx.sealed_store()?.delete(ctx.tenant(), &namespace, key_hmac).await?;
    if !removed {
        return Ok(outcome(false, vec![], Edits::default(), vec![]));
    }
    record(ctx, &doomed, &Edits::default(), reason);
    super::bootstrap::clear_cache();
    Ok(outcome(false, doomed, Edits::default(), vec![]))
}

/// A model that can silently delete memories is a worse failure than one that hoards them, so the
/// capability is off unless the grant names it.
fn assert_may_delete(ctx: &Ctx) -> Result<()> {
    if ctx.principal.may_delete {
        return Ok(());
    }
    Err(DomainError::forbidden(format!(
        "client {} may not delete. Deleting is opt-in per client: set \"mayDelete\": true on the \
         grant.",
        ctx.principal.client
    )))
}

/// Deleting is a destructive write, so it needs the write grant at the row's stored level, and the
/// read grant too: a client that cannot see a row has no business removing it.
fn deletable(ctx: &Ctx, m: &Memory) -> bool {
    can_read(&ctx.principal, &m.namespace, m.sensitivity)
        && can_write(&ctx.principal, &m.namespace, m.sensitivity)
}

fn to_doomed(m: &Memory, similarity: Option<f64>) -> Doomed {
    Doomed {
        id: m.id.clone(),
        namespace: m.namespace.clone(),
        sensitivity: m.sensitivity,
        source_client: m.source_client.clone(),
        created_at: m.created_at.to_rfc3339(),
        preview: preview(&m.content),
        similarity,
    }
}

/// An empty preview means a private row that would not decrypt. Deleting it is still the right
/// answer, and more clearly so: a row nobody can read is exactly the kind of thing to remove.
fn preview(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "(encrypted; this server could not read it)".to_string();
    }
    if flat.chars().count() <= PREVIEW_CHARS {
        return flat;
    }
    format!("{}…", flat.chars().take(PREVIEW_CHARS).collect::<String>())
}

/// The chain edits of a batch, as strings for the outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Edits {
    revived: Vec<String>,
    spliced: Vec<String>,
}

impl Edits {
    /// What a plan will do, for the dry run. The splice list is unknown until the store applies
    /// it, so a dry run reports the revivals and says the rest will be spliced.
    fn planned(plan: &DeletePlan) -> Self {
        Self { revived: plan.revive.iter().map(|u| u.to_string()).collect(), spliced: vec![] }
    }

    fn from_chain(edits: &crate::ports::memory::ChainEdits) -> Self {
        Self {
            revived: edits.revived.iter().map(|u| u.to_string()).collect(),
            spliced: edits.spliced.iter().map(|u| u.to_string()).collect(),
        }
    }

    fn extend(&mut self, other: &Edits) {
        self.revived.extend(other.revived.iter().cloned());
        self.spliced.extend(other.spliced.iter().cloned());
    }
}

/// Deletions are recorded so that "what happened to that fact" has an answer. The tool call itself
/// is recorded by the transport like every other call; this line is what names the rows, which a
/// per-call row cannot. The rows a delete brought back to life get their own line, because a fact
/// reappearing is the question that gets asked next.
fn record(ctx: &Ctx, rows: &[Doomed], edits: &Edits, reason: Option<&str>) {
    for row in rows {
        tracing::warn!(
            id = %row.id,
            namespace = %row.namespace,
            sensitivity = %row.sensitivity,
            client = %ctx.principal.client,
            session = ctx.session_id.as_deref().unwrap_or("-"),
            reason = reason.unwrap_or("-"),
            "memory deleted"
        );
    }
    for id in &edits.revived {
        tracing::warn!(id = %id, client = %ctx.principal.client, "memory revived by a delete");
    }
    for id in &edits.spliced {
        tracing::info!(id = %id, client = %ctx.principal.client, "memory re-pointed by a delete");
    }
}

fn outcome(dry_run: bool, rows: Vec<Doomed>, edits: Edits, blocked: Vec<String>) -> ForgetOutcome {
    let consequences = consequences(&rows);
    let text = render(dry_run, &rows, &consequences, &edits, &blocked);
    ForgetOutcome {
        dry_run,
        count: rows.len(),
        rows,
        consequences,
        revived: edits.revived,
        spliced: edits.spliced,
        blocked,
        text,
    }
}

/// What deletion means, per level present in this batch. Said out loud because the three levels
/// differ in what survives, and the operator confirming a delete is the person who needs to know.
fn consequences(rows: &[Doomed]) -> Vec<String> {
    let mut out = Vec::new();
    if rows.iter().any(|r| r.sensitivity == Sensitivity::Open) {
        out.push(
            "open: the row goes. A plaintext backup taken before now still contains it."
                .to_string(),
        );
    }
    if rows.iter().any(|r| r.sensitivity == Sensitivity::Private) {
        out.push(
            "private: the wrapped DEK goes with the row, so the ciphertext in any older backup is \
             already unreadable."
                .to_string(),
        );
    }
    if rows.iter().any(|r| r.sensitivity == Sensitivity::Sealed) {
        out.push(
            "sealed: this removes the only copy. The server holds no key and cannot help recover it."
                .to_string(),
        );
    }
    out
}

/// The dry run's whole job: print exactly what would go, in a form somebody can say yes to.
fn render(
    dry_run: bool,
    rows: &[Doomed],
    consequences: &[String],
    edits: &Edits,
    blocked: &[String],
) -> String {
    if rows.is_empty() {
        let mut text = if dry_run {
            "Nothing matches. Nothing would be deleted.".to_string()
        } else {
            "Nothing matched. Nothing was deleted.".to_string()
        };
        if !blocked.is_empty() {
            text.push('\n');
            text.push_str(&blocked_line(blocked));
        }
        return text;
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "{} {} {}:",
        if dry_run { "Would delete" } else { "Deleted" },
        rows.len(),
        if rows.len() == 1 { "row" } else { "rows" }
    ));
    for r in rows {
        let similarity = match r.similarity {
            Some(s) => format!(" {s:.2}"),
            None => String::new(),
        };
        lines.push(format!(
            "- {} [{}, {}]{}  {}",
            r.id, r.namespace, r.sensitivity, similarity, r.preview
        ));
    }
    for line in consequences {
        lines.push(String::new());
        lines.push(line.clone());
    }
    if !edits.revived.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{} {} row{} this had retired: {}",
            if dry_run { "Would revive" } else { "Revived" },
            edits.revived.len(),
            if edits.revived.len() == 1 { "" } else { "s" },
            edits.revived.join(", ")
        ));
    }
    if !edits.spliced.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "Re-pointed {} retired row{} at the successor, so they stay retired: {}",
            edits.spliced.len(),
            if edits.spliced.len() == 1 { "" } else { "s" },
            edits.spliced.join(", ")
        ));
    }
    if !blocked.is_empty() {
        lines.push(String::new());
        lines.push(blocked_line(blocked));
    }
    lines.join("\n")
}

fn blocked_line(blocked: &[String]) -> String {
    format!(
        "Left alone {} row{} whose deletion would revive a row outside your grant: {}",
        blocked.len(),
        if blocked.len() == 1 { "" } else { "s" },
        blocked.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy::NamespaceGrant;

    fn principal(read: Vec<NamespaceGrant>, write: Vec<NamespaceGrant>) -> Principal {
        let mut p = Principal::empty("browser");
        p.read = read;
        p.write = write;
        p.may_delete = true;
        p
    }

    fn link(ns: &str, level: Sensitivity) -> ChainLink {
        ChainLink { id: uuid::Uuid::new_v4(), namespace: ns.into(), sensitivity: level }
    }

    fn head_with(predecessors: Vec<ChainLink>) -> ChainNeighbours {
        ChainNeighbours { row: Some((None, None)), predecessors, successors: vec![] }
    }

    #[test]
    fn a_row_with_no_chain_needs_no_edits() {
        let p = principal(vec![], vec![]);
        assert_eq!(plan_for(&p, &head_with(vec![])), Plan::Apply(DeletePlan::default()));
        assert_eq!(plan_for(&p, &ChainNeighbours::default()), Plan::Apply(DeletePlan::default()));
    }

    #[test]
    fn a_head_revives_only_predecessors_the_caller_could_delete() {
        let p =
            principal(vec![NamespaceGrant::open("user:me")], vec![NamespaceGrant::open("user:me")]);
        let mine = link("user:me", Sensitivity::Open);
        let plan = plan_for(&p, &head_with(vec![mine.clone()]));
        assert_eq!(plan, Plan::Apply(DeletePlan { revive: vec![mine.id], splice_to: None }));
    }

    #[test]
    fn a_head_whose_predecessor_is_above_the_ceiling_blocks_the_delete() {
        let p =
            principal(vec![NamespaceGrant::open("user:me")], vec![NamespaceGrant::open("user:me")]);
        let private = link("user:me", Sensitivity::Private);
        assert_eq!(plan_for(&p, &head_with(vec![private])), Plan::Blocked);
    }

    #[test]
    fn a_head_whose_predecessor_is_in_another_namespace_blocks_the_delete() {
        let p =
            principal(vec![NamespaceGrant::open("user:me")], vec![NamespaceGrant::open("user:me")]);
        let foreign = link("personal:finance", Sensitivity::Open);
        assert_eq!(plan_for(&p, &head_with(vec![foreign])), Plan::Blocked);
    }

    #[test]
    fn a_read_grant_alone_does_not_let_a_delete_revive_a_row() {
        let p = principal(vec![NamespaceGrant::open("user:me")], vec![]);
        assert_eq!(
            plan_for(&p, &head_with(vec![link("user:me", Sensitivity::Open)])),
            Plan::Blocked
        );
    }

    #[test]
    fn a_row_with_a_successor_splices_every_predecessor_onto_it_whatever_the_grant() {
        let p = principal(vec![], vec![]);
        let successor = uuid::Uuid::new_v4();
        let neighbours = ChainNeighbours {
            row: Some((None, Some(successor))),
            predecessors: vec![link("personal:finance", Sensitivity::Private)],
            successors: vec![link("user:me", Sensitivity::Open)],
        };
        assert_eq!(
            plan_for(&p, &neighbours),
            Plan::Apply(DeletePlan { revive: vec![], splice_to: Some(successor) })
        );
    }

    fn doomed(id: &str, level: Sensitivity) -> Doomed {
        Doomed {
            id: id.into(),
            namespace: "user:me".into(),
            sensitivity: level,
            source_client: "mac".into(),
            created_at: "2026-08-19T10:00:00+00:00".into(),
            preview: "The port is 8080".into(),
            similarity: Some(0.93),
        }
    }

    fn plain(dry_run: bool, rows: &[Doomed]) -> String {
        render(dry_run, rows, &consequences(rows), &Edits::default(), &[])
    }

    #[test]
    fn a_dry_run_names_every_row_it_would_take() {
        let rows = vec![doomed("a", Sensitivity::Open), doomed("b", Sensitivity::Open)];
        let text = plain(true, &rows);
        assert!(text.starts_with("Would delete 2 rows:"));
        assert!(text.contains("- a [user:me, open] 0.93  The port is 8080"));
        assert!(text.contains("- b ["));
    }

    #[test]
    fn a_dry_run_over_nothing_says_so_rather_than_printing_an_empty_list() {
        assert!(plain(true, &[]).contains("Nothing would be deleted"));
        assert!(plain(false, &[]).contains("Nothing was deleted"));
    }

    #[test]
    fn the_report_names_what_a_delete_revived_and_what_it_left_alone() {
        let rows = vec![doomed("a", Sensitivity::Open)];
        let edits = Edits { revived: vec!["p".into()], spliced: vec!["q".into(), "r".into()] };
        let text = render(false, &rows, &consequences(&rows), &edits, &["z".to_string()]);
        assert!(text.contains("Revived 1 row this had retired: p"), "{text}");
        assert!(text.contains("Re-pointed 2 retired rows at the successor"), "{text}");
        assert!(
            text.contains(
                "Left alone 1 row whose deletion would revive a row outside your grant: z"
            ),
            "{text}"
        );

        let empty = render(true, &[], &[], &Edits::default(), &["z".to_string()]);
        assert!(empty.contains("Nothing would be deleted"), "{empty}");
        assert!(empty.contains("Left alone 1 row"), "{empty}");
    }

    #[test]
    fn the_private_consequence_names_the_dek_going_with_the_row() {
        let rows = vec![doomed("a", Sensitivity::Private)];
        assert!(consequences(&rows)[0].contains("wrapped DEK goes with the row"));
    }

    #[test]
    fn the_sealed_consequence_says_the_copy_is_the_only_one() {
        let rows = vec![doomed("a", Sensitivity::Sealed)];
        assert!(consequences(&rows)[0].contains("only copy"));
    }

    #[test]
    fn one_line_per_level_present_and_none_for_a_level_that_is_not() {
        let rows = vec![
            doomed("a", Sensitivity::Open),
            doomed("b", Sensitivity::Open),
            doomed("c", Sensitivity::Private),
        ];
        let lines = consequences(&rows);
        assert_eq!(lines.len(), 2);
        assert!(!lines.iter().any(|l| l.starts_with("sealed")));
    }

    #[test]
    fn a_preview_is_flattened_and_bounded() {
        assert_eq!(preview("two\n  lines"), "two lines");
        let long = preview(&"x".repeat(500));
        assert_eq!(long.chars().count(), PREVIEW_CHARS + 1, "one char for the ellipsis");
    }
}
