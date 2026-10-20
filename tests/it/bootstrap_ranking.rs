//! How `context_bootstrap` chooses its rows, pinned against a real Postgres (decisions 0025 and
//! 0034).
//!
//! Project is newest first, profile puts standing rules first and is newest first inside each
//! group, recent keeps to the active project and the caller's standing rules and skips what they
//! printed, a query adds related rows and orders the profile, and a row that restates one already
//! chosen gives up its slot. Rows go in through SQL rather than `write::run` so each
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
use lumberroom_server::domain::errors::{DomainError, Result};
use lumberroom_server::domain::policy::NamespaceGrant;
use lumberroom_server::domain::similarity;
use lumberroom_server::domain::types::{Invocation, Principal};
use lumberroom_server::mcp::views;
use lumberroom_server::ports::Embedder;
use lumberroom_server::services::bootstrap::{self, Digest};
use lumberroom_server::services::{Ctx, Repos};
use sqlx::{AssertSqlSafe, PgPool};

use crate::common;
use crate::common::SERIAL;

const DIMS: usize = 768;
const PROJECT: &str = "alpha";

struct Store {
    pool: PgPool,
    admin: PgPool,
    name: &'static str,
    tenant: String,
    /// `ctx` sets environment variables and loads the config, which other modules do too.
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Store {
    /// A migrated database of its own, dropped first if a crashed run left it behind.
    async fn fresh(name: &'static str) -> Option<Store> {
        let serial = SERIAL.lock().await;
        let (admin, base) = common::connect_admin().await?;
        // DDL takes no bind parameter. Audited: every caller passes a compile-time literal.
        step!(
            "dropping a leftover database",
            sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")))
                .execute(&admin)
                .await
        );
        step!(
            "creating the database",
            sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&admin).await
        );
        let pool =
            step!("connecting to the database", postgres::connect(&format!("{base}/{name}")).await);
        postgres::migrate(&pool).await.expect("migrations apply to an empty database");
        Some(Store { pool, admin, name, tenant: "me".into(), _serial: serial })
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
        // The hash embedder has no family entry, so its route_max_top is a guess and the related
        // section would stay out. A test that wants the guessed floor clears this in `tune`.
        cfg.embed.thresholds = similarity::parse_overrides("route_max_top=0.65").unwrap();
        tune(&mut cfg);
        let memories = Arc::new(postgres::PgMemoryRepository::new(self.pool.clone()));
        let cfg = Arc::new(cfg);
        Ctx {
            cfg: Arc::clone(&cfg),
            repos: Repos {
                aliases: Arc::new(postgres::PgAliasRepository::new(self.pool.clone())),
                memories: memories.clone(),
                registry: Arc::new(postgres::PgRegistryRepository::new(self.pool.clone())),
                tool_calls: Arc::new(postgres::PgToolCallRepository::new(self.pool.clone())),
                sealed: Some(Arc::new(postgres::PgSealedRepository::new(self.pool.clone()))),
                ciphertext: Some(memories),
                oauth: None,
            },
            embedders: common::test_embedders(Arc::new(HashEmbedder::new(DIMS)), &cfg),
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

/// A copy of `v` with 0.1 added at `j`: cosine about 0.995 with `v`.
fn tilted(v: &[f32], j: usize) -> Vec<f32> {
    let mut v = v.to_vec();
    v[j] += 0.1;
    v
}

/// The hash embedder's query vector for `text`. A row stored with it has cosine 1.0 to a query of
/// the same text.
async fn qvec(text: &str) -> Vec<f32> {
    HashEmbedder::new(DIMS).embed_query(text).await.unwrap()
}

fn ids(section: &[bootstrap::Fact]) -> Vec<String> {
    section.iter().map(|f| f.id.clone()).collect()
}

fn every_id(d: &Digest) -> Vec<String> {
    let mut all = ids(&d.profile);
    all.extend(ids(&d.project_context));
    all.extend(ids(&d.recent));
    all.extend(ids(&d.related));
    all
}

/// Fails every query embed and embeds documents as the hash embedder does, under its id.
struct QueryEmbedFails(HashEmbedder);

#[async_trait::async_trait]
impl Embedder for QueryEmbedFails {
    fn id(&self) -> String {
        self.0.id()
    }
    fn dim(&self) -> usize {
        self.0.dim()
    }
    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.0.embed_documents(texts).await
    }
    async fn embed_query(&self, _text: &str) -> Result<Vec<f32>> {
        Err(DomainError::internal("embedder down"))
    }
}

macro_rules! store_or_skip {
    ($name:expr) => {
        match Store::fresh($name).await {
            Some(s) => s,
            None => {
                common::skipping();
                return;
            }
        }
    };
}

/// (a) Without a query and without a rule tag, recency orders profile, and it holds eight rows.
/// An old row with a tag outside `BOOTSTRAP_RULE_TAGS` does not jump the queue.
///
/// Fails if any tag sorts a row first, or if the default profile limit moves off 8.
#[tokio::test]
async fn the_profile_shows_the_eight_newest_rows_without_a_query_or_a_rule_tag() {
    let s = store_or_skip!("bootstrap_ranking_a");
    let old =
        s.put("user:me", "old tagged identity", &["identity"], Some(axis(0)), 24.0 * 30.0).await;
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
    assert_eq!(ctx.cfg.bootstrap.profile_limit, 8, "the default profile limit");

    let d = bootstrap::run(&ctx, None).await.unwrap();
    let want: Vec<String> = newer.iter().rev().take(8).cloned().collect();
    assert_eq!(ids(&d.profile), want, "the eight newest, newest first");
    assert!(!every_id(&d).contains(&old), "the old tagged row stays out of every section");
    s.drop().await;
}

/// The incident behind decision 0034: another surface wrote a burst of personal facts after the
/// owner's standing rules, and the session-start digest, which carries no query, printed the facts.
///
/// Fails if profile goes back to recency alone, if the pool stops reading rules first, or if
/// recent prints an untagged personal row again.
#[tokio::test]
async fn standing_rules_lead_the_profile_over_newer_personal_rows_without_a_query() {
    let s = store_or_skip!("bootstrap_ranking_y");
    let mut rules = Vec::new();
    for (i, tag) in ["preference", "rule", "convention"].into_iter().enumerate() {
        let at = 24.0 * 20.0 + i as f64;
        rules
            .push(s.put("user:me", &format!("standing rule {i}"), &[tag], Some(axis(i)), at).await);
    }
    let mut facts = Vec::new();
    for i in 0..30 {
        let at = 1.0 + i as f64;
        let body = format!("personal fact {i}");
        facts.push(s.put("user:me", &body, &["household"], Some(axis(10 + i)), at).await);
    }
    s.put(&format!("project:{PROJECT}"), "alpha fact", &[], Some(axis(60)), 2.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.profile)[..3], rules[..], "the rules first, newest first");
    assert_eq!(ids(&d.profile)[3..], facts[..5], "then the newest facts fill the limit");
    for id in ids(&d.recent) {
        assert!(!facts.contains(&id), "recent printed an untagged personal row");
    }
    let text = &d.text;
    let first_rule = text.find("standing rule 0").expect("a rule printed");
    assert!(text.find("personal fact").is_none_or(|at| at > first_rule), "{text}");
    s.drop().await;
}

/// Another surface restates a rule without its tag. The pass drops the older tagged twin, and the
/// untagged survivor used to sort behind nine other rules and fall past the profile limit, so the
/// fact vanished from every section.
///
/// Fails if a survivor stops taking the standing mark of the tagged twin it dropped.
#[tokio::test]
async fn an_untagged_restatement_of_a_rule_keeps_the_rule_in_the_profile() {
    let s = store_or_skip!("bootstrap_ranking_rule_twin");
    for i in 0..9 {
        let at = 10.0 + i as f64;
        s.put("user:me", &format!("standing rule {i}"), &["rule"], Some(axis(10 + i)), at).await;
    }
    let tagged =
        s.put("user:me", "stage files by explicit path", &["rule"], Some(axis(0)), 5.0).await;
    let untagged =
        s.put("user:me", "always stage files by explicit path", &[], Some(near(0, 1)), 1.0).await;
    let ctx = s.ctx(|_| {});
    assert_eq!(ctx.cfg.bootstrap.profile_limit, 8, "the default profile limit");

    let d = bootstrap::run(&ctx, None).await.unwrap();
    let profile = ids(&d.profile);
    assert!(!every_id(&d).contains(&tagged), "the older twin drops");
    assert_eq!(profile.first(), Some(&untagged), "the survivor leads as the newest rule");
    assert!(d.text.contains("stage files by explicit path"), "{}", d.text);
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
    for i in 0..10 {
        rest.push(
            s.put(&ns, &format!("project fact {i}"), &[], Some(axis(10 + i)), 3.0 + i as f64).await,
        );
    }
    let ctx = s.ctx(|_| {});
    assert_eq!(ctx.cfg.bootstrap.project_limit, 10, "the default project limit");

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    let mut want = vec![newer.clone()];
    want.extend(rest.iter().take(9).cloned());
    assert_eq!(ids(&d.project_context), want, "the twin's slot goes to the next row");
    assert!(!every_id(&d).contains(&older), "the older twin is in no section, recent included");
    s.drop().await;
}

/// (c) Recent keeps to the active project and the rule-tagged rows in `user:me` and `global`, and
/// prints what project and profile did not. The `project:beta` rows and an untagged user row are
/// the newest in the store and still stay out of it.
///
/// Fails if recent reads every readable namespace again, admits untagged user rows, or stops
/// skipping chosen rows.
#[tokio::test]
async fn recent_keeps_to_the_active_project_and_the_callers_own_namespaces() {
    let s = store_or_skip!("bootstrap_ranking_c");
    let ns = format!("project:{PROJECT}");
    let mut alpha = Vec::new();
    for i in 0..12 {
        let at = 1.0 + i as f64 * 0.5;
        alpha.push(s.put(&ns, &format!("alpha fact {i}"), &[], Some(axis(i)), at).await);
    }
    let mut mine = Vec::new();
    for i in 0..9 {
        let at = 1.25 + i as f64 * 0.5;
        mine.push(s.put("user:me", &format!("rule {i}"), &["rule"], Some(axis(20 + i)), at).await);
    }
    let untagged = s.put("user:me", "an untagged personal row", &[], Some(axis(50)), 0.05).await;
    let mut beta = Vec::new();
    for i in 0..3 {
        let at = 0.1 + i as f64 * 0.1;
        beta.push(
            s.put("project:beta", &format!("beta fact {i}"), &[], Some(axis(40 + i)), at).await,
        );
    }
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), alpha[..10].to_vec());
    assert_eq!(ids(&d.profile), mine[..8].to_vec());
    assert_eq!(
        ids(&d.recent),
        vec![mine[8].clone(), alpha[10].clone(), alpha[11].clone()],
        "the overflow of the scoped namespaces, newest first"
    );
    let all = every_id(&d);
    for id in &beta {
        assert!(!all.contains(id), "a project:beta row reached a section without a query");
    }
    assert!(!ids(&d.recent).contains(&untagged), "recent holds rules, not every user row");
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
    let ctx = s.ctx(|c| c.bootstrap.dedup_cosine = Some(1.0));

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

