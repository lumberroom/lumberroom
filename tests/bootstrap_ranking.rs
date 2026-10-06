//! How `context_bootstrap` chooses its rows, pinned against a real Postgres (decision 0025).
//!
//! Profile and project are newest first, recent skips what they printed, and a row that restates
//! one already chosen gives up its slot. Rows go in through SQL rather than `write::run` so each
//! test controls `created_at` and the vector exactly; the hash embedder would make a near-duplicate
//! a matter of luck.
//!
//! Each test builds its own database, so the suite's shared `lumberroom_rust_test` and its
//! truncates stay out of this. Skipped when `DATABASE_URL` is unset or unreachable. A count of zero
//! here is not a green run.

use std::sync::Arc;

use lumberroom_server::adapters::embedding::HashEmbedder;
use lumberroom_server::adapters::postgres;
use lumberroom_server::config::{self, Config};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::services::bootstrap::{self, Digest};
use lumberroom_server::services::{Ctx, Repos};
use sqlx::{AssertSqlSafe, PgPool};

const DIMS: usize = 768;
const PROJECT: &str = "alpha";

struct Store {
    pool: PgPool,
    admin: PgPool,
    name: &'static str,
    tenant: String,
}

impl Store {
    /// A migrated database of its own, dropped first if a crashed run left it behind.
    async fn fresh(name: &'static str) -> Option<Store> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let base = url.rsplit_once('/')?.0.to_string();
        let admin = PgPool::connect(&url).await.ok()?;
        // DDL takes no bind parameter. Audited: every caller passes a compile-time literal.
        sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
            .execute(&admin)
            .await
            .ok()?;
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin)
            .await
            .ok()?;
        let pool = postgres::connect(&format!("{base}/{name}")).await.ok()?;
        postgres::migrate(&pool).await.expect("migrations apply to an empty database");
        Some(Store { pool, admin, name, tenant: "me".into() })
    }

    async fn drop(self) {
        self.pool.close().await;
        let _ = sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
    }

    /// One open row, `hours_ago` old, with the vector given or none.
    async fn put(
        &self,
        namespace: &str,
        content: &str,
        tags: &[&str],
        embedding: Option<Vec<f32>>,
        hours_ago: f64,
    ) -> String {
        let id = uuid::Uuid::new_v4();
        let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        sqlx::query(
            "INSERT INTO memory (id, tenant_id, namespace, content, embedding, tags, source_client,
                                 created_at)
             VALUES ($1, $2, $3, $4, $5, $6, 'ranking-test',
                     now() - make_interval(secs => $7))",
        )
        .bind(id)
        .bind(&self.tenant)
        .bind(namespace)
        .bind(content)
        .bind(embedding.map(pgvector::Vector::from))
        .bind(&tags)
        .bind(hours_ago * 3600.0)
        .execute(&self.pool)
        .await
        .expect("insert");
        id.to_string()
    }

    /// A Ctx over this database, shaped by `tune` before anything reads the config.
    fn ctx(&self, tune: impl FnOnce(&mut Config)) -> Ctx {
        std::env::set_var("AUTH_TOKENS", format!("mac:{}", "m".repeat(32)));
        std::env::set_var("EMBED_PROVIDER", "hash");
        let mut cfg = config::load().expect("config loads");
        cfg.tenant_id = self.tenant.clone();
        // The digest cache is process-wide and keyed without a database, so a cached digest from
        // one test would answer another that shares a client and grant.
        cfg.bootstrap.cache_ms = 0;
        tune(&mut cfg);
        let memories = Arc::new(postgres::PgMemoryRepository::new(self.pool.clone()));
        Ctx {
            cfg: Arc::new(cfg),
            repos: Repos {
                aliases: Arc::new(postgres::PgAliasRepository::new(self.pool.clone())),
                memories: memories.clone(),
                registry: Arc::new(postgres::PgRegistryRepository::new(self.pool.clone())),
                tool_calls: Arc::new(postgres::PgToolCallRepository::new(self.pool.clone())),
                sealed: Some(Arc::new(postgres::PgSealedRepository::new(self.pool.clone()))),
                ciphertext: Some(memories),
                oauth: None,
            },
            embedder: Arc::new(HashEmbedder::new(DIMS)),
            keys: None,
            kek_verified: false,
            principal: reader("mac", NamespaceGrant::everything()),
            invocation: Invocation::Cli,
            session_id: None,
        }
    }
}

