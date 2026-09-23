//! Returning a refused cleanup finding to the queue, against a real database. Skipped when none is
//! reachable.
//!
//!   DATABASE_URL=postgres://lumberroom:pw@127.0.0.1:5432/lumberroom cargo test -j 1 --test cleanup_unreject
//!
//! One property carries this file: a cluster the owner refused stays out of the queue however many
//! times the pass finds it again, and comes back when he says so.
//!
//! Both halves are claims about what SQL does. `queue` swallows a cluster it already holds in any
//! state, rejected included, and that is deliberate: it is what makes an hourly pass safe to run
//! hourly. The price is a finding refused because the pass that wrote it was wrong, which blocks
//! its own replacement under the same cluster key.
//!
//! The assertions go through `CleanupRepository` rather than `cleanup::run`. A pass has a
//! similarity query in front of it, and a test that drives the whole pass can watch that query find
//! the pair and read it as the queue having reopened. That mistake was already made here once and
//! it made the test pass against the mutation it existed to catch. `repo.queue` is the exact call
//! `cleanup::run` makes once it has a cluster, and nothing else is in the way.

use std::sync::Arc;

use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres::{self, PgCleanupRepository};
use lumberroom_server::config::{self, Config};
use lumberroom_server::domain::cleanup::{CleanupKind, Disposition};
use lumberroom_server::domain::errors::Kind;
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::ports::cleanup::{CleanupRepository, NewMember, NewProposal, QueueOutcome};
use lumberroom_server::services::{cleanup, Ctx, Repos};
use sqlx::PgPool;

mod common;

const TEST_DB: &str = "lumberroom_rust_test";
const OWNER_TOKEN: &str = "uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu";

/// Every test here truncates the shared test database, so they serialise themselves rather than
/// relying on `--test-threads=1` being remembered. Cargo runs one test binary at a time, so this
/// mutex and the ones in the other binaries do not have to know about each other.
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
    repo: Arc<dyn CleanupRepository>,
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

    async fn state_of(&self, id: &str) -> String {
        sqlx::query_scalar("SELECT state FROM cleanup_proposal WHERE id = $1")
            .bind(uuid::Uuid::parse_str(id).unwrap())
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
        // No key and nothing encrypted. Every row here is written at `open`, and the queue this
        // file is about holds ids and rationales rather than content.
        keys: None,
        kek_verified: false,
        principal: owner_like("mac"),
        invocation: Invocation::Cli,
        session_id: Some("cleanup-unreject-test".into()),
    };

    let repo: Arc<dyn CleanupRepository> = Arc::new(PgCleanupRepository::new(pool.clone()));
    Some(Harness { ctx, repo, pool, _serial: guard, _db: db_lock })
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

/// A row inserted straight into the table, bypassing `write::run`, which collapses a near-identical
/// write into the row it matches and so cannot make the duplicates this queue is about.
async fn put_raw(h: &Harness, content: &str) -> String {
    let id = uuid::Uuid::new_v4();
    let vectors = h.ctx.embedder.embed_documents(vec![content.to_string()]).await.unwrap();
    let embedding = pgvector::Vector::from(vectors[0].clone());
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, embedding, source_client,
                             embedding_model, sensitivity)
         VALUES ($1, $2, 'user:me', $3, $4, 'test', 'hash', 'open')",
    )
    .bind(id)
    .bind(h.tenant())
    .bind(content)
    .bind(embedding)
    .execute(&h.pool)
    .await
    .unwrap();
    id.to_string()
}

/// The cluster a pass hands `queue`, built the same way every time so a second call carries the
/// same cluster key. This is the pass finding the same pair again.
fn cluster(keep: &str, retire: &str) -> NewProposal {
    NewProposal {
        kind: CleanupKind::Paraphrase,
        namespace: "user:me".into(),
        keep_id: Some(keep.into()),
        rationale: "both rows say the deploy runbook is in DEPLOY.md".into(),
        produced_by: "cosine".into(),
        similarity: Some(0.97),
        posted_by: None,
        members: vec![
            NewMember {
                memory_id: keep.into(),
                disposition: Disposition::Keep,
                seen_content: "the deploy runbook lives in DEPLOY.md".into(),
            },
            NewMember {
                memory_id: retire.into(),
                disposition: Disposition::Retire,
                seen_content: "deploy runbook: DEPLOY.md".into(),
            },
        ],
    }
}

/// Whether the queue is offering this finding for a decision.
async fn waiting(h: &Harness, id: &str) -> bool {
    h.repo
        .list(h.tenant(), Some("proposed"), 100, &NamespaceGrant::everything())
        .await
        .unwrap()
        .iter()
        .any(|p| p.id == id)
}

