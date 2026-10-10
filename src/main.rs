//! Entry point and composition root. The only module that knows every concrete type.
//!
//! Boot order matters: config, then database, then migrations, then the schema guard, then the KEK
//! fingerprint check, then the embedder warms, and only then does the listener open. Nothing accepts
//! traffic until the model has produced a real vector, so the first tool call a model ever makes is
//! a fast one.
//!
//! The binary also carries three key-material subcommands. They live here rather than in the node
//! CLI because argon2 and CSPRNG bytes are not things a shell script should improvise, and the
//! operator needs both before the server will start in oauth mode.

// The modules live in lib.rs so the integration suite can reach them; main is the entry point.
use lumberroom_server::{adapters, command, config, crypto, domain, http, mcp, ports, services};

use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;

use adapters::postgres::{self as pg, KekCheck};
use config::{EmbedProvider, EmbedderSpec, KekProvider};
use crypto::kek::{EnvKeyProvider, FileKeyProvider, KeyProvider};
use domain::embedding_migration::{Configured, ControlMode, FlipScope, Intent, UnitState};
use domain::errors::{DomainError, Result};
use domain::similarity::{self, SimilarityThresholds};
use mcp::AppState;
use ports::{Embedder, EmbeddingMigrationRepository, FreeSpace};
use services::embedders::EmbedderSet;
use services::embedding_migration::{Knobs, SingleUnit, Steer, Sweep};

const USAGE: &str = "\
lumberroom-server: durable memory over MCP

  lumberroom-server                  run the server
  lumberroom-server hash-password    read a password on stdin, print an argon2id hash for OWNER_PASSWORD_HASH
  lumberroom-server generate-kek     print a fresh key-encryption key as hex, for KEK_PATH
  lumberroom-server verify-kek       report whether the configured KEK is the one this store was sealed with
  lumberroom-server verify-embedding  check the embedding configuration against the store, load nothing
  lumberroom-server embeddings <verb> switch embedding models: status | start | flip | rollback | retire
";

