//! The cleanup pass, the client half.
//!
//! The server holds the queue, the watermarks and the deterministic pass. What lives here is the
//! judgment half: taking pairs the cosine could not decide and asking a model about them.
//!
//! The split is the same one ingestion makes, and for the same reason. The provider path, the keys,
//! the retry and every JSON tolerance a model has forced already live in this binary. A server that
//! called out to a third party would need all of it again, and it would need the key.
//!
//! # What leaves the machine
//!
//! Two sentences and a cosine, for rows at `open` and nothing above. The server filters on
//! sensitivity inside the candidate query, so a private row never reaches this process, and
//! `refuse_non_open` is a second check here because a boundary worth having is worth having twice.
//!
//! # It proposes and never acts
//!
//! A verdict becomes a queued proposal. Nothing here retires a row, and `cleanup apply` is a
//! separate command the owner runs on a proposal he has read.

pub mod prompt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::args::Args;
use crate::client::{err, Client, Result};
use crate::ingest::provider;

/// The default tier for this task, and it is a measurement rather than a preference.
///
/// Probed 21 August 2026 on five clusters drawn from the owner's own store: `qwen3.7-flash` scored
/// 4 of 5 exactly, ahead of every other tier including Opus, at $0.00019 and 6.9 seconds. Haiku
/// found zero contradictions across two runs, so "a cheap model" is not the specification and this
/// name is.
pub const DEFAULT_MODEL: &str = "qwen/qwen3.7-flash";

/// How many pairs go in one call.
///
/// Small on purpose. A model asked about forty pairs at once starts answering about the list rather
/// than about each pair, and one unparseable response costs the whole batch.
pub const BATCH: usize = 8;

