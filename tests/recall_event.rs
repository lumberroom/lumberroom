//! The per-call recall event log, against a real Postgres.
//!
//! `recall_emission` keeps one aggregate row per fact and tool, so it cannot say which rows one
//! particular search or digest returned. `recall_event` keeps one row per (call, memory) behind
//! `RECALL_EVENT_LOG`. These tests pin what it records, what it never records, and when its rows go.
//!
//! Each test builds its own database, so nothing here truncates the shared `lumberroom_rust_test`.
//! The writes land on a spawned task, so every read polls until `recall_emission` shows the same
//! statement committed. Skipped when `DATABASE_URL` is unset or unreachable: a count of zero here
//! is not a green run.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::crypto::kek::{EnvKeyProvider, KeyProvider};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::ports::{Emission, RecallCall, RecallEvent};
use lumberroom_server::services::{bootstrap, forget, recall_events, search, write, Ctx, Repos};
use sqlx::{AssertSqlSafe, PgPool, Row};

const TEST_KEK_HEX: &str = "5375747254657374204b454b20666f722074686520696e746567726174696f6e";
const TEST_KEK_VAR: &str = "LUMBERROOM_RECALL_EVENT_TEST_KEK";
const TEST_KEK_ID: &str = "kek-recall-event";

/// The digest cache is a process-wide static and `config::load` reads the process environment, so
/// the tests in this binary take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Probe {
    ctx: Ctx,
    pool: PgPool,
    admin: PgPool,
    name: &'static str,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Probe {
    async fn drop_db(self) {
        self.pool.close().await;
        // DDL takes no bind parameter. Audited: every caller passes a compile-time literal.
        let _ = sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
    }
}

/// A setup step that may be missing, with the reason printed. A run that skips reports `ok`, so the
/// printed line is the only thing that tells a skip from a pass.
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

/// A migrated database of its own and a `Ctx` over it, built the way `main.rs` composes one.
async fn probe(name: &'static str, tune: impl FnOnce(&mut Config)) -> Option<Probe> {
    let serial = SERIAL.lock().await;
    let url = step!("reading DATABASE_URL", std::env::var("DATABASE_URL"));
    let base = url.rsplit_once('/')?.0.to_string();
    let admin = step!("connecting to the admin database", PgPool::connect(&url).await);
    // Audited: `name` is a compile-time literal at every call site.
    step!(
        "dropping a leftover probe database",
        sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
            .execute(&admin)
            .await
    );
    step!(
        "creating the probe database",
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&admin).await
    );
    let pool = step!(
        "connecting to the probe database",
        postgres::connect(&format!("{base}/{name}")).await
    );
    postgres::migrate(&pool).await.expect("migrations apply to an empty database");

    std::env::set_var("AUTH_TOKENS", format!("mac:{}", "m".repeat(32)));
    std::env::set_var("EMBED_PROVIDER", "hash");
    std::env::set_var(TEST_KEK_VAR, TEST_KEK_HEX);
    let mut cfg = config::load().expect("config loads");
    tune(&mut cfg);

    let keys: Arc<dyn KeyProvider> = Arc::new(EnvKeyProvider::new(TEST_KEK_VAR, TEST_KEK_ID));
    let kek = keys.kek().await.expect("test key");
    let check = postgres::verify_kek(
        &pool,
        &cfg.tenant_id,
        TEST_KEK_ID,
        &lumberroom_server::crypto::kek::fingerprint(&kek),
        keys.provider(),
    )
    .await
    .expect("kek check");
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
        keys: Some(keys),
        kek_verified,
        principal: Principal {
            client: "mac".into(),
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
        },
        invocation: Invocation::Cli,
        session_id: Some("recall-event-session".into()),
    };
    bootstrap::clear_cache();
    eprintln!("probe database {name} migrated");
    Some(Probe { ctx, pool, admin, name, _serial: serial })
}

fn log_on(cfg: &mut Config) {
    cfg.recall_events.enabled = true;
}

