//! Restore mode, against a real database. Skipped when none is reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom \
//!     cargo test --test archive_restore
//!
//! Restore is the second insert path in this codebase, so the tests are about what it keeps rather
//! than about what it writes.
//!
//! The fixture that carries the most is the one with no KEK. Nothing below the service layer ties
//! `private` to ciphertext: the CHECK constraint keys on `content` against `content_ct` and never
//! reads `sensitivity`, and the adapter picks the representation from whether a sealed value was
//! passed. A restore into a KEK-less install that skipped `assert_can_encrypt` would write every
//! private fact to disk in the clear and report success, and no row count would notice.
//! `Key::Absent` exists for that one refusal.
//!
//! The chain test is the other half. `supersedes` and `superseded_by` are both foreign keys into
//! `memory` and neither is deferrable, so a row bound to a successor that has not been inserted yet
//! fails on the insert. Restore walks the archive in file order and a retired row precedes its
//! successor, which is the case every store holding a single correction produces.

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
use lumberroom_server::domain::errors::Kind;
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Memory, Principal};
use lumberroom_server::services::{archive, bootstrap, write, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_TEST_KEK";
const TEST_KEK_ID: &str = "kek-test";
const OWNER_TOKEN: &str = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr";

/// Restore preserves these, which is the whole reason the mode exists.
const ID_FIRST: &str = "0195c0de-0000-7000-8000-000000000001";
const ID_SECOND: &str = "0195c0de-0000-7000-8000-000000000002";
const CREATED_AT: &str = "2024-03-01T09:15:00Z";
const OCCURRED_AT: &str = "2024-02-14T00:00:00Z";
const SUPERSEDED_AT: &str = "2026-08-30T11:04:00Z";

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

    /// Rows holding readable text. The assertion a KEK-less install has to fail: a private fact
    /// stored in the clear satisfies every row count and only this one says so.
    async fn plaintext_rows(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM memory WHERE tenant_id = $1 AND content IS NOT NULL",
        )
        .bind(self.tenant())
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn row(&self, id: &str) -> Memory {
        self.ctx
            .repos
            .memories
            .find_by_id(self.tenant(), uuid::Uuid::parse_str(id).unwrap())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("row {id} is not in the store"))
    }
}

/// Whether this install can encrypt. The one axis the fixtures differ on.
#[derive(Clone, Copy)]
enum Key {
    Present,
    Absent,
}

async fn setup(key: Key) -> Option<Harness> {
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

    // `Key::Absent` is an install running with KEK_PROVIDER=none, which is the stock open-source
    // deployment. Nothing else about it differs.
    let (keys, kek_verified) = match key {
        Key::Present => {
            let keys: Arc<dyn KeyProvider> =
                Arc::new(EnvKeyProvider::new(TEST_KEK_VAR, TEST_KEK_ID));
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
            (Some(keys), !matches!(check, postgres::KekCheck::Mismatch { .. }))
        }
        Key::Absent => (None, false),
    };

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
        keys,
        kek_verified,
        principal: owner_like("mac"),
        invocation: Invocation::Cli,
        session_id: Some("archive-restore-test".into()),
    };
    bootstrap::clear_cache();

    Some(Harness { ctx, pool, _serial: guard, _db: db_lock })
}