/// The gap this exists to close, end to end.
///
/// Every step is one a pass or the owner actually takes, in the order they take them. The step that
/// used to have no answer is the fourth: the pass runs again with its bug fixed, finds the same two
/// rows, and the queue says nothing, because the cluster key it computes is the key of the row the
/// owner refused an hour ago.
#[tokio::test]
async fn a_refused_finding_stays_out_of_the_queue_until_the_owner_puts_it_back() {
    let h = harness_or_skip!();
    let keep = put_raw(&h, "the deploy runbook lives in DEPLOY.md").await;
    let retire = put_raw(&h, "deploy runbook: DEPLOY.md").await;

    let (outcome, id) = h.repo.queue(h.tenant(), cluster(&keep, &retire)).await.unwrap();
    assert_eq!(outcome, QueueOutcome::Queued);
    assert!(waiting(&h, &id).await, "a new finding waits for a decision");

    let (again, same) = h.repo.queue(h.tenant(), cluster(&keep, &retire)).await.unwrap();
    assert_eq!(again, QueueOutcome::AlreadyKnown, "the next hour's pass finds the same pair");
    assert_eq!(same, id, "and it names the row already in the queue");

    assert!(h.repo.decide(h.tenant(), &id, "rejected", Some("different machines")).await.unwrap());
    assert!(!waiting(&h, &id).await, "a refused finding leaves the waiting list");

    // The pass, fixed and run again. Nothing about this call knows the earlier one was refused.
    let (after, still) = h.repo.queue(h.tenant(), cluster(&keep, &retire)).await.unwrap();
    assert_eq!(after, QueueOutcome::AlreadyKnown, "the cluster key blocks the replacement");
    assert_eq!(still, id);
    assert!(!waiting(&h, &id).await, "and the owner reads a queue that says nothing");

    assert!(h.repo.unreject(h.tenant(), &id).await.unwrap(), "the way back");
    assert!(waiting(&h, &id).await, "the finding is offered again");

    let back = h
        .repo
        .get(h.tenant(), &id, &NamespaceGrant::everything())
        .await
        .unwrap()
        .expect("the row is still there");
    assert_eq!(back.state, "proposed");
    assert_eq!(back.reason, None, "the note explained a decision that has been undone");
    assert!(back.decided_at.is_some(), "the row still says somebody decided it once");
    assert_eq!(back.members.len(), 2, "the cluster is intact, so apply still has both rows");
    assert_eq!(back.keep_id.as_deref(), Some(keep.as_str()));
}

/// One row leaves `rejected` once, and only from `rejected`.
///
/// The predicate is in the WHERE clause rather than checked first, for the reason `decide` gives:
/// two callers racing would otherwise both win, and the second would reopen a finding the first had
/// already carried out.
#[tokio::test]
async fn only_a_refused_finding_returns_and_it_returns_once() {
    let h = harness_or_skip!();
    let keep = put_raw(&h, "the staging host is quartz").await;
    let retire = put_raw(&h, "staging runs on quartz").await;
    let (_, id) = h.repo.queue(h.tenant(), cluster(&keep, &retire)).await.unwrap();

    assert!(
        !h.repo.unreject(h.tenant(), &id).await.unwrap(),
        "a waiting finding has nothing to return from"
    );
    assert_eq!(h.state_of(&id).await, "proposed", "and the row was not touched");

    assert!(h.repo.decide(h.tenant(), &id, "rejected", None).await.unwrap());
    assert!(h.repo.unreject(h.tenant(), &id).await.unwrap());
    assert!(
        !h.repo.unreject(h.tenant(), &id).await.unwrap(),
        "the second caller in a race loses rather than deciding again"
    );
    assert_eq!(h.state_of(&id).await, "proposed");

    // Applied is the state where a second undo would be a retraction of work already done.
    assert!(h.repo.decide(h.tenant(), &id, "applied", None).await.unwrap());
    assert!(!h.repo.unreject(h.tenant(), &id).await.unwrap());
    assert_eq!(h.state_of(&id).await, "applied");

    assert!(!h.repo.unreject(h.tenant(), "not-a-uuid").await.unwrap());
    assert!(
        !h.repo.unreject(h.tenant(), &uuid::Uuid::new_v4().to_string()).await.unwrap(),
        "an id nothing carries is not an error, it is a row that did not move"
    );
}

/// A tenant reaches its own queue and no other's.
#[tokio::test]
async fn the_undo_does_not_reach_across_tenants() {
    let h = harness_or_skip!();
    let keep = put_raw(&h, "the nickname is QUARTZ-A").await;
    let retire = put_raw(&h, "nickname QUARTZ-A").await;
    let (_, id) = h.repo.queue(h.tenant(), cluster(&keep, &retire)).await.unwrap();
    assert!(h.repo.decide(h.tenant(), &id, "rejected", Some("not the same host")).await.unwrap());

    assert!(!h.repo.unreject("somebody-else", &id).await.unwrap());
    assert_eq!(h.state_of(&id).await, "rejected", "another tenant's id names nothing here");

    assert!(h.repo.unreject(h.tenant(), &id).await.unwrap());
}

/// The two refusals a caller has to be able to tell apart.
///
/// An id nothing carries and an id in the wrong state answer differently, because the owner reading
/// the first has typed something wrong and the owner reading the second has not.
#[tokio::test]
async fn the_service_answers_missing_and_wrongly_stated_apart() {
    let h = harness_or_skip!();
    let keep = put_raw(&h, "the backup runs at 03:00").await;
    let retire = put_raw(&h, "backups run at 3am").await;
    let (_, id) = h.repo.queue(h.tenant(), cluster(&keep, &retire)).await.unwrap();

    let nothing = uuid::Uuid::new_v4().to_string();
    let err = cleanup::unreject(&h.ctx, h.repo.as_ref(), &nothing).await.unwrap_err();
    assert_eq!(err.kind, Kind::NotFound, "{err:?}");

    let err = cleanup::unreject(&h.ctx, h.repo.as_ref(), &id).await.unwrap_err();
    assert_eq!(err.kind, Kind::Conflict, "a waiting finding is a conflict rather than a miss");
    assert!(
        err.to_string().contains("proposed"),
        "and the message says what state it is in: {err}"
    );

    assert!(h.repo.decide(h.tenant(), &id, "rejected", Some("wrong pair")).await.unwrap());
    cleanup::unreject(&h.ctx, h.repo.as_ref(), &id).await.expect("a refused finding returns");
    assert_eq!(h.state_of(&id).await, "proposed");

    let err = cleanup::unreject(&h.ctx, h.repo.as_ref(), &id).await.unwrap_err();
    assert_eq!(err.kind, Kind::Conflict, "and returning it twice is the same conflict");
}