async fn remember(ctx: &Ctx, content: &str, namespace: &str, tags: &[&str]) -> uuid::Uuid {
    let tags = Some(tags.iter().map(|t| (*t).to_string()).collect());
    let out = write::run(ctx, content, namespace, tags, None, None, None).await.expect("write");
    out.id.parse().expect("uuid")
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.expect("count")
}

/// Polls until `sql` counts at least `want`, or the deadline passes, and returns the last count.
async fn wait_for(pool: &PgPool, sql: &'static str, want: i64) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let n = count(pool, sql).await;
        if n >= want || Instant::now() > deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `(call_id, tool, client, session_id, project, memory_id, namespace, section, rank)`, in rank
/// order within each call and section.
type EventRow = (
    uuid::Uuid,
    String,
    String,
    Option<String>,
    Option<String>,
    uuid::Uuid,
    String,
    Option<String>,
    i32,
);

async fn events(pool: &PgPool) -> Vec<EventRow> {
    sqlx::query(
        "SELECT call_id, tool, client, session_id, project, memory_id, namespace, section, rank
           FROM recall_event
          ORDER BY emitted_at, call_id, section NULLS FIRST, rank",
    )
    .fetch_all(pool)
    .await
    .expect("events")
    .into_iter()
    .map(|r| {
        (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6), r.get(7), r.get(8))
    })
    .collect()
}

/// A stock config writes nothing to the log. The emission row proves the shared statement ran, so
/// the zero is an answer rather than a race.
///
/// Fails if `RECALL_EVENT_LOG` defaults on, or if the off path still writes events.
#[tokio::test]
async fn the_log_is_off_by_default_and_writes_nothing() {
    let Some(p) = probe("recall_event_off_probe", |_| {}).await else { return };
    assert!(!p.ctx.cfg.recall_events.enabled, "RECALL_EVENT_LOG must default off in the engine");
    assert_eq!(p.ctx.cfg.recall_events.retention_days, 30);

    remember(&p.ctx, "the kestrel host listens on port 5433", "project:alpha", &[]).await;
    let res = search::run(&p.ctx, "kestrel host port", None, None, Some("alpha"), None, None)
        .await
        .expect("search");
    assert!(!res.hits.is_empty());

    let emitted = wait_for(&p.pool, "SELECT count(*) FROM recall_emission", 1).await;
    assert!(emitted >= 1, "the emission statement never committed");
    assert_eq!(count(&p.pool, "SELECT count(*) FROM recall_event").await, 0);
    p.drop_db().await;
}

/// One search, one call id, one row per returned memory, ranked in the order the caller saw.
///
/// Fails if the rank drifts from the hit order, if two searches share a call id, or if a returned
/// row goes unlogged.
#[tokio::test]
async fn a_search_logs_each_returned_row_with_its_rank_under_one_call_id() {
    let Some(p) = probe("recall_event_search_probe", log_on).await else { return };
    for i in 0..4 {
        remember(&p.ctx, &format!("kestrel deploy note number {i}"), "project:alpha", &[]).await;
    }

    let first = search::run(&p.ctx, "kestrel deploy note", None, None, Some("alpha"), None, None)
        .await
        .expect("search");
    let n = first.hits.len() as i64;
    assert!(n >= 2, "the seed should return several rows, got {n}");
    assert_eq!(wait_for(&p.pool, "SELECT count(*) FROM recall_event", n).await, n);

    let rows = events(&p.pool).await;
    let call = rows[0].0;
    for (i, (hit, row)) in first.hits.iter().zip(&rows).enumerate() {
        assert_eq!(row.0, call, "every row of one search shares its call id");
        assert_eq!(row.1, "memory_search");
        assert_eq!(row.2, "mac", "the client is named as tool_calls names it");
        assert_eq!(row.3.as_deref(), Some("recall-event-session"));
        assert_eq!(row.4.as_deref(), Some("project:alpha"), "the active project");
        assert_eq!(row.5.to_string(), hit.id, "rank {} holds the hit the caller saw there", i + 1);
        assert_eq!(row.6, hit.namespace);
        assert_eq!(row.7, None, "a search has no section");
        assert_eq!(row.8, i as i32 + 1, "ranks start at 1 and follow the hit order");
    }

    search::run(&p.ctx, "kestrel deploy note", None, None, None, None, None)
        .await
        .expect("second search");
    let both = wait_for(&p.pool, "SELECT count(*) FROM recall_event", n + 1).await;
    assert!(both > n);
    let calls = count(&p.pool, "SELECT count(DISTINCT call_id) FROM recall_event").await;
    assert_eq!(calls, 2, "a second search gets its own call id");
    let unscoped: Option<String> =
        sqlx::query_scalar("SELECT project FROM recall_event WHERE call_id <> $1 LIMIT 1")
            .bind(call)
            .fetch_one(&p.pool)
            .await
            .expect("second call");
    assert_eq!(unscoped, None, "a search with no project records none");
    p.drop_db().await;
}