macro_rules! harness_or_skip {
    ($key:expr) => {
        match setup($key).await {
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

/// A row as an export of a two-year-old store would carry it: every counter and every clock set.
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
        occurred_at: Some(OCCURRED_AT.into()),
        occurred_until: None,
        access_count: 4,
        last_accessed_at: Some("2026-01-09T18:20:00Z".into()),
        last_confirmed_at: Some("2026-01-09T18:20:00Z".into()),
        created_at: CREATED_AT.into(),
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
    let bytes =
        w.finish(SUPERSEDED_AT.into(), true, Excluded::default(), &Sealing::Plaintext).unwrap();
    archive::open(ctx, &bytes, &Sealing::Plaintext).unwrap()
}

#[tokio::test]
async fn restore_refuses_a_target_that_holds_a_memory() {
    let h = harness_or_skip!(Key::Present);
    write::run(&h.ctx, "The study desktop runs Ubuntu 26.04.", "user:me", None, None, None, None)
        .await
        .expect("seeding one row");

    let archive = archive_of(&h.ctx, vec![memory(ID_FIRST, "A fact from another store.", "open")]);
    let err = archive::apply(&h.ctx, &archive, archive::Mode::Restore, false, &HashMap::new())
        .await
        .expect_err("restore ran into an occupied store");

    assert_eq!(err.kind, Kind::Conflict);
    assert!(err.to_string().contains("holds"), "{err}");
    assert_eq!(h.rows().await, 1, "restore wrote into a store it should have refused");
}

#[tokio::test]
async fn restore_refuses_a_private_row_when_the_install_cannot_encrypt() {
    // The refusal whose absence is silent. Without it the row lands in `content` as readable text
    // and the report says the restore succeeded.
    let h = harness_or_skip!(Key::Absent);
    let row = memory(ID_FIRST, "Renewal for the flat is due in March.", "private");
    let archive = archive_of(&h.ctx, vec![row]);

    let err = archive::apply(&h.ctx, &archive, archive::Mode::Restore, false, &HashMap::new())
        .await
        .expect_err("a private row was restored into an install with no key");

    // Unavailable rather than Validation: nothing the caller sends fixes this, and it works once
    // the operator configures a key.
    assert_eq!(err.kind, Kind::Unavailable);
    assert_eq!(h.rows().await, 0, "the refusal came after a write");
    assert_eq!(h.plaintext_rows().await, 0, "a private fact reached disk in the clear");
}

#[tokio::test]
async fn a_restored_row_keeps_its_id_and_its_timestamps() {
    let h = harness_or_skip!(Key::Present);
    let archive = archive_of(&h.ctx, vec![memory(ID_FIRST, "Coffee, black, no sugar.", "open")]);

    let report = archive::apply(&h.ctx, &archive, archive::Mode::Restore, false, &HashMap::new())
        .await
        .expect("the restore");
    assert_eq!(report.applied, 1, "refusals: {:?}", report.refused);

    let row = h.row(ID_FIRST).await;
    assert_eq!(row.id, ID_FIRST, "restore minted a new id");
    assert_eq!(row.created_at.to_rfc3339(), "2024-03-01T09:15:00+00:00");
    let occurred = row.occurred_at.map(|t| t.to_rfc3339());
    assert_eq!(occurred.as_deref(), Some("2024-02-14T00:00:00+00:00"));
    assert_eq!(row.access_count, 4);
    assert!(row.last_confirmed_at.is_some());
    assert_eq!(row.source_client, "claude-code");

    // Provenance travels; vectors do not. A query vector from one model against document vectors
    // from another returns confident nonsense rather than an error, so the destination embeds every
    // row it accepts with its own model and records that model rather than the archive's.
    assert_ne!(row.embedding_model.as_deref(), Some("bge-small-en-v1.5"));
}

#[tokio::test]
async fn a_restored_chain_keeps_both_links() {
    // The retired row comes first in the file and names a successor that has not been inserted yet.
    // `superseded_by` is a foreign key into this table and it is not deferrable, so a restore that
    // binds the link on the insert fails here on every store holding a single correction.
    let h = harness_or_skip!(Key::Present);

    let mut first = memory(ID_FIRST, "The study desktop runs Ubuntu 24.04.", "open");
    first.superseded_by = Some(ID_SECOND.into());
    first.superseded_at = Some(SUPERSEDED_AT.into());
    let mut second = memory(ID_SECOND, "The study desktop runs Ubuntu 26.04.", "open");
    second.supersedes = Some(ID_FIRST.into());

    let archive = archive_of(&h.ctx, vec![first, second]);
    let report = archive::apply(&h.ctx, &archive, archive::Mode::Restore, false, &HashMap::new())
        .await
        .expect("the restore");
    assert_eq!(report.applied, 2, "refusals: {:?}", report.refused);

    let retired = h.row(ID_FIRST).await;
    let live = h.row(ID_SECOND).await;
    assert_eq!(retired.superseded_by.as_deref(), Some(ID_SECOND));
    assert_eq!(live.supersedes.as_deref(), Some(ID_FIRST));
    assert!(live.is_live(), "the successor came back retired");
}
