//! `lumberroom-server embeddings <status|start|flip|rollback|retire>`: records the operator's
//! intent in `embedding_control` and reports what the server's sweep did with it. The server makes
//! every embedding call and every fill, flip and deletion; this process only reads state, decides
//! through `domain::embedding_command`, and writes the intent under the control row lock.
//!
//! It runs inside the server container (`docker compose exec`, or `docker compose run --rm -T`
//! with the server down), so it reaches the store over the container's `DATABASE_URL` and computes
//! embedder ids from the same environment the server booted on.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::adapters::embedding::{create_spec, id_for};
use crate::config::{Config, EmbedProvider, EmbedderSpec};
use crate::domain::embedding_command::{self as rules, Decision, View};
use crate::domain::embedding_migration::{
    Blocked, ControlMode, Counts, DiskStatus, Intent, Phase, Published, UnitState, UnitStatus, Verb,
};
use crate::domain::embedding_slot::VectorSlot;
use crate::domain::errors::{DomainError, Result};
use crate::domain::similarity::{Legacy, Registry, SimilarityThresholds};
use crate::ports::{ChangeOutcome, Embedder, EmbeddingMigrationRepository, FreeSpace};

pub const USAGE: &str = "\
usage: lumberroom-server embeddings <verb> [flag]

  status [--json]       the intent, the server's last pass, each unit's phase, thresholds, next step
  start [--no-wait]     dual-write and fill the configured model no unit is on yet
  flip [--no-wait]      move every unit onto the target once its fill completes
  rollback [--no-wait]  before a flip: cancel the start; after one: flip back to the old model
  retire [--no-wait]    delete the old model's vectors once every rollback window has closed

Exit codes: 0 written and applied, or nothing to write; 1 refused; 2 usage or wrong mode;
3 written but no server applied it within the wait.
";

/// The text `start` embeds once through a remote target before it writes anything.
pub const PROBE_TEXT: &str = "lumberroom embeddings probe";

const ENV_MODE: &str = "This server takes its embedding switch from .env \
                        (EMBED_MIGRATION_CONTROL=env). Read progress on the console.";

#[derive(Debug)]
pub struct Outcome {
    /// 0 written and applied, or a no-op; 1 refused; 2 usage or wrong mode; 3 written, not applied
    /// within the wait.
    pub code: i32,
    pub output: String,
}

/// Builds an embedder for the start probe from one configured block.
pub type ProbeFn = dyn Fn(&EmbedderSpec) -> Option<Arc<dyn Embedder>> + Send + Sync;

/// What `run` reads from the process, injectable for tests.
pub struct CommandEnv {
    pub cfg: Arc<Config>,
    pub repo: Arc<dyn EmbeddingMigrationRepository>,
    pub registry: Arc<Registry>,
    /// Builds the start target's embedder for the probe; returns None for a local target, which
    /// boot already loaded.
    pub probe: Arc<ProbeFn>,
    pub disk: Option<Arc<dyn FreeSpace>>,
    pub now: fn() -> DateTime<Utc>,
    /// Between reads of `applied_generation` while waiting. 2 s in production.
    pub poll: std::time::Duration,
}

/// `lumberroom-server embeddings <args>`: loads config, connects with a pool of 2, builds a
/// `CommandEnv`, runs `execute`, prints `output`, returns `code`.
pub async fn run(args: Vec<String>, registry: Registry) -> Result<i32> {
    use crate::adapters::disk::StatvfsFreeSpace;
    use crate::adapters::postgres::{self as pg, PgEmbeddingMigrationRepository};

    let cfg = Arc::new(crate::config::load()?);
    // Usage and env mode answer before any connection, so a wrong mode never waits on the store.
    if let Some(early) = preflight(&args, &Settings::from_config(&cfg)) {
        println!("{}", early.output);
        return Ok(early.code);
    }
    // Two connections: one for the control row transaction, one spare for the reads around it. A
    // command that took the server's pool size could crowd the server it runs beside.
    let db = crate::config::DbConfig {
        max_connections: 2,
        max_connections_explicit: false,
        acquire_timeout_secs: cfg.db.acquire_timeout_secs,
    };
    let pool = pg::connect_with(&cfg.database_url, &db).await?;
    let disk: Option<Arc<dyn FreeSpace>> = (cfg.embed.disk.floor_mb > 0)
        .then(|| Arc::new(StatvfsFreeSpace { path: cfg.embed.disk.path.clone() }) as _);
    let env = CommandEnv {
        cfg,
        repo: Arc::new(PgEmbeddingMigrationRepository::new(pool.clone())),
        registry: Arc::new(registry),
        probe: Arc::new(probe_embedder),
        disk,
        now: Utc::now,
        poll: std::time::Duration::from_secs(2),
    };
    let outcome = execute(&args, &env).await;
    pool.close().await;
    let outcome = outcome?;
    println!("{}", outcome.output);
    Ok(outcome.code)
}

pub async fn execute(args: &[String], env: &CommandEnv) -> Result<Outcome> {
    let io = Io {
        repo: env.repo.as_ref(),
        registry: env.registry.as_ref(),
        probe: env.probe.as_ref(),
        disk: env.disk.as_deref(),
        now: env.now,
        poll: env.poll,
    };
    execute_with(args, &Settings::from_config(&env.cfg), &io).await
}

