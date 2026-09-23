//! memory_search(query, namespaces?, limit?, project?, include_superseded?) -> rows[]
//!
//! Namespace strategy, and why it deviates from the letter of PRD §5: the default set is
//! 'user:me' + 'global' + the active project, exactly as specified, but other project namespaces
//! are also searched at a score penalty rather than excluded. A model that forgets to pass
//! `project` would otherwise be told nothing is known while the fact sits one namespace away.
//! Precedence still holds. SEARCH_INCLUDE_ALL_PROJECTS=false restores strict behaviour.
//!
//! Every namespace travels with the sensitivity ceiling this caller holds for it, and the pair goes
//! into the query. Filtering after the fetch would mean a row the client may not see had already
//! entered the process that answers that client, which is the failure Phase 3 §1 is written to
//! prevent. The check repeated over the results here is a second line, not the line.
//!
//! Ranking stays behind the port, including the usage boost. Re-ranking in Rust would mean the
//! rows the boost promotes were never in the candidate set the database returned.
//!
//! # What may be published as a namespace name
//!
//! One rule, the same one the digest inventory follows: a namespace name reaches a response only
//! once a both-axes filter has put a row behind it. `also_searched` used to publish the discovery
//! set, which comes from `namespace_counts` and is pre-policy by contract, narrowed by
//! `filter_readable`. That call applies the namespace axis alone, so a ceiling of open over a
//! namespace holding only private rows published a name the second axis had already refused. It is
//! the digest inventory bug under a different field name. `namespaces`, the primary set, is
//! different: it comes from the caller's own argument or from `default_read_namespaces`, never from
//! the store, so echoing it back confirms the caller's own grant and nothing about what is stored.

use serde::Serialize;

use super::Ctx;
use crate::adapters::auth::filter_readable;
use crate::domain::errors::{DomainError, Result};
use crate::domain::namespaces;
use crate::domain::policy::NamespaceCeiling;
use crate::domain::types::Sensitivity;
use crate::ports::{Emission, SearchQuery, Weights};

/// The name this tool records its emissions under, and the same string `recall_emission.tool`
/// holds. Kept beside the code that writes it so the two cannot drift.
pub const SEARCH_TOOL: &str = "memory_search";

#[derive(Debug, Serialize)]
pub struct SearchResult {
    /// The primary set, from the caller's own list or from the default read namespaces. Never from
    /// the store, which is why it may be published as it stands.
    pub namespaces: Vec<String>,
    /// Namespaces outside the primary set that *answered*, taken from the hits this caller is
    /// holding. Every one of those rows carries its own namespace in the same response, so this
    /// list discloses nothing the caller does not already have.
    pub also_searched: Vec<String>,
    pub hits: Vec<Hit>,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub tags: Vec<String>,
    /// The app that wrote the row, by name (decision 0020).
    pub source: String,
    /// The stored id, for the console, which prints it until it moves to `source` too. Never
    /// serialised: this struct is `memory_search`'s wire shape, and an MCP answer carries the name.
    #[serde(skip_serializing)]
    pub source_client: String,
    pub sensitivity: Sensitivity,
    pub created_at: String,
    pub score: f64,
    pub similarity: f64,
    pub primary: bool,
    /// Only present when the caller asked for history, so a live-row search keeps its Phase 1 shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// When the fact held, as distinct from `created_at`, which is when the store learned it. Both
    /// skipped when absent, and most rows have no date, so a payload only grows a key where there
    /// is something to say. A reader who cannot tell "stored today" from "true since June" is
    /// reading the wrong clock, which is the whole reason valid time exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_until: Option<String>,
}