/// A digest logs every row it returned, tagged with its section and ranked inside it.
///
/// Fails if a section label is wrong, if a rank restarts in the wrong place, or if a row in the
/// payload goes unlogged.
#[tokio::test]
async fn a_bootstrap_tags_each_row_with_its_section() {
    let Some(p) = probe("recall_event_digest_probe", log_on).await else { return };
    remember(&p.ctx, "Dana prefers TypeScript for tooling", "user:me", &["preference"]).await;
    remember(&p.ctx, "Dana reviews pull requests before lunch", "user:me", &[]).await;
    remember(&p.ctx, "alpha ships from the main branch", "project:alpha", &[]).await;
    remember(&p.ctx, "beta has its own staging database", "project:beta", &[]).await;

    let digest = bootstrap::run(&p.ctx, Some("alpha")).await.expect("bootstrap");
    let mut want: Vec<(String, String, i32)> = Vec::new();
    for (section, facts) in [
        ("profile", &digest.profile),
        ("project", &digest.project_context),
        ("recent", &digest.recent),
    ] {
        for (i, f) in facts.iter().enumerate() {
            want.push((section.to_string(), f.id.clone(), i as i32 + 1));
        }
    }
    assert!(!digest.profile.is_empty() && !digest.project_context.is_empty());
    assert!(!digest.recent.is_empty());

    let n = want.len() as i64;
    assert_eq!(wait_for(&p.pool, "SELECT count(*) FROM recall_event", n).await, n);
    let rows = events(&p.pool).await;
    let mut got: Vec<(String, String, i32)> = rows
        .iter()
        .map(|r| (r.7.clone().expect("a digest row has a section"), r.5.to_string(), r.8))
        .collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    assert!(rows.iter().all(|r| r.1 == "context_bootstrap"));
    assert!(rows.iter().all(|r| r.4.as_deref() == Some("project:alpha")));
    assert_eq!(count(&p.pool, "SELECT count(DISTINCT call_id) FROM recall_event").await, 1);
    p.drop_db().await;
}

/// A digest served from the cache was still handed to a caller, so it is a call with rows.
///
/// Fails if the log skips the cache-hit path, which would undercount exactly the repeat bootstraps
/// an evaluation has to tell apart from new sessions.
#[tokio::test]
async fn a_cached_digest_logs_its_own_call() {
    let Some(p) = probe("recall_event_cache_probe", log_on).await else { return };
    remember(&p.ctx, "Dana prefers short commit subjects", "user:me", &["preference"]).await;

    let first = bootstrap::run(&p.ctx, None).await.expect("bootstrap");
    let n = (first.profile.len() + first.project_context.len() + first.recent.len()) as i64;
    assert!(n >= 1);
    assert_eq!(wait_for(&p.pool, "SELECT count(*) FROM recall_event", n).await, n);

    let second = bootstrap::run(&p.ctx, None).await.expect("bootstrap");
    assert!(second.cached, "the second call should come from the cache");
    assert_eq!(wait_for(&p.pool, "SELECT count(*) FROM recall_event", 2 * n).await, 2 * n);
    assert_eq!(count(&p.pool, "SELECT count(DISTINCT call_id) FROM recall_event").await, 2);
    p.drop_db().await;
}

