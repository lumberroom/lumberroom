//! Building an archive, and applying one.
//!
//! The only file that knows both the archive format and the store. `crates/archive` holds no
//! database handle and `src/adapters` holds no opinion about archives; this is where the two meet.
//!
//! # An archive is all or nothing
//!
//! [`build`] refuses a caller who cannot read the whole store, rather than filtering rows to what
//! the caller may see. A filtered archive is a partial copy that looks like a complete one, and the
//! person who finds out is the person who already cancelled their old install.
//!
//! This is also why it does not reuse `list_for_export`. That path is bounded by
//! `EXPORT_MAX_SENSITIVITY`, which defaults to `open`, so an archive built through it on a stock
//! deployment would hold open rows alone and say nothing about the rest.
//!
//! # Re-running a half-applied merge
//!
//! The caller passes the id map from the interrupted run and [`apply`] skips any archive id already
//! in it. Idempotence lives there and not in the write path's dedupe bands, which cannot provide
//! it: both bands sit inside `if supersedes_id.is_none()` in `src/services/write.rs`, the
//! exact-match path additionally requires `resolved == Sensitivity::Open`, and `collapse_target`
//! returns `None` for a row that is not live. A replay leaning on them duplicates every superseded
//! row, every private row, and every row the first pass retired.
//!
//! # Private content travels as plaintext
//!
//! No record carries `content_ct`, `dek_wrapped` or `kek_id`. `envelope::seal` authenticates the
//! row id as associated data, so ciphertext copied to another row or another install fails its tag
//! check rather than opening. A private row is decrypted on the way out, travels inside the age
//! layer, and is resealed under the destination's own key.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use lumberroom_archive::container::Sealing;
use lumberroom_archive::reader::Archive;
use lumberroom_archive::writer::ArchiveWriter;
use lumberroom_archive::{
    AliasRecord, ArchiveError, Excluded, MemoryRecord, Record, RegistryHistoryRecord,
    RegistryRecord, SealedItemRecord, Source,
};

use super::embedders::UnitEmbedding;
use super::Ctx;
use crate::adapters::auth::assert_writable;
use crate::domain::errors::{DomainError, Result};
use crate::domain::types::{Memory, Sensitivity};
use crate::domain::{namespaces, tripwire};
use crate::ports::alias::NewAlias;
use crate::ports::memory::RestoreRow;

/// Rows per page out of the repository. Bounds memory and nothing else.
const PAGE: i64 = 500;

/// Replaced registry values carried per key.
///
/// A bound rather than a total, because `history` takes a limit and an unbounded read against a key
/// somebody rewrote in a loop is how a batch job becomes an incident. A key with more versions than
/// this loses its oldest, which the header's counts do not report: `registry_history` is the one
/// section an archive can be short on without saying so.
const HISTORY_PER_KEY: i64 = 1000;

/// Why a replaced registry value stays in the file.
///
/// `registry_history` is append-only and the store fills it: a row lands there when an upsert
/// replaces the value above it, inside the same transaction. No port inserts one on its own, and
/// adding a write path so an import could forge a version history is the wrong shape for a table
/// whose whole claim is that it records what the store did. An archive carries these rows for the
/// person reading the file, and an import counts them rather than dropping them in silence.
const REGISTRY_HISTORY_NO_WRITE_PATH: &str = "registry_history_has_no_write_path";

/// The archive named a supersession target that never landed here.
const SUPERSEDES_NOT_APPLIED: &str = "supersedes_target_not_applied";

/// A private row whose ciphertext would not open on the way out.
const WOULD_NOT_OPEN: &str = "private_rows_would_not_open";

/// Coherence allows this because `DomainError` is ours. It lives here rather than in
/// `domain::errors` because this is the only file that meets both types, and the domain layer
/// naming a crate it never calls would be the wrong direction for the dependency.
impl From<ArchiveError> for DomainError {
    fn from(e: ArchiveError) -> Self {
        match e {
            // Ours: nothing the uploader sends fixes a write that failed mid-file.
            ArchiveError::Io(_) => DomainError::internal("archive io error"),
            // Everything else is a fact about the file the caller handed over, and the crate's
            // messages are written for the person holding it.
            other => DomainError::validation(other.to_string()),
        }
    }
}

// -- build -------------------------------------------------------------------------------------

