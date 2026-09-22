//! The commands, matching `bin/lumberroom.mjs` in output and in exit code.
//!
//! The two clients share `~/.config/lumberroom/config.json` and the acceptance scripts under `scripts/`,
//! so a difference in either is a bug rather than a style choice. Where this client diverges the
//! divergence is named in a comment.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};

use crate::args::Args;
use crate::client::{err, err_code, Client, Result};
use crate::{format, out, out_json, read_line, wire};

pub(crate) fn require_token(c: &Client, config_path: &str) -> Result<()> {
    if c.has_token() {
        return Ok(());
    }
    Err(err_code(
        format!("no token. Pass --token, set LUMBERROOM_TOKEN, run 'lumberroom login', or write {config_path}"),
        2,
    ))
}

/// Compact JSON, the way `JSON.stringify(value)` renders it.
pub(crate) fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

pub(crate) fn typed<T: serde::de::DeserializeOwned>(body: &Value, what: &str) -> Result<T> {
    serde_json::from_value(body.clone()).map_err(|e| {
        err(format!("{what} response is not the expected shape ({e}): {}", compact(body)))
    })
}

/// The two accepted forms for a valid-time argument: `2026-03-01` read as midnight UTC, or a full
/// RFC 3339 instant. Mirrors `parse_occurred_at` in the server crate's `src/mcp/tools.rs` word for
/// word for `field == "occurred_at"`, restated here because this crate cannot depend on that one.
/// Two surfaces refusing the same input differently is how an owner learns to distrust both, so
/// keep this in sync when the original changes.
pub(crate) fn parse_two_date_forms(
    field: &str,
    raw: &str,
) -> std::result::Result<DateTime<Utc>, String> {
    let value = raw.trim();

    if let Ok(instant) = DateTime::parse_from_rfc3339(value) {
        return Ok(instant.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists on every calendar date")
            .and_utc());
    }
    Err(date_refusal(field, value))
}

/// One message for every rejected form, matching the server's `refusal` in wording and in leaving
/// out any suggested repair: a guessed day would be a fact wearing the shape of a fact.
fn date_refusal(field: &str, value: &str) -> String {
    format!(
        "{field} `{}` is not one of the two accepted forms. Pass a date, `2026-03-01`, read as \
midnight UTC, or a full RFC 3339 instant, `2026-03-01T09:30:00Z`. A bare month or year cannot be \
represented, so omit {field} rather than choosing a day.",
        clip_date(value)
    )
}

/// Keep a stray paragraph out of the refusal, matching the server's own clip length.
fn clip_date(value: &str) -> String {
    const LIMIT: usize = 64;
    match value.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}...", &value[..cut]),
        None => value.to_string(),
    }
}

/// Reads a date-shaped flag and sends it on as an RFC 3339 UTC instant, whichever of the two forms
/// the owner typed. A bare flag with no value is refused rather than read as absent: silently
/// dropping the date is the exact failure the server's own parser exists to prevent.
fn optional_date_flag(args: &Args, key: &str, field: &str) -> Result<Option<String>> {
    if args.is_bare(key) {
        return Err(err(format!(
            "--{key} needs a value: a date `2026-03-01` or a full RFC 3339 instant"
        )));
    }
    match args.value(key) {
        Some(raw) => parse_two_date_forms(field, raw).map(|dt| Some(dt.to_rfc3339())).map_err(err),
        None => Ok(None),
    }
}

pub async fn doctor(c: &Client) -> Result<()> {
    out(&format!("endpoint: {}", c.cfg.mcp_url));
    let (health_status, health) = c.http_get("/healthz").await?;
    out(&format!("healthz:  {health_status} {}", compact(&health)));
    let (ready_status, ready) = c.http_get("/readyz").await?;
    out(&format!("readyz:   {ready_status} {}", compact(&ready)));

    // The label reads the file rather than the resolved credential on purpose: an OAuth block that
    // a leftover static token is shadowing must not report itself as the credential in use.
    let (using_oauth, client_id, expires_at, has_refresh) = {
        let file = c.file.borrow();
        (
            file.has_oauth_access_token() && file.str_field("token").is_none(),
            file.oauth("client_id").map(str::to_string),
            file.oauth("expires_at").map(str::to_string),
            file.oauth("refresh_token").is_some(),
        )
    };
    let credential = if using_oauth {
        format!("oauth (client {})", client_id.unwrap_or_else(|| "unknown".into()))
    } else if c.has_token() {
        "static token".to_string()
    } else {
        "none configured".to_string()
    };
    out(&format!("credential: {credential}"));
    if using_oauth {
        out(&format!("oauth token expires: {}", expires_at.unwrap_or_else(|| "unknown".into())));
        out(&format!("refresh token on file: {}", if has_refresh { "yes" } else { "no" }));
    }

    require_token(c, &c.file.borrow().path.display().to_string())?;
    let (who_status, who) = c.http_get("/admin/whoami").await?;
    out(&format!("whoami:   {who_status} {}", compact(&who)));

    // Two modes, two lines. /readyz reports what the SERVER runs; whoami reports the mode of the
    // CREDENTIAL that just authenticated. They differ legitimately, since every mode honours static
    // tokens, and one line carrying both reads as a server bug.
    if let Some(mode) = ready.get("auth_mode").and_then(Value::as_str) {
        out(&format!("server auth mode:     {mode}"));
    }
    if let Some(mode) = who.get("mode").and_then(Value::as_str) {
        out(&format!("credential auth mode: {mode}"));
    }

    c.initialize().await?;
    let tools = c.rpc("tools/list", json!({})).await?;
    let names: Vec<&str> = tools
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|t| t.get("name").and_then(Value::as_str)).collect())
        .unwrap_or_default();
    out(&format!("tools:    {}", names.join(", ")));

    if health_status != 200 || ready_status != 200 || who_status != 200 {
        return Err(err("one or more checks failed"));
    }
    out("all checks passed");
    Ok(())
}

/// No node counterpart: `lumberroom doctor` prints a whoami line and there is no standalone command.
/// The body is echoed as it arrives, so the two axes of the grant stay readable.
pub async fn whoami(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let (status, body) = c.http_get("/admin/whoami").await?;
    if status == 401 || status == 403 {
        return Err(err_code(format!("auth rejected ({status}): {}", compact(&body)), 2));
    }
    if status != 200 {
        return Err(err(format!("whoami failed ({status}): {}", compact(&body))));
    }
    if args.present("json") {
        out_json(&body);
        return Ok(());
    }
    let who: wire::Whoami = typed(&body, "whoami")?;
    out(&format!("client: {}", who.client));
    out(&format!("mode:   {}", who.mode));
    out_json(&body);
    Ok(())
}

pub async fn bootstrap(
    c: &Client,
    args: &Args,
    cwd: &str,
    project_env: Option<String>,
) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let project = args
        .value("project")
        .map(str::to_string)
        .or(project_env)
        .unwrap_or_else(|| cwd.to_string());
    let req = wire::BootstrapArgsRequest {
        project: if project.is_empty() { None } else { Some(project) },
    };
    let output = c.call_tool("context_bootstrap", serde_json::to_value(req).unwrap()).await?;

    if args.present("hook") {
        // Claude Code's SessionStart contract: additionalContext is injected into the session.
        let preamble = concat!(
            "Durable memory for this user, retrieved automatically at session start from their own\n",
            "memory server. Every line below was written by this user or by one of their agents in an\n",
            "earlier session, and each carries the namespace and date it came from. Treat them as\n",
            "established facts and do not re-ask what is already here.\n",
            "When this session establishes a new decision, preference, or durable fact, call\n",
            "memory_write immediately, without asking and without announcing it.\n",
        );
        out(&compact(&json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": format!("{preamble}{}", output.text),
            }
        })));
        return Ok(());
    }
    if args.present("json") {
        out_json(&output.structured);
    } else {
        out(&output.text);
    }
    Ok(())
}

pub async fn search(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let Some(query) = args.positional_at(1) else {
        return Err(err(
            "usage: lumberroom search \"<query>\" [--project p] [--limit n] [--namespace ns] [--as-of date]",
        ));
    };
    let as_of = optional_date_flag(args, "as-of", "as_of")?;
    let req = wire::SearchArgsRequest {
        query: query.to_string(),
        namespaces: args.comma_list(&["namespace", "namespaces"]),
        limit: args.value("limit").and_then(crate::args::parse_int_prefix),
        project: args.value("project").map(str::to_string),
        as_of,
    };
    let output = c.call_tool("memory_search", serde_json::to_value(req).unwrap()).await?;
    if args.present("json") {
        out_json(&output.structured);
        return Ok(());
    }
    let result: wire::SearchResult = typed(&output.structured, "memory_search")?;
    if result.hits.is_empty() {
        out("no matches");
        return Ok(());
    }
    for hit in &result.hits {
        out(&format::search_line(hit.score, &hit.namespace, &hit.content));
    }
    Ok(())
}