/// A remote target only. A local model was loaded and warmed at boot, and a hash embedder makes no
/// call. A remote spec that fails to build still answers, with an embedder that returns the build
/// error, so the probe refuses the start instead of skipping.
fn probe_embedder(spec: &EmbedderSpec) -> Option<Arc<dyn Embedder>> {
    if spec.provider != EmbedProvider::Openai {
        return None;
    }
    Some(match create_spec(spec) {
        Ok(embedder) => embedder,
        Err(e) => Arc::new(Unbuilt { id: id_for(spec), dim: spec.dim, error: e.log_message() }),
    })
}

struct Unbuilt {
    id: String,
    dim: usize,
    error: String,
}

#[async_trait::async_trait]
impl Embedder for Unbuilt {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed_documents(&self, _texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Err(DomainError::unavailable(format!("cannot build the embedder: {}", self.error)))
    }
    async fn embed_query(&self, _text: &str) -> Result<Vec<f32>> {
        Err(DomainError::unavailable(format!("cannot build the embedder: {}", self.error)))
    }
}

/// The settings the command reads from `Config`, apart so the tests need no hand-built `Config`
/// (`config::load` reads the process environment, and parallel tests that set it flake).
struct Settings {
    control: ControlMode,
    /// `EMBED_MIGRATE_SECS`; 0 turns the sweep off.
    interval_secs: u64,
    rollback_days: i64,
    /// `EMBED_DISK_FLOOR_MB` in bytes; 0 when no floor is set.
    floor_bytes: u64,
    /// `EMBED_*`, then `EMBED_PREVIOUS_*` when set, each with its overrides.
    blocks: Vec<(EmbedderSpec, Vec<(String, f64)>)>,
    /// Legal with one block only; config refuses them beside a second.
    legacy: Vec<Legacy>,
}

impl Settings {
    fn from_config(cfg: &Config) -> Self {
        let mut blocks = vec![(cfg.embed.current_spec(), cfg.embed.thresholds.clone())];
        if let Some(previous) = cfg.embed.previous_spec() {
            blocks.push((previous, cfg.embed.previous_thresholds.clone()));
        }
        Settings {
            control: cfg.embed.migrate.control,
            interval_secs: cfg.embed.migrate.secs,
            rollback_days: cfg.embed.migrate.rollback_days,
            floor_bytes: cfg.embed.disk.floor_mb.saturating_mul(1_048_576),
            blocks,
            legacy: cfg.legacy_thresholds(),
        }
    }
}

/// `CommandEnv` without the `Config`, borrowed.
struct Io<'a> {
    repo: &'a dyn EmbeddingMigrationRepository,
    registry: &'a Registry,
    probe: &'a ProbeFn,
    disk: Option<&'a dyn FreeSpace>,
    now: fn() -> DateTime<Utc>,
    poll: std::time::Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Parsed {
    /// None for `status`.
    verb: Option<Verb>,
    json: bool,
    no_wait: bool,
}

/// By hand, in `main.rs`'s style: one verb, then `--json` (status) or `--no-wait` (the writes).
fn parse(args: &[String]) -> std::result::Result<Parsed, Outcome> {
    let usage = |problem: String| Outcome { code: 2, output: format!("{problem}\n\n{USAGE}") };
    let mut words = args.iter().map(String::as_str);
    let verb = match words.next() {
        Some("status") => None,
        Some("start") => Some(Verb::Start),
        Some("flip") => Some(Verb::Flip),
        Some("rollback") => Some(Verb::Rollback),
        Some("retire") => Some(Verb::Retire),
        Some("-h" | "--help" | "help") => {
            return Err(Outcome { code: 0, output: USAGE.to_string() })
        }
        Some(other) => return Err(usage(format!("unknown verb {other:?}"))),
        None => return Err(usage("no verb given".to_string())),
    };
    let mut parsed = Parsed { verb, json: false, no_wait: false };
    for word in words {
        match (word, verb) {
            ("--json", None) if !parsed.json => parsed.json = true,
            ("--no-wait", Some(_)) if !parsed.no_wait => parsed.no_wait = true,
            _ => {
                let name = verb.map_or("status", Verb::as_str);
                return Err(usage(format!("{name} does not take {word:?}")));
            }
        }
    }
    Ok(parsed)
}

/// The answers that need no store: usage and env mode.
fn preflight(args: &[String], settings: &Settings) -> Option<Outcome> {
    if let Err(outcome) = parse(args) {
        return Some(outcome);
    }
    (settings.control == ControlMode::Env)
        .then(|| Outcome { code: 2, output: ENV_MODE.to_string() })
}