/// The whole store as one `.lumber` file.
pub async fn build(ctx: &Ctx, sealing: &Sealing) -> Result<Vec<u8>> {
    if !super::reads_whole_store(&ctx.principal) {
        return Err(DomainError::forbidden(
            "an archive is a copy of the whole store, so it needs a grant over every namespace at \
             sealed. This client holds a narrower one, and a partial archive that looked complete \
             would be the worse answer.",
        ));
    }
    // Retired rows and replaced registry values are most of what makes an archive a store rather
    // than a snapshot, so the capability that guards them guards this too. Reached the same way
    // `services::history` reaches it, because a second spelling of one check is how the two
    // disagreed last time.
    super::history::assert_may_read(ctx)?;

    let mut w = ArchiveWriter::new(Source {
        kind: "oss".to_string(),
        build: env!("CARGO_PKG_VERSION").to_string(),
    });
    let mut excluded: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Memories, keyset paged on id. A page is decrypted before it is written, so the ciphertext of
    // the whole store is never held at once.
    let mut cursor: Option<uuid::Uuid> = None;
    let mut namespaces_seen: HashSet<String> = HashSet::new();
    loop {
        let mut page = ctx.repos.memories.list_whole_store(ctx.tenant(), PAGE, cursor).await?;
        let got = page.len() as i64;
        if got == 0 {
            break;
        }
        cursor = page.last().and_then(|m| uuid::Uuid::parse_str(&m.id).ok());

        // A row that will not open is counted and left out. A row that vanished with no count is
        // the failure `excluded` exists to prevent.
        let unopened = super::decrypt(ctx, page.iter_mut().collect()).await;
        for m in page {
            if unopened.contains(&m.id) {
                *excluded.entry(WOULD_NOT_OPEN).or_default() += 1;
                continue;
            }
            namespaces_seen.insert(m.namespace.clone());
            w.push(Record::Memory(memory_record(&m)));
        }

        // A cursor that stopped advancing means an id the database returned is not a uuid. Reading
        // the same page forever is the worse answer.
        if got < PAGE || cursor.is_none() {
            break;
        }
    }

    // Aliases first, because their namespaces widen the set the registry read is built from.
    let aliases = ctx.repos.aliases.list(ctx.tenant(), None).await?;
    for a in &aliases {
        namespaces_seen.insert(a.namespace.clone());
        w.push(Record::Alias(AliasRecord {
            namespace: a.namespace.clone(),
            alias: a.alias.clone(),
            canonical: a.canonical.clone(),
            since: a.since.map(|t| t.to_rfc3339()),
            until: a.until.map(|t| t.to_rfc3339()),
            origin: a.origin.clone(),
            created_at: a.created_at.to_rfc3339(),
        }));
    }

    // Sealed items. Absent store means none exist rather than a failed archive: `Repos::sealed` is
    // optional for a deployment that runs before the table does.
    if let Some(store) = ctx.repos.sealed.as_ref() {
        for ns in store.namespaces(ctx.tenant()).await? {
            namespaces_seen.insert(ns);
        }
        for item in store.list_for_archive(ctx.tenant()).await? {
            w.push(Record::SealedItem(SealedItemRecord {
                namespace: item.namespace,
                key_hmac: item.key_hmac,
                ciphertext: item.ciphertext,
                alg: item.alg,
                source_client: item.source_client,
                // `SealedItem` carries one timestamp. The record has two, so both report when the
                // row was created; a caller reading `updated_at` off an archive learns nothing a
                // `put` did not already stamp on `created_at`.
                created_at: item.created_at.to_rfc3339(),
                updated_at: item.created_at.to_rfc3339(),
            }));
        }
    }

    // The registry read takes a namespace list because its ceiling filter runs per namespace inside
    // the query, and no port enumerates registry namespaces on their own. The set is built from the
    // namespaces this run has already seen plus the defaults, which is the same construction
    // `services::export` uses and carries the same gap: a registry entry in a namespace holding no
    // memory, no alias and no sealed item is missed.
    let mut names: Vec<String> = namespaces_seen.into_iter().collect();
    names.extend(namespaces::default_read_namespaces(None)?);
    names.sort();
    namespaces::dedupe(&mut names);
    let readable = crate::adapters::auth::filter_readable(&ctx.principal, &names);

    let entries = ctx.repos.registry.list(ctx.tenant(), &readable).await?;
    for e in &entries {
        w.push(Record::Registry(RegistryRecord {
            namespace: e.namespace.clone(),
            // Part of the key, not a label. `(namespace, kind, key)` is what the registry is keyed
            // on, and no rule derives the kind from the key: `machines.desktop.os` is a `host`
            // because a writer said so. Without it an archive could show a registry row and never
            // put it back.
            kind: e.kind.clone(),
            key: e.key.clone(),
            value: value_text(&e.value),
            sensitivity: e.sensitivity.as_str().to_string(),
            source_client: e.provenance.source_client.clone(),
            // The registry stores valid time and a version, not a pair of row timestamps, so both
            // fields report the date the provenance carries.
            created_at: e.provenance.valid_from.clone(),
            updated_at: e.provenance.valid_from.clone(),
        }));
    }

    // One read per key. A registry with ten thousand keys is not a shape this store has, and the
    // alternative is a port method that enumerates every replaced value in the tenant.
    for e in &entries {
        let versions = ctx
            .repos
            .registry
            .history(
                ctx.tenant(),
                &e.namespace,
                Sensitivity::Sealed,
                &e.kind,
                &e.key,
                HISTORY_PER_KEY,
            )
            .await?;
        for v in versions {
            w.push(Record::RegistryHistory(RegistryHistoryRecord {
                namespace: v.namespace,
                kind: v.kind,
                key: v.key,
                value: value_text(&v.value),
                sensitivity: v.sensitivity.as_str().to_string(),
                recorded_at: v.replaced_at.to_rfc3339(),
            }));
        }
    }

    let total: u64 = excluded.values().sum();
    let reasons = excluded.iter().map(|(reason, n)| format!("{n} rows: {reason}")).collect();
    Ok(w.finish(Utc::now().to_rfc3339(), true, Excluded { total, reasons }, sealing)?)
}