/// The log stores ids, so a private row is logged even though `recall_emission` skips it.
///
/// Fails if the event path inherits the emission path's encrypted-row filter.
#[tokio::test]
async fn a_private_row_is_logged_by_id_and_never_digested() {
    let Some(p) = probe("recall_event_private_probe", log_on).await else { return };
    let tags = Some(vec![]);
    let out = write::run(
        &p.ctx,
        "the kestrel vault opens with the blue key",
        "project:alpha",
        tags,
        None,
        Some("private"),
        None,
    )
    .await
    .expect("private write");
    let id: uuid::Uuid = out.id.parse().unwrap();

    let res = search::run(&p.ctx, "kestrel vault blue key", None, None, Some("alpha"), None, None)
        .await
        .expect("search");
    assert!(res.hits.iter().any(|h| h.id == out.id), "the owner reads the private row");

    assert_eq!(wait_for(&p.pool, "SELECT count(*) FROM recall_event", 1).await, 1);
    let logged: i64 = sqlx::query_scalar("SELECT count(*) FROM recall_event WHERE memory_id = $1")
        .bind(id)
        .fetch_one(&p.pool)
        .await
        .unwrap();
    assert_eq!(logged, 1);
    assert_eq!(count(&p.pool, "SELECT count(*) FROM recall_emission").await, 0);
    p.drop_db().await;
}

/// The event list can name a row the emission list does not, such as a private row, and a forget
/// can delete it before the spawned insert runs. That event drops; the emissions still land.
///
/// Fails if the events CTE inserts an id with no memory row: the foreign key then fails the whole
/// statement and the live row's emission is lost with it.
#[tokio::test]
async fn an_event_for_a_deleted_row_does_not_cost_the_emissions() {
    let Some(p) = probe("recall_event_gone_probe", log_on).await else { return };
    let live = remember(&p.ctx, "the kestrel host is still here", "project:alpha", &[]).await;
    let gone = uuid::Uuid::new_v4();
    let event = |memory_id, rank| RecallEvent {
        memory_id,
        namespace: "project:alpha".into(),
        section: None,
        rank,
    };
    p.ctx.repos.memories.record_emissions(
        p.ctx.tenant(),
        "memory_search",
        None,
        vec![Emission { content_sha256: "digest-of-the-live-row".into(), memory_id: live }],
        Some(RecallCall {
            call_id: uuid::Uuid::new_v4(),
            client: "mac".into(),
            project: None,
            events: vec![event(live, 1), event(gone, 2)],
        }),
    );

    let emitted = wait_for(&p.pool, "SELECT count(*) FROM recall_emission", 1).await;
    assert_eq!(emitted, 1, "the live row's emission was lost with the missing row's event");
    let logged: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT memory_id FROM recall_event").fetch_all(&p.pool).await.unwrap();
    assert_eq!(logged, vec![live]);
    p.drop_db().await;
}

/// Insert one event at a chosen age, bypassing the tools, so retention can be tested on time.
async fn aged_event(pool: &PgPool, tenant: &str, memory: uuid::Uuid, days_ago: i32) {
    sqlx::query(
        "INSERT INTO recall_event
             (tenant_id, call_id, tool, client, memory_id, namespace, rank, emitted_at)
         VALUES ($1, gen_random_uuid(), 'memory_search', 'mac', $2, 'project:alpha', 1,
                 now() - make_interval(days => $3))",
    )
    .bind(tenant)
    .bind(memory)
    .bind(days_ago)
    .execute(pool)
    .await
    .expect("aged event");
}