fn reader(client: &str, read: Vec<NamespaceGrant>) -> Principal {
    Principal {
        client: client.into(),
        token_id: "test".into(),
        mode: "token",
        scopes: vec![],
        read,
        write: vec![],
        registry_write: false,
        sealed_capable: false,
        may_delete: false,
        may_ingest: false,
        may_read_history: false,
    }
}

/// The `i`th unit axis. Two different axes have cosine 0, so only rows built to be twins collapse.
fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DIMS];
    v[i] = 1.0;
    v
}

/// Axis `i` tilted slightly towards axis `j`: cosine about 0.995 with `axis(i)`.
fn near(i: usize, j: usize) -> Vec<f32> {
    let mut v = axis(i);
    v[j] = 0.1;
    v
}

fn ids(section: &[bootstrap::Fact]) -> Vec<String> {
    section.iter().map(|f| f.id.clone()).collect()
}

fn every_id(d: &Digest) -> Vec<String> {
    let mut all = ids(&d.profile);
    all.extend(ids(&d.project_context));
    all.extend(ids(&d.recent));
    all
}

macro_rules! store_or_skip {
    ($name:expr) => {
        match Store::fresh($name).await {
            Some(s) => s,
            None => {
                eprintln!("skipping: no database reachable");
                return;
            }
        }
    };
}

/// (a) The owner's ruling: recency alone orders profile, and it holds ten rows. An old row tagged
/// `preference` used to sort above every newer one.
///
/// Fails if the tag-first ORDER BY returns, or if the default profile limit moves off 10.
#[tokio::test]
async fn the_profile_shows_the_ten_newest_rows_and_no_tag_jumps_the_queue() {
    let s = store_or_skip!("bootstrap_ranking_a");
    let old = s
        .put("user:me", "old tagged preference", &["preference"], Some(axis(0)), 24.0 * 30.0)
        .await;
    let mut newer = Vec::new();
    for i in 0..50 {
        // Written after the old one, the last of them newest.
        let id = s
            .put(
                "user:me",
                &format!("preference number {i}"),
                &[],
                Some(axis(i + 1)),
                50.0 - i as f64,
            )
            .await;
        newer.push(id);
    }
    let ctx = s.ctx(|_| {});
    assert_eq!(ctx.cfg.bootstrap.profile_limit, 10, "the default profile limit");

    let d = bootstrap::run(&ctx, None).await.unwrap();
    let want: Vec<String> = newer.iter().rev().take(10).cloned().collect();
    assert_eq!(ids(&d.profile), want, "the ten newest, newest first");
    assert!(!every_id(&d).contains(&old), "the old tagged row stays out of every section");
    s.drop().await;
}

/// (b) Two rows that say the same thing take one slot, the newer one keeps it, and the next row
/// fills the slot the older twin would have taken.
///
/// Fails if the service stops calling the dedup pass, or calls it oldest first.
#[tokio::test]
async fn two_near_duplicates_take_one_slot_and_the_newer_one_keeps_it() {
    let s = store_or_skip!("bootstrap_ranking_b");
    let ns = format!("project:{PROJECT}");
    let newer = s.put(&ns, "deploys go through make cloud-up", &[], Some(axis(0)), 1.0).await;
    let older = s.put(&ns, "deploy with make cloud-up", &[], Some(near(0, 1)), 2.0).await;
    let mut rest = Vec::new();
    for i in 0..6 {
        rest.push(
            s.put(&ns, &format!("project fact {i}"), &[], Some(axis(10 + i)), 3.0 + i as f64).await,
        );
    }
    let ctx = s.ctx(|_| {});
    assert_eq!(ctx.cfg.bootstrap.project_limit, 5, "the default project limit");

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    let mut want = vec![newer.clone()];
    want.extend(rest.iter().take(4).cloned());
    assert_eq!(ids(&d.project_context), want, "the twin's slot goes to the next row");
    assert!(!every_id(&d).contains(&older), "the older twin is in no section, recent included");
    s.drop().await;
}

