//! Merge mode, against a real database. Skipped when none is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom \
//!     cargo test --test archive_merge
//!
//! One fixture carries most of this file, and its shape is the whole point. Three rows: an open
//! one, a second that supersedes it, and a private one. Those last two are precisely the rows the
//! write path's dedupe bands cannot recognise on a second pass. Both bands sit inside
//! `if supersedes_id.is_none()`, the exact-match path also requires the row to resolve to `open`,
//! and `collapse_target` returns `None` for a row that is no longer live. A replay leaning on them
//! duplicates every superseded row, every private row, and every row the first pass retired. A
//! fixture of three plain open rows passes while the feature is broken, which is why this one is
//! not that.
//!
//! The second property here is the id remapping. Merge mints new ids, so a chain in the file has to
//! be rewritten onto them, and a row the write path collapsed into an existing one leaves a
//! survivor its successor must point at. That case has no obviously right answer, so the design
//! picked one and this pins it.
//!
//! The third is file order. An export reads the store `ORDER BY id` over v4 uuids, so a chain
//! reaches the file in whatever order its ids happen to fall in, and half of all real archives
//! carry the successor ahead of the row it retired. A fixture that always lists the predecessor
//! first never asks what merge does with a `supersedes` naming a row it has not reached yet.
//!
//! Archives are sealed `Plaintext` throughout. The age layer is the format crate's own test's
//! subject, and a passphrase stretch per fixture would buy this file nothing but seconds.

use std::collections::HashMap;
use std::sync::Arc;

use lumberroom_archive::container::Sealing;
use lumberroom_archive::reader::Archive;
use lumberroom_archive::writer::ArchiveWriter;
use lumberroom_archive::{Excluded, MemoryRecord, Record, Source};
use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::services::{archive, bootstrap, write, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";
const OWNER_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The archive's own ids. Merge never keeps them, and half the assertions here are about that.
const ID_FIRST: &str = "0195c0de-0000-7000-8000-000000000001";
const ID_SECOND: &str = "0195c0de-0000-7000-8000-000000000002";
const ID_PRIVATE: &str = "0195c0de-0000-7000-8000-000000000003";

const WHEN: &str = "2026-08-30T11:04:00Z";

/// Every test here truncates the shared test database, so they serialise themselves rather than
/// relying on `--test-threads=1` being remembered.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A setup step that is allowed to be missing, with the reason printed rather than swallowed. The
/// suite skips rather than fails, which makes that sentence the only thing standing between a
/// broken run and a run somebody reads as a pass.
macro_rules! step {
    ($what:expr, $result:expr) => {
        match $result {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping: {} failed: {e:?}", $what);
                return None;
            }
        }
    };
}

struct Harness {
    ctx: Ctx,
    pool: PgPool,
    _serial: tokio::sync::MutexGuard<'static, ()>,
    /// Held for the whole test. The mutex above serialises this binary's own threads; this is what
    /// keeps the other test binaries out of the same database.
    _db: common::DbGuard,
}

impl Harness {
    fn tenant(&self) -> &str {
        self.ctx.tenant()
    }

    async fn rows(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM memory WHERE tenant_id = $1")
            .bind(self.tenant())
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn superseded_by(&self, id: &str) -> Option<String> {
        let found: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT superseded_by FROM memory WHERE tenant_id = $1 AND id = $2")
                .bind(self.tenant())
                .bind(uuid::Uuid::parse_str(id).unwrap())
                .fetch_one(&self.pool)
                .await
                .unwrap();
        found.map(|u| u.to_string())
    }

    /// Rows stored as ciphertext. Nothing below the service layer ties `private` to encryption, so
    /// a merge that wrote a private fact in the clear would satisfy every count in this file and
    /// only this query would notice.
    async fn sealed_rows(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM memory
              WHERE tenant_id = $1 AND sensitivity = 'private'
                AND content IS NULL AND content_ct IS NOT NULL",
        )
        .bind(self.tenant())
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }
}