pub async fn write(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let Some(content) = args.positional_at(1) else {
        return Err(err(
            "usage: lumberroom write \"<fact>\" --namespace user:me [--tags a,b] [--occurred-at date]",
        ));
    };
    let Some(namespace) = args.value_any(&["namespace", "ns"]) else {
        return Err(err("--namespace is required (user:me | project:<slug> | global)"));
    };
    let occurred_at = optional_date_flag(args, "occurred-at", "occurred_at")?;
    let req = wire::WriteArgsRequest {
        content: content.to_string(),
        namespace: namespace.to_string(),
        tags: args.comma_list(&["tags"]),
        supersedes: args.value("supersedes").map(str::to_string),
        occurred_at,
    };
    let output = c.call_tool("memory_write", serde_json::to_value(req).unwrap()).await?;
    if args.present("json") {
        out_json(&output.structured);
        return Ok(());
    }
    let outcome: wire::WriteOutcome = typed(&output.structured, "memory_write")?;
    out(&format::write_line(outcome.deduplicated, &outcome.id, &outcome.namespace));
    Ok(())
}

pub async fn stats(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let hours = args.int("hours", 168);
    let by_client = args.present("by-client");
    let path = if by_client {
        format!("/statsz?hours={hours}&by=client")
    } else {
        format!("/statsz?hours={hours}")
    };
    let (status, body) = c.http_get(&path).await?;
    if status != 200 {
        return Err(err(format!("stats failed ({status}): {}", compact(&body))));
    }
    if args.present("json") {
        out_json(&body);
        return Ok(());
    }
    if by_client {
        let stats: wire::ClientStats = typed(&body, "stats")?;
        out(&format!("window: last {}h", stats.window_hours));
        for row in &stats.by_client {
            out(&format::client_stats_line(row));
        }
        return Ok(());
    }
    let stats: wire::ToolStats = typed(&body, "stats")?;
    out(&format!("window: last {}h", stats.window_hours));
    out(&format::totals_line(&stats.totals));
    for row in &stats.by_tool {
        out(&format::tool_stats_line(row));
    }
    Ok(())
}

pub async fn registry(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    match args.positional_at(1) {
        Some("get") => {
            let (Some(kind), Some(key)) = (args.positional_at(2), args.positional_at(3)) else {
                return Err(err(
                    "usage: lumberroom registry get <kind> <key> [--namespace ns] [--project p]",
                ));
            };
            let req = wire::RegistryArgsRequest {
                kind: kind.to_string(),
                key: key.to_string(),
                namespace: args.value("namespace").map(str::to_string),
                project: args.value("project").map(str::to_string),
            };
            let output = c.call_tool("registry_get", serde_json::to_value(req).unwrap()).await?;
            out_json(&output.structured);
            Ok(())
        }
        Some("set") => {
            let (Some(kind), Some(key), Some(raw)) =
                (args.positional_at(2), args.positional_at(3), args.positional_at(4))
            else {
                return Err(err(
                    "usage: lumberroom registry set <kind> <key> <json-value> --namespace ns",
                ));
            };
            let Some(namespace) = args.value_any(&["namespace", "ns"]) else {
                return Err(err("--namespace is required"));
            };
            // A bare string is a legitimate value; do not force JSON quoting.
            let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
            let req = wire::RegistryWriteRequest { namespace, kind, key, value };
            let (status, body) = c
                .http_request(
                    reqwest::Method::POST,
                    "/admin/registry",
                    Some(serde_json::to_value(req).unwrap()),
                )
                .await?;
            if status != 200 {
                return Err(err(format!("registry set failed ({status}): {}", compact(&body))));
            }
            out_json(&body);
            Ok(())
        }
        _ => Err(err("usage: lumberroom registry <get|set> ...")),
    }
}

/// One row `forget` is willing to delete, with the score that put it on the list.
///
/// `score` is `None` for a lookup by id: that row is on the list because the caller named it, and
/// printing a similarity there would invent a number nothing measured.
pub struct Candidate {
    pub id: String,
    pub namespace: String,
    pub content: String,
    pub score: Option<f64>,
}

/// Which candidates from a `--query` list the caller actually chose.
///
/// `memory_search` always returns its `limit`, so a query naming two rows produces a list of
/// twenty and the last one may be unrelated. The old code handed that whole list to the delete
/// loop behind one "yes". Asking for `--pick` or `--all` makes the wide delete a thing the caller
/// typed rather than a thing the ranker decided.
#[derive(Debug, PartialEq)]
pub enum Selection {
    /// Nothing chosen. Print the list and delete nothing.
    None,
    /// 1-based positions in the printed list.
    Pick(Vec<usize>),
    /// Every candidate, which still has to survive a confirmation naming the count.
    All,
}

/// Parses `--pick 1,3,4`. 1-based to match the printed list, because the caller reads the numbers
/// off the screen rather than out of an array.
pub fn parse_pick(raw: &str, len: usize) -> Result<Vec<usize>> {
    let mut picked = Vec::new();
    for part in raw.split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        let n: usize = t
            .parse()
            .map_err(|_| err(format!("--pick takes numbers from the printed list, got {t:?}")))?;
        if n == 0 || n > len {
            return Err(err(format!("--pick {n} is outside the list, which has {len} entries")));
        }
        if !picked.contains(&n) {
            picked.push(n);
        }
    }
    if picked.is_empty() {
        return Err(err("--pick chose nothing"));
    }
    Ok(picked)
}

pub fn selection(args: &Args, len: usize) -> Result<Selection> {
    if let Some(raw) = args.value("pick") {
        return Ok(Selection::Pick(parse_pick(raw, len)?));
    }
    if args.present("all") {
        return Ok(Selection::All);
    }
    Ok(Selection::None)
}

pub async fn forget(
    c: &Client,
    args: &Args,
    confirm: impl FnOnce() -> std::io::Result<String>,
) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let id_arg = args.positional_at(1);
    let query = args.value("query");
    let dry_run = args.present("dry-run");
    if id_arg.is_none() && query.is_none() {
        return Err(err(
            "usage: lumberroom forget <id> | --query \"...\" [--pick 1,3 | --all] [--dry-run]",
        ));
    }

    let by_query = id_arg.is_none();

    let mut candidates: Vec<Candidate> = if let Some(id) = id_arg {
        let path = format!("/admin/memory/{}", urlencode(id));
        let (status, body) = c.http_get(&path).await?;
        if status == 404 {
            return Err(err(format!("no memory with id {id}")));
        }
        if status != 200 {
            return Err(err(format!("lookup failed ({status}): {}", compact(&body))));
        }
        let m: wire::Memory = typed(&body, "memory")?;
        vec![Candidate { id: m.id, namespace: m.namespace, content: m.content, score: None }]
    } else {
        let query = query.unwrap();
        let req = wire::SearchArgsRequest {
            query: query.to_string(),
            limit: Some(args.int("limit", 20)),
            ..Default::default()
        };
        let output = c.call_tool("memory_search", serde_json::to_value(req).unwrap()).await?;
        let result: wire::SearchResult = typed(&output.structured, "memory_search")?;
        if result.hits.is_empty() {
            out("no matches for that query, nothing to forget");
            return Ok(());
        }
        result
            .hits
            .into_iter()
            .map(|h| Candidate {
                id: h.id,
                namespace: h.namespace,
                content: h.content,
                score: Some(h.score),
            })
            .collect()
    };

    let plural = if candidates.len() == 1 { "" } else { "s" };
    out(&format!("{} candidate{plural}:", candidates.len()));
    for (i, cand) in candidates.iter().enumerate() {
        out(&format::candidate_line(i + 1, &cand.id, &cand.namespace, &cand.content, cand.score));
    }

    if dry_run {
        out("dry run: nothing deleted");
        return Ok(());
    }

    if by_query {
        match selection(args, candidates.len())? {
            Selection::None => {
                out("");
                out("nothing deleted. A query ranks the whole store, so this list runs from the \
rows you meant to the rows that merely scored next.");
                out("Choose from it with --pick 1,3, or delete every entry above with --all.");
                return Ok(());
            }
            Selection::Pick(picked) => {
                let mut kept = Vec::with_capacity(picked.len());
                for n in &picked {
                    let cand = &candidates[n - 1];
                    kept.push(Candidate {
                        id: cand.id.clone(),
                        namespace: cand.namespace.clone(),
                        content: cand.content.clone(),
                        score: cand.score,
                    });
                }
                candidates = kept;
                out("");
                let plural = if candidates.len() == 1 { "" } else { "s" };
                out(&format!("--pick chose {} of them:", candidates.len()));
                for (i, cand) in candidates.iter().enumerate() {
                    out(&format::candidate_line(
                        i + 1,
                        &cand.id,
                        &cand.namespace,
                        &cand.content,
                        cand.score,
                    ));
                }
                let _ = plural;
            }
            Selection::All => {}
        }
    }

    // The count rather than "yes". Typing "yes" costs the same whether the list holds two rows or
    // twenty, and the number is the one thing about a wide delete worth reading twice.
    let noun = if candidates.len() == 1 { "y" } else { "ies" };
    crate::prompt(&format!(
        "Delete {} memor{noun} above? Type \"{}\" to confirm: ",
        candidates.len(),
        candidates.len()
    ));
    let answer = confirm().map_err(|e| err(format!("cannot read the confirmation: {e}")))?;
    if answer.trim() != candidates.len().to_string() {
        out("aborted, nothing deleted");
        return Ok(());
    }

    let mut deleted = 0;
    for cand in &candidates {
        let path = format!("/admin/memory/{}", urlencode(&cand.id));
        let (status, _) = c.http_request(reqwest::Method::DELETE, &path, None).await?;
        if status == 200 {
            deleted += 1;
        } else {
            out(&format!("  failed to delete {} ({status})", cand.id));
        }
    }
    out(&format!("deleted {deleted} of {}", candidates.len()));
    Ok(())
}