/// (c) Recent spans every namespace, so it overlaps the two sections above it. It prints what they
/// did not.
///
/// Fails if recent stops skipping ids profile or project already chose.
#[tokio::test]
async fn recent_never_repeats_a_row_from_profile_or_project() {
    let s = store_or_skip!("bootstrap_ranking_c");
    let ns = format!("project:{PROJECT}");
    let mut axis_no = 0;
    let mut next = || {
        axis_no += 1;
        axis(axis_no)
    };
    for i in 0..4 {
        s.put("user:me", &format!("rule {i}"), &[], Some(next()), 1.0 + i as f64).await;
        s.put(&ns, &format!("alpha fact {i}"), &[], Some(next()), 1.5 + i as f64).await;
    }
    let mut elsewhere = Vec::new();
    for i in 0..3 {
        elsewhere.push(
            s.put("project:beta", &format!("beta fact {i}"), &[], Some(next()), 10.0 + i as f64)
                .await,
        );
    }
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    let above: Vec<String> = ids(&d.profile).into_iter().chain(ids(&d.project_context)).collect();
    assert_eq!(above.len(), 8, "every profile and project row fits");
    let recent = ids(&d.recent);
    for id in &recent {
        assert!(!above.contains(id), "recent repeated {id}");
    }
    assert_eq!(recent, elsewhere, "recent holds the rows nothing above printed, newest first");
    s.drop().await;
}

/// (d) A client without a grant on a namespace sees none of its rows in any section. One hidden
/// row has a vector nothing visible shares, so a leak through any section prints it. The other is
/// the newest twin of a visible row, so a pair list built past the grant would drop the visible
/// row instead.
///
/// Fails if any pool arm loses its `reachable` join or its ceiling.
#[tokio::test]
async fn a_client_never_sees_a_namespace_it_holds_no_grant_on() {
    let s = store_or_skip!("bootstrap_ranking_d");
    let ns = format!("project:{PROJECT}");
    let visible = s.put(&ns, "alpha listens on 8080", &[], Some(axis(0)), 2.0).await;
    let secret = s
        .put("project:secret", "alpha listens on 8080, says secret", &[], Some(axis(0)), 1.0)
        .await;
    let lone_secret =
        s.put("project:secret", "a secret with no visible twin", &[], Some(axis(9)), 0.1).await;
    let global = s.put("global", "global rule", &[], Some(axis(1)), 1.0).await;
    let mine = s.put("user:me", "my rule", &[], Some(axis(2)), 1.0).await;
    let private = s.put("user:me", "a private rule", &[], Some(axis(3)), 0.5).await;
    sqlx::query("UPDATE memory SET sensitivity = 'private' WHERE id = $1::uuid")
        .bind(&private)
        .execute(&s.pool)
        .await
        .unwrap();

    let mut ctx = s.ctx(|_| {});
    ctx.principal =
        reader("narrow", vec![NamespaceGrant::open("user:me"), NamespaceGrant::open(ns.clone())]);

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    let all = every_id(&d);
    assert!(!all.contains(&secret), "a namespace with no grant");
    assert!(!all.contains(&lone_secret), "a namespace with no grant, in any section");
    assert!(!all.contains(&global), "global is a namespace like any other and needs a grant");
    assert!(!all.contains(&private), "a row above the ceiling");
    assert!(all.contains(&visible), "the hidden twin reached the dedup pass");
    assert!(all.contains(&mine));
    s.drop().await;
}

/// (e) A row with no vector cannot be compared, so recency alone decides it. Neither can a zero
/// vector: pgvector gives NaN for its cosine distance, and NaN fails the comparison.
///
/// Fails if a NULL embedding breaks the pool's parse, which empties the section, or if the pass
/// treats a missing or zero vector as a match.
#[tokio::test]
async fn rows_without_a_usable_vector_are_kept() {
    let s = store_or_skip!("bootstrap_ranking_e");
    let a = s.put("user:me", "same words", &[], None, 1.0).await;
    let b = s.put("user:me", "same words", &[], None, 2.0).await;
    let z1 = s.put("user:me", "zero one", &[], Some(vec![0.0; DIMS]), 2.5).await;
    let z2 = s.put("user:me", "zero two", &[], Some(vec![0.0; DIMS]), 2.6).await;
    let c = s.put("user:me", "vector row", &[], Some(axis(0)), 3.0).await;
    let twin = s.put("user:me", "vector row again", &[], Some(axis(0)), 4.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, None).await.unwrap();
    assert_eq!(
        ids(&d.profile),
        vec![a, b, z1, z2, c],
        "no usable vector, no dedup; the vector twin still goes"
    );
    assert!(!every_id(&d).contains(&twin));
    s.drop().await;
}