#[tokio::main]
async fn main() {
    // Parsed by hand, in the existing style. A CLI crate would be a dependency and a derive macro
    // for four words.
    let arg = std::env::args().nth(1);
    // Every one of these prints the full cause on failure: there is no client to protect at this
    // point, and the operator reading it is the person who can fix it.
    let (result, prefix) = match arg.as_deref() {
        None => (run().await, "lumberroom-server failed to start"),
        Some("hash-password") => (hash_password(), "lumberroom-server hash-password"),
        Some("generate-kek") => (generate_kek(), "lumberroom-server generate-kek"),
        Some("verify-kek") => (verify_kek_command().await, "lumberroom-server verify-kek"),
        Some("verify-embedding") => {
            (verify_embedding_command().await, "lumberroom-server verify-embedding")
        }
        // The command owns its exit codes (0 applied or no-op, 1 refused, 2 usage or wrong mode,
        // 3 written but not applied), so only a failure to reach the store comes back here.
        Some("embeddings") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            match command::embeddings::run(args, similarity::Registry::engine()).await {
                Ok(code) => std::process::exit(code),
                Err(e) => (Err(e), "lumberroom-server embeddings"),
            }
        }
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return;
        }
        Some(other) => {
            eprint!("lumberroom-server: unknown subcommand {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("{prefix}: {}", e.log_message());
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    // Plain `Config` until the classification table is settled below. Sharing it before then would
    // mean handing out a policy that is still one database read away from being the real one.
    let mut cfg = config::load()?;
    init_tracing();

    tracing::info!(
        name = mcp::SERVER_NAME,
        version = mcp::SERVER_VERSION,
        auth_mode = cfg.mode_str(),
        dcr_enabled = cfg.auth.mode == config::AuthMode::Oauth && cfg.oauth.dcr_enabled,
        embed_provider = ?cfg.embed.provider,
        kek_provider = cfg.crypto.provider.as_str(),
        tenant = %cfg.tenant_id,
        clients = ?cfg.auth.grants.iter().map(|g| &g.client).collect::<Vec<_>>(),
        "starting"
    );

    let pool = pg::connect_with(&cfg.database_url, &cfg.db).await?;

    if cfg.run_migrations_on_boot {
        pg::migrate(&pool).await?;
        tracing::info!("migrations up to date");
    }
    let dim = pg::assert_embedding_dim(&pool, cfg.embed.dim).await?;
    pg::assert_embedding_b_dim(&pool, cfg.embed.dim).await?;
    pg::ensure_recall_settings(&pool).await?;
    tracing::info!(embedding_dim = dim, "schema checked");

    // The classification table, settled once, here. A boot question about what this store already
    // holds, the same shape as the KEK check below, and the reason it is not on a request path.
    // Precedence and the reason an empty rule set can never win are in
    // `config::resolve_sensitivity_defaults`.
    let source =
        cfg.apply_sensitivity_defaults(pg::sensitivity_defaults(&pool, &cfg.tenant_id).await?);
    config::log_effective_policy(&cfg, source);
    let cfg = Arc::new(cfg);

    // The key, then the check that this is the key the existing rows were sealed with. A private
    // write is refused until this passes, which is step 4 of the Phase 3 migration order and the one
    // that can strand data.
    let keys = key_provider(&cfg);
    let kek_verified = verify_kek_at_boot(&pool, &cfg, keys.as_ref()).await?;

    // Every embedding refusal runs before any weights load: a boot that is going to refuse should
    // not first spend the time a local model takes to warm.
    let registry = similarity::Registry::engine();
    refuse_hash_beside_previous(&cfg)?;
    let blocks = resolve_blocks(&cfg, &registry)?;
    for (model, keys) in &blocks.guessed {
        tracing::warn!(
            model = %model,
            keys = ?keys,
            "no threshold table entry for this embedding model; these keys use bge-base-en-v1.5's values"
        );
    }
    let migration: Arc<dyn EmbeddingMigrationRepository> =
        Arc::new(pg::PgEmbeddingMigrationRepository::new(pool.clone()));
    let seeded = migration.seed().await?;
    if !seeded.is_empty() {
        tracing::info!(
            units = ?seeded.iter().map(|s| s.unit.as_str()).collect::<Vec<_>>(),
            "seeded embedding state from the vectors each unit holds"
        );
    }
    let states = migration.states().await?;
    let intent = match cfg.embed.migrate.control {
        ControlMode::Command => migration.control().await?.0,
        ControlMode::Env => Intent::default(),
    };
    let view = configured_view(&cfg, &blocks, &states, &intent)?;
    embedding_boot_check(&cfg, &blocks, &states, &view)?;
    let disk = disk_floor(&cfg)?;

    let mut built = Vec::with_capacity(blocks.specs.len());
    for spec in &blocks.specs {
        let embedder = build_embedder(spec).await?;
        tracing::info!(id = %embedder.id(), "embedder ready");
        built.push(embedder);
    }
    let embedders = Arc::new(EmbedderSet::new(built, view.clone(), blocks.thresholds.clone()));
    embedders.set_states(states);

    // The concrete memory repository is kept so it can be handed up as two handles: the port the
    // services read through, and the ciphertext reader they decrypt through. One object, because a
    // second connection pool for the same table would be waste and a second cache of nothing.
    // The adapter is handed the search settings rather than reading the environment, so this call
    // is what makes SEARCH_FUSION real. Without it the variable parses, validates, boots clean and
    // ranks linearly, which is the worst shape a setting can have.
    let memories = Arc::new(pg::PgMemoryRepository::new(pool.clone()).with_search(&cfg.search));
    let oauth: Arc<dyn ports::OauthStore> = Arc::new(pg::PgOauthStore::new(pool.clone()));
    let ingest: Arc<dyn ports::IngestRepository> =
        Arc::new(pg::PgIngestRepository::new(pool.clone()));
    let cleanup: Arc<dyn ports::CleanupRepository> =
        Arc::new(pg::PgCleanupRepository::new(pool.clone()));
    let aliases: Arc<dyn lumberroom_server::ports::AliasRepository> =
        Arc::new(pg::PgAliasRepository::new(pool.clone()));
    warn_on_stranded_user_namespaces(memories.as_ref(), &cfg).await;

    // Seeding and every boot check above run with the sweep off too; only fill, flip and retire
    // stop.
    let sweep = if cfg.embed.migrate.secs == 0 {
        tracing::info!(
            "embedding sweep is off (EMBED_MIGRATE_SECS=0): no fill, flip or retire runs, and the \
             console shows no model-change status"
        );
        None
    } else {
        Some(Arc::new(Sweep::new(
            migration,
            Arc::clone(&embedders),
            row_opener(Arc::clone(&memories) as Arc<dyn services::SealedReader>, keys.clone()),
            Knobs::from_config(&cfg),
            steer(&cfg, &blocks, view),
            kek_verified,
            disk,
        )))
    };

    let repos = services::Repos {
        aliases: Arc::clone(&aliases),
        memories: memories.clone(),
        registry: Arc::new(pg::PgRegistryRepository::new(pool.clone())),
        tool_calls: Arc::new(pg::PgToolCallRepository::new(pool.clone())),
        sealed: Some(Arc::new(pg::PgSealedRepository::new(pool.clone()))),
        ciphertext: Some(memories),
        oauth: services::sources::label_store(cfg.auth.mode, &oauth),
    };

    let state = Arc::new(AppState {
        aliases: Arc::clone(&aliases),
        cfg: Arc::clone(&cfg),
        repos,
        oauth: Arc::clone(&oauth),
        ingest,
        cleanup: Arc::clone(&cleanup),
        embedders: Arc::clone(&embedders),
        embedding_status: sweep.as_ref().map(|s| Arc::clone(&s.status)),
        keys,
        kek_verified,
        proposals: Vec::new(),
    });
    let auth = adapters::auth::create(&cfg, Some(Arc::clone(&oauth)))?;

    if cfg.auth.mode == config::AuthMode::Oauth {
        spawn_oauth_purge(Arc::clone(&oauth));
    }
    spawn_cleanup(
        Arc::clone(&cfg),
        Arc::clone(&cleanup),
        Arc::clone(&state.repos.memories),
        Arc::clone(&embedders),
    );
    spawn_conflict_sweep(
        Arc::clone(&cfg),
        Arc::clone(&state.repos.memories),
        Arc::clone(&embedders),
        pool.clone(),
    );
    if let Some(sweep) = sweep {
        tokio::spawn(sweep.run_loop(
            Arc::new(SingleUnit(cfg.tenant_id.clone())),
            std::time::Duration::from_secs(cfg.embed.migrate.secs),
        ));
    }

    let app = http::router(Arc::clone(&state), auth)
        // The digest is a few KB; anything much larger is a mistake or an attack.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(1024 * 1024));

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| DomainError::internal(format!("cannot bind {addr}")).with_source(e))?;
    tracing::info!(%addr, path = "/mcp", "listening");

    // Connect info, so the login limiter can key on a peer address. Without it the limiter degrades
    // to its global window, which throttles the owner's own retry alongside an attacker's.
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| DomainError::internal("server error").with_source(e))?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Warn when rows sit in a `user:` namespace outside the default read set.
///
/// The personal namespace used to be `user:<TENANT_ID>` and is now always `user:me`. A store that
/// ran under any other tenant therefore holds rows under a name the profile section of the digest,
/// `registry::precedence` and `forget::by_query`'s default set no longer ask for. Search is the
/// exception and the reason the warning does not say "never read": `SEARCH_INCLUDE_ALL_PROJECTS`
/// defaults on, so `search::other_namespaces` still finds the name and searches it as a penalised
/// secondary whenever the grant covers it. An operator who greps, finds their rows in a search
/// result and reads "never" concludes the warning is noise.
///
/// A warning rather than a refusal. The rows are safe, and refusing to boot over recoverable data
/// an operator has not noticed yet is the wrong trade.
///
/// It does not tell anyone to merge. A store running two people under `user:alice` and `user:bob`
/// is misconfigured in a different way, and an UPDATE that collapses both into `user:me` cannot be
/// undone without a backup.
async fn warn_on_stranded_user_namespaces(
    memories: &dyn ports::MemoryRepository,
    cfg: &config::Config,
) {
    warn_on_grants_that_miss_user_me(cfg);

    let rows = match memories.user_namespace_rows(&cfg.tenant_id).await {
        Ok(rows) => rows,
        // Logged rather than dropped. Silence here is indistinguishable from a clean store, and an
        // operator who upgraded a store this query could not read would get no warning and no
        // reason for its absence.
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cannot tell whether any user namespace was stranded by the move to user:me"
            );
            return;
        }
    };

    let mut by_namespace: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for row in rows.iter().filter(|r| r.namespace != "user:me" && r.rows > 0) {
        by_namespace
            .entry(row.namespace.as_str())
            .or_default()
            .push(format!("{}={}", row.table, row.rows));
    }

    for (ns, tables) in by_namespace {
        tracing::warn!(
            namespace = %ns,
            rows = %tables.join(" "),
            "rows sit in a user namespace outside the default read set. The personal namespace is \
             now always user:me: the bootstrap profile, registry precedence and memory_forget's \
             default set ask for user:me and never for this name, though search still reaches it \
             while SEARCH_INCLUDE_ALL_PROJECTS is on and a grant covers it. If these rows are \
             yours, CHANGELOG.md carries the per-table migration; every table listed here has to \
             move, not just memory. If they belong to a second person, leave them and give that \
             person their own TENANT_ID."
        );
    }
}