/// Read an archive under this deployment's decompression ceiling.
///
/// The one entry point, so `MAX_DECOMPRESSED` is never read outside the config default. An operator
/// whose box fell over to one upload lowers `ARCHIVE_MAX_DECOMPRESSED_BYTES` and every caller
/// inherits it.
pub fn open(ctx: &Ctx, bytes: &[u8], sealing: &Sealing) -> Result<Archive> {
    Ok(Archive::open(bytes, sealing, ctx.cfg.quality.archive_max_decompressed_bytes)?)
}

fn memory_record(m: &Memory) -> MemoryRecord {
    MemoryRecord {
        id: m.id.clone(),
        namespace: m.namespace.clone(),
        content: m.content.clone(),
        tags: m.tags.clone(),
        source_client: m.source_client.clone(),
        sensitivity: m.sensitivity.as_str().to_string(),
        supersedes: m.supersedes.clone(),
        superseded_by: m.superseded_by.clone(),
        superseded_at: m.superseded_at.map(|t| t.to_rfc3339()),
        occurred_at: m.occurred_at.map(|t| t.to_rfc3339()),
        occurred_until: m.occurred_until.map(|t| t.to_rfc3339()),
        access_count: m.access_count,
        last_accessed_at: m.last_accessed_at.map(|t| t.to_rfc3339()),
        last_confirmed_at: m.last_confirmed_at.map(|t| t.to_rfc3339()),
        created_at: m.created_at.to_rfc3339(),
        embedding_model: m.embedding_model.clone(),
    }
}

/// A registry value as JSON text. A string value keeps its quotes, which is what makes the round
/// trip through `serde_json::from_str` total rather than ambiguous.
fn value_text(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
}

// -- apply -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Every row through the write path, new ids, chains rewritten. The default, and the only mode
    /// a hosted console offers.
    Merge,
    /// Ids and timestamps preserved, into a store holding no memory row. Merge cannot reproduce a
    /// store, which is the whole reason this exists.
    Restore,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApplyReport {
    /// Memory rows this run wrote. Aliases and registry rows are not counted here: a number mixing
    /// three tables answers no question anybody asks of it.
    pub applied: usize,
    /// Rows the caller's `prior` map already accounted for. The whole of a replay's safety.
    pub skipped_already_applied: usize,
    /// Rows the write path collapsed into an existing one. The survivor's id is in `id_map`, so a
    /// chain pointing at a collapsed row resolves to what survived it.
    pub collapsed: usize,
    /// `(archive id, reason)`. Carries dropped supersession links as well as refused rows, so a
    /// chain that came out short has a line saying which end went missing.
    pub refused: Vec<(String, String)>,
    /// Archive id to the id it holds here, `prior` included. A dry run hands back what it was given
    /// and nothing more: an invented id in a map the job persists is worse than no map.
    pub id_map: HashMap<String, String>,
    /// Chains this run relinked where the retired row kept an open end.
    ///
    /// `supersede` returns that fact rather than logging it, because a predecessor and its
    /// successor both reading as true at the changeover is a timeline hole the owner cannot see
    /// from the store. An import writes chains in bulk, so it is the surface most able to produce
    /// them and the one with the least excuse for staying quiet.
    pub chains_left_open: usize,
}

/// Put an archive into this store.
///
/// `prior` is the id map from an interrupted run of the same job, and it is the only thing making a
/// replay safe. Pass an empty map for a first run.
pub async fn apply(
    ctx: &Ctx,
    archive: &Archive,
    mode: Mode,
    dry_run: bool,
    prior: &HashMap<String, String>,
) -> Result<ApplyReport> {
    // The tripwire is the backstop for a row whose namespace inference was wrong in the expensive
    // direction, and a bulk import is where that goes wrong at scale. It runs at open alone, so
    // this refusal is about open rows and the documentation says so rather than implying more.
    if !ctx.cfg.policy.tripwire {
        return Err(DomainError::unavailable(
            "an archive import writes rows this server did not classify, and the credential \
             tripwire is off. Set SENSITIVITY_TRIPWIRE=true and import again. The tripwire \
             covers rows stored at open; nothing scans a private row in either direction.",
        ));
    }

    match mode {
        Mode::Merge => merge(ctx, archive, dry_run, prior).await,
        Mode::Restore => restore(ctx, archive, dry_run, prior).await,
    }
}