async fn execute_with(args: &[String], settings: &Settings, io: &Io<'_>) -> Result<Outcome> {
    if let Some(early) = preflight(args, settings) {
        return Ok(early);
    }
    let cmd = parse(args).expect("preflight parsed these arguments");

    let (intent, published) = io.repo.control().await?;
    let states = io.repo.states().await?;
    let blocks: Vec<String> = settings.blocks.iter().map(|(spec, _)| id_for(spec)).collect();
    // Resolved as boot resolves them, so the guessed keys this command acts on are the ones the
    // server refuses on.
    let thresholds: Vec<SimilarityThresholds> = blocks
        .iter()
        .zip(&settings.blocks)
        .map(|(id, (_, overrides))| io.registry.resolve(id, overrides, &settings.legacy))
        .collect();
    let guessed_acting: BTreeMap<String, Vec<String>> =
        thresholds.iter().map(|t| (t.model.clone(), io.registry.guessed_acting(t))).collect();
    let statuses = parse_statuses(published.status.as_ref());
    let now = (io.now)();

    let Some(verb) = cmd.verb else {
        let output = if cmd.json {
            let doc = serde_json::json!({
                "intent": intent,
                "published": published,
                "states": states,
            });
            serde_json::to_string_pretty(&doc)
                .map_err(|e| DomainError::internal("cannot render the status").with_source(e))?
        } else {
            status_text(
                &intent,
                &published,
                &states,
                &statuses,
                &blocks,
                &thresholds,
                settings,
                now,
            )
        };
        return Ok(Outcome { code: 0, output });
    };

    let mut lines = Vec::new();
    match rules::drift(&blocks, &published, settings.interval_secs, now) {
        Err(text) => return Ok(Outcome { code: 1, output: text }),
        Ok(Some(warning)) => lines.push(warning),
        Ok(None) => {}
    }

    let mut probe = None;
    let mut disk = None;
    if verb == Verb::Start {
        probe = run_probe(settings, io, &blocks, &states).await;
        if settings.floor_bytes > 0 {
            if let Some(free) = io.disk {
                let free_bytes = match free.free_bytes() {
                    Ok(n) => n,
                    Err(e) => {
                        return Ok(Outcome {
                            code: 1,
                            output: format!(
                                "start refused: cannot read free space for the disk floor: {}",
                                e.log_message()
                            ),
                        })
                    }
                };
                disk = Some(DiskStatus {
                    free_bytes,
                    floor_bytes: settings.floor_bytes,
                    paused: free_bytes <= settings.floor_bytes,
                });
            }
        }
    }

    // The repository hands `decide` the intent and the unit states it re-read under the row lock
    // (review M7). The decision is kept so its message can be printed after the commit.
    let last: Mutex<Option<Decision>> = Mutex::new(None);
    let decide = |locked: &Intent, locked_states: &[UnitState]| {
        let view = View {
            sweep_on: settings.interval_secs > 0,
            blocks: &blocks,
            states: locked_states,
            statuses: &statuses,
            published: &published,
            guessed_acting: &guessed_acting,
            rollback_days: settings.rollback_days,
            interval_secs: settings.interval_secs,
            now,
            disk,
            probe: probe.clone(),
        };
        let decision = rules::decide(verb, locked, &view);
        let answer = match &decision {
            Decision::Write { change, .. } => Ok(Some(change.clone())),
            Decision::NoOp(_) => Ok(None),
            Decision::Refuse(text) => Err(text.clone()),
        };
        *last.lock().unwrap_or_else(|e| e.into_inner()) = Some(decision);
        answer
    };
    let outcome = io.repo.change_intent(&decide).await?;
    let message = match last.into_inner().unwrap_or_else(|e| e.into_inner()) {
        Some(Decision::Write { message, .. } | Decision::NoOp(message)) => message,
        Some(Decision::Refuse(text)) => text,
        None => String::new(),
    };

    let written = match outcome {
        ChangeOutcome::Refused(text) => {
            lines.push(text);
            return Ok(Outcome { code: 1, output: lines.join("\n") });
        }
        ChangeOutcome::NoOp(_) => {
            lines.push(message);
            return Ok(Outcome { code: 0, output: lines.join("\n") });
        }
        ChangeOutcome::Written(written) => written,
    };
    lines.push(format!("generation {} written. {message}", written.generation));
    if cmd.no_wait {
        lines.push("not waiting (--no-wait); a server applies it on its next pass.".to_string());
        return Ok(Outcome { code: 0, output: lines.join("\n") });
    }

    let window = settings.interval_secs.saturating_mul(3).saturating_add(5);
    let deadline = now + chrono::Duration::seconds(i64::try_from(window).unwrap_or(i64::MAX));
    loop {
        let (_, seen) = io.repo.control().await?;
        if seen.applied_generation >= written.generation {
            lines.push(format!(
                "applied by the pass at {}.",
                seen.seen_at.map_or_else(|| "an unknown time".to_string(), when)
            ));
            for status in parse_statuses(seen.status.as_ref()) {
                lines.push(format!("unit {}: {}", status.unit, phase_name(status.phase)));
            }
            return Ok(Outcome { code: 0, output: lines.join("\n") });
        }
        if (io.now)() >= deadline {
            let last_pass = seen.seen_at.map_or_else(|| "never".to_string(), when);
            lines.push(format!(
                "generation {} is written, and no server applied it within {window} s (last \
                 pass: {last_pass}). The intent stays in embedding_control and applies on a \
                 server's next pass. With the server up: docker compose exec server \
                 lumberroom-server embeddings status. With it down: docker compose up -d server, \
                 or read the row with docker compose run --rm -T server lumberroom-server \
                 embeddings status.",
                written.generation
            ));
            return Ok(Outcome { code: 3, output: lines.join("\n") });
        }
        tokio::time::sleep(io.poll).await;
    }
}