async fn setup() -> Option<Harness> {
    let guard = SERIAL.lock().await;
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let base = admin_url.rsplit_once('/')?.0.to_string();
    let admin = step!("connecting to the admin database", PgPool::connect(&admin_url).await);

    let exists: Result<Option<i32>, _> =
        sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(TEST_DB)
            .fetch_optional(&admin)
            .await;
    let exists = step!("looking for the test database", exists);
    if exists.is_none() {
        // DDL cannot take a bind parameter, so this is the one statement here that has to be built
        // as a string. Audited: TEST_DB is a compile-time constant with no external input.
        let created = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {TEST_DB}")))
            .execute(&admin)
            .await;
        step!("creating the test database", created);
    }
    admin.close().await;

    let url = format!("{base}/{TEST_DB}");
    std::env::set_var("DATABASE_URL", &url);
    std::env::set_var(
        "AUTH_TOKENS",
        format!(
            r#"[{{"client":"mac","token":"{OWNER_TOKEN}","read":[{{"namespace":"*","max":"sealed"}}],"write":[{{"namespace":"*","max":"sealed"}}],"sealedCapable":true,"registryWrite":true}}]"#
        ),
    );
    std::env::set_var("EMBED_PROVIDER", "hash");
    std::env::set_var(TEST_KEK_VAR, TEST_KEK_HEX);

    // Before the truncate below, and before anything reads. Every other binary targeting this
    // database waits here.
    let db_lock = common::lock_database(&url).await?;
    let pool = step!("connecting to the test database", postgres::connect(&url).await);
    step!("migrating the test database", postgres::migrate(&pool).await);
    let truncated = sqlx::query(
        "TRUNCATE memory, registry, registry_history, entity_alias, sealed_item, tool_calls,
                  registry_alias, kek_state,
                  oauth_client, oauth_code, oauth_token, oauth_refresh,
                  ingest_proposal, ingest_proposal_source, ingest_watermark, ingest_run,
                  cleanup_proposal, cleanup_proposal_member, cleanup_watermark, subject_cardinality,
                  recall_emission
         RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await;
    step!("truncating the test database", truncated);

    let cfg: Config = step!("loading the config", config::load());
    let keys: Arc<dyn KeyProvider> = Arc::new(EnvKeyProvider::new(TEST_KEK_VAR, TEST_KEK_ID));
    let kek = step!("reading the test key", keys.kek().await);
    let check = postgres::verify_kek(
        &pool,
        &cfg.tenant_id,
        TEST_KEK_ID,
        &lumberroom_server::crypto::kek::fingerprint(&kek),
        keys.provider(),
    )
    .await;
    let check = step!("verifying the test key", check);
    let kek_verified = !matches!(check, postgres::KekCheck::Mismatch { .. });

    let memories = Arc::new(postgres::PgMemoryRepository::new(pool.clone()));
    let ctx = Ctx {
        cfg: Arc::new(cfg),
        repos: Repos {
            aliases: Arc::new(postgres::PgAliasRepository::new(pool.clone())),
            memories: memories.clone(),
            registry: Arc::new(postgres::PgRegistryRepository::new(pool.clone())),
            tool_calls: Arc::new(postgres::PgToolCallRepository::new(pool.clone())),
            sealed: Some(Arc::new(postgres::PgSealedRepository::new(pool.clone()))),
            ciphertext: Some(memories),
            oauth: None,
        },
        embedder: Arc::new(HashEmbedder::new(768)),
        // A private row is in the fixture on purpose, so this half of the harness is load-bearing:
        // without a key the write path refuses it and the replay assertions never run.
        keys: Some(keys),
        kek_verified,
        principal: owner_like("mac"),
        invocation: Invocation::Cli,
        session_id: Some("archive-merge-test".into()),
    };
    bootstrap::clear_cache();

    Some(Harness { ctx, pool, _serial: guard, _db: db_lock })
}

macro_rules! harness_or_skip {
    () => {
        match setup().await {
            Some(h) => h,
            None => {
                eprintln!("skipping: no database reachable");
                return;
            }
        }
    };
}

fn owner_like(client: &str) -> Principal {
    Principal {
        client: client.into(),
        token_id: "test".into(),
        mode: "token",
        scopes: vec![],
        read: NamespaceGrant::everything(),
        write: NamespaceGrant::everything(),
        registry_write: true,
        sealed_capable: true,
        may_delete: true,
        may_ingest: true,
        may_read_history: true,
    }
}

fn memory(id: &str, content: &str, sensitivity: &str) -> MemoryRecord {
    MemoryRecord {
        id: id.into(),
        namespace: "user:me".into(),
        content: content.into(),
        tags: vec!["preference".into()],
        source_client: "claude-code".into(),
        sensitivity: sensitivity.into(),
        supersedes: None,
        superseded_by: None,
        superseded_at: None,
        occurred_at: None,
        occurred_until: None,
        access_count: 4,
        last_accessed_at: None,
        last_confirmed_at: None,
        created_at: WHEN.into(),
        embedding_model: Some("bge-small-en-v1.5".into()),
    }
}

/// Bytes through the real writer and back through the real reader, so a fixture cannot hold a
/// record shape the format would not carry.
fn archive_of(ctx: &Ctx, records: Vec<MemoryRecord>) -> Archive {
    let mut w = ArchiveWriter::new(Source { kind: "oss".into(), build: "test".into() });
    for r in records {
        w.push(Record::Memory(r));
    }
    let bytes = w.finish(WHEN.into(), true, Excluded::default(), &Sealing::Plaintext).unwrap();
    archive::open(ctx, &bytes, &Sealing::Plaintext).unwrap()
}

/// The fixture the file's header argues for: a chain and a private row.
fn chain_and_a_private_row() -> Vec<MemoryRecord> {
    let mut first = memory(ID_FIRST, "The study desktop runs Ubuntu 24.04.", "open");
    first.superseded_by = Some(ID_SECOND.into());
    first.superseded_at = Some(WHEN.into());

    let mut second = memory(ID_SECOND, "The study desktop runs Ubuntu 26.04.", "open");
    second.supersedes = Some(ID_FIRST.into());

    let private = memory(ID_PRIVATE, "Renewal for the flat is due in March.", "private");

    vec![first, second, private]
}

/// The same three rows with the successor ahead of the row it retired.
fn a_chain_the_other_way_round() -> Vec<MemoryRecord> {
    let mut records = chain_and_a_private_row();
    records.swap(0, 1);
    records
}

#[tokio::test]
async fn replaying_a_half_applied_merge_inserts_nothing_new() {
    let h = harness_or_skip!();
    let archive = archive_of(&h.ctx, chain_and_a_private_row());

    let first = archive::apply(&h.ctx, &archive, archive::Mode::Merge, false, &HashMap::new())
        .await
        .expect("the first merge");
    assert_eq!(first.applied, 3, "refusals: {:?}", first.refused);
    assert_eq!(first.collapsed, 0);
    assert!(first.refused.is_empty(), "refusals: {:?}", first.refused);
    assert_eq!(first.id_map.len(), 3);
    assert_eq!(h.sealed_rows().await, 1, "the private row did not land as ciphertext");

    let before = h.rows().await;
    assert_eq!(before, 3);

    // The map from the interrupted run, which is the only thing that makes this safe. The dedupe
    // bands see none of these three rows: two carry a supersession link and one is private.
    let second = archive::apply(&h.ctx, &archive, archive::Mode::Merge, false, &first.id_map)
        .await
        .expect("the replay");
    assert_eq!(second.applied, 0);
    assert_eq!(second.collapsed, 0);
    assert_eq!(second.skipped_already_applied, 3);
    assert!(second.refused.is_empty(), "refusals: {:?}", second.refused);
    assert_eq!(h.rows().await, before, "the replay wrote rows");
}

#[tokio::test]
async fn merge_rewrites_a_chain_onto_the_ids_it_minted() {
    let h = harness_or_skip!();
    let archive = archive_of(&h.ctx, chain_and_a_private_row());

    let report = archive::apply(&h.ctx, &archive, archive::Mode::Merge, false, &HashMap::new())
        .await
        .expect("the merge");

    let new_first = report.id_map.get(ID_FIRST).expect("the retired row landed");
    let new_second = report.id_map.get(ID_SECOND).expect("its successor landed");
    assert_ne!(new_first, ID_FIRST, "merge kept the archive's id");

    // Walked rather than counted. A count of three rows is equally true of an import that dropped
    // the link and wrote both rows live, which is the failure this assertion exists for.
    assert_eq!(h.superseded_by(new_first).await.as_deref(), Some(new_second.as_str()));
    assert_eq!(h.superseded_by(new_second).await, None, "the live head was retired");
}

#[tokio::test]
async fn a_successor_read_before_its_predecessor_lands_without_a_refusal() {
    // A `supersedes` naming a row merge has not reached yet resolves to nothing on the first pass,
    // and the second pass closes the chain from the other end. Recording a refusal on the way past
    // reports a healthy import as damaged, which sends an operator hunting for a row that is fine.
    let h = harness_or_skip!();
    let archive = archive_of(&h.ctx, a_chain_the_other_way_round());

    let report = archive::apply(&h.ctx, &archive, archive::Mode::Merge, false, &HashMap::new())
        .await
        .expect("the merge");

    assert!(report.refused.is_empty(), "refusals: {:?}", report.refused);
    assert_eq!(report.applied, 3);
    assert_eq!(report.collapsed, 0);

    // The chain still has to close, and from this direction it can only close on the second pass.
    let new_first = report.id_map.get(ID_FIRST).expect("the retired row landed");
    let new_second = report.id_map.get(ID_SECOND).expect("its successor landed");
    assert_eq!(h.superseded_by(new_first).await.as_deref(), Some(new_second.as_str()));
    assert_eq!(h.superseded_by(new_second).await, None, "the live head was retired");
}

#[tokio::test]
async fn a_chain_whose_predecessor_collapsed_points_at_the_survivor() {
    // The remapping case with no obviously right answer, and the design picked one: a reference to
    // a row the write path collapsed resolves to the row that survived the collapse. Getting this
    // wrong drops the link and leaves two live rows that contradict each other.
    let h = harness_or_skip!();
    let content = "The study desktop runs Ubuntu 24.04.";

    let seeded = write::run(&h.ctx, content, "user:me", None, None, Some("open"), None)
        .await
        .expect("seeding the row the archive will collapse into");

    let mut first = memory(ID_FIRST, content, "open");
    first.superseded_by = Some(ID_SECOND.into());
    let mut second = memory(ID_SECOND, "The study desktop runs Ubuntu 26.04.", "open");
    second.supersedes = Some(ID_FIRST.into());
    let archive = archive_of(&h.ctx, vec![first, second]);

    let report = archive::apply(&h.ctx, &archive, archive::Mode::Merge, false, &HashMap::new())
        .await
        .expect("the merge");

    assert_eq!(report.collapsed, 1, "the duplicate was not collapsed");
    assert_eq!(report.applied, 1);
    assert_eq!(report.id_map.get(ID_FIRST), Some(&seeded.id), "the reference lost its survivor");

    let new_second = report.id_map.get(ID_SECOND).expect("the successor landed");
    assert_eq!(h.superseded_by(&seeded.id).await.as_deref(), Some(new_second.as_str()));
    assert_eq!(h.rows().await, 2, "the collapse still wrote a second copy");
}

#[tokio::test]
async fn a_dry_run_writes_nothing_and_still_reports_what_would_land() {
    let h = harness_or_skip!();
    let archive = archive_of(&h.ctx, chain_and_a_private_row());

    let report = archive::apply(&h.ctx, &archive, archive::Mode::Merge, true, &HashMap::new())
        .await
        .expect("the dry run");

    assert_eq!(report.applied, 3, "refusals: {:?}", report.refused);
    assert!(report.refused.is_empty(), "refusals: {:?}", report.refused);
    // An invented id in a map the job persists is worse than no map at all.
    assert!(report.id_map.is_empty());
    assert_eq!(h.rows().await, 0, "the dry run wrote rows");
}

#[tokio::test]
async fn an_import_refuses_to_start_when_the_tripwire_is_off() {
    // The tripwire is the backstop for a row whose classification was wrong in the expensive
    // direction, and a bulk import is where that goes wrong at scale. It runs at `open` alone, so
    // this refusal is about open rows and nothing here implies more.
    let h = harness_or_skip!();
    let archive = archive_of(&h.ctx, chain_and_a_private_row());

    let mut cfg = (*h.ctx.cfg).clone();
    cfg.policy.tripwire = false;
    let ctx = Ctx { cfg: Arc::new(cfg), ..h.ctx.clone() };

    let err = archive::apply(&ctx, &archive, archive::Mode::Merge, false, &HashMap::new())
        .await
        .expect_err("an import ran with the tripwire off");
    assert_eq!(err.kind, lumberroom_server::domain::errors::Kind::Unavailable);
    // The variable's name, not the word. An operator reading this refusal types what it says, and a
    // match on "tripwire" alone passes against a name no install has ever had.
    assert!(err.to_string().contains("SENSITIVITY_TRIPWIRE"), "{err}");
    assert_eq!(h.rows().await, 0);
}