async fn merge(
    ctx: &Ctx,
    archive: &Archive,
    dry_run: bool,
    prior: &HashMap<String, String>,
) -> Result<ApplyReport> {
    let mut report = ApplyReport { id_map: prior.clone(), ..Default::default() };
    // Membership is the question the chain rewrite asks, and a dry run answers it without an id.
    let mut landed: HashSet<String> = prior.keys().cloned().collect();
    // Pairs already linked, so a later pass does not retire a row twice.
    let mut linked: HashSet<(String, String)> = HashSet::new();
    // `(row, the predecessor it names)` for rows whose predecessor sat later in the file.
    //
    // The store is read `ORDER BY id` over v4 uuids, so file order says nothing about chain order
    // and roughly half of any archive's corrections arrive before the row they retire. Refusing at
    // the point of the miss reported data loss on rows the passes below go on to link.
    let mut deferred: Vec<(String, String)> = Vec::new();

    for rec in &archive.memories {
        if landed.contains(&rec.id) {
            report.skipped_already_applied += 1;
            continue;
        }

        // A link to a row that has not landed is deferred and the row is still written. Half a
        // chain beats a fact left out because its predecessor was refused, and a predecessor
        // further down the file is the common case rather than the exception.
        let mut target: Option<String> = None;
        if let Some(old) = rec.supersedes.as_deref() {
            match landed.contains(old) {
                true => target = report.id_map.get(old).cloned(),
                false => deferred.push((rec.id.clone(), old.to_string())),
            }
        }

        let occurred_at = match parse_opt(rec.occurred_at.as_deref()) {
            Ok(t) => t,
            Err(reason) => {
                report.refused.push((rec.id.clone(), reason));
                continue;
            }
        };

        if dry_run {
            match classify(ctx, &rec.namespace, &rec.content, rec.sensitivity.as_str()) {
                Ok(_) => {
                    report.applied += 1;
                    landed.insert(rec.id.clone());
                }
                Err(e) => report.refused.push((rec.id.clone(), e.client_message().to_string())),
            }
            continue;
        }

        // `run_restored` is `run_observed` with the carried cap. An exported row was valid where it
        // was written, and a cap of 2000 would refuse every longer one.
        //
        // `run_observed` rather than `run`, and the near-now fence is the whole difference.
        //
        // The fence refuses an `occurred_at` inside the last day because a model reading a date out
        // of text and writing today is a guess stored as a fact. An archived row's date came out of
        // the source store's own column, which is the same kind of metadata `services::ingest`
        // passes, and fencing it would refuse every row a person wrote yesterday. Migrating a store
        // and losing this week is not a trade an import gets to make silently.
        let outcome = super::write::run_restored(
            ctx,
            &rec.content,
            &rec.namespace,
            Some(rec.tags.clone()),
            target.as_deref(),
            Some(rec.sensitivity.as_str()),
            occurred_at,
        )
        .await;

        match outcome {
            Ok(w) => {
                if w.deduplicated {
                    report.collapsed += 1;
                } else {
                    report.applied += 1;
                }
                if let (Some(old), false) = (target.as_ref(), w.deduplicated) {
                    linked.insert((old.clone(), w.id.clone()));
                }
                landed.insert(rec.id.clone());
                report.id_map.insert(rec.id.clone(), w.id);
            }
            // A refusal is a fact about one row. The run carries on and the report names it,
            // because an import that stops on row four thousand leaves a store nobody can reason
            // about.
            Err(e) => report.refused.push((rec.id.clone(), e.client_message().to_string())),
        }
    }

    // The forward link. A row whose successor sits later in the file could not resolve on the first
    // pass, and a row whose successor carried no `supersedes` of its own never would.
    if !dry_run {
        for rec in &archive.memories {
            let Some(later) = rec.superseded_by.as_deref() else {
                continue;
            };
            // Cloned out of the map so the refusal below can borrow the report.
            let (Some(old), Some(new)) =
                (report.id_map.get(&rec.id).cloned(), report.id_map.get(later).cloned())
            else {
                continue;
            };
            if linked.contains(&(old.clone(), new.clone())) {
                continue;
            }
            let (Ok(old_id), Ok(new_id)) =
                (uuid::Uuid::parse_str(&old), uuid::Uuid::parse_str(&new))
            else {
                continue;
            };
            match ctx.repos.memories.supersede(ctx.tenant(), old_id, new_id).await {
                Ok(outcome) => {
                    if outcome.end_left_open {
                        report.chains_left_open += 1;
                    }
                    linked.insert((old, new));
                }
                Err(e) => report.refused.push((rec.id.clone(), e.client_message().to_string())),
            }
        }
    }

    // The backward link, settled last. A predecessor still missing here is missing for good: every
    // record has been walked, so this is the one place the refusal is a fact rather than a guess
    // about file order.
    for (id, predecessor) in &deferred {
        if !landed.contains(predecessor) {
            report.refused.push((id.clone(), SUPERSEDES_NOT_APPLIED.to_string()));
            continue;
        }
        // A dry run has no ids to link with. It got what it came for: the predecessor is in the
        // archive, so the chain survives the real run.
        if dry_run {
            continue;
        }
        // Cloned out of the map so the refusal below can borrow the report.
        let (Some(old), Some(new)) =
            (report.id_map.get(predecessor).cloned(), report.id_map.get(id).cloned())
        else {
            continue;
        };
        if linked.contains(&(old.clone(), new.clone())) {
            continue;
        }
        let (Ok(old_id), Ok(new_id)) = (uuid::Uuid::parse_str(&old), uuid::Uuid::parse_str(&new))
        else {
            continue;
        };
        match ctx.repos.memories.supersede(ctx.tenant(), old_id, new_id).await {
            Ok(outcome) => {
                if outcome.end_left_open {
                    report.chains_left_open += 1;
                }
                linked.insert((old, new));
            }
            Err(e) => report.refused.push((id.clone(), e.client_message().to_string())),
        }
    }

    apply_side_tables(ctx, archive, dry_run, &mut report).await?;

    if !dry_run {
        // Version 1 carries no `edge` section. Edges are derived, and a derived thing copied
        // forward is a second source of truth for no gain.
        super::graph::rebuild(ctx).await?;
        super::bootstrap::clear_cache();
    }
    Ok(report)
}