/// (h) A correction filed outside the active project, `user:me` and `global` no longer reaches
/// the digest without a query, so the stale row it corrects prints. A query that matches the
/// correction brings it in through related, and the newer row wins as decision 0025 rules.
///
/// Fails if the related pool stops joining the near-duplicate pass, or recent widens again.
#[tokio::test]
async fn a_correction_outside_the_scope_wins_only_through_a_matching_query() {
    let s = store_or_skip!("bootstrap_ranking_h");
    let text = "deploy from the release branch";
    let q = qvec(text).await;
    let stale =
        s.put("user:me", "deploy from the main branch", &[], Some(tilted(&q, 3)), 5.0).await;
    let fix = s.put("project:beta", text, &[], Some(q.clone()), 1.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.profile), vec![stale.clone()], "without a query the stale rule prints");
    assert!(!every_id(&d).contains(&fix));

    let d = bootstrap::run_with_query(&ctx, Some(PROJECT), Some(text)).await.unwrap();
    assert!(!every_id(&d).contains(&stale), "the newer correction drops its twin");
    assert_eq!(ids(&d.related), vec![fix]);
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

    let at = s.ctx(|c| c.bootstrap.dedup_cosine = Some(0.96));
    let d = bootstrap::run(&at, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), vec![newer.clone()], "cosine 0.96 at threshold 0.96");

    let above = s.ctx(|c| c.bootstrap.dedup_cosine = Some(0.960_000_1));
    let d = bootstrap::run(&above, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), vec![newer, older], "cosine 0.96 under 0.9600001");
    s.drop().await;
}