/// Warn when a grant names a user namespace but not `user:me`.
///
/// The row check above cannot see this one. A fresh store configured with `TENANT_ID=alice` and a
/// grant reading `user:alice` holds no rows to count, boots clean, then refuses every personal
/// write and answers `context_bootstrap` with an empty profile.
fn warn_on_grants_that_miss_user_me(cfg: &config::Config) {
    for grant in &cfg.auth.grants {
        for (axis, patterns) in [("read", grant.read_grants()), ("write", grant.write_grants())] {
            let named: Vec<String> = patterns
                .iter()
                .map(|p| p.namespace.trim().to_ascii_lowercase())
                .filter(|p| p.starts_with("user:"))
                .collect();
            if named.is_empty()
                || patterns.iter().any(|p| domain::namespaces::matches(&p.namespace, "user:me"))
            {
                continue;
            }
            tracing::warn!(
                client = %grant.client,
                axis,
                patterns = %named.join(", "),
                "a grant names a user namespace but does not cover user:me, which is the only one \
                 this build reads or writes by default. This client's personal writes will be \
                 refused and its bootstrap profile will be empty."
            );
        }
    }
}

/// Where the KEK comes from. `None` means writes at `private` are refused rather than stored in
/// plaintext, which is the only safe reading of a missing key.
fn key_provider(cfg: &config::Config) -> Option<Arc<dyn KeyProvider>> {
    match cfg.crypto.provider {
        KekProvider::None => None,
        KekProvider::File => Some(Arc::new(FileKeyProvider::new(
            cfg.crypto.kek_path.clone(),
            cfg.crypto.kek_id.clone(),
        ))),
        KekProvider::Env => Some(Arc::new(EnvKeyProvider::new(
            cfg.crypto.kek_env_var.clone(),
            cfg.crypto.kek_id.clone(),
        ))),
    }
}