/// Namespaces holding the same subject under an earlier name.
///
/// Best effort on purpose. An alias table that cannot be read must not take search down with it, so
/// a failure here logs and returns nothing: the caller gets the results they would have got before
/// this existed. Silence is the old behaviour rather than a new failure.
async fn alias_namespaces(ctx: &Ctx, primary: &[NamespaceCeiling]) -> Vec<NamespaceCeiling> {
    let mut out: Vec<NamespaceCeiling> = Vec::new();
    for p in primary {
        // Every namespace with a slug, not only projects. A personal area gets renamed as readily
        // as a codebase does, and limiting this to `project:` meant an alias recorded anywhere else
        // sat in the table doing nothing.
        let Some((prefix, slug)) = p.namespace.split_once(':') else {
            continue;
        };
        let names = match ctx.repos.aliases.group(&ctx.cfg.tenant_id, &p.namespace, slug).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e.log_message(), namespace = %p.namespace,
                    "the alias group did not load; searching without it");
                continue;
            }
        };
        for name in names {
            if name == slug {
                continue;
            }
            let candidate = format!("{prefix}:{name}");
            // Through the grant, never around it. A rename is not a reason to read a namespace the
            // caller was never given.
            let reachable = filter_readable(&ctx.principal, &[candidate.clone()]);
            for c in reachable {
                if !out.iter().any(|x| x.namespace == c.namespace)
                    && !primary.iter().any(|x| x.namespace == c.namespace)
                {
                    out.push(c);
                }
            }
        }
    }
    out
}