/// The matched row sits in a namespace no other section reads.
///
/// Fails if the related arm goes missing, or reads only the primary namespaces.
#[tokio::test]
async fn a_query_brings_in_a_related_row_from_any_readable_namespace() {
    let s = store_or_skip!("bootstrap_ranking_j");
    let text = "how do we rotate the backup keys";
    let hit = s.put("project:beta", text, &[], Some(qvec(text).await), 2.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.unwrap();
    assert_eq!(ids(&d.related), vec![hit]);
    assert_eq!(d.related_unavailable, None);
    s.drop().await;
}

/// Fails if the floor stops filtering the related arm.
#[tokio::test]
async fn a_row_below_the_model_floor_stays_out_of_the_related_section() {
    let s = store_or_skip!("bootstrap_ranking_k");
    s.put("project:beta", "an unrelated note", &[], Some(axis(5)), 2.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, None, Some("how do we rotate the backup keys"))
        .await
        .unwrap();
    assert!(d.related.is_empty(), "{:?}", ids(&d.related));
    s.drop().await;
}

/// Fails if related picks before profile, or repeats a row profile chose.
#[tokio::test]
async fn the_related_section_skips_rows_another_section_chose() {
    let s = store_or_skip!("bootstrap_ranking_l");
    let text = "how do we rotate the backup keys";
    let mine = s.put("user:me", text, &[], Some(qvec(text).await), 1.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.unwrap();
    assert!(ids(&d.profile).contains(&mine));
    assert!(!ids(&d.related).contains(&mine));
    s.drop().await;
}

/// Fails if the related pool stays out of the near-duplicate pass.
#[tokio::test]
async fn an_older_related_twin_of_a_project_row_gives_up_its_slot() {
    let s = store_or_skip!("bootstrap_ranking_m");
    let text = "how do we rotate the backup keys";
    let q = qvec(text).await;
    let alpha = s.put(&format!("project:{PROJECT}"), text, &[], Some(q.clone()), 1.0).await;
    let beta = s.put("project:beta", "rotate the backup keys monthly", &[], Some(q), 2.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, Some(PROJECT), Some(text)).await.unwrap();
    assert!(ids(&d.project_context).contains(&alpha));
    assert!(!every_id(&d).contains(&beta));
    s.drop().await;
}

/// A unit vector at cosine `c` to `q`, leaning towards axis `k`.
fn at_cosine(q: &[f32], k: usize, c: f32) -> Vec<f32> {
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let q: Vec<f32> = q.iter().map(|x| x / norm(q)).collect();
    let mut e: Vec<f32> = q.iter().map(|x| -q[k] * x).collect();
    e[k] += 1.0;
    let n = norm(&e);
    q.iter().zip(&e).map(|(a, b)| c * a + (1.0 - c * c).sqrt() * b / n).collect()
}

/// Two restatements of each other sit below a nearer row, and no other section holds either. The
/// newer drops the older, which would never have printed, so the nearer row keeps the lead.
///
/// Fails if any related row that dropped a twin jumps the order, whatever section the twin was in.
#[tokio::test]
async fn a_related_twin_that_replaced_nothing_printed_keeps_its_nearness_order() {
    let s = store_or_skip!("bootstrap_ranking_z");
    let text = "how do we rotate the backup keys";
    let q = qvec(text).await;
    let nearest = s.put("project:beta", text, &[], Some(q.clone()), 3.0).await;
    // Both lean the same way from the query, so they sit at cosine 0.9999 to each other and 0.8
    // or less to the nearest row, which keeps that row out of their pair.
    let newer = s
        .put("project:gamma", "rotate keys monthly", &[], Some(at_cosine(&q, 500, 0.80)), 1.0)
        .await;
    let older = s
        .put("project:gamma", "rotate keys each month", &[], Some(at_cosine(&q, 500, 0.79)), 2.0)
        .await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.unwrap();
    assert_eq!(ids(&d.related), vec![nearest, newer]);
    assert!(!every_id(&d).contains(&older));
    s.drop().await;
}

/// A newer correction in another project replaces the project row and leads related, so the fact
/// stays on the page.
///
/// Fails if a displacing related row loses its place at the front of the related pool.
#[tokio::test]
async fn a_newer_related_twin_replaces_the_project_row_and_leads_the_related_section() {
    let s = store_or_skip!("bootstrap_ranking_n");
    let text = "how do we rotate the backup keys";
    let q = qvec(text).await;
    let alpha = s.put(&format!("project:{PROJECT}"), text, &[], Some(q.clone()), 2.0).await;
    let beta = s.put("project:beta", "rotate the backup keys monthly", &[], Some(q), 1.0).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, Some(PROJECT), Some(text)).await.unwrap();
    assert!(!every_id(&d).contains(&alpha));
    assert_eq!(d.related.first().map(|f| f.id.clone()), Some(beta));
    s.drop().await;
}

/// Thirty newer rows, then one older row that matches the query.
async fn thirty_rows_and_an_older_match(s: &Store, text: &str) -> String {
    for i in 0..30 {
        s.put("user:me", &format!("rule {i}"), &[], Some(axis(100 + i)), 1.0 + i as f64).await;
    }
    s.put("user:me", text, &[], Some(qvec(text).await), 31.0).await
}

/// Fails if the profile pool stays at three times the limit under a query, or the service stops
/// ordering it by the scores.
#[tokio::test]
async fn a_query_reorders_the_profile_pool_by_similarity() {
    let s = store_or_skip!("bootstrap_ranking_o");
    let text = "how do we rotate the backup keys";
    let matched = thirty_rows_and_an_older_match(&s, text).await;
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.unwrap();
    assert_eq!(d.profile.first().map(|f| f.id.clone()), Some(matched));
    s.drop().await;
}

/// Fails if recent brings back profile-pool rows the query rank left out.
#[tokio::test]
async fn with_a_query_recent_skips_profile_rows_the_query_ranked_out() {
    let s = store_or_skip!("bootstrap_ranking_p");
    let mut mine = Vec::new();
    for i in 0..20 {
        let at = 1.0 + i as f64;
        mine.push(s.put("user:me", &format!("rule {i}"), &["rule"], Some(axis(i)), at).await);
    }
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, None, Some("zebra quokka marmalade")).await.unwrap();
    for id in ids(&d.recent) {
        assert!(!mine.contains(&id), "recent printed a profile-pool row {id}");
    }
    s.drop().await;
}