pub async fn supersede(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let (Some(old_id), Some(new_id)) = (args.positional_at(1), args.positional_at(2)) else {
        return Err(err("usage: lumberroom supersede <old-id> <new-id>"));
    };
    let req = wire::SupersedeRequest { new_id };
    let path = format!("/admin/memory/{}/supersede", urlencode(old_id));
    let (status, body) = c
        .http_request(reqwest::Method::POST, &path, Some(serde_json::to_value(req).unwrap()))
        .await?;
    if status != 200 {
        return Err(err(format!("supersede failed ({status}): {}", compact(&body))));
    }
    out(&format!("{old_id} is now superseded by {new_id}"));
    Ok(())
}

/// `lumberroom currency [--fixture <path>]`
///
/// Coverage always, accuracy only when a fixture is given. Decision 0014 part 2.
///
/// The fixture is JSONL, one case per line, and it is the owner's to write: a measure whose cases
/// were chosen by whoever built the store reports on its author rather than on the store. Each case
/// names the question, the instant, the id that held then, and the id that must not come back.
pub async fn currency(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;

    let mut cases: Vec<serde_json::Value> = Vec::new();
    if let Some(path) = args.value("fixture") {
        let text =
            std::fs::read_to_string(path).map_err(|e| err(format!("cannot read {path}: {e}")))?;
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let case: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| err(format!("{path} line {}: {e}", i + 1)))?;
            cases.push(case);
        }
        if cases.is_empty() {
            return Err(err(format!("{path} holds no cases")));
        }
    }

    let body = serde_json::json!({ "cases": cases });
    let (status, raw) =
        c.http_request(reqwest::Method::POST, "/admin/currency", Some(body)).await?;
    if status != 200 {
        return Err(err(format!("currency failed ({status}): {}", compact(&raw))));
    }
    if args.present("json") {
        out_json(&raw);
        return Ok(());
    }
    let report: wire::CurrencyReport = typed(&raw, "currency")?;

    let cov = &report.coverage;
    out(&format!("{} supersession pairs you can read", cov.pairs));
    match report.closed_fraction {
        Some(f) => out(&format!("  {} carry a closed interval, {:.0}%", cov.closed, f * 100.0)),
        None => out("  no pairs, so nothing to close"),
    }
    out(&format!(
        "  {} were replaced and still read as holding at every instant after their start",
        cov.dated_but_open
    ));
    out(&format!("  {} have a start date on both halves", cov.both_dated));

    match report.accuracy {
        None => {
            out("");
            out("No fixture, so nothing was asked. Pass --fixture <file.jsonl> to score the answers.");
        }
        Some(a) => {
            out("");
            out(&format!(
                "{} cases, {:.0}% answered with the fact that held",
                report.cases.len(),
                a * 100.0
            ));
            if report.returned_both > 0 {
                out(&format!(
                    "  {} returned the fact and its replacement together, which is the failure this measures",
                    report.returned_both
                ));
            }
            for case in report.cases.iter().filter(|c| c.also_returned_the_other || !c.found) {
                let why = if case.also_returned_the_other { "both versions" } else { "not found" };
                out(&format!("    [{why}] at {}: {}", case.as_of, case.question));
            }
        }
    }
    Ok(())
}

/// `lumberroom graph walk "<question>" [--degree-cap n] [--force]` and `lumberroom graph rebuild`.
///
/// A walk is expensive, so the router decides whether one is warranted and reports why either way.
/// `--force` walks regardless, which is how a refused question gets compared against what a walk
/// would have found.
pub async fn graph(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    match args.positional_at(1).unwrap_or("walk") {
        "rebuild" => {
            let (status, body) =
                c.http_request(reqwest::Method::POST, "/admin/graph/rebuild", None).await?;
            if status != 200 {
                return Err(err(format!("rebuild failed ({status}): {}", compact(&body))));
            }
            let n = body.get("edges").and_then(|v| v.as_i64()).unwrap_or(0);
            out(&format!("{n} edges, built from supersession links, aliases and shared tags"));
            Ok(())
        }
        "walk" => {
            let Some(q) = args.positional_at(2) else {
                return Err(err(
                    "usage: lumberroom graph walk \"<question>\" [--degree-cap n] [--force]",
                ));
            };
            let mut path = format!("/admin/graph/walk?q={}", urlencode(q));
            if let Some(cap) = args.value("degree-cap") {
                path.push_str(&format!("&degree_cap={}", urlencode(cap)));
            }
            if args.present("force") {
                path.push_str("&force=true");
            }
            let (status, body) = c.http_get(&path).await?;
            if status != 200 {
                return Err(err(format!("walk failed ({status}): {}", compact(&body))));
            }
            if args.present("json") {
                out_json(&body);
                return Ok(());
            }
            let reached =
                body.get("reached").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let edges = body.get("edges").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            match body.get("verdict") {
                Some(v) if !v.is_null() => {
                    let s = v.get("signals").cloned().unwrap_or_default();
                    let num = |k: &str| s.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                    out(&format!(
                        "{}: top {:.3}, spread {:.3}",
                        v.get("route").and_then(|r| r.as_str()).unwrap_or("?"),
                        num("top"),
                        num("spread")
                    ));
                    if let Some(why) = v.get("because").and_then(|b| b.as_array()) {
                        for reason in why {
                            out(&format!("  {}", reason.as_str().unwrap_or("")));
                        }
                    }
                }
                _ => out("walked without asking the router"),
            }
            out("");
            out(&format!("{} rows reached over {edges} edges", reached.len()));
            for r in reached.iter().take(20) {
                let hop = r.get("hop").and_then(|v| v.as_u64()).unwrap_or(0);
                let via = r.get("via").and_then(|v| v.as_str()).unwrap_or("seed");
                let content = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
                out(&format!(
                    "  hop {hop} {via:<13} {}",
                    content.chars().take(88).collect::<String>()
                ));
            }
            if reached.len() > 20 {
                out(&format!("  and {} more", reached.len() - 20));
            }
            Ok(())
        }
        other => Err(err(format!("unknown graph subcommand `{other}`. Available: walk, rebuild"))),
    }
}

/// `lumberroom arity` and its subcommands. Decision 0014 part 3.
///
/// Cardinality is the one thing about supersession no model can read off the text: "the limit is 40k
/// now" replaces its predecessor and "applying for b and c" replaces nothing, and the two sentences
/// are the same shape. The owner declares it per tag, and an undeclared tag proposes nothing.
pub async fn arity(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let sub = args.positional_at(1).unwrap_or("list");
    match sub {
        "list" => {
            let (status, body) = c.http_get("/admin/arity").await?;
            if status != 200 {
                return Err(err(format!("arity list failed ({status}): {}", compact(&body))));
            }
            let rows = body.get("rows").and_then(|r| r.as_array()).cloned().unwrap_or_default();
            if rows.is_empty() {
                out("no subjects declared");
                out("");
                out("Nothing is proposed for an undeclared subject. Look before you declare:");
                out("  lumberroom arity preview <tag>");
                return Ok(());
            }
            for r in &rows {
                let tag = r.get("tag").and_then(|v| v.as_str()).unwrap_or("?");
                let a = r.get("arity").and_then(|v| v.as_str()).unwrap_or("?");
                let note = r.get("note").and_then(|v| v.as_str()).unwrap_or("");
                out(&format!("  {tag:<24} {a:<8} {note}"));
            }
            Ok(())
        }
        "preview" => {
            let Some(tag) = args.positional_at(2) else {
                return Err(err("usage: lumberroom arity preview <tag>"));
            };
            let (status, body) = c.http_get(&format!("/admin/arity/{}", urlencode(tag))).await?;
            if status != 200 {
                return Err(err(format!("preview failed ({status}): {}", compact(&body))));
            }
            let dated = body.get("dated_rows").and_then(|v| v.as_u64()).unwrap_or(0);
            let skipped = body.get("same_day_skipped").and_then(|v| v.as_u64()).unwrap_or(0);
            let ends =
                body.get("would_end").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            out(&format!("{dated} dated facts carry `{tag}`"));
            out(&format!(
                "declaring it single would end {} of them, and nothing is deleted",
                ends.len()
            ));
            if skipped > 0 {
                out(&format!(
                    "  {skipped} pairs share a day and cannot be ordered, so they are skipped"
                ));
            }
            for e in &ends {
                let ec = e.get("earlier_content").and_then(|v| v.as_str()).unwrap_or("");
                let ea = e.get("earlier_occurred_at").and_then(|v| v.as_str()).unwrap_or("");
                let la = e.get("later_occurred_at").and_then(|v| v.as_str()).unwrap_or("");
                out(&format!("  ends {ea} at {la}"));
                out(&format!("    {}", ec.chars().take(96).collect::<String>()));
            }
            if !ends.is_empty() {
                out("");
                out(&format!("  lumberroom arity declare {tag} single"));
            }
            Ok(())
        }
        "declare" => {
            let (Some(tag), Some(a)) = (args.positional_at(2), args.positional_at(3)) else {
                return Err(err(
                    "usage: lumberroom arity declare <tag> <single|many> [--note ...]",
                ));
            };
            let body = serde_json::json!({ "tag": tag, "arity": a, "note": args.value("note") });
            let (status, raw) =
                c.http_request(reqwest::Method::POST, "/admin/arity", Some(body)).await?;
            if status != 200 {
                return Err(err(format!("declare failed ({status}): {}", compact(&raw))));
            }
            out(&format!("{tag} holds {a}"));
            Ok(())
        }
        "forget" => {
            let Some(tag) = args.positional_at(2) else {
                return Err(err("usage: lumberroom arity forget <tag>"));
            };
            let (status, raw) = c
                .http_request(
                    reqwest::Method::DELETE,
                    &format!("/admin/arity/{}", urlencode(tag)),
                    None,
                )
                .await?;
            if status != 200 {
                return Err(err(format!("forget failed ({status}): {}", compact(&raw))));
            }
            out(&format!("{tag} is no longer declared"));
            Ok(())
        }
        "run" => {
            let (status, raw) =
                c.http_request(reqwest::Method::POST, "/admin/supersession/run", None).await?;
            if status != 200 {
                return Err(err(format!("run failed ({status}): {}", compact(&raw))));
            }
            let n = |k: &str| raw.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            out(&format!(
                "{} declared subjects, {} pairs, {} queued, {} already known",
                n("tags_scanned"),
                n("pairs_found"),
                n("queued"),
                n("already_known")
            ));
            if n("same_day_skipped") > 0 {
                out(&format!(
                    "  {} pairs share a day and were skipped: an empty period reads as never true",
                    n("same_day_skipped")
                ));
            }
            if n("queued") > 0 {
                out("Nothing was applied. Review with: lumberroom cleanup list");
            }
            Ok(())
        }
        other => Err(err(format!(
            "unknown arity subcommand `{other}`. Available: list, preview, declare, forget, run"
        ))),
    }
}