async fn restore(
    ctx: &Ctx,
    archive: &Archive,
    dry_run: bool,
    prior: &HashMap<String, String>,
) -> Result<ApplyReport> {
    // An empty `prior` means this restore has not started, so any row in the target belongs to
    // somebody else and restore would write over a store it did not make. A non-empty one means the
    // job is resuming its own work and the rows it finds are the rows it wrote.
    let occupied = !ctx.repos.memories.list_whole_store(ctx.tenant(), 1, None).await?.is_empty();
    if prior.is_empty() && occupied {
        return Err(DomainError::conflict(
            "restore reproduces a store exactly, so it only runs into one that holds no memory \
             row. This tenant holds at least one. Import in merge mode, which mints new ids and \
             keeps what is already here.",
        ));
    }

    // Every row is classified before any row is written, and a single refusal stops the run.
    //
    // Merge tolerates a refused row because it adds to a store that already exists. Restore claims
    // to reproduce one, and a restore that wrote nine tenths of an archive leaves the only state
    // this mode cannot start from: a target holding rows. Refusing here costs one pure pass over
    // the records and no writes at all, which is the difference between an operator reading an
    // error and an operator holding half a store.
    let mut classified: Vec<(String, Sensitivity)> = Vec::with_capacity(archive.memories.len());
    for rec in &archive.memories {
        let checked = classify(ctx, &rec.namespace, rec.content.trim(), rec.sensitivity.as_str())
            .and_then(|pair| restorable_shape(rec).map(|()| pair));
        match checked {
            Ok(pair) => classified.push(pair),
            // The kind travels with it, so a missing key stays a 503 and a missing grant stays a
            // 403 rather than both arriving as a bad request.
            Err(e) => {
                return Err(DomainError::new(
                    e.kind,
                    format!(
                        "row {} of this archive cannot be restored into this server, so nothing \
                         was written: {}",
                        rec.id,
                        e.client_message()
                    ),
                ))
            }
        }
    }

    let mut report = ApplyReport { id_map: prior.clone(), ..Default::default() };
    // Fetched once for the job rather than per row. The write path fetches per write so a rotation
    // cannot be survived by a cached key; a restore is one unit of work, and fifty thousand file
    // reads for one key is the wrong trade.
    let mut kek: Option<crate::crypto::kek::Kek> = None;
    // Resolved once for the job, so every restored row carries vectors from one model.
    let u = ctx.embedders.for_unit(ctx.tenant())?;

    for (rec, (namespace, resolved)) in archive.memories.iter().zip(classified) {
        if report.id_map.contains_key(&rec.id) {
            report.skipped_already_applied += 1;
            continue;
        }
        if dry_run {
            report.applied += 1;
            continue;
        }
        let (row, missed) = restore_row(ctx, &u, rec, namespace, resolved, &mut kek).await?;
        ctx.repos.memories.restore_row(row).await?;
        super::write::log_missed(&u, &rec.id, missed.as_deref());
        report.applied += 1;
        // The id is the archive's own. The map is still written so an interrupted restore resumes
        // on the same terms a merge does.
        report.id_map.insert(rec.id.clone(), rec.id.clone());
    }

    // The chain, once every row it could name exists. Ahead of `graph::rebuild`, which seeds edges
    // from `supersedes` and would otherwise build a graph off columns that are still empty.
    if !dry_run {
        for rec in &archive.memories {
            // The shapes were parsed in the pre-pass, so nothing here can fail on a bad uuid.
            let (Ok(id), Ok(supersedes), Ok(superseded_by)) = (
                uuid::Uuid::parse_str(&rec.id),
                parse_uuid_opt(rec.supersedes.as_deref()),
                parse_uuid_opt(rec.superseded_by.as_deref()),
            ) else {
                continue;
            };
            let relinked =
                ctx.repos.memories.relink_restored(ctx.tenant(), id, supersedes, superseded_by);
            // A refusal here is one dropped link, which is what `refused` reports. Returning an
            // error would abort a run that has already written every row, leaving the one state
            // restore cannot start from again.
            if let Err(e) = relinked.await {
                report.refused.push((rec.id.clone(), e.client_message().to_string()));
            }
        }
    }

    apply_side_tables(ctx, archive, dry_run, &mut report).await?;

    if !dry_run {
        super::graph::rebuild(ctx).await?;
        super::bootstrap::clear_cache();
    }
    Ok(report)
}