pub async fn run(
    ctx: &Ctx,
    query: &str,
    requested: Option<Vec<String>>,
    limit: Option<i64>,
    project: Option<&str>,
    include_superseded: Option<bool>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<SearchResult> {
    // The capability check has to live here. A repository holds no principal, so the as-of statement
    // will hand retired rows to anything that sets the field, and a grant over live rows is not a
    // grant over the history behind them.
    //
    // `include_superseded` is the same door with a different handle. It returns the retired rows
    // with their content, which is what `memory_history` refuses the same client, and for a while
    // only `as_of` was gated: the replaced credential location that `may_read_history` exists to
    // protect came back through this flag. One check, one spelling of the refusal.
    if as_of.is_some() || include_superseded == Some(true) {
        super::history::assert_may_read(ctx)?;
    }
    // Both set is a caller that believes two different things about what it asked for. The as-of
    // statement applies no supersession filter of its own, so the flag would be silently ignored.
    if as_of.is_some() && include_superseded == Some(true) {
        return Err(DomainError::validation(
            "as_of already reads retired rows, so include_superseded cannot be set beside it",
        ));
    }

    let query = query.trim();
    if query.is_empty() {
        return Err(DomainError::validation("query cannot be empty"));
    }
    // Live rows only unless asked. History stays queryable, which is what makes the decision log a
    // side effect rather than a feature to build.
    let include_superseded = include_superseded.unwrap_or(false);

    let explicit = requested.as_ref().is_some_and(|n| !n.is_empty());
    let asked = match &requested {
        Some(list) if explicit => {
            let mut out =
                list.iter().map(|n| namespaces::normalize(n)).collect::<Result<Vec<_>>>()?;
            namespaces::dedupe(&mut out);
            out
        }
        _ => namespaces::default_read_namespaces(project)?,
    };

    let primary = filter_readable(&ctx.principal, &asked);
    if primary.is_empty() {
        return Ok(SearchResult { namespaces: vec![], also_searched: vec![], hits: vec![] });
    }

    // A rename splits a subject across namespaces, and that is the case aliases exist for. Six rows
    // in this store describe one project, two under `project:warden` and four under
    // `project:lumen`, because it was renamed twice. Neither query reaches the other's rows, and
    // no wording of the question fixes that: the filter runs before the ranking.
    //
    // So the expansion is over namespaces rather than over query text. Rewriting the text was the
    // obvious move and it is wrong twice: `websearch_to_tsquery` reads a space as AND, so adding a
    // name would demand both, and the same string feeds the embedder, where "lumen OR warden"
    // embeds as neither.
    let aliased = alias_namespaces(ctx, &primary).await;

    let secondary = if explicit || !ctx.cfg.search.include_all_projects {
        aliased
    } else {
        let mut out = other_namespaces(ctx, &primary).await?;
        for ns in aliased {
            if !out.iter().any(|c| c.namespace == ns.namespace) {
                out.push(ns);
            }
        }
        out
    };

    let limit = limit.unwrap_or(ctx.cfg.search.default_limit).min(ctx.cfg.search.max_limit);
    let embedding = ctx.embedder.embed_query(query).await?;

    let mut hits = ctx
        .repos
        .memories
        .search(SearchQuery {
            tenant_id: ctx.cfg.tenant_id.clone(),
            primary: primary.clone(),
            secondary: secondary.clone(),
            embedding,
            text: query.to_string(),
            limit,
            as_of,
            weights: Weights {
                vector: ctx.cfg.search.vector_weight,
                lexical: ctx.cfg.search.lexical_weight,
                secondary_penalty: ctx.cfg.search.other_project_penalty,
                usage: ctx.cfg.search.usage_weight,
            },
            include_superseded,
        })
        .await?;

    // Private rows arrive without their plaintext. Opening them here, after the query has already
    // applied the ceilings, means the decryption only ever runs over rows this caller may read.
    // A row that will not open is dropped and logged: one unreadable row must not break a search.
    let unopened = super::decrypt(ctx, hits.iter_mut().map(|h| &mut h.memory).collect()).await;
    if !unopened.is_empty() {
        hits.retain(|h| !unopened.contains(&h.memory.id));
    }

    // Second line of the sensitivity check. The query already applied the ceilings; this catches a
    // repository that got the SQL wrong, and a leak here is the one failure in this system that
    // cannot be walked back.
    let mut returned: Vec<uuid::Uuid> = Vec::with_capacity(hits.len());
    let mut out: Vec<Hit> = Vec::with_capacity(hits.len());
    // Filled from the rows that survived, never from the set that was scanned. See the publish rule
    // at the top of this file.
    let mut answered: Vec<String> = Vec::new();
    for hit in hits {
        if !admitted(&primary, &secondary, &hit.memory.namespace, hit.memory.sensitivity) {
            tracing::error!(
                id = %hit.memory.id,
                namespace = %hit.memory.namespace,
                sensitivity = %hit.memory.sensitivity,
                client = %ctx.principal.client,
                "repository returned a row outside the caller's ceiling; dropped"
            );
            continue;
        }
        if let Ok(id) = uuid::Uuid::parse_str(&hit.memory.id) {
            returned.push(id);
        }
        if !hit.primary && !answered.contains(&hit.memory.namespace) {
            answered.push(hit.memory.namespace.clone());
        }
        out.push(Hit {
            occurred_at: hit.memory.occurred_at.map(|t| t.to_rfc3339()),
            occurred_until: hit.memory.occurred_until.map(|t| t.to_rfc3339()),
            id: hit.memory.id,
            namespace: hit.memory.namespace,
            content: hit.memory.content,
            tags: hit.memory.tags,
            source: hit.memory.source_client.clone(),
            source_client: hit.memory.source_client,
            sensitivity: hit.memory.sensitivity,
            created_at: hit.memory.created_at.to_rfc3339(),
            score: hit.score,
            similarity: hit.similarity,
            primary: hit.primary,
            superseded_by: hit.memory.superseded_by,
        });
    }

    // Ageing signals, for the rows that were actually returned rather than the rows that were
    // considered. Fire and forget by contract: a search must not turn into a write storm and must
    // not pay for this.
    if !returned.is_empty() {
        ctx.repos.memories.touch_accessed(ctx.tenant(), returned);
    }

    // What the store handed out, so an extractor reading a transcript of this answer proposes a
    // confirmation rather than the same fact again. Recorded from `out`, after decryption and after
    // the ceiling check, because a row dropped by either was never handed to anyone.
    let emissions =
        emissions_for(ctx, out.iter().map(|h| (h.id.as_str(), h.content.as_str(), h.sensitivity)))
            .await;
    ctx.repos.memories.record_emissions(
        ctx.tenant(),
        SEARCH_TOOL,
        ctx.session_id.clone(),
        emissions,
    );

    let writers: Vec<String> = out.iter().map(|h| h.source_client.clone()).collect();
    let labels = super::sources::labels(ctx, &writers).await;
    for hit in &mut out {
        if let Some(name) = labels.get(&hit.source_client) {
            hit.source = name.clone();
        }
    }

    answered.sort();
    Ok(SearchResult { namespaces: names(&primary), also_searched: answered, hits: out })
}

/// The emission rows for a result set, digested under the KEK-derived key.
///
/// Encrypted rows are left out. Their digest would be the one record of a private row's plaintext
/// that the database holds beside the ciphertext, and however it is keyed, a row whose content
/// only exists under the KEK should not also exist as a hash a narrow credential can probe. The
/// cost is that an ingest pass never recognises a private fact as an echo and queues it again;
/// the owner declines it once more, which is the cheaper side.
///
/// The digest is `Digester::digest`, the same function that produces a proposal's fingerprint.
/// Using any other one here would give this layer a hash that can never meet a proposal. A digester
/// that cannot be built means the KEK did not read, and recording nothing is the right answer:
/// the read already succeeded and this record must not fail it.
pub(crate) async fn emissions_for<'a>(
    ctx: &Ctx,
    rows: impl Iterator<Item = (&'a str, &'a str, Sensitivity)>,
) -> Vec<Emission> {
    let candidates: Vec<(uuid::Uuid, &str)> = rows
        .filter(|(_, content, sensitivity)| !content.is_empty() && !sensitivity.is_encrypted())
        .filter_map(|(id, content, _)| uuid::Uuid::parse_str(id).ok().map(|id| (id, content)))
        .collect();
    if candidates.is_empty() {
        return vec![];
    }
    let digester = match crate::crypto::Digester::from_provider(ctx.keys.as_ref()).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e.log_message(), "could not derive the content digest key; no emissions recorded");
            return vec![];
        }
    };
    candidates
        .into_iter()
        .map(|(memory_id, content)| Emission {
            content_sha256: digester.digest(content),
            memory_id,
        })
        .collect()
}