/// Sixty newer rows in another namespace used to fill the recent pool on their own.
///
/// Fails if recent reads every readable namespace again.
#[tokio::test]
async fn a_flood_in_another_namespace_leaves_recent_to_the_project() {
    let s = store_or_skip!("bootstrap_ranking_q");
    let mut flood = Vec::new();
    for i in 0..60 {
        let at = 0.01 + i as f64 * 0.01;
        flood.push(s.put("project:beta", &format!("note {i}"), &[], Some(axis(i)), at).await);
    }
    let ns = format!("project:{PROJECT}");
    let mut alpha = Vec::new();
    for i in 0..13 {
        let at = 2.0 + i as f64 * 0.05;
        alpha.push(s.put(&ns, &format!("alpha fact {i}"), &[], Some(axis(100 + i)), at).await);
    }
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run(&ctx, Some(PROJECT)).await.unwrap();
    assert_eq!(ids(&d.project_context), alpha[..10].to_vec());
    assert_eq!(ids(&d.recent), alpha[10..].to_vec());
    let all = every_id(&d);
    assert!(flood.iter().all(|id| !all.contains(id)));
    s.drop().await;
}

/// Decision 0034 narrows 0028 for this one optional section.
///
/// Fails if a failed query embed fails the digest, or leaves the profile in query order.
#[tokio::test]
async fn a_failed_query_embed_leaves_the_related_section_out_and_serves_the_rest() {
    let s = store_or_skip!("bootstrap_ranking_r");
    let text = "how do we rotate the backup keys";
    let matched = s.put("user:me", text, &[], Some(qvec(text).await), 5.0).await;
    let newer = s.put("user:me", "tabs over spaces", &[], Some(axis(1)), 1.0).await;
    s.put("project:beta", text, &[], Some(qvec(text).await), 2.0).await;
    let mut ctx = s.ctx(|_| {});
    ctx.embedders =
        common::test_embedders(Arc::new(QueryEmbedFails(HashEmbedder::new(DIMS))), &ctx.cfg);

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.expect("the digest serves");
    assert!(d.related.is_empty());
    assert_eq!(d.related_unavailable.as_deref(), Some("embed_failed"));
    assert_eq!(ids(&d.profile), vec![newer, matched], "recency order");
    s.drop().await;
}