/// Rows past the window go, in batches; rows inside it stay.
///
/// Fails if the purge stops after one batch, uses the wrong side of the cutoff, or ignores the
/// window it was given.
#[tokio::test]
async fn retention_deletes_old_events_and_keeps_new_ones() {
    let Some(p) = probe("recall_event_retention_probe", log_on).await else { return };
    let id = remember(&p.ctx, "an aged fact", "project:alpha", &[]).await;
    let tenant = p.ctx.tenant().to_string();
    for _ in 0..5 {
        aged_event(&p.pool, &tenant, id, 31).await;
    }
    aged_event(&p.pool, &tenant, id, 29).await;
    aged_event(&p.pool, &tenant, id, 0).await;

    // A batch of two forces three rounds for five old rows.
    let gone =
        recall_events::purge(p.ctx.repos.memories.as_ref(), &tenant, 30, 2).await.expect("purge");
    assert_eq!(gone, 5);
    let left: Vec<i32> = sqlx::query_scalar(
        "SELECT extract(day FROM now() - emitted_at)::int FROM recall_event ORDER BY 1",
    )
    .fetch_all(&p.pool)
    .await
    .unwrap();
    assert_eq!(left, vec![0, 29]);
    p.drop_db().await;
}

/// A purge is scoped to its tenant, and a search files its rows under the caller's tenant.
///
/// Fails if the purge drops the tenant predicate, or if a search writes a tenant other than its
/// own.
#[tokio::test]
async fn retention_leaves_another_tenant_alone() {
    let Some(p) = probe("recall_event_tenant_probe", log_on).await else { return };
    let mine = remember(&p.ctx, "kestrel tenancy probe", "project:alpha", &[]).await;
    let theirs = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO memory (id, tenant_id, namespace, content, source_client)
         VALUES ($1, 'other', 'project:alpha', 'their fact', 'them')",
    )
    .bind(theirs)
    .execute(&p.pool)
    .await
    .expect("other tenant's row");
    aged_event(&p.pool, "other", theirs, 90).await;
    aged_event(&p.pool, p.ctx.tenant(), mine, 90).await;

    let gone = recall_events::purge(p.ctx.repos.memories.as_ref(), p.ctx.tenant(), 30, 1000)
        .await
        .expect("purge");
    assert_eq!(gone, 1);
    assert_eq!(
        count(&p.pool, "SELECT count(*) FROM recall_event WHERE tenant_id = 'other'").await,
        1
    );

    search::run(&p.ctx, "kestrel tenancy probe", None, None, Some("alpha"), None, None)
        .await
        .expect("search");
    wait_for(&p.pool, "SELECT count(*) FROM recall_event WHERE tenant_id <> 'other'", 1).await;
    let tenants: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT tenant_id FROM recall_event WHERE tenant_id <> 'other'",
    )
    .fetch_all(&p.pool)
    .await
    .unwrap();
    assert_eq!(tenants, vec![p.ctx.tenant().to_string()]);
    p.drop_db().await;
}

/// Forgetting a row takes its events with it.
///
/// Fails if the foreign key loses its cascade: the delete is then refused, or leaves the events.
#[tokio::test]
async fn forgetting_a_memory_removes_its_events() {
    let Some(p) = probe("recall_event_forget_probe", log_on).await else { return };
    let doomed = remember(&p.ctx, "kestrel forget probe one", "project:alpha", &[]).await;
    let kept = remember(&p.ctx, "kestrel forget probe two", "project:alpha", &[]).await;
    search::run(&p.ctx, "kestrel forget probe", None, None, Some("alpha"), None, None)
        .await
        .expect("search");
    assert_eq!(wait_for(&p.pool, "SELECT count(*) FROM recall_event", 2).await, 2);

    forget::by_id(&p.ctx, &doomed.to_string(), Some("test"), false).await.expect("forget");

    let left: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT memory_id FROM recall_event").fetch_all(&p.pool).await.unwrap();
    assert_eq!(left, vec![kept]);
    p.drop_db().await;
}