/// Compare the live KEK against the fingerprint this store recorded, and record it on first sight.
///
/// A mismatch is not a startup failure. Every open row still reads and writes, and taking the whole
/// store down would be a worse outcome than refusing the private writes. It is loud in the log and
/// reported by `/readyz`, because a server that silently refuses every private write looks healthy
/// otherwise. There is no branch here that falls back to plaintext.
async fn verify_kek_at_boot(
    pool: &sqlx::PgPool,
    cfg: &config::Config,
    keys: Option<&Arc<dyn KeyProvider>>,
) -> Result<bool> {
    let Some(keys) = keys else { return Ok(false) };

    let kek = keys.kek().await?;
    let fingerprint = crypto::kek::fingerprint(&kek);
    let check =
        pg::verify_kek(pool, &cfg.tenant_id, &keys.kek_id(), &fingerprint, keys.provider()).await?;

    match check {
        KekCheck::Recorded => {
            tracing::info!(
                kek_id = %keys.kek_id(),
                provider = keys.provider(),
                "recorded the encryption key for this store"
            );
            Ok(true)
        }
        KekCheck::Matches => {
            tracing::info!(kek_id = %keys.kek_id(), "encryption key verified");
            Ok(true)
        }
        KekCheck::Mismatch { recorded_kek_id } => {
            tracing::error!(
                recorded_kek_id,
                live_kek_id = %keys.kek_id(),
                provider = keys.provider(),
                "the configured KEK is not the key this store was sealed with; private writes stay \
                 refused and existing private rows will not open. Restore the original key, or \
                 accept that the sealed rows are lost and clear kek_state."
            );
            Ok(false)
        }
    }
}

/// Expired codes and tokens are kept for a grace period so a replay stays detectable, then deleted.
/// Hourly, off the request path, because nothing else calls `purge_expired` and those three tables
/// otherwise grow forever.
/// The cleanup pass, on a timer inside this process.
///
/// Cron was the first answer and it was the wrong shape for this product. lumberroom is described as one
/// always-on server, and an always-on server that needs an external scheduler is one the owner has
/// to remember to install, on a host whose cron may not be running, in a container that has none.
/// A `tokio::spawn` beside `spawn_oauth_purge` has none of that: it starts with the server, stops
/// with it, and needs no lock, because a single task cannot overlap itself the way two cron
/// invocations can.
///
/// **The deterministic half only, and that is a boundary rather than a first step.** This process
/// holds the KEK. Decision 0011 keeps the provider call in the `lumberroom` client so that no outbound
/// connection to a third party is ever opened from here, and running the model half on this timer
/// would erase the line while looking like a convenience.
///
/// `run` takes a tenant rather than a `Ctx` for the same reason: a background pass has no caller,
/// and a synthetic principal invented to satisfy a signature is one somebody later reuses where it
/// decides an answer.
fn spawn_cleanup(
    cfg: Arc<config::Config>,
    repo: Arc<dyn ports::CleanupRepository>,
    memories: Arc<dyn ports::MemoryRepository>,
    embedders: Arc<EmbedderSet>,
) {
    let interval = cfg.cleanup.interval_secs;
    if interval == 0 {
        tracing::info!("scheduled cleanup is off (CLEANUP_INTERVAL_SECS=0)");
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
        // The first tick fires at once. Skipping it keeps a restart loop from walking the store on
        // every boot, which on a crash loop is the only thing the database would be doing.
        tick.tick().await;
        loop {
            tick.tick().await;
            let scope = cfg.cleanup.namespace.as_deref();
            match services::cleanup::run(
                &cfg.tenant_id,
                repo.as_ref(),
                embedders.as_ref(),
                scope,
                "hourly",
                cfg.cleanup.limit,
                None,
            )
            .await
            {
                Ok((report, _for_the_model)) => {
                    // Logged only when it found something. A pass that runs every hour and says so
                    // every hour buries the one line that matters.
                    if report.queued > 0 || report.closed_as_answered > 0 || report.truncated {
                        tracing::info!(
                            queued = report.queued,
                            already_known = report.already_known,
                            closed = report.closed_as_answered,
                            truncated = report.truncated,
                            for_the_model = report.for_the_model,
                            "cleanup pass wrote proposals"
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e.log_message(), "cleanup pass failed"),
            }
            // The recall log's retention rides on this timer rather than a scheduler of its own,
            // which is why config refuses RECALL_EVENT_LOG with this pass off. It runs with the log
            // off too, so rows written before the owner turned it off still age out.
            match services::recall_events::purge(
                memories.as_ref(),
                &cfg.tenant_id,
                cfg.recall_events.retention_days,
                services::recall_events::PURGE_BATCH,
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!(rows = n, "deleted recall events past their retention"),
                Err(e) => tracing::warn!(error = %e.log_message(), "recall event purge failed"),
            }
        }
    });
}

/// The conflict sweeper: the only caller of `memory_conflict_record`, and the backfill.
///
/// Two wakes drive one loop. The listener hears the `memory_conflict` notification a writer's
/// commit sends; the timer covers every notification Postgres dropped while nobody listened, and
/// the rows that existed before this process started. A listener that fails at boot costs latency
/// and nothing else, so its error is a warning and boot goes on.
///
/// The listener holds one pooled connection for the life of the process. The default pool of 10
/// leaves nine for requests; an owner who sets `DB_MAX_CONNECTIONS` low should count it.
///
/// `CONFLICT_SWEEP_SECS=0` turns off the timer and the listener together. Nothing else records a
/// pair, so the log line says so.
fn spawn_conflict_sweep(
    cfg: Arc<config::Config>,
    repo: Arc<dyn ports::MemoryRepository>,
    embedders: Arc<EmbedderSet>,
    pool: sqlx::PgPool,
) {
    let secs = cfg.quality.conflict_sweep_secs;
    if secs == 0 {
        tracing::info!(
            "conflict sweeper is off (CONFLICT_SWEEP_SECS=0): no conflict pairs will be recorded"
        );
        return;
    }
    let wakes = Arc::new(services::conflicts::Wakes::default());
    let on_wake = Arc::clone(&wakes);
    tokio::spawn(async move {
        if let Err(e) = pg::conflict_wake::listen(&pool, move |t| on_wake.wake(t)).await {
            tracing::warn!(
                error = %e.log_message(),
                "conflict wake listener did not start; pairs wait for the timer sweep"
            );
        }
    });
    tokio::spawn(services::conflicts::run_loop(
        repo,
        cfg.tenant_id.clone(),
        embedders,
        std::time::Duration::from_millis(cfg.quality.conflict_sweep_budget_ms),
        std::time::Duration::from_secs(secs),
        wakes,
    ));
}

fn spawn_oauth_purge(store: Arc<dyn ports::OauthStore>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match store.purge_expired().await {
                Ok(n) if n > 0 => tracing::info!(rows = n, "purged expired oauth credentials"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e.log_message(), "oauth purge failed"),
            }
        }
    });
}