/// Fails if a guessed floor turns the section on, or turns the profile order off with it.
#[tokio::test]
async fn a_model_without_a_measured_floor_leaves_the_related_section_out() {
    let s = store_or_skip!("bootstrap_ranking_s");
    let text = "how do we rotate the backup keys";
    s.put("project:beta", text, &[], Some(qvec(text).await), 2.0).await;
    let matched = thirty_rows_and_an_older_match(&s, text).await;
    let ctx = s.ctx(|c| c.embed.thresholds.clear());

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.unwrap();
    assert!(d.related.is_empty());
    assert_eq!(d.related_unavailable.as_deref(), Some("no_measured_floor"));
    assert_eq!(d.profile.first().map(|f| f.id.clone()), Some(matched));
    s.drop().await;
}

/// A narrow client's query matches a row in a namespace it holds no grant on and a row above its
/// ceiling, both nearer than the readable twin.
///
/// Fails if the related arm loses its `reachable` join or its ceiling.
#[tokio::test]
async fn the_related_section_honours_the_grant_and_the_ceiling() {
    let s = store_or_skip!("bootstrap_ranking_t");
    let text = "how do we rotate the backup keys";
    let q = qvec(text).await;
    let nogrant = s.put("project:secret", "SENTINEL-NOGRANT", &[], Some(q.clone()), 1.0).await;
    let private = s.put("user:me", "SENTINEL-PRIVATE", &[], Some(q.clone()), 1.0).await;
    sqlx::query("UPDATE memory SET sensitivity = 'private' WHERE id = $1::uuid")
        .bind(&private)
        .execute(&s.pool)
        .await
        .unwrap();
    let twin = s.put("project:beta", "readable twin", &[], Some(tilted(&q, 3)), 3.0).await;
    let mut ctx = s.ctx(|_| {});
    ctx.principal = reader(
        "narrow",
        vec![
            NamespaceGrant::open("user:me"),
            NamespaceGrant::open(format!("project:{PROJECT}")),
            NamespaceGrant::open("project:beta"),
        ],
    );

    let d = bootstrap::run_with_query(&ctx, Some(PROJECT), Some(text)).await.unwrap();
    let all = every_id(&d);
    assert!(!all.contains(&nogrant) && !all.contains(&private), "{all:?}");
    let full = serde_json::to_string(&views::digest(&d, true)).unwrap();
    for sentinel in ["SENTINEL-NOGRANT", "SENTINEL-PRIVATE"] {
        assert!(!d.text.contains(sentinel), "{sentinel} in the text");
        assert!(!full.contains(sentinel), "{sentinel} in the rows: true JSON");
    }
    assert_eq!(ids(&d.related), vec![twin]);
    s.drop().await;
}