fn names(ceilings: &[NamespaceCeiling]) -> Vec<String> {
    ceilings.iter().map(|c| c.namespace.clone()).collect()
}

fn admitted(
    primary: &[NamespaceCeiling],
    secondary: &[NamespaceCeiling],
    namespace: &str,
    sensitivity: Sensitivity,
) -> bool {
    primary.iter().chain(secondary).any(|c| c.namespace == namespace && sensitivity <= c.max)
}

/// Candidate namespaces outside the primary set, for the query and for nothing else.
///
/// `namespace_counts` is pre-policy by contract and `filter_readable` applies the namespace axis
/// alone, so these names are a query argument rather than an answer. The counts are dropped here and
/// the names go no further than `SearchQuery::secondary`, where each one travels with its ceiling and
/// the second axis runs in SQL.
async fn other_namespaces(
    ctx: &Ctx,
    exclude: &[NamespaceCeiling],
) -> Result<Vec<NamespaceCeiling>> {
    let counts = ctx.repos.memories.namespace_counts(ctx.tenant()).await?;
    let mut candidates: Vec<String> =
        counts.into_keys().filter(|ns| !exclude.iter().any(|c| &c.namespace == ns)).collect();
    candidates.sort();
    Ok(filter_readable(&ctx.principal, &candidates))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ceiling(namespace: &str, max: Sensitivity) -> NamespaceCeiling {
        NamespaceCeiling { namespace: namespace.into(), max }
    }

    #[test]
    fn a_row_at_or_below_the_ceiling_is_admitted() {
        let primary = vec![ceiling("user:me", Sensitivity::Private)];
        assert!(admitted(&primary, &[], "user:me", Sensitivity::Open));
        assert!(admitted(&primary, &[], "user:me", Sensitivity::Private));
    }

    #[test]
    fn a_row_above_the_ceiling_is_dropped_even_in_a_granted_namespace() {
        let primary = vec![ceiling("user:me", Sensitivity::Open)];
        assert!(!admitted(&primary, &[], "user:me", Sensitivity::Private));
    }

    #[test]
    fn a_row_from_an_ungranted_namespace_is_dropped() {
        let primary = vec![ceiling("user:me", Sensitivity::Sealed)];
        assert!(!admitted(&primary, &[], "personal:finance", Sensitivity::Open));
    }

    #[test]
    fn the_secondary_set_carries_its_own_ceiling() {
        let secondary = vec![ceiling("project:other", Sensitivity::Open)];
        assert!(admitted(&[], &secondary, "project:other", Sensitivity::Open));
        assert!(!admitted(&[], &secondary, "project:other", Sensitivity::Private));
    }
}