/// Fill a start date on one fact that never carried one.
///
/// The server refuses any date the fact's own text does not name, so this cannot invent history. It
/// exists because the near-now fence means a fact recorded on the day it happened lost its date with
/// no way to supply it afterwards.
pub async fn fill_date(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let (Some(id), Some(date)) = (args.positional_at(1), args.positional_at(2)) else {
        return Err(err("usage: lumberroom fill-date <id> <YYYY-MM-DD>"));
    };
    let path = format!("/admin/memory/{}/fill-date", urlencode(id));
    let body = serde_json::json!({ "occurred_at": date });
    let (status, body) = c.http_request(reqwest::Method::POST, &path, Some(body)).await?;
    if status != 200 {
        return Err(err(format!("fill-date failed ({status}): {}", compact(&body))));
    }
    out(&format!("{id} now carries occurred_at {date}"));
    Ok(())
}

/// Prints the supersession chain for one fact, oldest first: the value, the period it held, and
/// how it ended. Assumes `GET /admin/memory/{id}/history`; see `wire_in` in the phase-7 handoff.
pub async fn history(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let Some(id) = args.positional_at(1) else {
        return Err(err("usage: lumberroom history <id>"));
    };
    let path = format!("/admin/memory/{}/history", urlencode(id));
    let (status, body) = c.http_get(&path).await?;
    if status == 403 {
        return Err(err_code(
            format!(
                "history is refused ({status}): the credential's grant does not include \
may_read_history, a capability that is off by default and granted separately from read"
            ),
            2,
        ));
    }
    if status == 404 {
        return Err(err(format!("no memory with id {id}, or the server has no history route yet")));
    }
    if status != 200 {
        return Err(err(format!("history failed ({status}): {}", compact(&body))));
    }
    if args.present("json") {
        out_json(&body);
        return Ok(());
    }
    let chain: wire::HistoryChain = typed(&body, "history")?;
    if chain.entries.is_empty() {
        out("no history for that id");
        return Ok(());
    }
    for line in format::history_lines(&chain.entries) {
        out(&line);
    }
    Ok(())
}

/// `alias set|list|forget`, routed at `/admin/alias` the way registry writes are routed at
/// `/admin/registry`. Exact paths assumed; see `wire_in` in the phase-7 handoff.
pub async fn alias(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    match args.positional_at(1) {
        Some("set") => alias_set(c, args).await,
        Some("list") => alias_list(c, args).await,
        Some("forget") => alias_forget(c, args).await,
        _ => Err(err("usage: lumberroom alias <set|list|forget> ...")),
    }
}

async fn alias_set(c: &Client, args: &Args) -> Result<()> {
    let (Some(name), Some(canonical)) = (args.positional_at(2), args.positional_at(3)) else {
        return Err(err(
            "usage: lumberroom alias set <name> <canonical> --namespace ns [--since date] [--until date]",
        ));
    };
    let Some(namespace) = args.value_any(&["namespace", "ns"]) else {
        return Err(err("--namespace is required"));
    };
    let since = optional_date_flag(args, "since", "since")?;
    let until = optional_date_flag(args, "until", "until")?;
    let req = wire::AliasSetRequest { namespace, alias: name, canonical, since, until };
    let (status, body) = c
        .http_request(
            reqwest::Method::POST,
            "/admin/alias",
            Some(serde_json::to_value(req).unwrap()),
        )
        .await?;
    if status != 200 {
        return Err(err(format!("alias set failed ({status}): {}", compact(&body))));
    }
    out(&format!("{name} now resolves to {canonical} in {namespace}"));
    Ok(())
}

async fn alias_list(c: &Client, args: &Args) -> Result<()> {
    let path = match args.value("namespace") {
        Some(ns) => format!("/admin/alias?namespace={}", urlencode(ns)),
        None => "/admin/alias".to_string(),
    };
    let (status, body) = c.http_get(&path).await?;
    if status != 200 {
        return Err(err(format!("alias list failed ({status}): {}", compact(&body))));
    }
    if args.present("json") {
        out_json(&body);
        return Ok(());
    }
    let list: wire::AliasList = typed(&body, "alias list")?;
    if list.aliases.is_empty() {
        out("no aliases");
        return Ok(());
    }
    for a in &list.aliases {
        out(&format::alias_line(a));
    }
    Ok(())
}

async fn alias_forget(c: &Client, args: &Args) -> Result<()> {
    let Some(name) = args.positional_at(2) else {
        return Err(err("usage: lumberroom alias forget <name> --namespace ns"));
    };
    let Some(namespace) = args.value_any(&["namespace", "ns"]) else {
        return Err(err("--namespace is required"));
    };
    let path = format!("/admin/alias/{}?namespace={}", urlencode(name), urlencode(namespace));
    let (status, body) = c.http_request(reqwest::Method::DELETE, &path, None).await?;
    if status == 404 {
        return Err(err(format!("no alias {name} in {namespace}")));
    }
    if status != 200 {
        return Err(err(format!("alias forget failed ({status}): {}", compact(&body))));
    }
    out(&format!("forgot alias {name} in {namespace}"));
    Ok(())
}

pub async fn export(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let Some(target) = args.value("obsidian") else {
        return Err(err("usage: lumberroom export --obsidian <path> [--max-sensitivity open]"));
    };
    let max_sensitivity = args.value("max-sensitivity").unwrap_or("open");
    let page_size = 200;
    let mut offset = 0;
    let mut total = 0usize;
    loop {
        let path = format!(
            "/admin/export?max_sensitivity={max_sensitivity}&limit={page_size}&offset={offset}"
        );
        let (status, body) = c.http_get(&path).await?;
        if status != 200 {
            return Err(err(format!("export failed ({status}): {}", compact(&body))));
        }
        let page: wire::ExportPage = typed(&body, "export")?;
        let count = page.rows.len();
        for m in &page.rows {
            write_note(std::path::Path::new(target), m)?;
        }
        total += count;
        if count < page_size {
            break;
        }
        offset += page_size;
    }
    out(&format!("wrote {total} notes to {target}"));
    Ok(())
}

fn write_note(root: &std::path::Path, m: &wire::Memory) -> Result<()> {
    let dir = root.join(format::obsidian_dir(&m.namespace));
    std::fs::create_dir_all(&dir)
        .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.md", m.id));
    std::fs::write(&path, format::obsidian_note(m))
        .map_err(|e| err(format!("cannot write {}: {e}", path.display())))
}

pub async fn clients(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let (status, body) = c.http_get("/oauth/clients").await?;
    if status == 404 {
        return Err(err("server has no /oauth/clients: it is not running in oauth or oidc mode"));
    }
    if status != 200 {
        return Err(err(format!("clients failed ({status}): {}", compact(&body))));
    }
    if args.present("json") {
        out_json(&body);
        return Ok(());
    }
    let list: wire::ClientList = typed(&body, "clients")?;
    if list.clients.is_empty() {
        out("no clients registered");
        return Ok(());
    }
    for record in &list.clients {
        out(&format::client_line(record));
    }
    Ok(())
}

// ---- eval ----
//
// Recall against the owner's own fixture, scored here rather than on the server.
//
// The server grew `services::eval` on the same night this landed, and the choice between calling it
// through a route and computing the numbers here came down to what a comparison is worth. This
// client's job is to print what `bin/lumberroom.mjs eval` prints, and that client scores client-side over
// `memory_search` with the caller's own token. Running the same tool with the same limit under the
// same grant makes the two agree by construction; a route would put a different principal and a
// different code path behind one of them and leave the numbers to be reconciled by hand. No route
// is needed for this command, so the command works wherever a token does.
//
// The cost is a third copy of the arithmetic, after node's and `src/services/eval.rs`. The test
// vectors below are the ones that file pins. Change one and change both.