/// The vector copy opens for a query even with the dedup pass off.
///
/// Fails if `pooled_vectors` stays gated on the dedup threshold alone.
#[tokio::test]
async fn a_dedup_cosine_of_one_still_orders_the_profile_by_the_query() {
    let s = store_or_skip!("bootstrap_ranking_u");
    let text = "how do we rotate the backup keys";
    let matched = thirty_rows_and_an_older_match(&s, text).await;
    let ctx = s.ctx(|c| c.bootstrap.dedup_cosine = Some(1.0));

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.unwrap();
    assert_eq!(d.profile.first().map(|f| f.id.clone()), Some(matched));
    s.drop().await;
}

/// Fails if recent's pool is sized without room for every profile-pool row it drops.
#[tokio::test]
async fn recent_still_fills_when_the_newest_rows_are_all_user_rows_under_a_query() {
    let s = store_or_skip!("bootstrap_ranking_v");
    for i in 0..30 {
        let at = 0.01 + i as f64 * 0.01;
        s.put("user:me", &format!("rule {i}"), &["rule"], Some(axis(i)), at).await;
    }
    let ns = format!("project:{PROJECT}");
    let mut alpha = Vec::new();
    for i in 0..13 {
        let at = 2.0 + i as f64 * 0.05;
        alpha.push(s.put(&ns, &format!("alpha fact {i}"), &[], Some(axis(100 + i)), at).await);
    }
    let ctx = s.ctx(|_| {});

    let d = bootstrap::run_with_query(&ctx, Some(PROJECT), Some("zebra quokka marmalade"))
        .await
        .unwrap();
    assert_eq!(ids(&d.project_context), alpha[..10].to_vec());
    assert_eq!(ids(&d.recent), alpha[10..].to_vec());
    s.drop().await;
}