/// `lumberroom-server hash-password`: stdin in, PHC string out, nothing else on stdout.
///
/// The password never reaches argv, where it would be visible to every process on the box and land
/// in a shell history. `install.sh` pipes it in with no TTY, so this reads stdin either way and only
/// bothers with the prompt and the echo dance when a person is typing.
fn hash_password() -> Result<()> {
    use argon2::password_hash::PasswordHasher;
    use argon2::Argon2;

    let password = read_password()?;
    let password = password.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        return Err(DomainError::validation("no password on stdin"));
    }
    if password.chars().count() < 12 {
        return Err(DomainError::validation(
            "the owner's password guards every memory in the store. Use at least 12 characters.",
        ));
    }

    // 16 bytes from the OS. `hash_password` would generate a salt too, through password-hash's own
    // getrandom path; this keeps the salt on the same CSPRNG every key in `crypto` comes from.
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt)
        .map_err(|e| DomainError::internal(format!("os rng failure: {e}")))?;

    let hash = Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map_err(|e| DomainError::internal(format!("argon2 failed: {e}")))?
        .to_string();

    // Exactly one line on stdout. install.sh captures this and writes it to .env.
    println!("{hash}");
    Ok(())
}

/// Read one line without echoing it, when there is a terminal to turn echo off on.
///
/// `stty` rather than a terminal crate: it is the same mechanism a shell script would use, and the
/// alternative is a dependency for one syscall. If it is missing, the operator is told the password
/// will be visible instead of being quietly recorded on their screen.
fn read_password() -> Result<String> {
    let interactive = std::io::stdin().is_terminal();
    let mut echo_off = false;
    if interactive {
        echo_off = stty("-echo");
        if !echo_off {
            eprintln!("warning: cannot turn terminal echo off, what you type will be visible");
        }
        // The prompt goes to stderr so stdout carries the hash and nothing else.
        eprint!("password: ");
        let _ = std::io::stderr().flush();
    }

    let mut buf = String::new();
    let read = std::io::stdin().read_to_string(&mut buf);
    if echo_off {
        stty("echo");
        eprintln!();
    }
    read.map_err(|e| DomainError::internal("cannot read stdin").with_source(e))?;
    Ok(buf)
}