/// The parts of a record restore reads without touching the store: a uuid for an id, uuids on both
/// chain links, and timestamps it can parse.
///
/// Checked in the pre-pass with everything else, because a malformed row two thirds of the way down
/// a file would otherwise abort a run that had already written the two thirds above it.
fn restorable_shape(rec: &MemoryRecord) -> Result<()> {
    uuid::Uuid::parse_str(&rec.id)
        .map_err(|_| DomainError::validation("a restored row needs a uuid for an id"))?;
    parse_uuid_opt(rec.supersedes.as_deref())?;
    parse_uuid_opt(rec.superseded_by.as_deref())?;
    for (field, raw) in [
        ("created_at", Some(rec.created_at.as_str())),
        ("superseded_at", rec.superseded_at.as_deref()),
        ("occurred_at", rec.occurred_at.as_deref()),
        ("occurred_until", rec.occurred_until.as_deref()),
        ("last_accessed_at", rec.last_accessed_at.as_deref()),
        ("last_confirmed_at", rec.last_confirmed_at.as_deref()),
    ] {
        let parsed =
            parse_opt(raw).map_err(|why| DomainError::validation(format!("{field}: {why}")))?;
        // `created_at` is the one that cannot be absent. Restore exists to preserve it, and a row
        // stamped with today's date is the outcome this whole mode was written to avoid.
        if field == "created_at" && parsed.is_none() {
            return Err(DomainError::validation("a restored row needs a created_at"));
        }
    }
    Ok(())
}

/// One row as it will be inserted.
///
/// The refusals ran in [`classify`] before this was called, so the failures left here are the ones
/// that need the embedder, the key or the clock. The dedupe bands, the exact collapse and the id
/// and timestamp stamping are the three checks restore skips, and skipping the last of those is
/// what restore is for.
async fn restore_row(
    ctx: &Ctx,
    u: &UnitEmbedding,
    rec: &MemoryRecord,
    namespace: String,
    resolved: Sensitivity,
    kek: &mut Option<crate::crypto::kek::Kek>,
) -> Result<(RestoreRow, Option<String>)> {
    let id = uuid::Uuid::parse_str(&rec.id)
        .map_err(|_| DomainError::validation("a restored row needs a uuid for an id"))?;
    let content = rec.content.trim();

    // Both models, as a write does, so a restore during a migration leaves nothing extra to fill.
    let super::write::Embedded { vector: embedding, second, missed } =
        super::write::embed_document(u, content, super::write::shadow_wait(ctx)).await?;

    // Sealed under this install's key, bound to this row's id. Never the archive's bytes: no
    // archive carries any, and copied ciphertext fails its tag check even when the id survives.
    let sealed = match resolved.is_encrypted() {
        true => {
            if kek.is_none() {
                let provider = ctx.keys.as_ref().ok_or_else(|| {
                    DomainError::internal("reached the encryption path with no key provider")
                })?;
                *kek = Some(provider.kek().await?);
            }
            let key = kek.as_ref().expect("kek fetched above");
            Some(crate::crypto::envelope::seal(key, id, content)?)
        }
        false => None,
    };

    let row = RestoreRow {
        tenant_id: ctx.tenant().to_string(),
        id,
        namespace,
        // Empty for an encrypted row, matching the write path. A repository that ignores the
        // contract then fails its CHECK constraint instead of storing the plaintext.
        content: match &sealed {
            Some(_) => String::new(),
            None => content.to_string(),
        },
        embedding,
        tags: rec.tags.clone(),
        source_client: rec.source_client.clone(),
        embedding_model: u.embedder.id(),
        slot: u.slot,
        second,
        sensitivity: resolved,
        sealed,
        // Both links are foreign keys into this table and neither is deferrable, so binding them
        // here fails with 23503 on any store holding a correction: the archive is ordered by id
        // and the row a link names is as likely to sit below it as above. `restore` writes them
        // through `relink_restored` once every row has landed.
        supersedes: None,
        superseded_by: None,
        superseded_at: parse_opt(rec.superseded_at.as_deref()).map_err(DomainError::validation)?,
        occurred_at: parse_opt(rec.occurred_at.as_deref()).map_err(DomainError::validation)?,
        occurred_until: parse_opt(rec.occurred_until.as_deref())
            .map_err(DomainError::validation)?,
        access_count: rec.access_count,
        last_accessed_at: parse_opt(rec.last_accessed_at.as_deref())
            .map_err(DomainError::validation)?,
        last_confirmed_at: parse_opt(rec.last_confirmed_at.as_deref())
            .map_err(DomainError::validation)?,
        created_at: parse_opt(Some(rec.created_at.as_str()))
            .map_err(DomainError::validation)?
            .ok_or_else(|| DomainError::validation("a restored row needs a created_at"))?,
    };
    Ok((row, missed))
}