/// (f) A threshold of 1.0 is the off switch the setting documents.
///
/// Fails if 1.0 is compared as a cosine, where two identical vectors still collapse.
#[tokio::test]
async fn a_dedup_cosine_of_one_turns_the_pass_off() {
    let s = store_or_skip!("bootstrap_ranking_f");
    let ns = format!("project:{PROJECT}");
    let newer = s.put(&ns, "deploys go through make cloud-up", &[], Some(axis(0)), 1.0).await;
    let older = s.put(&ns, "deploy with make cloud-up", &[], Some(axis(0)), 2.0).await;
    let ctx = s.ctx(|c| c.bootstrap.dedup_cosine = 1.0);

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), vec![newer, older]);
    s.drop().await;
}

/// (g) The owner's ruling keeps the newer row, whichever section it lands in. A project row that
/// restates an older profile row wins, and profile fills the slot from its own pool.
///
/// Fails if sections dedup in priority order instead of by age.
#[tokio::test]
async fn a_newer_project_row_beats_its_older_twin_in_profile() {
    let s = store_or_skip!("bootstrap_ranking_g");
    let ns = format!("project:{PROJECT}");
    let stale = s.put("user:me", "the db listens on 5432", &[], Some(axis(0)), 5.0).await;
    let other = s.put("user:me", "tabs over spaces", &[], Some(axis(1)), 6.0).await;
    let fresh = s.put(&ns, "the db listens on 5433", &[], Some(near(0, 2)), 1.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), vec![fresh]);
    assert_eq!(ids(&d.profile), vec![other], "the stale twin is gone and nothing else moved in");
    assert!(!every_id(&d).contains(&stale));
    s.drop().await;
}

/// (h) A correction filed in a namespace no section above recent reads still hides the stale row
/// it corrects. Recent prints the correction.
///
/// Fails if a row profile chose can survive a newer twin from the recent pool.
#[tokio::test]
async fn a_newer_correction_in_another_namespace_hides_the_older_twin() {
    let s = store_or_skip!("bootstrap_ranking_h");
    let stale = s.put("user:me", "deploy from the main branch", &[], Some(axis(0)), 5.0).await;
    let fix =
        s.put("project:beta", "deploy from the release branch", &[], Some(near(0, 3)), 1.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    assert!(d.profile.is_empty(), "the stale rule left profile: {:?}", ids(&d.profile));
    assert_eq!(ids(&d.recent), vec![fix]);
    assert!(!every_id(&d).contains(&stale));
    s.drop().await;
}

/// (i) The threshold edge sits in SQL, compared as a cosine distance against `1 - threshold` in
/// float8. Axes (3, 4) and (4, 3) have a cosine of exactly 24/25.
///
/// Fails if the comparison becomes strict.
#[tokio::test]
async fn a_pair_at_the_threshold_collapses_and_one_just_below_does_not() {
    let s = store_or_skip!("bootstrap_ranking_i");
    let ns = format!("project:{PROJECT}");
    let mut a = vec![0.0f32; DIMS];
    a[0] = 3.0;
    a[1] = 4.0;
    let mut b = vec![0.0f32; DIMS];
    b[0] = 4.0;
    b[1] = 3.0;
    let newer = s.put(&ns, "first wording", &[], Some(a), 1.0).await;
    let older = s.put(&ns, "second wording", &[], Some(b), 2.0).await;

    let at = s.ctx(|c| c.bootstrap.dedup_cosine = 0.96);
    let d = bootstrap::run(&at, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), vec![newer.clone()], "cosine 0.96 at threshold 0.96");

    let above = s.ctx(|c| c.bootstrap.dedup_cosine = 0.960_000_1);
    let d = bootstrap::run(&above, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), vec![newer, older], "cosine 0.96 under 0.9600001");
    s.drop().await;
}