fn stty(flag: &str) -> bool {
    std::process::Command::new("stty")
        .arg(flag)
        .stdin(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `lumberroom-server generate-kek`: the key on stdout so `> kek.file` works, the warning on stderr.
fn generate_kek() -> Result<()> {
    let hex = crypto::kek::generate_kek_hex()?;
    println!("{hex}");
    eprintln!(
        "Store this once and keep a copy somewhere the server cannot reach. Losing it makes every \
         private row permanently unreadable: the per-row keys are wrapped under it and there is no \
         second copy. Then: chmod 600 the file and point KEK_PATH at it."
    );
    Ok(())
}

/// `lumberroom-server verify-kek`: the fingerprint of the configured key, and whether this store agrees.
///
/// Not read-only, and it says so: on a store with nothing recorded this writes the fingerprint, which
/// is the same thing a boot does and the step that makes private writes possible at all.
async fn verify_kek_command() -> Result<()> {
    let cfg = config::load()?;
    let Some(keys) = key_provider(&cfg) else {
        println!("kek_provider: none");
        println!("verified:     no");
        println!("Encryption is off, so every write at private is refused. Set KEK_PROVIDER.");
        return Ok(());
    };

    let kek = keys.kek().await?;
    let fingerprint = crypto::kek::fingerprint(&kek);
    println!("kek_provider: {}", keys.provider());
    println!("kek_id:       {}", keys.kek_id());
    println!("fingerprint:  {fingerprint}");

    let pool = pg::connect_with(&cfg.database_url, &cfg.db).await?;
    let check =
        pg::verify_kek(&pool, &cfg.tenant_id, &keys.kek_id(), &fingerprint, keys.provider())
            .await?;
    pool.close().await;

    match check {
        KekCheck::Recorded => {
            println!(
                "verified:     yes (nothing was recorded before, so this key is now the one \
                      this store is sealed with)"
            );
            Ok(())
        }
        KekCheck::Matches => {
            println!("verified:     yes (matches what this store was sealed with)");
            Ok(())
        }
        KekCheck::Mismatch { recorded_kek_id } => {
            println!("verified:     NO");
            println!(
                "This store was sealed under {recorded_kek_id}, which is a different key. Private \
                 writes are refused and existing private rows will not open under the configured \
                 key. Restore the original key."
            );
            // Non-zero, so a deploy script that runs this as a check fails instead of continuing.
            std::process::exit(3);
        }
    }
}

/// `lumberroom-server verify-embedding`: the embedding boot checks, with no embedder built and no
/// weights loaded.
///
/// An operator runs it before recreating the server, as `verify-kek` checks a key; no script in
/// `deploy/` or `scripts/` calls either one. It may run before the new image migrates the store,
/// so a store with no `embedding_b` column or no state table counts as pre-migration and only the
/// configuration is checked. It seeds nothing:
/// a unit with vectors and no state row is the boot's to seed.
async fn verify_embedding_command() -> Result<()> {
    let cfg = config::load()?;
    let registry = similarity::Registry::engine();
    refuse_hash_beside_previous(&cfg)?;
    let blocks = resolve_blocks(&cfg, &registry)?;
    let control = match cfg.embed.migrate.control {
        ControlMode::Command => "command",
        ControlMode::Env => "env",
    };
    println!("control:  {control}");
    for (role, id) in ["current", "previous"].iter().zip(&blocks.ids) {
        println!("{role}:  {id}");
    }
    for (model, keys) in &blocks.guessed {
        println!("guessed:  {model} runs on bge-base-en-v1.5's values for {}", keys.join(", "));
    }
    if disk_floor(&cfg)?.is_some() {
        println!("disk:     statvfs({}) answers", cfg.embed.disk.path);
    }

    let pool = pg::connect_with(&cfg.database_url, &cfg.db).await?;
    let read = read_embedding_store(&pool, &cfg).await;
    pool.close().await;
    let (states, intent) = match read? {
        Some(read) => read,
        None => {
            println!(
                "store:    pre-migration (no memory.embedding_b column or no embedding_state \
                 table). Checked the configuration only; the new image's migrations create both."
            );
            (Vec::new(), Intent::default())
        }
    };
    for s in &states {
        println!(
            "unit:     {} active on {} in slot {}",
            s.unit,
            s.active_model(),
            s.active_slot.as_str()
        );
    }

    let view = configured_view(&cfg, &blocks, &states, &intent)?;
    embedding_boot_check(&cfg, &blocks, &states, &view)?;
    println!("verified: yes");
    Ok(())
}

/// The states and, in command mode, the intent, after both width checks. None on a pre-migration
/// store.
async fn read_embedding_store(
    pool: &sqlx::PgPool,
    cfg: &config::Config,
) -> Result<Option<(Vec<UnitState>, Intent)>> {
    if !pg::embedding_migration_schema(pool).await? {
        return Ok(None);
    }
    pg::assert_embedding_dim(pool, cfg.embed.dim).await?;
    pg::assert_embedding_b_dim(pool, cfg.embed.dim).await?;
    let repo = pg::PgEmbeddingMigrationRepository::new(pool.clone());
    let states = repo.states().await?;
    let intent = match cfg.embed.migrate.control {
        ControlMode::Command => repo.control().await?.0,
        ControlMode::Env => Intent::default(),
    };
    Ok(Some((states, intent)))
}

/// The configured embedding blocks, `EMBED_*` and then `EMBED_PREVIOUS_*` when set, each with its
/// model's thresholds resolved. Computed from config alone, so `verify-embedding` and the boot
/// refuse on the same values.
struct Blocks {
    specs: Vec<EmbedderSpec>,
    /// `id_for` each spec, in the same order.
    ids: Vec<String>,
    thresholds: HashMap<String, Arc<SimilarityThresholds>>,
    guessed_acting: BTreeMap<String, Vec<String>>,
    /// Every guessed key per model, acting or not, for the boot warning.
    guessed: Vec<(String, Vec<String>)>,
}

/// Resolves each model's thresholds once, here and nowhere else. The fork extends the registry
/// before this call. An unknown override key or a failed cross-key check stops the boot; a key the
/// table has no value for runs on bge-base-en-v1.5's and the caller names it.
fn resolve_blocks(cfg: &config::Config, registry: &similarity::Registry) -> Result<Blocks> {
    let mut sources =
        vec![(cfg.embed.current_spec(), cfg.embed.thresholds.as_slice(), "EMBED_THRESHOLDS")];
    if let Some(previous) = cfg.embed.previous_spec() {
        sources.push((
            previous,
            cfg.embed.previous_thresholds.as_slice(),
            "EMBED_PREVIOUS_THRESHOLDS",
        ));
    }
    // Empty whenever two blocks are configured: config refuses an old single threshold variable
    // beside a second model, since nobody can say which model it was tuned on.
    let legacy = cfg.legacy_thresholds();

    let mut blocks = Blocks {
        specs: Vec::new(),
        ids: Vec::new(),
        thresholds: HashMap::new(),
        guessed_acting: BTreeMap::new(),
        guessed: Vec::new(),
    };
    for (spec, overrides, variable) in sources {
        let unknown = registry.unknown_keys(overrides);
        if !unknown.is_empty() {
            return Err(DomainError::validation(format!(
                "{variable} names keys no table registers: {}. Known keys: {}.",
                unknown.join(", "),
                registry.keys().iter().map(|k| k.key).collect::<Vec<_>>().join(", ")
            )));
        }
        let id = adapters::embedding::id_for(&spec);
        let t = registry.resolve(&id, overrides, &legacy);
        // Installer deployments copied an .env.example that set DEDUPE_THRESHOLD=0.97 and its
        // siblings. Those beat the family table, so a store that switches models keeps bge's scale
        // with no other sign. Before the check, so a refusal the old value caused arrives explained.
        for (l, table) in registry.legacy_departures(&t.model, overrides, &legacy) {
            tracing::warn!(
                variable = l.variable,
                value = l.value,
                table_value = table,
                model = %t.model,
                "an old single threshold variable overrides this model's table value; clear it \
                 unless it was tuned on this model, or move it into EMBED_THRESHOLDS"
            );
        }
        let problems = registry.check(&t);
        if !problems.is_empty() {
            return Err(DomainError::validation(problems.join("; ")));
        }
        let guessed: Vec<String> = t.guessed().into_iter().map(str::to_string).collect();
        if !guessed.is_empty() {
            blocks.guessed.push((id.clone(), guessed));
        }
        blocks.guessed_acting.insert(id.clone(), registry.guessed_acting(&t));
        blocks.thresholds.insert(id.clone(), Arc::new(t));
        blocks.ids.push(id);
        blocks.specs.push(spec);
    }
    Ok(blocks)
}

/// The hash embedder's vectors are a token sketch no model's vectors compare with, so a migration
/// to or from it moves nothing a search can use. It stays legal on its own, for tests.
fn refuse_hash_beside_previous(cfg: &config::Config) -> Result<()> {
    let Some(previous) = &cfg.embed.previous else { return Ok(()) };
    let hashed: Vec<&str> =
        [("EMBED_PROVIDER", cfg.embed.provider), ("EMBED_PREVIOUS_PROVIDER", previous.provider)]
            .into_iter()
            .filter(|(_, provider)| *provider == EmbedProvider::Hash)
            .map(|(variable, _)| variable)
            .collect();
    if hashed.is_empty() {
        return Ok(());
    }
    Err(DomainError::validation(format!(
        "{} selects the hash embedder while EMBED_PREVIOUS_* is set. Its vectors are a token \
         sketch that no model's vectors can be compared with. Configure a model in both blocks, \
         or remove the EMBED_PREVIOUS_* block.",
        hashed.join(" and ")
    )))
}

/// The view the phase rules read. Env mode fixes it here from `.env`; command mode derives it from
/// the control row, and the sweep derives it again on every pass.
fn configured_view(
    cfg: &config::Config,
    blocks: &Blocks,
    states: &[UnitState],
    intent: &Intent,
) -> Result<Configured> {
    let m = &cfg.embed.migrate;
    match m.control {
        ControlMode::Env => Ok(Configured {
            current: blocks.ids[0].clone(),
            previous: blocks.ids.get(1).cloned(),
            retire: m.retire.clone(),
            flip: m.flip.clone().unwrap_or(FlipScope::All),
            rollback_days: m.rollback_days,
            guessed_acting: blocks.guessed_acting.clone(),
            generation: None,
        }),
        // The default unit's active model, as the sweep's first listed unit gives it.
        ControlMode::Command => domain::embedding_command::configured_from_intent(
            intent,
            &blocks.ids,
            states.iter().find(|s| s.unit == cfg.tenant_id).map(UnitState::active_model),
            m.rollback_days,
            &blocks.guessed_acting,
        )
        .map_err(DomainError::validation),
    }
}

/// `boot_check`, plus the two command-mode rules it cannot see. With no target written, command
/// mode's view names only the model the default unit is active on, so `boot_check` alone would
/// pass a unit active on a model no block configures, and a request for it would fail on every
/// call. It would also pass two blocks naming one model.
fn embedding_boot_check(
    cfg: &config::Config,
    blocks: &Blocks,
    states: &[UnitState],
    view: &Configured,
) -> Result<()> {
    if cfg.embed.migrate.control == ControlMode::Command {
        let refusals = command_mode_refusals(&blocks.ids, states);
        if !refusals.is_empty() {
            return Err(DomainError::validation(refusals.join("\n")));
        }
    }
    domain::embedding_phase::boot_check(states, view).map_err(DomainError::validation)
}

fn command_mode_refusals(ids: &[String], states: &[UnitState]) -> Vec<String> {
    let mut refusals = Vec::new();
    if let [current, previous] = ids {
        if current == previous {
            refusals.push(format!(
                "EMBED_PREVIOUS_* names the same model as EMBED_*: {current}. Remove the \
                 EMBED_PREVIOUS_* block, or point it at the other model."
            ));
        }
    }
    let off: Vec<String> = states
        .iter()
        .filter(|s| !ids.iter().any(|id| id == s.active_model()))
        .map(|s| format!("{} (active on {})", s.unit, s.active_model()))
        .collect();
    if !off.is_empty() {
        refusals.push(format!(
            "units active on a model no block configures: {}. This server configures {}. Either \
             set EMBED_* back to the model each unit is active on, or name that model in \
             EMBED_PREVIOUS_* and run lumberroom-server embeddings start.",
            off.join(", "),
            ids.join(" and ")
        ));
    }
    refusals
}

/// The free-space reader the sweep pauses on, or None with no floor set. The first read happens
/// here, so a path statvfs cannot read refuses the boot instead of pausing every pass.
fn disk_floor(cfg: &config::Config) -> Result<Option<Arc<dyn FreeSpace>>> {
    let d = &cfg.embed.disk;
    if d.floor_mb == 0 {
        return Ok(None);
    }
    let disk = adapters::disk::StatvfsFreeSpace { path: d.path.clone() };
    let free = disk.free_bytes().map_err(|e| {
        DomainError::validation(format!(
            "EMBED_DISK_FLOOR_MB={} needs the free space under EMBED_DISK_PATH={}, and statvfs \
             failed: {}. Point EMBED_DISK_PATH at a path on the database's filesystem as this \
             process sees it, or set EMBED_DISK_FLOOR_MB=0.",
            d.floor_mb,
            d.path,
            e.log_message()
        ))
    })?;
    tracing::info!(
        path = %d.path,
        free_bytes = free,
        floor_bytes = d.floor_mb.saturating_mul(1_048_576),
        "embedding disk floor set"
    );
    Ok(Some(Arc::new(disk)))
}

/// Where the sweep's view comes from: `.env` at boot, or the control row on every pass.
fn steer(cfg: &config::Config, blocks: &Blocks, view: Configured) -> Steer {
    match cfg.embed.migrate.control {
        ControlMode::Env => Steer::Env(view),
        ControlMode::Command => Steer::Command {
            blocks: blocks.ids.clone(),
            rollback_days: cfg.embed.migrate.rollback_days,
            guessed_acting: blocks.guessed_acting.clone(),
        },
    }
}

/// With no KEK provider the sweep cannot open a private row. `kek_verified` is false then, so the
/// sweep skips those rows and reports `blocked: kek` instead of counting each as a failure.
fn row_opener(
    reader: Arc<dyn services::SealedReader>,
    keys: Option<Arc<dyn KeyProvider>>,
) -> Arc<dyn services::row_opener::RowOpener> {
    match keys {
        Some(keys) => Arc::new(services::row_opener::ServiceKeyOpener { reader, keys }),
        None => Arc::new(services::row_opener::NoOpener),
    }
}

/// A model that fails to load stops the boot. Falling back to another embedder would write vectors
/// that no existing row can be compared with (decision 0028).
///
/// Only a local model warms here. A remote embedder makes no call at boot: a probe would put
/// compose in a restart loop behind a sidecar that starts late.
async fn build_embedder(spec: &EmbedderSpec) -> Result<Arc<dyn Embedder>> {
    let started = std::time::Instant::now();
    let embedder = adapters::embedding::create_spec(spec)?;
    // Thresholds and state were resolved against `id_for`. An adapter whose id drifted from it
    // would serve every request with no thresholds.
    let expected = adapters::embedding::id_for(spec);
    if embedder.id() != expected {
        return Err(DomainError::internal(format!(
            "the embedder reports id {} where config computes {expected}",
            embedder.id()
        )));
    }
    if spec.provider == EmbedProvider::Local {
        embedder.embed_documents(vec!["warm".to_string()]).await?;
        tracing::info!(id = %expected, ms = started.elapsed().as_millis(), "embedder loaded");
    }
    Ok(embedder)
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("LOG_LEVEL")
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap_or_default();
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!(signal = "SIGINT", "shutting down"),
        _ = terminate => tracing::info!(signal = "SIGTERM", "shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::embedding_slot::VectorSlot;

    fn unit(name: &str, active: &str) -> UnitState {
        UnitState {
            unit: name.into(),
            active_slot: VectorSlot::A,
            model_a: Some(active.into()),
            model_b: None,
            flipped_at: None,
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_mode_passes_units_on_either_block() {
        let states = [unit("me", "bge"), unit("other", "gemma")];
        assert!(command_mode_refusals(&ids(&["gemma", "bge"]), &states).is_empty());
    }

    #[test]
    fn command_mode_refuses_a_unit_active_on_no_block() {
        let refusals = command_mode_refusals(&ids(&["gemma"]), &[unit("me", "bge")]);
        assert_eq!(refusals.len(), 1);
        assert!(refusals[0].contains("me (active on bge)"), "{}", refusals[0]);
        assert!(refusals[0].contains("configures gemma"), "{}", refusals[0]);
    }

    #[test]
    fn command_mode_refuses_two_blocks_naming_one_model() {
        let refusals = command_mode_refusals(&ids(&["bge", "bge"]), &[unit("me", "bge")]);
        assert_eq!(refusals.len(), 1);
        assert!(refusals[0].contains("same model"), "{}", refusals[0]);
    }
}