/// Checks the output only: whether the related arm still scans at a limit of 0 is not visible
/// here.
///
/// Fails if a related limit of 0 prints a related row, switches off the profile order with it, or
/// still reports a reason for a section the operator turned off.
#[tokio::test]
async fn with_related_limit_zero_a_query_adds_no_related_rows() {
    let s = store_or_skip!("bootstrap_ranking_w");
    let text = "how do we rotate the backup keys";
    s.put("project:beta", text, &[], Some(qvec(text).await), 2.0).await;
    let matched = thirty_rows_and_an_older_match(&s, text).await;
    let ctx = s.ctx(|c| c.bootstrap.related_limit = 0);

    let d = bootstrap::run_with_query(&ctx, None, Some(text)).await.unwrap();
    assert!(d.related.is_empty());
    assert_eq!(d.related_unavailable, None);
    assert_eq!(d.profile.first().map(|f| f.id.clone()), Some(matched));

    let guessed = s.ctx(|c| {
        c.bootstrap.related_limit = 0;
        c.embed.thresholds.clear();
    });
    let d = bootstrap::run_with_query(&guessed, None, Some(text)).await.unwrap();
    assert_eq!(d.related_unavailable, None, "a guessed floor with the section off");

    let mut failing = s.ctx(|c| c.bootstrap.related_limit = 0);
    failing.embedders =
        common::test_embedders(Arc::new(QueryEmbedFails(HashEmbedder::new(DIMS))), &failing.cfg);
    let d = bootstrap::run_with_query(&failing, None, Some(text)).await.unwrap();
    assert_eq!(d.related_unavailable, None, "a failed embed with the section off");
    s.drop().await;
}

/// Fails if the cache key leaves the query out.
#[tokio::test]
async fn two_queries_do_not_share_a_cache_entry() {
    let s = store_or_skip!("bootstrap_ranking_x");
    s.put("user:me", "a rule", &[], Some(axis(1)), 1.0).await;
    let mut ctx = s.ctx(|c| c.bootstrap.cache_ms = 30_000);
    ctx.principal = reader("cache-queries-test", NamespaceGrant::everything());
    bootstrap::clear_cache();

    let a = bootstrap::run_with_query(&ctx, None, Some("first request")).await.unwrap();
    let b = bootstrap::run_with_query(&ctx, None, Some("second request")).await.unwrap();
    let again = bootstrap::run_with_query(&ctx, None, Some("second request")).await.unwrap();
    assert!(!a.cached && !b.cached, "a new query builds its own digest");
    assert!(again.cached, "the same query reads the cache");
    bootstrap::clear_cache();
    s.drop().await;
}