/// One embedding from a remote start target. None when no probe applies: no target, or a target
/// that is not remote.
async fn run_probe(
    settings: &Settings,
    io: &Io<'_>,
    blocks: &[String],
    states: &[UnitState],
) -> Option<std::result::Result<(), String>> {
    let target = rules::start_target(blocks, states)?;
    let (spec, _) = settings.blocks.iter().find(|(spec, _)| id_for(spec) == target)?;
    if spec.provider != EmbedProvider::Openai {
        return None;
    }
    let embedder = (io.probe)(spec)?;
    Some(match embedder.embed_documents(vec![PROBE_TEXT.to_string()]).await {
        Ok(vectors) if vectors.len() == 1 && vectors[0].len() == spec.dim => Ok(()),
        Ok(vectors) => Err(format!(
            "it answered {} vectors of width {}, where one of width {} (EMBED_DIM) was expected",
            vectors.len(),
            vectors.first().map_or(0, Vec::len),
            spec.dim
        )),
        Err(e) => Err(e.log_message()),
    })
}

fn when(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Steady => "steady",
        Phase::Filling => "filling",
        Phase::Held => "held",
        Phase::Ready => "ready",
        Phase::Flipped => "flipped",
        Phase::Retiring => "retiring",
        Phase::Blocked => "blocked",
    }
}

fn blocked_text(b: &Blocked) -> String {
    match b {
        Blocked::OtherHoldsThird(m) => format!("other_holds_third ({m})"),
        Blocked::Kek => "kek".to_string(),
        Blocked::Failed(n) => format!("failed ({n} rows)"),
        Blocked::Guessed(keys) => format!("guessed ({})", keys.join(", ")),
    }
}

#[allow(clippy::too_many_arguments)]
fn status_text(
    intent: &Intent,
    published: &Published,
    states: &[UnitState],
    statuses: &[UnitStatus],
    blocks: &[String],
    thresholds: &[SimilarityThresholds],
    settings: &Settings,
    now: DateTime<Utc>,
) -> String {
    let none = || "none".to_string();
    let mut out = vec!["mode: command".to_string()];
    out.push(match intent.verb {
        None => format!("intent: none (generation {})", intent.generation),
        Some(verb) => format!(
            "intent: {} at generation {}, {}; target {}, flip {}, retire {}",
            verb.as_str(),
            intent.generation,
            intent.requested_at.map_or_else(|| "time unknown".to_string(), when),
            intent.target.clone().unwrap_or_else(none),
            if intent.flip { "on" } else { "off" },
            intent.retire.clone().unwrap_or_else(none),
        ),
    });

    let summary = published.status.as_ref().and_then(|s| s.get("summary"));
    match published.seen_at {
        None => out.push("server: no pass published yet".to_string()),
        Some(seen) => {
            let rate = summary
                .and_then(|s| s.get("rate_rows_per_min"))
                .and_then(serde_json::Value::as_f64)
                .map_or_else(|| "unknown".to_string(), |r| format!("{r:.1}"));
            let disk = summary
                .and_then(|s| s.get("disk"))
                .and_then(|d| serde_json::from_value::<DiskWire>(d.clone()).ok())
                .map_or_else(
                    || "disk floor off".to_string(),
                    |d| {
                        format!(
                            "disk {} bytes free, floor {} bytes{}",
                            d.free_bytes,
                            d.floor_bytes,
                            if d.paused { ", paused: disk_floor" } else { "" }
                        )
                    },
                );
            out.push(format!(
                "server: last pass {}, applied generation {}, models [{}], rate {rate} rows/min, \
                 {disk}",
                when(seen),
                published.applied_generation,
                published.server_models.join(", ")
            ));
        }
    }
    match rules::drift(blocks, published, settings.interval_secs, now) {
        Ok(None) => {}
        Ok(Some(warning)) => out.push(warning),
        Err(_) => out.push(format!(
            "warning: the server's last pass ran [{}], and this command computes [{}]. If .env \
             changed, recreate the server (docker compose up -d server).",
            published.server_models.join(", "),
            blocks.join(", ")
        )),
    }

    for state in states {
        let Some(s) = statuses.iter().find(|s| s.unit == state.unit) else {
            out.push(format!(
                "unit {}: no status published; active slot {} on {}, other {}",
                state.unit,
                state.active_slot.as_str(),
                state.active_model(),
                state.other_model().unwrap_or("none")
            ));
            continue;
        };
        let mut line = format!(
            "unit {}: {}, active slot {} on {}, other {}, pending {} of {} eligible, failed {}, \
             retire_after {}",
            s.unit,
            phase_name(s.phase),
            s.active_slot.as_str(),
            s.active,
            s.other.clone().unwrap_or_else(none),
            s.counts.other_pending,
            s.counts.eligible,
            s.counts.failed,
            s.retire_after.map_or_else(none, when),
        );
        if let Some(b) = &s.blocked {
            line.push_str(&format!(", blocked: {}", blocked_text(b)));
        }
        out.push(line);
    }
    let rollback = summary.and_then(|s| s.get("rollback")).and_then(serde_json::Value::as_str);
    out.push(format!("rollback: {}", rollback.unwrap_or("unknown")));
    if let Some(error) = summary.and_then(|s| s.get("error")).and_then(serde_json::Value::as_str) {
        out.push(format!("error: {error}"));
    }
    for t in thresholds {
        out.push(format!("thresholds {}:", t.model));
        for (key, r) in &t.values {
            let source = serde_json::to_value(r.source)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            out.push(format!("  {key} {} ({source})", r.value));
        }
    }
    out.push(format!("next: {}", next_step(intent, statuses)));
    out.join("\n")
}