/// The refusals a row meets before anything is written, and the level it lands at.
///
/// Both entry points need this: restore because it is a second insert path, and a merge dry run
/// because a report saying "everything would apply" and an import refusing four thousand rows is
/// the outcome the dry run exists to prevent. The order matches `services::write::run`, so the
/// message a dry run predicts is the message the real run produces.
fn classify(
    ctx: &Ctx,
    namespace: &str,
    content: &str,
    sensitivity: &str,
) -> Result<(String, Sensitivity)> {
    if content.trim().is_empty() {
        return Err(DomainError::validation("a memory row with no content cannot be applied"));
    }
    let namespace = namespaces::normalize(namespace)?;
    namespaces::check_writable(&namespace)?;

    // A credentials namespace never takes plaintext, whatever an archive says it holds. The shape
    // is what the row claims to be, so the shape decides, ahead of the operator-editable table.
    if namespaces::requires_client_sealing(&namespace) {
        return Err(DomainError::validation(format!(
            "{namespace} holds client-encrypted items and this row arrived as plaintext. Sealed \
             items travel in their own section and are copied rather than rewritten."
        )));
    }

    let requested = Sensitivity::parse(sensitivity).ok_or_else(|| {
        DomainError::validation(format!(
            "sensitivity {sensitivity:?} is not one of open, private, sealed"
        ))
    })?;
    // The archive's level is a request, not an override. A namespace whose default is higher here
    // than it was at the source raises the row.
    let resolved = ctx.cfg.policy.defaults.resolve_for_write(&namespace, Some(requested));

    if resolved > ctx.cfg.policy.max_write_sensitivity {
        return Err(DomainError::validation(format!(
            "this row classifies as {resolved} in {namespace} and this server accepts up to {}",
            ctx.cfg.policy.max_write_sensitivity
        )));
    }
    assert_writable(&ctx.principal, &namespace, resolved)?;

    // The one check whose absence is silent. Nothing below this layer ties `private` to ciphertext:
    // the CHECK constraint keys on `content` against `content_ct` and never reads `sensitivity`, so
    // a restore into an install with no KEK would write every private fact to disk in the clear and
    // report success.
    if resolved.is_encrypted() {
        ctx.assert_can_encrypt()?;
    }

    if resolved == Sensitivity::Open {
        if let Some(finding) = tripwire::scan(content) {
            // The matched text stays in the log. An error message carrying a secret puts it in
            // whatever transcript or bug report the error lands in next.
            tracing::warn!(
                client = %ctx.principal.client,
                namespace = %namespace,
                rule = %finding.rule,
                "credential tripwire refused an archived row at open"
            );
            return Err(DomainError::validation(format!(
                "this row matches the {} pattern and will not be stored at open. The matched text \
                 is not repeated here on purpose.",
                finding.rule
            )));
        }
    }

    Ok((namespace, resolved))
}