/// One fixture line. Named for the JSONL keys rather than for the enum it becomes, because the file
/// on disk is the contract: `client/eval-fixture.example.jsonl` documents it and the owner writes it
/// by hand.
#[derive(Debug, Clone)]
struct EvalCase {
    question: String,
    /// The row this question should return. `None` marks an anti-case: the store must say nothing.
    expect_id: Option<String>,
    origin: Option<String>,
}

#[derive(Debug, Default)]
struct EvalScore {
    normal: usize,
    anti: usize,
    /// One entry per normal case, `None` where the expected row never came back.
    ranks: Vec<Option<usize>>,
    violations: Vec<EvalViolation>,
}

#[derive(Debug)]
struct EvalViolation {
    question: String,
    origin: Option<String>,
    got: Option<String>,
}

/// Where the expected row landed, zero-based.
///
/// Byte for byte, which is `h.id === c.expect_id` in node. Folding case or trimming here would be
/// the kinder comparison and it is the wrong one: a fixture node scores as a miss would score as a
/// hit here, and the owner would be reading two recall numbers for one fixture with nothing on
/// screen to say why. The store writes lowercase UUIDs, so a case that needs the fold is a case
/// whose id was mistyped.
fn eval_rank(hit_ids: &[String], expect_id: &str) -> Option<usize> {
    hit_ids.iter().position(|id| id == expect_id)
}

/// Fraction of normal cases whose expected row landed inside the top `k`, a miss counting as zero.
/// `None` for an empty set: a mean over nothing is not zero.
fn eval_recall_at(ranks: &[Option<usize>], k: usize) -> Option<f64> {
    if ranks.is_empty() {
        return None;
    }
    let hits = ranks.iter().filter(|r| r.is_some_and(|rank| rank < k)).count();
    Some(hits as f64 / ranks.len() as f64)
}

/// Mean reciprocal rank over the same set, a miss contributing zero rather than being dropped.
fn eval_mrr(ranks: &[Option<usize>]) -> Option<f64> {
    if ranks.is_empty() {
        return None;
    }
    let sum: f64 = ranks.iter().map(|r| r.map_or(0.0, |rank| 1.0 / (rank + 1) as f64)).sum();
    Some(sum / ranks.len() as f64)
}

/// Score cases whose searches have already run. `results` pairs a case with the ids it returned, in
/// rank order.
fn eval_score(results: &[(EvalCase, Vec<String>)]) -> EvalScore {
    let mut s = EvalScore::default();
    for (case, hit_ids) in results {
        match &case.expect_id {
            // Any hit at all, at any score. A threshold here would let a confident wrong answer
            // pass by being a little less confident, and the fixture's claim about these questions
            // is that the store holds nothing for them.
            None => {
                s.anti += 1;
                if !hit_ids.is_empty() {
                    s.violations.push(EvalViolation {
                        question: case.question.clone(),
                        origin: case.origin.clone(),
                        got: hit_ids.first().cloned(),
                    });
                }
            }
            Some(expect_id) => {
                s.normal += 1;
                s.ranks.push(eval_rank(hit_ids, expect_id));
            }
        }
    }
    s
}

/// Parse the fixture. One JSON object per line, blank lines skipped, refusals worded as node words
/// them so an owner reading either client's error looks in the same place.
///
/// Two divergences from `bin/lumberroom.mjs`, both deliberate. Line numbers count lines in the file; node
/// drops the blanks first and numbers what is left, so its "line 4" sends the owner to the wrong
/// line of the file he is about to edit. And an empty fixture is refused here where node prints
/// `cases: 0 (0 normal, 0 anti-case)` and exits 0, which reads as a pass: a scheduled run against a
/// fixture nobody wrote would report success forever. The exit codes differ for that one input.
fn parse_eval_fixture(raw: &str, path: &str) -> Result<Vec<EvalCase>> {
    let mut cases = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|_| {
            let clip: String = line.chars().take(80).collect();
            err(format!("fixture line {} is not valid JSON: {clip}", i + 1))
        })?;
        let question = v
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| err(format!("fixture line {} has no question", i + 1)))?
            .to_string();
        // `expect === 'none'` and nothing looser. Every other value falls to the refusal below
        // rather than being read as an anti-case, because a fixture saying `"None"` is a fixture
        // whose author is guessing at the format.
        let anti = v.get("expect").and_then(Value::as_str) == Some("none");
        // Compared byte for byte later, so the id is stored as written. Only the emptiness test
        // trims, which is asking whether the key carries anything at all.
        let expect_id = v
            .get("expect_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        // Divergence from `bin/lumberroom.mjs`, and the one place this command does not copy it. Node
        // reads a line with neither key as a normal case, compares every hit id against `undefined`
        // and scores a guaranteed miss. A mistyped `expect_id` then shows up as a recall
        // regression, which sends the owner into the ranking to look for a bug in the store.
        let case = match (anti, expect_id) {
            (true, Some(_)) => {
                return Err(err(format!(
                    "fixture line {} names both expect_id and expect: \"none\". One says the store \
holds this row and the other says it holds nothing for the question.",
                    i + 1
                )))
            }
            (true, None) => EvalCase { question, expect_id: None, origin: origin_of(&v) },
            (false, Some(id)) => EvalCase { question, expect_id: Some(id), origin: origin_of(&v) },
            (false, None) => {
                return Err(err(format!(
                    "fixture line {} names neither expect_id nor expect: \"none\", so there is \
nothing to score it against. See client/eval-fixture.example.jsonl for the format.",
                    i + 1
                )))
            }
        };
        cases.push(case);
    }
    if cases.is_empty() {
        return Err(err(format!(
            "fixture {path} holds no cases. See client/eval-fixture.example.jsonl for the format."
        )));
    }
    Ok(cases)
}

fn origin_of(v: &Value) -> Option<String> {
    v.get("origin").and_then(Value::as_str).map(str::to_string)
}

/// `lumberroom eval [--fixture <path>] [--json]`.
///
/// Recall against questions the owner wrote, which is the opposite bias to a public benchmark: it
/// measures whether the store answers what he actually asks, and it can only test what he thought
/// to ask. `eval-longmemeval` is the other direction, a published haystack somebody else built.
///
/// An anti-case is pass or fail on its own and never enters the recall average. Silence about a
/// fact the store holds is one kind of failure; a confident answer about a fact it does not hold is
/// worse, and averaging the second into the first hides it.
pub async fn eval(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;

    // A bare `--fixture` would otherwise fall through to the default path and score a fixture the
    // owner did not name, which is a wrong answer rather than a missing one.
    if args.is_bare("fixture") {
        return Err(err("--fixture needs a value: the path to a JSONL fixture"));
    }
    // `$HOME/.config/lumberroom/eval-fixture.jsonl`, matching node. LUMBERROOM_CONFIG points at config.json and
    // does not move the fixture in either client.
    let fixture_path = args.value("fixture").map(str::to_string).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::Path::new(&home)
            .join(".config")
            .join("lumberroom")
            .join("eval-fixture.jsonl")
            .display()
            .to_string()
    });
    let raw = std::fs::read_to_string(&fixture_path).map_err(|_| {
        err(format!(
            "cannot read fixture {fixture_path}. See client/eval-fixture.example.jsonl for the format."
        ))
    })?;
    let cases = parse_eval_fixture(&raw, &fixture_path)?;

    // Five, because recall@5 is the deepest number printed. Node sends the same, and a deeper fetch
    // would change the figures without changing the question.
    const LIMIT: i64 = 5;
    let mut results: Vec<(EvalCase, Vec<String>)> = Vec::with_capacity(cases.len());
    for case in cases {
        // Namespace, project and history left at the default, the way an owner typing the question
        // would leave them.
        let req = wire::SearchArgsRequest {
            query: case.question.clone(),
            namespaces: None,
            limit: Some(LIMIT),
            project: None,
            as_of: None,
        };
        let output = c.call_tool("memory_search", serde_json::to_value(req).unwrap()).await?;
        let found: wire::SearchResult = typed(&output.structured, "memory_search")?;
        results.push((case, found.hits.into_iter().map(|h| h.id).collect()));
    }

    let s = eval_score(&results);
    let total = s.normal + s.anti;

    if args.present("json") {
        // Field names match `services::eval::EvalReport`, so an owner comparing a scheduled run
        // against a manual one is reading the same keys. Not in node, which prints text only.
        out_json(&json!({
            "cases": total,
            "normal_cases": s.normal,
            "anti_cases": s.anti,
            "recall_at_1": eval_recall_at(&s.ranks, 1),
            "recall_at_5": eval_recall_at(&s.ranks, 5),
            "mrr": eval_mrr(&s.ranks),
            "limit": LIMIT,
            "violations": s.violations.iter().map(|v| json!({
                "question": v.question,
                "origin": v.origin,
                "got": v.got,
            })).collect::<Vec<_>>(),
        }));
    } else {
        for line in eval_lines(&s) {
            out(&line);
        }
    }

    if !s.violations.is_empty() {
        // Divergence, and it is in the exit path rather than the output. Node prints the FAIL lines
        // to stdout and sets exitCode 1 with nothing on stderr. This client can only exit non-zero
        // by returning an error, which prints one `lumberroom: ` line to stderr. Both exit 1, both list
        // every violation on stdout, and the extra stderr line is what that costs.
        return Err(err_code(
            format!(
                "{} anti-case violation{}",
                s.violations.len(),
                if s.violations.len() == 1 { "" } else { "s" }
            ),
            1,
        ));
    }
    Ok(())
}