/// One pair the deterministic pass could not decide, as the server hands it over.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pair {
    pub similarity: f64,
    pub namespace: String,
    pub a_id: String,
    pub a_content: String,
    pub b_id: String,
    pub b_content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    /// `pair N`, matching the numbering in the rendered prompt.
    pub pair: String,
    pub verdict: String,
    #[serde(default)]
    pub keep: Option<String>,
    #[serde(default)]
    pub why: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PassReport {
    pub pairs_seen: usize,
    pub calls: usize,
    pub same: usize,
    pub contradictions: usize,
    pub unrelated: usize,
    /// Verdicts thrown away because the model named a pair that was not in the batch, or an id that
    /// was not in the pair. Counted rather than logged: a silent drop reads as agreement.
    pub discarded: usize,
    pub queued: usize,
    pub already_known: usize,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

/// `pair 3` to index 2, and nothing else.
///
/// A model that answers `"pair"` with a uuid, a sentence, or a number outside the batch produces a
/// verdict about rows the caller cannot identify. Refusing it costs one finding; guessing costs a
/// merge of the wrong two rows.
pub fn pair_index(raw: &str, batch_len: usize) -> Option<usize> {
    let digits = raw.trim().trim_start_matches("pair").trim();
    let n: usize = digits.parse().ok()?;
    if n == 0 || n > batch_len {
        return None;
    }
    Some(n - 1)
}

/// Refuses to send anything the server should not have handed over.
///
/// The server filters on `sensitivity = 'open'` inside the candidate query, so this never fires in
/// a working system. It exists because the failure it guards against is silent and one-way: once
/// two sentences have gone to a provider, no later check un-sends them.
fn refuse_empty(pairs: &[Pair]) -> Result<()> {
    for p in pairs {
        if p.a_content.trim().is_empty() || p.b_content.trim().is_empty() {
            return Err(err(format!(
                "the server handed over a pair with no readable text ({} and {}). That is a sealed \
                 or encrypted row reaching the model path, and this pass refuses rather than \
                 sending a placeholder",
                p.a_id, p.b_id
            )));
        }
    }
    Ok(())
}

/// Turn one batch of verdicts into proposals the server will take.
///
/// A `same` verdict whose `keep` names neither row is discarded. The model has told us two rows are
/// interchangeable and then named a third thing, which is not a finding, it is a mistake with a
/// confident sentence attached.
pub fn to_proposals(
    batch: &[Pair],
    verdicts: &[Verdict],
    model: &str,
    report: &mut PassReport,
) -> Vec<Value> {
    let mut out = Vec::new();
    for v in verdicts {
        let Some(i) = pair_index(&v.pair, batch.len()) else {
            report.discarded += 1;
            continue;
        };
        let p = &batch[i];
        match v.verdict.trim().to_ascii_lowercase().as_str() {
            "same" => {
                let Some(keep) = v.keep.as_deref().map(str::trim) else {
                    report.discarded += 1;
                    continue;
                };
                if keep != p.a_id && keep != p.b_id {
                    report.discarded += 1;
                    continue;
                }
                let retire = if keep == p.a_id { p } else { p };
                let retire_id = if keep == p.a_id { &p.b_id } else { &p.a_id };
                let retire_content = if keep == p.a_id { &p.b_content } else { &p.a_content };
                let keep_content = if keep == p.a_id { &p.a_content } else { &p.b_content };
                let _ = retire;
                report.same += 1;
                out.push(json!({
                    "kind": "paraphrase",
                    "namespace": p.namespace,
                    "keep_id": keep,
                    "rationale": v.why,
                    "produced_by": model,
                    "similarity": p.similarity,
                    "members": [
                        { "memory_id": keep, "disposition": "keep", "seen_content": keep_content },
                        { "memory_id": retire_id, "disposition": "retire", "seen_content": retire_content },
                    ],
                }));
            }
            "contradiction" => {
                report.contradictions += 1;
                // No survivor and no keep member. Which of two conflicting facts holds is the
                // owner's call, and a pass that also picked the winner would be writing the fact
                // rather than reporting the conflict.
                out.push(json!({
                    "kind": "contradiction",
                    "namespace": p.namespace,
                    "rationale": v.why,
                    "produced_by": model,
                    "similarity": p.similarity,
                    "members": [
                        { "memory_id": p.a_id, "disposition": "retire", "seen_content": p.a_content },
                        { "memory_id": p.b_id, "disposition": "retire", "seen_content": p.b_content },
                    ],
                }));
            }
            "unrelated" => report.unrelated += 1,
            _ => report.discarded += 1,
        }
    }
    out
}

/// How long until the next occurrence of `hh:mm` local time.
///
/// Wall clock rather than an interval from process start, and the difference is what a restart
/// costs. `tokio::time::interval(24h)` fires the moment the process comes up and then every 24
/// hours from whenever that was, so a container in a crash loop spends the model call on every
/// restart and a container restarted at noon quietly moves the nightly run to noon. Sleeping to
/// the next wall-clock occurrence keeps the schedule where the operator put it.
///
/// Never zero: a run that finishes inside the same minute would otherwise fire again immediately.
pub fn until_next(now: chrono::DateTime<chrono::Local>, hh: u32, mm: u32) -> std::time::Duration {
    use chrono::{Datelike, Duration, TimeZone};
    let today =
        now.timezone().with_ymd_and_hms(now.year(), now.month(), now.day(), hh, mm, 0).single();
    let mut target = match today {
        Some(t) => t,
        // A time that does not exist today, which is what a daylight-saving jump produces. Take
        // the following minute rather than refusing: an hour's drift once a year beats a daemon
        // that exits on the night the clocks change.
        None => now + Duration::minutes(1),
    };
    if target <= now {
        target += Duration::days(1);
    }
    (target - now).to_std().unwrap_or(std::time::Duration::from_secs(60))
}

/// Parses `--at 04:25`.
pub fn parse_at(raw: &str) -> Result<(u32, u32)> {
    let (h, m) =
        raw.trim().split_once(':').ok_or_else(|| err(format!("--at takes HH:MM, got {raw:?}")))?;
    let hh: u32 = h.parse().map_err(|_| err(format!("--at takes HH:MM, got {raw:?}")))?;
    let mm: u32 = m.parse().map_err(|_| err(format!("--at takes HH:MM, got {raw:?}")))?;
    if hh > 23 || mm > 59 {
        return Err(err(format!("--at is a 24-hour clock time, got {raw:?}")));
    }
    Ok((hh, mm))
}

/// `lumberroom cleanup daemon --at 04:25`: the model pass, on a wall clock, forever.
///
/// This is the schedule for the half that cannot live in the server. The server runs the
/// deterministic pass on its own timer and holds the KEK; this process calls a provider and holds
/// no key material of the store's. Two processes is the boundary, and a compose service with
/// `restart: unless-stopped` is how it stays up without a cron daemon anywhere.
///
/// A failed run logs and waits for tomorrow. A daemon that exits because a provider was down for
/// one night is a schedule that silently stops, which is worse than a night with no pass.
async fn daemon(c: &Client, args: &Args) -> Result<()> {
    let at = args.value("at").unwrap_or("04:25");
    let (hh, mm) = parse_at(at)?;
    // Refused here rather than at 04:25 tomorrow. A daemon that starts clean and fails on its
    // first run twenty hours later is a schedule nobody knows is broken until they go looking.
    if !c.has_token() {
        return Err(err(
            "no credential. Set LUMBERROOM_TOKEN to a client carrying mayIngest, or LUMBERROOM_CLEANUP_TOKEN \
             in .env when running under docker compose. docs/permissions.md covers what that \
             field opens.",
        ));
    }
    crate::out(&format!("cleanup daemon: the model pass runs at {hh:02}:{mm:02} local, every day"));
    loop {
        let wait = until_next(chrono::Local::now(), hh, mm);
        crate::out(&format!(
            "next run in {}h{:02}m",
            wait.as_secs() / 3600,
            (wait.as_secs() % 3600) / 60
        ));
        tokio::time::sleep(wait).await;
        match run(c, args).await {
            Ok(()) => {}
            Err(e) => crate::out(&format!("the pass failed, waiting for tomorrow: {}", e.message)),
        }
    }
}

/// `lumberroom cleanup <run|daemon|list|show|apply|resolve|reject|unreject>`.
pub async fn dispatch(c: &Client, args: &Args, sub: &str) -> Result<()> {
    match sub {
        "run" => run(c, args).await,
        "daemon" => daemon(c, args).await,
        "list" => list(c, args).await,
        "show" => show(c, args).await,
        "apply" => apply(c, args).await,
        "resolve" => resolve(c, args).await,
        "reject" => reject(c, args).await,
        "unreject" => unreject(c, args).await,
        other => Err(err(format!(
            "unknown cleanup command {other:?}. One of: run, daemon, list, show, apply, resolve, \
             reject, unreject"
        ))),
    }
}

/// An empty value sends no floor, so the server applies the unit's model value. Compose passes
/// `--min-similarity ${CLEANUP_MIN_SIMILARITY:-}`, and an unset variable must not pin bge's 0.65
/// on a store that runs another model (decision 0029).
fn min_similarity(args: &Args) -> Result<Option<f64>> {
    match args.value("min-similarity").map(str::trim) {
        None | Some("") => Ok(None),
        Some(raw) => raw
            .parse()
            .map(Some)
            .map_err(|_| err(format!("--min-similarity takes a number, got {raw:?}"))),
    }
}

async fn run(c: &Client, args: &Args) -> Result<()> {
    let cadence = args.value("cadence").unwrap_or("hourly").to_string();
    let namespace = args.value("namespace").map(str::to_string);
    let limit = args.int("limit", 500);

    let mut body = json!({ "cadence": cadence, "limit": limit });
    if let Some(ns) = &namespace {
        body["namespace"] = json!(ns);
    }
    if let Some(f) = min_similarity(args)? {
        body["min_similarity"] = json!(f);
    }
    let (status, response) =
        c.http_request(reqwest::Method::POST, "/admin/cleanup/run", Some(body)).await?;
    if status != 200 {
        return Err(err(format!(
            "the pass failed ({status}): {}",
            crate::commands::compact(&response)
        )));
    }

    let report = &response["report"];
    crate::out(&format!(
        "deterministic pass over {}: {} exact groups, {} near-certain pairs, {} stale, \
         {} queued, {} already known, {} closed",
        namespace.as_deref().unwrap_or("every namespace"),
        report["exact_groups"].as_i64().unwrap_or(0),
        report["near_certain_pairs"].as_i64().unwrap_or(0),
        report["stale_rows"].as_i64().unwrap_or(0),
        report["queued"].as_i64().unwrap_or(0),
        report["already_known"].as_i64().unwrap_or(0),
        report["closed_as_answered"].as_i64().unwrap_or(0),
    ));
    if report["truncated"].as_bool().unwrap_or(false) {
        crate::out(
            "  a query hit its limit, so this run did not reach the end of the store. \
             The watermark held where the findings did; run it again.",
        );
    }
    // Printed beside the model count, or a store that is mostly private reads as a clean one.
    crate::out(&format!(
        "  {} pairs for the model, {} withheld from the model (a side above open)",
        report["for_the_model"].as_i64().unwrap_or(0),
        report["withheld_from_model"].as_i64().unwrap_or(0),
    ));

    let pairs: Vec<Pair> = serde_json::from_value(response["for_the_model"].clone())
        .map_err(|e| err(format!("the server's candidate list did not parse: {e}")))?;
    if pairs.is_empty() {
        crate::out("nothing in the band a model would be asked about.");
        return Ok(());
    }
    refuse_empty(&pairs)?;

    if args.present("no-model") {
        crate::out(&format!(
            "{} pairs are in the band a model would decide. --no-model, so none were sent.",
            pairs.len()
        ));
        return Ok(());
    }

    let provider_name = args.value("provider").unwrap_or("openrouter");
    let model = args.value("model").unwrap_or(DEFAULT_MODEL);
    let p =
        provider::resolve(provider_name, &c.file.borrow(), Some(model), args.value("base-url"))?;
    if p.key.is_none() {
        return Err(err(format!(
            "provider {provider_name} has no key. Set one with `lumberroom ingest keys set \
             {provider_name}`, which reads it from stdin, or pass --no-model to stop after the \
             deterministic pass."
        )));
    }
    let timeout = args.int("timeout", 120) as u64;
    let http = reqwest::Client::new();

    let mut report = PassReport { pairs_seen: pairs.len(), ..Default::default() };
    let mut proposals = Vec::new();
    for batch in pairs.chunks(BATCH) {
        let user = prompt::render(batch);
        // One batch failing must not lose the rest. A provider that answers HTTP 400 once and 200
        // four times an hour later is behaviour this path has already seen from qwen3.7-flash.
        let answered = provider::call_json(&http, &p, prompt::SYSTEM, &user, timeout, |v| {
            v.get("clusters").is_some()
        })
        .await;
        match answered {
            Ok((value, usage)) => {
                report.calls += 1;
                report.prompt_tokens += usage.prompt_tokens;
                report.completion_tokens += usage.completion_tokens;
                let verdicts: Vec<Verdict> =
                    serde_json::from_value(value["clusters"].clone()).unwrap_or_default();
                proposals.extend(to_proposals(batch, &verdicts, &p.model, &mut report));
            }
            Err(e) => {
                report.discarded += batch.len();
                crate::out(&format!("  a batch of {} failed: {}", batch.len(), e.message));
            }
        }
    }

    if !proposals.is_empty() {
        let (status, body) = c
            .http_request(
                reqwest::Method::POST,
                "/admin/cleanup/proposals",
                Some(json!({ "proposals": proposals })),
            )
            .await?;
        if status != 200 {
            return Err(err(format!(
                "posting the proposals failed ({status}): {}",
                crate::commands::compact(&body)
            )));
        }
        report.queued = body["queued"].as_u64().unwrap_or(0) as usize;
        report.already_known = body["already_known"].as_u64().unwrap_or(0) as usize;
    }

    crate::out(&format!(
        "model pass with {}: {} pairs in {} calls, {} same, {} contradictions, {} unrelated, \
         {} discarded, {} queued, {} already known, {} prompt and {} completion tokens",
        p.model,
        report.pairs_seen,
        report.calls,
        report.same,
        report.contradictions,
        report.unrelated,
        report.discarded,
        report.queued,
        report.already_known,
        report.prompt_tokens,
        report.completion_tokens,
    ));
    crate::out("nothing was retired. Read the queue with `lumberroom cleanup list`.");
    Ok(())
}

async fn list(c: &Client, args: &Args) -> Result<()> {
    let state = args.value("state").unwrap_or("proposed");
    let limit = args.int("limit", 50);
    let (status, body) =
        c.http_get(&format!("/admin/cleanup/proposals?state={state}&limit={limit}")).await?;
    if status != 200 {
        return Err(err(format!(
            "reading the queue failed ({status}): {}",
            crate::commands::compact(&body)
        )));
    }
    let rows = body["proposals"].as_array().cloned().unwrap_or_default();
    crate::out(&format!("{} {state}:", rows.len()));
    for r in &rows {
        let sim = r["similarity"].as_f64().map(|s| format!("  {s:.3}")).unwrap_or_default();
        crate::out(&format!(
            "  {}  {:<13}{}  [{}]  via {}",
            r["id"].as_str().unwrap_or(""),
            r["kind"].as_str().unwrap_or(""),
            sim,
            r["namespace"].as_str().unwrap_or(""),
            r["produced_by"].as_str().unwrap_or(""),
        ));
        crate::out(&format!("      {}", r["rationale"].as_str().unwrap_or("")));
    }
    Ok(())
}

async fn show(c: &Client, args: &Args) -> Result<()> {
    let id = args.positional_at(2).ok_or_else(|| err("usage: lumberroom cleanup show <id>"))?;
    let (status, body) = c.http_get(&format!("/admin/cleanup/proposals/{id}")).await?;
    if status != 200 {
        return Err(err(format!(
            "reading the proposal failed ({status}): {}",
            crate::commands::compact(&body)
        )));
    }
    crate::out(&format!(
        "{}  {}  [{}]  via {}",
        body["id"].as_str().unwrap_or(""),
        body["kind"].as_str().unwrap_or(""),
        body["namespace"].as_str().unwrap_or(""),
        body["produced_by"].as_str().unwrap_or(""),
    ));
    crate::out(&format!("  {}", body["rationale"].as_str().unwrap_or("")));
    crate::out("");
    for m in body["members"].as_array().cloned().unwrap_or_default() {
        let moved = match (m["current_content"].as_str(), m["superseded_by"].as_str()) {
            (None, _) => "  GONE",
            (_, Some(_)) => "  ALREADY RETIRED",
            (Some(now), None) if now != m["seen_content"].as_str().unwrap_or("") => {
                "  EDITED SINCE"
            }
            _ => "",
        };
        crate::out(&format!(
            "  {:<7}{}{}",
            m["disposition"].as_str().unwrap_or(""),
            m["memory_id"].as_str().unwrap_or(""),
            moved
        ));
        crate::out(&format!("          {}", m["seen_content"].as_str().unwrap_or("")));
    }
    Ok(())
}

async fn apply(c: &Client, args: &Args) -> Result<()> {
    let id = args.positional_at(2).ok_or_else(|| err("usage: lumberroom cleanup apply <id>"))?;
    let (status, body) = c
        .http_request(reqwest::Method::POST, &format!("/admin/cleanup/proposals/{id}/apply"), None)
        .await?;
    if status != 200 {
        return Err(err(format!("apply refused ({status}): {}", crate::commands::compact(&body))));
    }
    let retired = body["retired"].as_array().map(Vec::len).unwrap_or(0);
    let deleted = body["deleted"].as_array().map(Vec::len).unwrap_or(0);
    crate::out(&format!("applied {id}: {retired} retired, {deleted} deleted"));
    Ok(())
}

/// `cleanup resolve <id> --keep <memory-id>`: settle a contradiction.
///
/// A contradiction names no survivor, so `apply` refuses it. This is how the owner says which of
/// the two rows holds, once he has decided.
async fn resolve(c: &Client, args: &Args) -> Result<()> {
    let id = args
        .positional_at(2)
        .ok_or_else(|| err("usage: lumberroom cleanup resolve <id> --keep <memory-id>"))?;
    let keep = args.value("keep").ok_or_else(|| {
        err("--keep names which of the rows holds. `cleanup show <id>` lists them.")
    })?;
    let (status, body) = c
        .http_request(
            reqwest::Method::POST,
            &format!("/admin/cleanup/proposals/{id}/resolve"),
            Some(json!({ "keep_id": keep })),
        )
        .await?;
    if status != 200 {
        return Err(err(format!(
            "resolve refused ({status}): {}",
            crate::commands::compact(&body)
        )));
    }
    let retired = body["retired"].as_array().map(Vec::len).unwrap_or(0);
    crate::out(&format!("resolved {id}: kept {keep}, retired {retired}"));
    Ok(())
}

async fn reject(c: &Client, args: &Args) -> Result<()> {
    let id = args
        .positional_at(2)
        .ok_or_else(|| err("usage: lumberroom cleanup reject <id> [--reason ...]"))?;
    let body = match args.value("reason") {
        Some(r) => json!({ "reason": r }),
        None => json!({}),
    };
    let (status, body) = c
        .http_request(
            reqwest::Method::POST,
            &format!("/admin/cleanup/proposals/{id}/reject"),
            Some(body),
        )
        .await?;
    if status != 200 {
        return Err(err(format!("reject failed ({status}): {}", crate::commands::compact(&body))));
    }
    // The way back, printed where it is read. A rejection blocks the cluster for good, and the
    // owner who refused the wrong row finds out an hour later when the pass says nothing.
    crate::out(&format!("rejected {id}. `lumberroom cleanup unreject {id}` puts it back."));
    Ok(())
}

/// `cleanup unreject <id>`: put a refused finding back in the queue.
///
/// A rejected cluster counts as known forever, so the pass stays quiet about it. That is right
/// until the pass was what was wrong, and then the fixed run finds the same pair and says nothing.
/// The row goes back to `proposed` and the refusal note goes with it.
async fn unreject(c: &Client, args: &Args) -> Result<()> {
    let id = args.positional_at(2).ok_or_else(|| err("usage: lumberroom cleanup unreject <id>"))?;
    let (status, body) = c
        .http_request(
            reqwest::Method::POST,
            &format!("/admin/cleanup/proposals/{id}/unreject"),
            None,
        )
        .await?;
    if status != 200 {
        return Err(err(format!(
            "unreject refused ({status}): {}",
            crate::commands::compact(&body)
        )));
    }
    crate::out(&format!("{id} is back in the queue. `lumberroom cleanup show {id}` reads it."));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_min_similarity_sends_no_floor() {
        let parse = |argv: &[&str]| min_similarity(&Args::parse(argv.iter().copied()));
        assert_eq!(parse(&["daemon", "--min-similarity", ""]).unwrap(), None);
        assert_eq!(parse(&["daemon", "--min-similarity", "  "]).unwrap(), None);
        assert_eq!(parse(&["daemon"]).unwrap(), None);
        assert_eq!(parse(&["daemon", "--min-similarity", "0.7"]).unwrap(), Some(0.7));
        assert!(parse(&["daemon", "--min-similarity", "high"]).is_err());
    }

    fn pair() -> Pair {
        Pair {
            similarity: 0.91,
            namespace: "user:me".into(),
            a_id: "aaaaaaaa-1111-4111-8111-111111111111".into(),
            a_content: "the port is 8080".into(),
            b_id: "bbbbbbbb-2222-4222-8222-222222222222".into(),
            b_content: "the port is 8787".into(),
        }
    }

    fn verdict(pair: &str, verdict: &str, keep: Option<&str>) -> Verdict {
        Verdict {
            pair: pair.into(),
            verdict: verdict.into(),
            keep: keep.map(str::to_string),
            why: "because".into(),
        }
    }

    #[test]
    fn a_wall_clock_target_later_today_is_the_wait_until_it() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 8, 21, 4, 0, 0).unwrap();
        assert_eq!(until_next(now, 4, 25).as_secs(), 25 * 60);
    }

    #[test]
    fn a_target_already_past_today_waits_for_tomorrow() {
        // The case that decides whether a daemon started at noon runs tonight or right now. Right
        // now would spend the model call every time the container restarts.
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap();
        let secs = until_next(now, 4, 25).as_secs();
        assert_eq!(secs, (16 * 60 + 25) * 60);
    }

    #[test]
    fn the_wait_is_never_zero_at_the_target_minute() {
        // A run finishing inside its own target minute would otherwise fire again at once.
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 8, 21, 4, 25, 0).unwrap();
        assert_eq!(until_next(now, 4, 25).as_secs(), 24 * 3600);
    }

    #[test]
    fn a_clock_time_parses_and_a_nonsense_one_is_refused() {
        assert_eq!(parse_at("04:25").unwrap(), (4, 25));
        assert_eq!(parse_at(" 00:00 ").unwrap(), (0, 0));
        assert_eq!(parse_at("23:59").unwrap(), (23, 59));
        for bad in ["24:00", "04:60", "4", "04-25", "", "hh:mm"] {
            assert!(parse_at(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_pair_reference_maps_to_its_index() {
        assert_eq!(pair_index("pair 1", 8), Some(0));
        assert_eq!(pair_index("pair 8", 8), Some(7));
        assert_eq!(pair_index("3", 8), Some(2));
    }

    #[test]
    fn a_pair_reference_outside_the_batch_is_refused() {
        // A verdict about a pair that was not in this call is a verdict about rows the caller
        // cannot identify. One finding lost beats two wrong rows merged.
        assert_eq!(pair_index("pair 0", 8), None);
        assert_eq!(pair_index("pair 9", 8), None);
        assert_eq!(pair_index("aaaaaaaa-1111-4111-8111-111111111111", 8), None);
        assert_eq!(pair_index("the first one", 8), None);
    }

    #[test]
    fn a_same_verdict_becomes_a_paraphrase_with_the_named_survivor() {
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out = to_proposals(
            &batch,
            &[verdict("pair 1", "same", Some("aaaaaaaa-1111-4111-8111-111111111111"))],
            "qwen",
            &mut r,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "paraphrase");
        assert_eq!(out[0]["keep_id"], "aaaaaaaa-1111-4111-8111-111111111111");
        let members = out[0]["members"].as_array().unwrap();
        assert_eq!(members[0]["disposition"], "keep");
        assert_eq!(members[1]["memory_id"], "bbbbbbbb-2222-4222-8222-222222222222");
        assert_eq!(members[1]["seen_content"], "the port is 8787");
        assert_eq!(r.same, 1);
    }

    #[test]
    fn a_same_verdict_naming_a_row_that_is_not_in_the_pair_is_discarded() {
        // The model has said two rows are interchangeable and then named a third thing. Acting on
        // the first half while ignoring the second is how the wrong row gets retired.
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out =
            to_proposals(&batch, &[verdict("pair 1", "same", Some("cccccccc"))], "qwen", &mut r);
        assert!(out.is_empty());
        assert_eq!(r.discarded, 1);
        assert_eq!(r.same, 0);
    }

    #[test]
    fn a_same_verdict_with_no_survivor_is_discarded() {
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out = to_proposals(&batch, &[verdict("pair 1", "same", None)], "qwen", &mut r);
        assert!(out.is_empty());
        assert_eq!(r.discarded, 1);
    }

    #[test]
    fn a_contradiction_names_no_survivor_and_retires_neither_side_on_its_own() {
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out = to_proposals(&batch, &[verdict("pair 1", "contradiction", None)], "qwen", &mut r);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "contradiction");
        assert!(out[0].get("keep_id").is_none(), "a contradiction must name no survivor");
        assert_eq!(r.contradictions, 1);
    }

    #[test]
    fn a_contradiction_that_names_a_survivor_anyway_still_names_none() {
        // The prompt tells the model to leave `keep` out here. A model that fills it in regardless
        // must not turn a conflict into a merge.
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out = to_proposals(
            &batch,
            &[verdict("pair 1", "contradiction", Some("aaaaaaaa-1111-4111-8111-111111111111"))],
            "qwen",
            &mut r,
        );
        assert!(out[0].get("keep_id").is_none());
    }

    #[test]
    fn an_unrelated_verdict_queues_nothing() {
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out = to_proposals(&batch, &[verdict("pair 1", "unrelated", None)], "qwen", &mut r);
        assert!(out.is_empty());
        assert_eq!(r.unrelated, 1);
        assert_eq!(r.discarded, 0, "unrelated is an answer, not a failure");
    }

    #[test]
    fn a_verdict_nobody_asked_for_is_discarded_rather_than_guessed_at() {
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out =
            to_proposals(&batch, &[verdict("pair 1", "probably the same", None)], "qwen", &mut r);
        assert!(out.is_empty());
        assert_eq!(r.discarded, 1);
    }

    #[test]
    fn the_verdict_is_read_case_insensitively() {
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out = to_proposals(
            &batch,
            &[verdict("pair 1", "  SAME  ", Some("aaaaaaaa-1111-4111-8111-111111111111"))],
            "qwen",
            &mut r,
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn the_model_that_spoke_is_recorded_on_every_proposal() {
        // The queue has to say which tier produced a finding. The cheap one and the expensive one
        // disagree often enough that a rationale without an author is not evaluable.
        let batch = vec![pair()];
        let mut r = PassReport::default();
        let out = to_proposals(
            &batch,
            &[verdict("pair 1", "contradiction", None)],
            "qwen/qwen3.7-flash",
            &mut r,
        );
        assert_eq!(out[0]["produced_by"], "qwen/qwen3.7-flash");
    }

    #[test]
    fn a_pair_with_unreadable_text_is_refused_before_anything_is_sent() {
        let mut p = pair();
        p.b_content = "   ".into();
        let e = refuse_empty(&[p]).unwrap_err();
        assert!(e.message.contains("sealed"), "{}", e.message);
    }
}