/// The table's exact shape. A new column has to be added here on purpose, which is the moment
/// somebody asks whether it carries content.
///
/// Fails on any column beyond these, including a content, hash or query column.
#[tokio::test]
async fn the_table_holds_ids_and_times_and_no_content() {
    let Some(p) = probe("recall_event_schema_probe", |_| {}).await else { return };
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'recall_event'
          ORDER BY column_name",
    )
    .fetch_all(&p.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            "call_id",
            "client",
            "emitted_at",
            "id",
            "memory_id",
            "namespace",
            "project",
            "rank",
            "section",
            "session_id",
            "tenant_id",
            "tool",
        ]
    );
    // A default would file one tenant's calls under another's name.
    let tenant_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns
          WHERE table_name = 'recall_event' AND column_name = 'tenant_id'",
    )
    .fetch_one(&p.pool)
    .await
    .unwrap();
    assert_eq!(tenant_default, None);
    p.drop_db().await;
}

/// Caller-observed latency of `memory_search` and `context_bootstrap`, log off against log on, 20
/// runs each. Ignored because it measures rather than asserts:
///
///   ./scripts/cargo.sh test -j 1 --test recall_event -- --ignored --nocapture latency
#[tokio::test]
#[ignore]
async fn latency_with_and_without_the_log() {
    const ROWS: usize = 400;
    const RUNS: usize = 20;
    // Off, on, on, off: the second database a run creates starts warmer than the first, and the
    // mirrored order lets that bias show up on both sides instead of flattering one.
    for on in [false, true, true, false] {
        let name = if on { "recall_event_latency_on" } else { "recall_event_latency_off" };
        let Some(p) = probe(name, |c| c.recall_events.enabled = on).await else { return };
        for i in 0..ROWS {
            let ns = match i % 4 {
                0 => "user:me",
                1 => "global",
                2 => "project:alpha",
                _ => "project:beta",
            };
            let tags: &[&str] = if i % 10 == 0 { &["preference"] } else { &[] };
            remember(
                &p.ctx,
                &format!("seeded fact {i} about kestrel deploy {}", i * 7919),
                ns,
                tags,
            )
            .await;
        }
        // One warm-up of each, so neither side pays for a cold cache or a first prepare.
        search::run(&p.ctx, "kestrel deploy", None, Some(8), Some("alpha"), None, None)
            .await
            .unwrap();
        bootstrap::clear_cache();
        bootstrap::run(&p.ctx, Some("alpha")).await.unwrap();

        let mut s = Vec::with_capacity(RUNS);
        for i in 0..RUNS {
            let q = format!("kestrel deploy {}", i * 13);
            let t = Instant::now();
            search::run(&p.ctx, &q, None, Some(8), Some("alpha"), None, None).await.unwrap();
            s.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let mut b = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            bootstrap::clear_cache();
            let t = Instant::now();
            bootstrap::run(&p.ctx, Some("alpha")).await.unwrap();
            b.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let mut c = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let t = Instant::now();
            let d = bootstrap::run(&p.ctx, Some("alpha")).await.unwrap();
            c.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(d.cached);
        }
        // Let the spawned writes drain before the database goes.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let logged = count(&p.pool, "SELECT count(*) FROM recall_event").await;
        for (label, xs) in
            [("memory_search", &mut s), ("bootstrap build", &mut b), ("bootstrap cached", &mut c)]
        {
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = xs.iter().sum::<f64>() / xs.len() as f64;
            println!(
                "log={} {label}: n={} median={:.3}ms p90={:.3}ms mean={:.3}ms min={:.3}ms max={:.3}ms",
                if on { "on " } else { "off" },
                xs.len(),
                xs[xs.len() / 2],
                xs[(xs.len() * 9) / 10],
                mean,
                xs[0],
                xs[xs.len() - 1],
            );
        }
        println!("log={} recall_event rows written: {logged}", if on { "on " } else { "off" });
        p.drop_db().await;
    }
}