/// `Number.prototype.toFixed`, digit for digit.
///
/// Rust's `{:.1}` rounds a tie to the even digit and JavaScript rounds it to the larger number, so
/// the two disagree on values a fixture reaches: sixteen normal cases with one hit gives recall
/// 6.25%, which node prints as 6.3 and `{:.1}` prints as 6.2. One number under two names is the
/// thing this command exists to avoid.
///
/// Rounding runs over the decimal expansion rather than over `x * 10^f`, because that multiply
/// carries its own error and can move a value across the tie it is being asked about. Twenty spare
/// digits is past the exact expansion of every dyadic value these metrics produce.
fn to_fixed(x: f64, f: usize) -> String {
    if !x.is_finite() {
        return format!("{:.*}", f, x);
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    let expanded = format!("{:.*}", f + 20, x.abs());
    let (int_part, frac) = expanded.split_once('.').unwrap_or((expanded.as_str(), ""));
    let kept = &frac[..f];
    let tail = &frac[f..];
    // Half up: a tail of exactly 5 followed by zeros rounds away from zero, which is what the
    // specification's "pick the larger n" means for a positive value.
    let round_up = tail.chars().next().is_some_and(|c| c >= '5');
    let mut digits: Vec<u8> = format!("{int_part}{kept}").into_bytes();
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let s = String::from_utf8(digits).unwrap_or_default();
    if f == 0 {
        return format!("{sign}{s}");
    }
    let split = s.len() - f;
    format!("{sign}{}.{}", &s[..split], &s[split..])
}

/// The report, line for line as `bin/lumberroom.mjs` prints it. Split out so a test can read it without a
/// server: the alignment in `MRR:      ` and the percent formatting are the parity surface.
fn eval_lines(s: &EvalScore) -> Vec<String> {
    let total = s.normal + s.anti;
    let mut lines = vec![format!("cases: {total} ({} normal, {} anti-case)", s.normal, s.anti)];
    match (eval_recall_at(&s.ranks, 1), eval_recall_at(&s.ranks, 5), eval_mrr(&s.ranks)) {
        (Some(r1), Some(r5), Some(mrr)) => {
            lines.push(format!("recall@1: {}%", to_fixed(r1 * 100.0, 1)));
            lines.push(format!("recall@5: {}%", to_fixed(r5 * 100.0, 1)));
            lines.push(format!("MRR:      {}", to_fixed(mrr, 3)));
        }
        _ => lines.push("no normal cases; recall@1/@5/MRR not computed".to_string()),
    }
    lines.push(String::new());
    lines.push(format!("anti-case violations: {}", s.violations.len()));
    for v in &s.violations {
        lines.push(format!(
            "  FAIL  \"{}\"  ({})  returned {}",
            v.question,
            v.origin.as_deref().unwrap_or("unlabelled"),
            v.got.as_deref().unwrap_or("undefined")
        ));
    }
    lines
}

/// `encodeURIComponent` for a path segment. Ids are UUIDs, so this is a guard rather than a need.
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Args {
        Args::parse(argv.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_query_forget_with_no_selection_chooses_nothing() {
        // The whole point. memory_search returns its limit, so the caller has to name what they
        // meant before anything is deleted.
        assert_eq!(selection(&args(&["forget", "--query", "ports"]), 20).unwrap(), Selection::None);
    }

    #[test]
    fn pick_reads_the_printed_numbers_and_is_one_based() {
        assert_eq!(parse_pick("1,3", 20).unwrap(), vec![1, 3]);
        assert_eq!(parse_pick(" 2 , 2 , 5 ", 20).unwrap(), vec![2, 5]);
    }

    #[test]
    fn pick_refuses_a_number_that_is_not_on_the_list() {
        assert!(parse_pick("0", 20).is_err());
        assert!(parse_pick("21", 20).is_err());
        assert!(parse_pick("2,99", 20).is_err());
    }

    #[test]
    fn pick_refuses_something_that_is_not_a_number() {
        // "--pick all" is the mistake this catches: it would otherwise parse to an empty list and
        // read as "chose nothing" when the caller meant every row.
        assert!(parse_pick("all", 20).is_err());
        assert!(parse_pick("1-3", 20).is_err());
        assert!(parse_pick("", 20).is_err());
    }

    #[test]
    fn all_is_a_flag_the_caller_types_rather_than_a_default() {
        assert_eq!(
            selection(&args(&["forget", "--query", "ports", "--all"]), 20).unwrap(),
            Selection::All
        );
    }

    #[test]
    fn a_uuid_survives_url_encoding_unchanged() {
        let id = "9f1c2b4e-0000-4a1b-8c3d-1122334455aa";
        assert_eq!(urlencode(id), id);
    }

    #[test]
    fn a_path_traversal_id_is_escaped() {
        assert_eq!(urlencode("../admin"), "..%2Fadmin");
    }

    #[test]
    fn compact_json_has_no_spaces() {
        assert_eq!(compact(&json!({ "ok": true, "n": 1 })), "{\"ok\":true,\"n\":1}");
    }

    #[test]
    fn a_bare_date_reads_as_midnight_utc() {
        let parsed = parse_two_date_forms("occurred_at", "2026-03-01").expect("accepted");
        assert_eq!(parsed.to_rfc3339(), "2026-03-01T00:00:00+00:00");
    }

    #[test]
    fn a_full_instant_survives_with_its_offset_converted_to_utc() {
        let parsed =
            parse_two_date_forms("occurred_at", "2026-03-01T12:30:00+05:30").expect("accepted");
        assert_eq!(parsed.to_rfc3339(), "2026-03-01T07:00:00+00:00");
    }

    #[test]
    fn a_bare_month_is_refused_naming_both_forms() {
        // Exact equality against the server's own wording (src/mcp/tools.rs::refusal), so drift
        // between the two surfaces is caught here rather than found by an owner at 2am.
        let message = parse_two_date_forms("occurred_at", "2026-03").unwrap_err();
        assert_eq!(
            message,
            "occurred_at `2026-03` is not one of the two accepted forms. Pass a date, \
`2026-03-01`, read as midnight UTC, or a full RFC 3339 instant, `2026-03-01T09:30:00Z`. A bare \
month or year cannot be represented, so omit occurred_at rather than choosing a day."
        );
    }

    #[test]
    fn the_refusal_names_the_flags_own_field_for_since_and_until() {
        let message = parse_two_date_forms("since", "2026").unwrap_err();
        assert!(message.starts_with("since `2026`"), "{message}");
        assert!(message.ends_with("omit since rather than choosing a day."), "{message}");
    }

    #[test]
    fn a_bare_flag_with_no_value_is_refused_rather_than_read_as_absent() {
        let args = Args::parse(["write", "a fact", "--occurred-at"]);
        let err = optional_date_flag(&args, "occurred-at", "occurred_at").unwrap_err();
        assert!(err.message.contains("needs a value"), "{}", err.message);
    }

    #[test]
    fn an_absent_flag_is_none_not_an_error() {
        let args = Args::parse(["write", "a fact"]);
        assert_eq!(optional_date_flag(&args, "occurred-at", "occurred_at").unwrap(), None);
    }

    // ---- eval ----
    //
    // The metric vectors here are the ones `src/services/eval.rs` pins for the server's copy of the
    // same arithmetic. Change one and change both.

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn normal(question: &str, expect_id: &str) -> EvalCase {
        EvalCase { question: question.into(), expect_id: Some(expect_id.into()), origin: None }
    }

    fn anti(question: &str, origin: Option<&str>) -> EvalCase {
        EvalCase { question: question.into(), expect_id: None, origin: origin.map(str::to_string) }
    }

    #[test]
    fn rank_reads_the_first_position_and_nothing_after_it() {
        assert_eq!(eval_rank(&ids(&["a", "b", "c"]), "a"), Some(0));
        assert_eq!(eval_rank(&ids(&["a", "b", "c"]), "c"), Some(2));
        assert_eq!(eval_rank(&ids(&["a", "b"]), "z"), None);
        assert_eq!(eval_rank(&[], "a"), None);
    }

    #[test]
    fn a_mistyped_id_misses_here_exactly_as_it_misses_in_node() {
        // Kind is the wrong thing to be. Matching an uppercase or padded id would hand back a
        // recall number `bin/lumberroom.mjs` disagrees with on the same fixture.
        let stored = ids(&["9f1c2b4e-0000-4a1b-8c3d-1122334455aa"]);
        assert_eq!(eval_rank(&stored, "9f1c2b4e-0000-4a1b-8c3d-1122334455aa"), Some(0));
        assert_eq!(eval_rank(&stored, "9F1C2B4E-0000-4A1B-8C3D-1122334455AA"), None);
        assert_eq!(eval_rank(&stored, " 9f1c2b4e-0000-4a1b-8c3d-1122334455aa"), None);
    }

    #[test]
    fn an_expect_value_other_than_the_exact_word_none_is_refused() {
        let e = parse_eval_fixture("{\"question\":\"q\",\"expect\":\"None\"}\n", "f").unwrap_err();
        assert!(e.message.contains("neither expect_id nor"), "{}", e.message);
    }

    #[test]
    fn recall_counts_a_miss_as_zero_rather_than_dropping_it() {
        let ranks = vec![Some(0), None, Some(3)];
        assert_eq!(eval_recall_at(&ranks, 1), Some(1.0 / 3.0));
        assert_eq!(eval_recall_at(&ranks, 5), Some(2.0 / 3.0));
    }

    #[test]
    fn recall_over_no_cases_is_absent_rather_than_zero() {
        assert_eq!(eval_recall_at(&[], 1), None);
        assert_eq!(eval_mrr(&[]), None);
    }

    #[test]
    fn mrr_pins_to_a_hand_computed_value() {
        // 1/1 + 0 + 1/4, over three cases.
        let got = eval_mrr(&[Some(0), None, Some(3)]).expect("three cases");
        assert!((got - (1.25 / 3.0)).abs() < 1e-12, "{got}");
    }

    #[test]
    fn a_case_that_never_appears_scores_zero_everywhere() {
        let s = eval_score(&[(normal("where do I live", "row-1"), ids(&["x", "y"]))]);
        assert_eq!(s.normal, 1);
        assert_eq!(eval_recall_at(&s.ranks, 1), Some(0.0));
        assert_eq!(eval_recall_at(&s.ranks, 5), Some(0.0));
        assert_eq!(eval_mrr(&s.ranks), Some(0.0));
        assert!(s.violations.is_empty());
    }

    #[test]
    fn an_anti_case_that_returns_anything_is_a_violation_and_stays_out_of_recall() {
        let s = eval_score(&[
            (normal("what os do I run", "row-1"), ids(&["row-1"])),
            (anti("my bank account number", Some("never stored")), ids(&["row-9", "row-8"])),
        ]);
        assert_eq!(s.normal, 1);
        assert_eq!(s.anti, 1);
        // The perfect normal case stays perfect. A violation is neither averaged away nor allowed
        // to drag the recall number, because the two say different things.
        assert_eq!(eval_recall_at(&s.ranks, 1), Some(1.0));
        assert_eq!(eval_mrr(&s.ranks), Some(1.0));
        assert_eq!(s.violations.len(), 1);
        assert_eq!(s.violations[0].got.as_deref(), Some("row-9"));
    }

    #[test]
    fn a_quiet_anti_case_is_no_violation() {
        let s = eval_score(&[(anti("a wifi password I never mentioned", None), vec![])]);
        assert!(s.violations.is_empty());
        assert_eq!(s.anti, 1);
        assert_eq!(s.normal, 0);
    }

    #[test]
    fn an_empty_result_set_scores_to_an_empty_report() {
        let s = eval_score(&[]);
        assert_eq!(s.normal, 0);
        assert_eq!(s.anti, 0);
        assert_eq!(eval_recall_at(&s.ranks, 1), None);
        assert!(s.violations.is_empty());
    }

    #[test]
    fn a_fixture_of_only_anti_cases_says_so_instead_of_printing_a_recall_of_zero() {
        let s = eval_score(&[(anti("q1", None), vec![]), (anti("q2", None), ids(&["row-2"]))]);
        assert_eq!(
            eval_lines(&s),
            vec![
                "cases: 2 (0 normal, 2 anti-case)".to_string(),
                "no normal cases; recall@1/@5/MRR not computed".to_string(),
                String::new(),
                "anti-case violations: 1".to_string(),
                "  FAIL  \"q2\"  (unlabelled)  returned row-2".to_string(),
            ]
        );
    }

    #[test]
    fn the_report_matches_bin_lumberroom_mjs_line_for_line() {
        // Generated by running `bin/lumberroom.mjs eval`'s own scoring and printing over these five
        // cases with the searches stubbed out. Every character here came from node, including the
        // six spaces after `MRR:` and the rounding of 66.666...% to 66.7.
        let s = eval_score(&[
            (normal("what os does my desktop run", "row-1"), ids(&["row-1", "x"])),
            (normal("how do I deploy lumberroom", "row-2"), ids(&["a", "b", "row-2"])),
            (normal("what did I decide about the rust rewrite", "row-3"), vec![]),
            (anti("my bank account number", Some("never stored")), ids(&["row-9", "row-8"])),
            (anti("a wifi password I never mentioned", None), vec![]),
        ]);
        assert_eq!(
            eval_lines(&s),
            vec![
                "cases: 5 (3 normal, 2 anti-case)".to_string(),
                "recall@1: 33.3%".to_string(),
                "recall@5: 66.7%".to_string(),
                "MRR:      0.444".to_string(),
                String::new(),
                "anti-case violations: 1".to_string(),
                "  FAIL  \"my bank account number\"  (never stored)  returned row-9".to_string(),
            ]
        );
    }

    #[test]
    fn a_tie_rounds_the_way_javascript_rounds_it() {
        // Sixteen normal cases with one hit. `{:.1}` gives 6.2 and node gives 6.3, which is the
        // whole reason `to_fixed` exists. Checked against node over every recall percentage a
        // fixture of up to 64 normal cases can produce.
        assert_eq!(to_fixed(6.25, 1), "6.3");
        assert_eq!(to_fixed(18.75, 1), "18.8");
        assert_eq!(to_fixed(0.0625, 3), "0.063");
        assert_eq!(to_fixed(12.5, 1), "12.5");
        assert_eq!(to_fixed(100.0, 1), "100.0");
        assert_eq!(to_fixed(0.0, 3), "0.000");
        assert_eq!(to_fixed(99.99, 1), "100.0");
        assert_eq!(to_fixed(1.0 / 3.0 * 100.0, 1), "33.3");
        assert_eq!(to_fixed(2.0 / 3.0 * 100.0, 1), "66.7");
    }

    #[test]
    fn the_fixture_example_checked_into_the_repo_parses() {
        let raw = include_str!("../../../client/eval-fixture.example.jsonl");
        let cases = parse_eval_fixture(raw, "example").expect("the documented format parses");
        assert_eq!(cases.len(), 5);
        assert_eq!(cases[0].expect_id.as_deref(), Some("11111111-1111-4111-8111-111111111111"));
        assert_eq!(cases[3].expect_id, None, "expect: none is an anti-case");
        assert!(cases[3].origin.as_deref().is_some_and(|o| o.starts_with("anti-case")));
    }

    #[test]
    fn blank_lines_are_skipped_and_a_broken_line_names_its_number() {
        let raw = "\n{\"question\":\"a\",\"expect_id\":\"row-1\"}\n\n  \n{\"question\":\"b\",\"expect\":\"none\"}\n";
        assert_eq!(parse_eval_fixture(raw, "f").expect("parses").len(), 2);

        let broken = "{\"question\":\"a\",\"expect_id\":\"row-1\"}\nnot json at all\n";
        let e = parse_eval_fixture(broken, "f").unwrap_err();
        assert_eq!(e.message, "fixture line 2 is not valid JSON: not json at all");
    }

    #[test]
    fn a_case_that_scores_against_nothing_is_refused_rather_than_counted_as_a_miss() {
        // The divergence from node, and the reason for it: node reads this line as a normal case,
        // finds no hit whose id equals `undefined`, and books a silent zero.
        let e = parse_eval_fixture("{\"question\":\"what os\"}\n", "f").unwrap_err();
        assert!(e.message.contains("neither expect_id nor"), "{}", e.message);

        let both = "{\"question\":\"q\",\"expect\":\"none\",\"expect_id\":\"row-1\"}\n";
        let e = parse_eval_fixture(both, "f").unwrap_err();
        assert!(e.message.contains("names both"), "{}", e.message);

        let e = parse_eval_fixture("{\"expect\":\"none\"}\n", "f").unwrap_err();
        assert_eq!(e.message, "fixture line 1 has no question");
    }

    #[test]
    fn an_empty_fixture_is_refused_by_name() {
        let e = parse_eval_fixture("\n\n", "/tmp/f.jsonl").unwrap_err();
        assert!(e.message.starts_with("fixture /tmp/f.jsonl holds no cases"), "{}", e.message);
    }
}

/// Below this the two arms took comparable time, which means one plan ran twice.
const SELF_COMPARISON_SPEEDUP: f64 = 2.0;

/// How much slower the exact scan was than the indexed one, or `None` when the timings are too
/// small to divide.
fn two_arm_speedup(report: &Value) -> Option<f64> {
    if let Some(given) = report.get("exact_speedup").and_then(Value::as_f64) {
        return Some(given);
    }
    let index_ms = report.get("index_ms").and_then(Value::as_f64)?;
    let exact_ms = report.get("exact_ms").and_then(Value::as_f64)?;
    if !index_ms.is_finite() || !exact_ms.is_finite() || index_ms <= 0.0 {
        return None;
    }
    Some(exact_ms / index_ms)
}

/// The HNSW recall monitor.
///
/// The recall figure is the least trustworthy line this command prints. The monitor has twice
/// compared an exact scan against an exact scan and reported perfect recall: once because
/// `SET LOCAL` on a pooled connection outside a transaction is a warning and no effect, and once
/// because the planner declines the index at k=1. Neither showed up in the recall number and both
/// showed up as two timings that matched, which is why the timings are printed and guarded rather
/// than summarised away.
pub async fn recall(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let sample = args.int("sample", 25);
    let k = args.int("k", 10);
    let (status, body) = c.http_get(&format!("/admin/recall?sample={sample}&k={k}")).await?;
    if status != 200 {
        return Err(err(format!("recall failed ({status}): {}", compact(&body))));
    }
    if args.present("json") {
        out_json(&body);
        return Ok(());
    }

    let sampled = body.get("sampled").and_then(Value::as_i64).unwrap_or(0);
    if sampled == 0 {
        out("store is empty, nothing to measure");
        return Ok(());
    }
    let k_used = body.get("k").and_then(Value::as_i64).unwrap_or(k);
    let recall_at_k = body.get("recall_at_k").and_then(Value::as_f64).unwrap_or(0.0);
    let misses = body.get("top_one_misses").and_then(Value::as_i64).unwrap_or(0);
    let index_ms = body.get("index_ms").and_then(Value::as_f64).unwrap_or(0.0);
    let exact_ms = body.get("exact_ms").and_then(Value::as_f64).unwrap_or(0.0);

    out(&format!(
        "sampled {sampled} stored memories, comparing indexed search against an exact scan"
    ));
    out(&format!("recall@{k_used}: {:.1}%", recall_at_k * 100.0));
    out(&format!("nearest-neighbour misses: {misses} of {sampled}"));
    out(&format!("indexed {index_ms}ms total, exact {exact_ms}ms total"));

    let speedup = two_arm_speedup(&body);
    let self_comparison = speedup.is_some_and(|s| s < SELF_COMPARISON_SPEEDUP);
    match speedup {
        None => {
            out("");
            out("WARNING: the timings are too small to divide, so nothing here says the index ran at");
            out("all. Raise --sample, or run this against a store with more rows in it.");
        }
        Some(s) => out(&format!("exact scan took {s:.1}x the indexed time")),
    }
    if self_comparison {
        out("");
        out("WARNING: the two arms took comparable time, so this is very likely an exact scan");
        out("compared against an exact scan and the recall figure above means nothing. The");
        out("planner declines the index at small k and on small stores. Raise k, or seed more");
        out("rows, then run it again.");
    }

    if recall_at_k < 0.9 && !self_comparison {
        out("");
        out("recall is below 90%. Raise hnsw.ef_search, or rebuild the index with a higher");
        out("ef_construction. Weakest queries:");
        for w in body.get("worst").and_then(Value::as_array).unwrap_or(&vec![]) {
            let pct = w.get("recall").and_then(Value::as_f64).unwrap_or(0.0) * 100.0;
            let query = w.get("query").and_then(Value::as_str).unwrap_or("");
            out(&format!("  {pct:.0}%  {query}"));
        }
    }
    Ok(())
}

/// Every tool this credential can call, as the server lists them.
///
/// `tools/list` is filtered per credential, so this doubles as the answer to what a grant opens.
pub async fn tools(c: &Client) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    c.initialize().await?;
    let result = c.rpc("tools/list", json!({})).await?;
    for tool in result.get("tools").and_then(Value::as_array).unwrap_or(&vec![]) {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        let description = tool.get("description").and_then(Value::as_str).unwrap_or("");
        out(&format!("{name}\n  {description}\n"));
    }
    Ok(())
}

/// argon2 lives in the server image, so this prints the command rather than hashing here.
///
/// Computing it locally would mean either a second argon2 implementation to keep in step with the
/// one that verifies, or a weaker hash that looks the same in `.env`.
pub fn hash_password() -> Result<()> {
    out("argon2 is not in this client, so this does not compute a hash itself.");
    out("Run it inside the server image, which already links argon2:");
    out("");
    out("  docker compose run --rm -T server lumberroom-server hash-password");
    out("");
    out("It reads the password from stdin and prints an argon2 PHC string. Put that in .env as");
    out("OWNER_PASSWORD_HASH and restart the server before switching AUTH_MODE=oauth.");
    Ok(())
}

/// `lumberroom archive export|import`, a new top-level group.
///
/// Not a mode under `export`, which already means `--obsidian`, and not a subcommand of `import`,
/// whose subcommand list is `prompt, claude, memory-dump` and whose documented promise is that
/// nothing reaches the store without review. An archive import writes directly, so it cannot live
/// under a word that promises otherwise.
pub async fn archive(c: &Client, args: &Args) -> Result<()> {
    match args.positional_at(1) {
        Some("export") => archive_export(c, args).await,
        Some("import") => archive_import(c, args).await,
        Some(other) => {
            Err(err(format!("unknown archive subcommand `{other}`. Available: export, import")))
        }
        None => Err(err("archive needs a subcommand. Available: export, import")),
    }
}

/// One line off stdin, trimmed of the newline a shell or a piped `echo` leaves on it. Never from
/// argv: `ps` shows every argument on the box to every user on it, and a passphrase is the one
/// flag value that must not appear there.
fn read_stdin_passphrase() -> Result<String> {
    let line =
        read_line().map_err(|e| err(format!("cannot read the passphrase from stdin: {e}")))?;
    let phrase = line.trim_end_matches(['\n', '\r']).to_string();
    if phrase.is_empty() {
        return Err(err("--passphrase-stdin was set but stdin carried no passphrase"));
    }
    Ok(phrase)
}

/// Either a passphrase or an explicit opt into plaintext, never both and never neither. Mirrors
/// the refusal `crates/ops/src/backup.rs:26` already gives a plaintext database dump behind the
/// same `--allow-plaintext` flag: an archive holds every private fact in the store, so leaving
/// both flags off is refused rather than read as a default.
fn resolve_passphrase(args: &Args, verb: &str) -> Result<Option<String>> {
    let plaintext = args.present("allow-plaintext");
    let requested = args.present("passphrase-stdin");
    match (requested, plaintext) {
        (true, true) => Err(err("pass --passphrase-stdin or --allow-plaintext, not both")),
        (false, false) => {
            Err(err(format!("archive {verb} needs --passphrase-stdin or --allow-plaintext")))
        }
        (true, false) => read_stdin_passphrase().map(Some),
        (false, true) => Ok(None),
    }
}

/// `lumberroom archive export <path> (--passphrase-stdin | --allow-plaintext)`
///
/// The server does the sealing: it holds the age implementation already and the alternative is a
/// second one in this crate agreeing with it byte for byte. The passphrase goes in the request
/// body, so this posts rather than gets: an intermediary is free to drop a body on a GET, and the
/// route answers both verbs for exactly that reason.
///
/// `--allow-plaintext` travels as its own field. A missing passphrase alone is not consent, and
/// the server refuses that request rather than writing a file anyone holding it can read.
pub async fn archive_export(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let Some(target) = args.positional_at(2) else {
        return Err(err(
            "usage: lumberroom archive export <path> (--passphrase-stdin | --allow-plaintext)",
        ));
    };
    let passphrase = resolve_passphrase(args, "export")?;
    // Consent is derived from the passphrase that actually travels, so no request can ask for a
    // plaintext archive while carrying a passphrase.
    let req = wire::ArchiveExportRequest { allow_plaintext: passphrase.is_none(), passphrase };
    let body = serde_json::to_value(&req)
        .map_err(|e| err(format!("cannot encode the archive request: {e}")))?;
    let (status, bytes) =
        c.http_send_bytes(reqwest::Method::POST, "/admin/archive/export", Some(body)).await?;
    if status != 200 {
        let detail = String::from_utf8_lossy(&bytes);
        return Err(err(format!(
            "archive export failed ({status}): {}",
            crate::client::truncate(&detail, 300)
        )));
    }
    std::fs::write(target, &bytes).map_err(|e| err(format!("cannot write {target}: {e}")))?;
    out(&format!("wrote {} bytes to {target}", bytes.len()));
    Ok(())
}

/// `lumberroom archive import <path> (--passphrase-stdin | --allow-plaintext) [--restore] [--dry-run]`
///
/// Merge is the default and the only mode the hosted console offers; `--restore` is a CLI-only
/// capability that reproduces a store exactly rather than folding it into one. `--dry-run` asks
/// for the report without writing anything, which is the one safety net an archive import gets in
/// place of the review queue every other import command holds facts in first.
pub async fn archive_import(c: &Client, args: &Args) -> Result<()> {
    require_token(c, &c.file.borrow().path.display().to_string())?;
    let Some(source) = args.positional_at(2) else {
        return Err(err(
            "usage: lumberroom archive import <path> (--passphrase-stdin | --allow-plaintext) \
             [--restore] [--dry-run]",
        ));
    };
    let passphrase = resolve_passphrase(args, "import")?;
    let bytes = std::fs::read(source).map_err(|e| err(format!("cannot read {source}: {e}")))?;
    let restore = args.present("restore");
    let dry_run = args.present("dry-run");
    // Both flags are derived from what actually travels rather than read off argv a second time,
    // so no request can claim a plaintext archive while carrying a passphrase.
    let req = wire::ArchiveImportRequest {
        archive_base64: B64.encode(&bytes),
        allow_plaintext: passphrase.is_none(),
        passphrase,
        restore,
        dry_run,
    };
    let body = serde_json::to_value(&req)
        .map_err(|e| err(format!("cannot encode the archive request: {e}")))?;
    let (status, resp) =
        c.http_request(reqwest::Method::POST, "/admin/archive/import", Some(body)).await?;
    if status != 200 {
        return Err(err(format!("archive import failed ({status}): {}", compact(&resp))));
    }
    if args.present("json") {
        out_json(&resp);
        return Ok(());
    }
    let report: wire::ApplyReport = typed(&resp, "archive import")?;
    let mode_word = if restore { "restored" } else { "merged" };
    let verb = if dry_run { format!("would have {mode_word}") } else { mode_word.to_string() };
    out(&format!(
        "{verb} {} rows, skipped {} already applied, collapsed {} duplicates, refused {}",
        report.applied,
        report.skipped_already_applied,
        report.collapsed,
        report.refused.len()
    ));
    for (id, reason) in &report.refused {
        out(&format!("  refused {id}: {reason}"));
    }
    Ok(())
}
