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
use lumberroom_server::{adapters, config, crypto, domain, http, mcp, ports, services};

use std::io::{IsTerminal, Read, Write};
use std::sync::Arc;

use adapters::embedding::LocalEmbedder;
use adapters::postgres::{self as pg, KekCheck};
use config::{EmbedProvider, KekProvider};
use crypto::kek::{EnvKeyProvider, FileKeyProvider, KeyProvider};
use domain::errors::{DomainError, Result};
use mcp::AppState;
use ports::Embedder;

const USAGE: &str = "\
lumberroom-server: durable memory over MCP

  lumberroom-server                  run the server
  lumberroom-server hash-password    read a password on stdin, print an argon2id hash for OWNER_PASSWORD_HASH
  lumberroom-server generate-kek     print a fresh key-encryption key as hex, for KEK_PATH
  lumberroom-server verify-kek       report whether the configured KEK is the one this store was sealed with
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

    let pool = pg::connect(&cfg.database_url).await?;

    if cfg.run_migrations_on_boot {
        pg::migrate(&pool).await?;
        tracing::info!("migrations up to date");
    }
    let dim = pg::assert_embedding_dim(&pool, cfg.embed.dim).await?;
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

    let (embedder, degraded) = warm_embedder(&cfg).await?;
    tracing::info!(id = %embedder.id(), degraded, "embedder ready");

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
        embedder,
        degraded_embedder: degraded,
        keys,
        kek_verified,
        proposals: Vec::new(),
    });
    let auth = adapters::auth::create(&cfg, Some(Arc::clone(&oauth)))?;

    if cfg.auth.mode == config::AuthMode::Oauth {
        spawn_oauth_purge(Arc::clone(&oauth));
    }
    spawn_cleanup(Arc::clone(&cfg), Arc::clone(&cleanup));

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
fn spawn_cleanup(cfg: Arc<config::Config>, repo: Arc<dyn ports::CleanupRepository>) {
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
        }
    });
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
    use argon2::password_hash::{PasswordHasher, SaltString};
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

    // 16 bytes from the OS. `SaltString::generate` would work too and would pull in a second RNG
    // path; this is the same CSPRNG every key in `crypto` comes from.
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt)
        .map_err(|e| DomainError::internal(format!("os rng failure: {e}")))?;
    let salt = SaltString::encode_b64(&salt)
        .map_err(|e| DomainError::internal(format!("cannot encode a salt: {e}")))?;

    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
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

    let pool = pg::connect(&cfg.database_url).await?;
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

/// With EMBED_ALLOW_FALLBACK the server degrades to the hash embedder rather than refusing to
/// start, and readiness reports the degradation. Mixed embedders in one table hurt recall, which
/// is why the default is to fail loudly instead.
async fn warm_embedder(cfg: &config::Config) -> Result<(Arc<dyn Embedder>, bool)> {
    if cfg.embed.provider != EmbedProvider::Local {
        return Ok((adapters::embedding::create(cfg)?, false));
    }

    let started = std::time::Instant::now();
    match LocalEmbedder::new(&cfg.embed.model, cfg.embed.dim, &cfg.embed.cache_dir) {
        Ok(local) => match local.warm().await {
            Ok(()) => {
                tracing::info!(ms = started.elapsed().as_millis(), "embedder loaded");
                Ok((Arc::new(local), false))
            }
            Err(e) if cfg.embed.allow_fallback => {
                tracing::error!(error = %e.log_message(), "embedder failed to warm, falling back to hash");
                Ok((Arc::new(adapters::embedding::HashEmbedder::new(cfg.embed.dim)), true))
            }
            Err(e) => Err(e),
        },
        Err(e) if cfg.embed.allow_fallback => {
            tracing::error!(error = %e.log_message(), "embedder failed to load, falling back to hash");
            Ok((Arc::new(adapters::embedding::HashEmbedder::new(cfg.embed.dim)), true))
        }
        Err(e) => Err(e),
    }
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