/// Sealed items, aliases, registry rows and registry history, in both modes.
///
/// Three of the four replay. Registry history does not, for the reason
/// [`REGISTRY_HISTORY_NO_WRITE_PATH`] carries, and an import counts those rows instead.
///
/// Every failure here is one row and the run carries on, the same rule the memory loop follows. An
/// archive whose registry the destination refuses is still an archive whose memories landed, and an
/// operator reading a list of refusals can act on it. A run that aborted on the first one cannot be
/// resumed from anywhere useful.
async fn apply_side_tables(
    ctx: &Ctx,
    archive: &Archive,
    dry_run: bool,
    report: &mut ApplyReport,
) -> Result<()> {
    if !dry_run {
        // Sealed items are copied, never rewritten. The server holds no key for them and binds no
        // associated data, so the bytes that came out of one store open in another under the same
        // client key. `sealed::put` is the same door a client writes through, which is what keeps
        // the sealed grant, the base64 check and the size ceiling on this path.
        for s in &archive.sealed_items {
            let put = super::sealed::put(ctx, &s.namespace, &s.key_hmac, &s.ciphertext, &s.alg);
            if let Err(e) = put.await {
                report.refused.push((
                    // The key is already an HMAC the client computed, so naming it here reveals
                    // nothing the archive did not already carry.
                    format!("sealed_item {}/{}", s.namespace, s.key_hmac),
                    e.client_message().to_string(),
                ));
            }
        }

        for a in &archive.aliases {
            let since = parse_opt(a.since.as_deref()).map_err(DomainError::validation)?;
            let until = parse_opt(a.until.as_deref()).map_err(DomainError::validation)?;
            let put = ctx
                .repos
                .aliases
                .put(
                    ctx.tenant(),
                    NewAlias {
                        namespace: a.namespace.clone(),
                        alias: a.alias.clone(),
                        canonical: a.canonical.clone(),
                        since,
                        until,
                        origin: a.origin.clone(),
                    },
                )
                .await;
            if let Err(e) = put {
                report.refused.push((
                    format!("alias {}/{}", a.namespace, a.alias),
                    e.client_message().to_string(),
                ));
            }
        }

        // The registry, back through `services::registry::set`.
        //
        // The service rather than the port, because the checks between them are the ones a bulk
        // import most needs: the registry-write capability, the canonical key, the namespace
        // ceiling and the tripwire. `apply` refuses to run at all with the tripwire off, and
        // reaching past it to `upsert` would make that refusal a lie for every registry row.
        //
        // The cost is provenance. The row lands stamped with the importing client and today's
        // date, and the archive's `created_at` stays in the file. A registry entry is a fact about
        // a machine rather than a dated observation, so the value is what has to survive.
        for r in &archive.registry {
            // `build` wrote the value as JSON text so a string keeps its quotes. Parsing it back is
            // what makes the round trip total: without this, `8080` returns as the string "8080".
            let value = match serde_json::from_str::<serde_json::Value>(&r.value) {
                Ok(v) => v,
                Err(_) => {
                    report.refused.push((
                        format!("registry {}/{}", r.namespace, r.key),
                        "value is not JSON".to_string(),
                    ));
                    continue;
                }
            };
            let set = super::registry::set(
                ctx,
                &r.namespace,
                &r.kind,
                &r.key,
                &value,
                Some(r.sensitivity.as_str()),
                None,
            );
            if let Err(e) = set.await {
                report.refused.push((
                    format!("registry {}/{}", r.namespace, r.key),
                    e.client_message().to_string(),
                ));
            }
        }
    }

    // Counted in both modes, dry run included. A replaced value that cannot come back is the one
    // thing an operator wants to read before the import rather than after it.
    for r in &archive.registry_history {
        report.refused.push((
            format!("registry_history {}/{}", r.namespace, r.key),
            REGISTRY_HISTORY_NO_WRITE_PATH.to_string(),
        ));
    }
    Ok(())
}

/// An RFC 3339 instant, or a reason naming the field rather than an aborted archive.
///
/// Timestamps travel as strings so one unparseable instant costs one row. A reader that refused the
/// whole file over it would be the worse answer.
fn parse_opt(raw: Option<&str>) -> std::result::Result<Option<DateTime<Utc>>, String> {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(text)
        .map(|t| Some(t.with_timezone(&Utc)))
        .map_err(|_| format!("{text:?} is not an RFC 3339 timestamp"))
}

fn parse_uuid_opt(raw: Option<&str>) -> Result<Option<uuid::Uuid>> {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    uuid::Uuid::parse_str(text)
        .map(Some)
        .map_err(|_| DomainError::validation(format!("{text:?} is not a uuid")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::errors::Kind;

    #[test]
    fn an_archive_error_about_the_file_reaches_the_caller_and_an_io_error_does_not() {
        let short = ArchiveError::Short { section: "memory", expected: 2, found: 1 };
        let mapped: DomainError = short.into();
        assert_eq!(mapped.kind, Kind::Validation);
        assert!(mapped.client_message().contains("memory"));

        let io: DomainError = ArchiveError::Io(std::io::Error::other("disk went away")).into();
        assert_eq!(io.kind, Kind::Internal);
        assert!(!io.client_message().contains("disk"));
    }

    #[test]
    fn a_timestamp_that_will_not_parse_names_itself_rather_than_failing_silently() {
        assert_eq!(parse_opt(None).unwrap(), None);
        assert_eq!(parse_opt(Some("  ")).unwrap(), None);
        assert!(parse_opt(Some("2026-08-30T11:04:00Z")).unwrap().is_some());
        assert!(parse_opt(Some("30 August")).unwrap_err().contains("30 August"));
    }

    #[test]
    fn a_registry_value_keeps_its_json_type_through_the_archive() {
        assert_eq!(value_text(&serde_json::json!("Ubuntu 26.04")), "\"Ubuntu 26.04\"");
        assert_eq!(value_text(&serde_json::json!(8080)), "8080");
    }
}