fn next_step(intent: &Intent, statuses: &[UnitStatus]) -> &'static str {
    let any = |p: Phase| statuses.iter().any(|s| s.phase == p);
    if statuses.is_empty() {
        "start the server and wait one pass; no status is published yet"
    } else if any(Phase::Blocked) {
        "clear the blocked reason above; the sweep retries every pass"
    } else if any(Phase::Filling) {
        "wait for the fill; run embeddings status until every unit is held"
    } else if any(Phase::Held) && !intent.flip {
        "embeddings flip"
    } else if any(Phase::Held) || any(Phase::Ready) {
        "the server flips each unit on its next pass"
    } else if any(Phase::Retiring) {
        "wait; the server deletes the retired vectors once each rollback window closes"
    } else if any(Phase::Flipped) {
        "check retrieval; embeddings rollback if it is worse, embeddings retire once the window closes"
    } else {
        "nothing to do. To switch models, add the new block to .env and run embeddings start"
    }
}

/// `published.status["units"]`. The domain types serialize only, so these mirrors read them back;
/// a missing or unparsable value is an empty list.
fn parse_statuses(status: Option<&serde_json::Value>) -> Vec<UnitStatus> {
    let Some(units) = status.and_then(|s| s.get("units")) else { return Vec::new() };
    serde_json::from_value::<Vec<UnitStatusWire>>(units.clone())
        .map(|list| list.into_iter().map(UnitStatus::from).collect())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct DiskWire {
    free_bytes: u64,
    floor_bytes: u64,
    paused: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PhaseWire {
    Steady,
    Filling,
    Held,
    Ready,
    Flipped,
    Retiring,
    Blocked,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
enum BlockedWire {
    OtherHoldsThird(String),
    Kek,
    Failed(i64),
    Guessed(Vec<String>),
}

#[derive(Deserialize)]
struct CountsWire {
    eligible: i64,
    other_pending: i64,
    active_holes: i64,
    foreign_model: i64,
    failed: i64,
    without_vector: i64,
    retire_pending: i64,
}

#[derive(Deserialize)]
struct UnitStatusWire {
    unit: String,
    phase: PhaseWire,
    active_slot: VectorSlot,
    active: String,
    other: Option<String>,
    #[serde(flatten)]
    counts: CountsWire,
    failed_ids: Vec<uuid::Uuid>,
    blocked: Option<BlockedWire>,
    flipped_at: Option<DateTime<Utc>>,
    retire_after: Option<DateTime<Utc>>,
}

impl From<UnitStatusWire> for UnitStatus {
    fn from(w: UnitStatusWire) -> Self {
        let c = w.counts;
        UnitStatus {
            unit: w.unit,
            phase: match w.phase {
                PhaseWire::Steady => Phase::Steady,
                PhaseWire::Filling => Phase::Filling,
                PhaseWire::Held => Phase::Held,
                PhaseWire::Ready => Phase::Ready,
                PhaseWire::Flipped => Phase::Flipped,
                PhaseWire::Retiring => Phase::Retiring,
                PhaseWire::Blocked => Phase::Blocked,
            },
            active_slot: w.active_slot,
            active: w.active,
            other: w.other,
            counts: Counts {
                eligible: c.eligible,
                other_pending: c.other_pending,
                active_holes: c.active_holes,
                foreign_model: c.foreign_model,
                failed: c.failed,
                without_vector: c.without_vector,
                retire_pending: c.retire_pending,
            },
            failed_ids: w.failed_ids,
            blocked: w.blocked.map(|b| match b {
                BlockedWire::OtherHoldsThird(m) => Blocked::OtherHoldsThird(m),
                BlockedWire::Kek => Blocked::Kek,
                BlockedWire::Failed(n) => Blocked::Failed(n),
                BlockedWire::Guessed(keys) => Blocked::Guessed(keys),
            }),
            flipped_at: w.flipped_at,
            retire_after: w.retire_after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteEmbedConfig;
    use crate::domain::embedding_migration::{
        Counts, FlipOutcome, FlipRequest, IntentChange, PendingRow,
    };
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    const BGE: &str = "BAAI/bge-base-en-v1.5";
    const GEMMA: &str = "google/embeddinggemma-2";

    fn spec(model: &str) -> EmbedderSpec {
        EmbedderSpec {
            provider: EmbedProvider::Openai,
            model: model.into(),
            dim: 4,
            cache_dir: "/nonexistent".into(),
            remote: RemoteEmbedConfig {
                base_url: "http://127.0.0.1:1/v1".into(),
                timeout_secs: 1,
                ..Default::default()
            },
        }
    }

    fn id(model: &str) -> String {
        id_for(&spec(model))
    }

    /// Gemma as `EMBED_*`, bge as `EMBED_PREVIOUS_*`, the sweep every 30 s.
    fn settings() -> Settings {
        Settings {
            control: ControlMode::Command,
            interval_secs: 30,
            rollback_days: 7,
            floor_bytes: 0,
            blocks: vec![(spec(GEMMA), vec![]), (spec(BGE), vec![])],
            legacy: vec![],
        }
    }

    fn on(unit: &str, model: &str, other: Option<&str>) -> UnitState {
        UnitState {
            unit: unit.into(),
            active_slot: VectorSlot::A,
            model_a: Some(model.into()),
            model_b: other.map(str::to_string),
            flipped_at: None,
        }
    }

    fn status_json(state: &UnitState, phase: Phase) -> serde_json::Value {
        serde_json::to_value(UnitStatus {
            unit: state.unit.clone(),
            phase,
            active_slot: state.active_slot,
            active: state.active_model().to_string(),
            other: state.other_model().map(str::to_string),
            counts: Counts { eligible: 300, ..Counts::default() },
            failed_ids: vec![],
            blocked: None,
            flipped_at: None,
            retire_after: None,
        })
        .unwrap()
    }

    fn real_now() -> DateTime<Utc> {
        Utc::now()
    }

    /// In-memory control row and states. `change_intent` mirrors the adapter's contract: decide
    /// against the row and the states it holds, then write `generation + 1`.
    struct FakeRepo {
        intent: Mutex<Intent>,
        published: Mutex<Published>,
        states: Vec<UnitState>,
        /// What the lock re-read returns; `states` when None.
        locked_states: Option<Vec<UnitState>>,
        /// The `control()` call after which a server applies the written generation; None never.
        applies_after: Option<usize>,
        control_reads: AtomicUsize,
        change_calls: AtomicUsize,
    }

    impl FakeRepo {
        fn new(states: Vec<UnitState>) -> Self {
            let published = Published {
                applied_generation: 0,
                server_models: vec![id(GEMMA), id(BGE)],
                status: Some(serde_json::json!({
                    "summary": { "rollback": "unavailable" },
                    "units": states.iter().map(|s| status_json(s, Phase::Steady)).collect::<Vec<_>>(),
                })),
                seen_at: Some(Utc::now()),
            };
            FakeRepo {
                intent: Mutex::new(Intent::default()),
                published: Mutex::new(published),
                states,
                locked_states: None,
                applies_after: None,
                control_reads: AtomicUsize::new(0),
                change_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingMigrationRepository for FakeRepo {
        async fn seed(&self) -> Result<Vec<UnitState>> {
            unreachable!("the command never seeds")
        }
        async fn states(&self) -> Result<Vec<UnitState>> {
            Ok(self.states.clone())
        }
        async fn state(&self, _unit: &str) -> Result<Option<UnitState>> {
            unreachable!("the command reads every state")
        }
        async fn name_slot(&self, _: &str, _: VectorSlot, _: &str) -> Result<bool> {
            unreachable!("only the sweep names slots")
        }
        async fn counts(
            &self,
            _: &str,
            _: &UnitState,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Counts> {
            unreachable!("the command reads counts from the published status")
        }
        async fn next_batch(
            &self,
            _: &str,
            _: VectorSlot,
            _: &str,
            _: Option<uuid::Uuid>,
            _: i64,
        ) -> Result<Vec<PendingRow>> {
            unreachable!("only the sweep fills")
        }
        async fn store(
            &self,
            _: &str,
            _: uuid::Uuid,
            _: VectorSlot,
            _: &str,
            _: Vec<f32>,
        ) -> Result<bool> {
            unreachable!("only the sweep stores")
        }
        async fn flip(&self, _: &FlipRequest) -> Result<FlipOutcome> {
            unreachable!("only the sweep flips")
        }
        async fn retire_batch(&self, _: &str, _: VectorSlot, _: &str, _: i64) -> Result<u64> {
            unreachable!("only the sweep retires")
        }
        async fn clear_slot(&self, _: &str, _: VectorSlot, _: &str) -> Result<bool> {
            unreachable!("only the sweep clears slots")
        }
        async fn control(&self) -> Result<(Intent, Published)> {
            let reads = self.control_reads.fetch_add(1, Ordering::SeqCst) + 1;
            let intent = self.intent.lock().unwrap().clone();
            let mut published = self.published.lock().unwrap();
            if self.applies_after.is_some_and(|n| reads > n) {
                published.applied_generation = intent.generation;
            }
            Ok((intent, published.clone()))
        }
        async fn generation(&self) -> Result<i64> {
            Ok(self.intent.lock().unwrap().generation)
        }
        async fn change_intent(
            &self,
            decide: &(dyn for<'i, 's> Fn(
                &'i Intent,
                &'s [UnitState],
            ) -> std::result::Result<Option<IntentChange>, String>
                  + Send
                  + Sync),
        ) -> Result<ChangeOutcome> {
            self.change_calls.fetch_add(1, Ordering::SeqCst);
            let mut intent = self.intent.lock().unwrap();
            let states = self.locked_states.as_ref().unwrap_or(&self.states);
            Ok(match decide(&intent, states) {
                Err(text) => ChangeOutcome::Refused(text),
                Ok(None) => ChangeOutcome::NoOp(intent.clone()),
                Ok(Some(c)) => {
                    *intent = Intent {
                        generation: intent.generation + 1,
                        target: c.target,
                        flip: c.flip,
                        retire: c.retire,
                        verb: Some(c.verb),
                        requested_at: Some(Utc::now()),
                    };
                    ChangeOutcome::Written(intent.clone())
                }
            })
        }
        async fn publish(&self, _: &Published) -> Result<()> {
            unreachable!("only the sweep publishes")
        }
    }

    struct FakeEmbedder {
        answer: std::result::Result<usize, String>,
    }

    #[async_trait::async_trait]
    impl Embedder for FakeEmbedder {
        fn id(&self) -> String {
            id(GEMMA)
        }
        fn dim(&self) -> usize {
            4
        }
        async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
            assert_eq!(texts, vec![PROBE_TEXT.to_string()]);
            match &self.answer {
                Ok(width) => Ok(vec![vec![0.5; *width]]),
                Err(e) => Err(DomainError::unavailable(e.clone())),
            }
        }
        async fn embed_query(&self, _text: &str) -> Result<Vec<f32>> {
            unreachable!("the probe embeds a document")
        }
    }

    fn no_probe(_: &EmbedderSpec) -> Option<Arc<dyn Embedder>> {
        Some(Arc::new(FakeEmbedder { answer: Ok(4) }))
    }

    async fn exec_with(
        args: &[&str],
        settings: &Settings,
        repo: &FakeRepo,
        probe: &ProbeFn,
        now: fn() -> DateTime<Utc>,
    ) -> Outcome {
        let registry = Registry::engine();
        let io = Io {
            repo,
            registry: &registry,
            probe,
            disk: None,
            now,
            poll: std::time::Duration::from_millis(1),
        };
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        execute_with(&args, settings, &io).await.unwrap()
    }

    async fn exec(args: &[&str], repo: &FakeRepo) -> Outcome {
        exec_with(args, &settings(), repo, &no_probe, real_now).await
    }

    #[tokio::test]
    async fn env_mode_exits_two_for_every_verb() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let mut s = settings();
        s.control = ControlMode::Env;
        for verb in ["status", "start", "flip", "rollback", "retire"] {
            let out = exec_with(&[verb], &s, &repo, &no_probe, real_now).await;
            assert_eq!(out.code, 2, "{verb}");
            assert!(out.output.contains("EMBED_MIGRATION_CONTROL=env"), "{}", out.output);
        }
        assert_eq!(repo.control_reads.load(Ordering::SeqCst), 0);
        assert_eq!(repo.change_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unknown_flag_exits_two() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        for args in [
            &["start", "--force"][..],
            &["start", "--json"],
            &["status", "--no-wait"],
            &["status", "--json", "--json"],
            &["switch"],
            &[],
        ] {
            let out = exec(args, &repo).await;
            assert_eq!(out.code, 2, "{args:?}");
            assert!(out.output.contains("usage: lumberroom-server embeddings"), "{}", out.output);
        }
        assert_eq!(repo.change_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_write_waits_for_the_applied_generation() {
        let mut repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        // The first read is the one before the decision; the server applies on the third.
        repo.applies_after = Some(2);
        let out = exec(&["start"], &repo).await;
        assert_eq!(out.code, 0, "{}", out.output);
        assert!(out.output.contains("generation 1 written"), "{}", out.output);
        assert!(out.output.contains("unit me: steady"), "{}", out.output);
        assert_eq!(repo.control_reads.load(Ordering::SeqCst), 3);
        assert_eq!(repo.intent.lock().unwrap().target, Some(id(GEMMA)));
    }

    #[tokio::test]
    async fn a_write_with_no_wait_returns_at_once() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let out = exec(&["start", "--no-wait"], &repo).await;
        assert_eq!(out.code, 0, "{}", out.output);
        assert_eq!(repo.control_reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_write_nobody_applies_exits_three() {
        // Each read of the clock moves it 10 s, so the 95 s wait (3 x 30 s + 5 s) runs out fast.
        fn ticking() -> DateTime<Utc> {
            static TICKS: AtomicI64 = AtomicI64::new(0);
            let t = TICKS.fetch_add(10, Ordering::SeqCst);
            "2026-10-20T12:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::Duration::seconds(t)
        }
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let out = exec_with(&["start"], &settings(), &repo, &no_probe, ticking).await;
        assert_eq!(out.code, 3, "{}", out.output);
        for part in [
            "generation 1 is written",
            "95 s",
            "docker compose exec server",
            "docker compose run --rm -T server",
        ] {
            assert!(out.output.contains(part), "{part:?} missing from: {}", out.output);
        }
    }

    #[tokio::test]
    async fn a_refusal_exits_one_and_writes_nothing() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let out = exec_with(&["flip"], &settings(), &repo, &no_probe, real_now).await;
        assert_eq!(out.code, 1, "{}", out.output);
        assert_eq!(repo.intent.lock().unwrap().generation, 0);
    }

    #[tokio::test]
    async fn a_noop_exits_zero_and_writes_nothing() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let out = exec(&["rollback"], &repo).await;
        assert_eq!(out.code, 0, "{}", out.output);
        assert_eq!(repo.intent.lock().unwrap().generation, 0);
    }

    #[tokio::test]
    async fn the_decision_reads_the_states_the_lock_returns() {
        // Before the lock the unit looks ready to start; under it a third model has appeared.
        let mut repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        repo.locked_states = Some(vec![on("me", &id(BGE), Some("openai:some/other-model"))]);
        let out = exec(&["start", "--no-wait"], &repo).await;
        assert_eq!(out.code, 1, "{}", out.output);
        assert!(out.output.contains("openai:some/other-model"), "{}", out.output);
        assert_eq!(repo.intent.lock().unwrap().generation, 0);
    }

    #[tokio::test]
    async fn a_drifted_server_refuses_before_the_lock() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        repo.published.lock().unwrap().server_models = vec![id(BGE)];
        let out = exec(&["start"], &repo).await;
        assert_eq!(out.code, 1, "{}", out.output);
        assert!(out.output.contains("docker compose up -d server"), "{}", out.output);
        assert_eq!(repo.change_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn start_refuses_a_remote_target_whose_probe_fails() {
        fn failing(_: &EmbedderSpec) -> Option<Arc<dyn Embedder>> {
            Some(Arc::new(FakeEmbedder { answer: Err("connection refused".into()) }))
        }
        fn narrow(_: &EmbedderSpec) -> Option<Arc<dyn Embedder>> {
            Some(Arc::new(FakeEmbedder { answer: Ok(3) }))
        }
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let out = exec_with(&["start"], &settings(), &repo, &failing, real_now).await;
        assert_eq!(out.code, 1, "{}", out.output);
        assert!(out.output.contains("connection refused"), "{}", out.output);
        let out = exec_with(&["start"], &settings(), &repo, &narrow, real_now).await;
        assert_eq!(out.code, 1, "{}", out.output);
        assert!(out.output.contains("width 3"), "{}", out.output);
        assert_eq!(repo.intent.lock().unwrap().generation, 0);
    }

    #[tokio::test]
    async fn status_json_carries_intent_published_and_states() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let out = exec(&["status", "--json"], &repo).await;
        assert_eq!(out.code, 0);
        let doc: serde_json::Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(doc["intent"]["generation"], 0);
        assert_eq!(doc["published"]["server_models"][0], id(GEMMA));
        assert_eq!(doc["states"][0]["unit"], "me");
        assert_eq!(doc["states"][0]["model_a"], id(BGE));
    }

    #[tokio::test]
    async fn status_text_names_units_thresholds_and_the_next_step() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        let out = exec(&["status"], &repo).await;
        assert_eq!(out.code, 0);
        for part in [
            "mode: command",
            "intent: none (generation 0)",
            "unit me: steady, active slot a on openai:BAAI/bge-base-en-v1.5",
            "rollback: unavailable",
            "thresholds openai:google/embeddinggemma-2:",
            "  dedupe 0.995 (study)",
            "  route_max_top 0.65 (carried)",
            "thresholds openai:BAAI/bge-base-en-v1.5:",
            "  dedupe 0.97 (shipped)",
            "next: nothing to do",
        ] {
            assert!(out.output.contains(part), "{part:?} missing from:\n{}", out.output);
        }
    }

    #[tokio::test]
    async fn status_warns_when_no_pass_ran_recently() {
        let repo = FakeRepo::new(vec![on("me", &id(BGE), None)]);
        repo.published.lock().unwrap().seen_at = None;
        let out = exec(&["status"], &repo).await;
        assert_eq!(out.code, 0);
        assert!(out.output.contains("no embedding pass ran"), "{}", out.output);
    }

    #[test]
    fn published_units_parse_back_into_unit_statuses() {
        let full = UnitStatus {
            unit: "me".into(),
            phase: Phase::Blocked,
            active_slot: VectorSlot::B,
            active: id(GEMMA),
            other: Some(id(BGE)),
            counts: Counts {
                eligible: 12_000,
                other_pending: 1,
                active_holes: 2,
                foreign_model: 3,
                failed: 4,
                without_vector: 5,
                retire_pending: 6,
            },
            failed_ids: vec![uuid::Uuid::nil()],
            blocked: Some(Blocked::Guessed(vec!["dedupe".into()])),
            flipped_at: Some("2026-10-12T09:31:00Z".parse().unwrap()),
            retire_after: Some("2026-10-19T09:31:00Z".parse().unwrap()),
        };
        for blocked in [
            Some(Blocked::OtherHoldsThird("x".into())),
            Some(Blocked::Kek),
            Some(Blocked::Failed(7)),
            full.blocked.clone(),
            None,
        ] {
            let unit = UnitStatus { blocked, ..full.clone() };
            let doc = serde_json::json!({ "units": [serde_json::to_value(&unit).unwrap()] });
            assert_eq!(parse_statuses(Some(&doc)), vec![unit]);
        }
        assert!(parse_statuses(None).is_empty());
        assert!(parse_statuses(Some(&serde_json::json!({ "units": "garbage" }))).is_empty());
    }
}
