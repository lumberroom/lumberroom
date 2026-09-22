//! `lumberroom review`: the loop over `GET /admin/review/queue` and `POST /admin/review/decide`,
//! the non-prompting flags of spec 6.2, and `--dates`/`--registry`, which moved here unchanged.
//!
//! `Answer` carries a resolved verdict and, for `s`/`o`, the row to keep, rather than a full
//! `DecisionRequest`: a merge or a delete needs text or a target the key press has not gathered
//! yet, and a request type borrowing from data that does not exist at that point is a lifetime
//! knot with no payoff. `post_decision` is the one place a `DecisionRequest` gets built, from
//! locals that outlive the call.

use serde_json::Value;

use crate::args::Args;
use crate::client::{err, Client, Result};
use crate::commands::{compact, parse_two_date_forms, require_token, typed, urlencode};
use crate::{out, out_json, wire};

/// `lumberroom review`, every form. Every read of stdin goes through `read_line`.
pub async fn run(
    c: &Client,
    args: &Args,
    read_line: &mut impl FnMut() -> std::io::Result<String>,
) -> Result<()> {
    let config_path = c.file.borrow().path.display().to_string();
    require_token(c, &config_path)?;

    if args.present("dates") {
        return run_dates(c, args).await;
    }
    if args.present("registry") {
        return run_registry(c, args).await;
    }
    if args.present("dismissed") {
        return run_dismissed(c, args).await;
    }
    if let Some(raw) = args.value("undismiss") {
        return run_undismiss(c, raw).await;
    }

    if let Some(action) = flag_decision(args)? {
        return run_flag_decision(c, action, args.present("json"), read_line).await;
    }

    if args.present("json") {
        return run_json(c, args).await;
    }

    run_loop(c, args, read_line).await
}

// ---- the interactive loop ----

async fn run_loop(
    c: &Client,
    args: &Args,
    read_line: &mut impl FnMut() -> std::io::Result<String>,
) -> Result<()> {
    let limit = args.int("limit", 50).clamp(1, 200);
    let mut offset: i64 = args.int("offset", 0).max(0);
    let mut skipped: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut running = Tally::default();

    'paging: loop {
        let queue = fetch_queue(c, args, offset, limit).await?;
        running.dismissed_in_ledger = queue.dismissed;

        if queue.items.iter().all(|it| skipped.contains(&it.key)) {
            if queue.has_more && !queue.items.is_empty() {
                offset += limit;
                continue 'paging;
            }
            break 'paging;
        }

        let total = queue.items.len();
        for (i, item) in queue.items.iter().enumerate() {
            if skipped.contains(&item.key) {
                continue;
            }
            print_header(&queue, item, i + 1, total);
            print_rows(item);

            'answer: loop {
                out(&key_line(&item.verdicts, item.rows.len() > 1));
                crate::prompt("> ");
                let line =
                    read_line().map_err(|e| err(format!("cannot read the answer: {e}")))?;
                if line.is_empty() {
                    out(&tally(&running));
                    return Ok(());
                }
                match answer_to_decision(item, line.trim()) {
                    Answer::Again => continue 'answer,
                    Answer::Skip => {
                        skipped.insert(item.key.clone());
                        running.read += 1;
                        running.skipped += 1;
                        break 'answer;
                    }
                    Answer::Quit => {
                        out(&tally(&running));
                        return Ok(());
                    }
                    Answer::Act { verdict, keep } => {
                        report_decision(
                            c,
                            &item.key,
                            verdict,
                            keep.as_deref(),
                            None,
                            None,
                            None,
                            None,
                            &mut running,
                        )
                        .await;
                        offset = 0;
                        continue 'paging;
                    }
                    Answer::NeedsText => match merge_text(read_line)? {
                        None => continue 'answer,
                        Some(text) => {
                            report_decision(
                                c,
                                &item.key,
                                wire::Verdict::Merge,
                                None,
                                None,
                                Some(&text),
                                None,
                                None,
                                &mut running,
                            )
                            .await;
                            offset = 0;
                            continue 'paging;
                        }
                    },
                    Answer::NeedsDelete => {
                        let which = if item.rows.len() > 1 {
                            crate::prompt("delete which? (o/n): ");
                            let which =
                                read_line().map_err(|e| err(format!("cannot read the answer: {e}")))?;
                            if which.is_empty() {
                                out(&tally(&running));
                                return Ok(());
                            }
                            which
                        } else {
                            String::new()
                        };
                        let Some(target) = delete_target(item, which.trim()) else {
                            out("not a row on this item");
                            continue 'answer;
                        };
                        match confirm_delete(read_line, &target)? {
                            None => {
                                out(&tally(&running));
                                return Ok(());
                            }
                            Some(false) => {
                                out("aborted, nothing deleted");
                                continue 'answer;
                            }
                            Some(true) => {}
                        }
                        report_decision(
                            c,
                            &item.key,
                            wire::Verdict::Delete,
                            None,
                            Some(&target),
                            None,
                            None,
                            None,
                            &mut running,
                        )
                        .await;
                        offset = 0;
                        continue 'paging;
                    }
                }
            }
        }

        if queue.has_more {
            offset += limit;
            continue 'paging;
        }
        break 'paging;
    }

    out(&tally(&running));
    Ok(())
}

/// Sends the decision, prints the server's line or its refusal, and updates the running tally.
/// A refusal never advances the tally: nothing changed in the store.
#[allow(clippy::too_many_arguments)]
async fn report_decision(
    c: &Client,
    key: &str,
    verdict: wire::Verdict,
    keep: Option<&str>,
    id: Option<&str>,
    content: Option<&str>,
    tags: Option<Vec<String>>,
    occurred_at: Option<String>,
    running: &mut Tally,
) {
    match post_decision(c, key, verdict, keep, id, content, tags, occurred_at, None).await {
        Ok((_, d)) => {
            out(&one_line(&d));
            running.read += 1;
            bump(running, verdict);
        }
        Err(e) => out(&format!("refused: {}", e.message)),
    }
}

fn bump(t: &mut Tally, verdict: wire::Verdict) {
    match verdict {
        wire::Verdict::Supersede => t.superseded += 1,
        wire::Verdict::Merge => t.merged += 1,
        wire::Verdict::KeepBoth => t.kept_both += 1,
        wire::Verdict::Delete => t.deleted += 1,
        wire::Verdict::Confirm => t.confirmed += 1,
        wire::Verdict::Apply => t.applied += 1,
        wire::Verdict::Dismiss => t.dismissed += 1,
    }
}

fn print_header(queue: &wire::ReviewQueue, item: &wire::ReviewItem, pos: usize, total: usize) {
    let mut line = format!("[{pos}/{total}] {}", source_label(item.source));
    if let Some(sim) = item.similarity {
        line.push_str(&format!("  {sim:.3}"));
    }
    if let Some(age) = item.age_days {
        line.push_str(&format!("  {age}d old"));
    }
    line.push_str(&format!("  {}", item.namespace));
    if queue.dismissed > 0 {
        line.push_str(&format!("  ({} dismissed)", queue.dismissed));
    }
    out(&line);
}

fn source_label(source: wire::Source) -> &'static str {
    match source {
        wire::Source::Conflict => "conflict",
        wire::Source::Stale => "stale",
        wire::Source::Proposal => "proposal",
    }
}

fn print_rows(item: &wire::ReviewItem) {
    let labels: &[&str] = if item.rows.len() == 2 { &["older", "newer"] } else { &["row"] };
    for (i, row) in item.rows.iter().enumerate() {
        let label = labels.get(i).copied().unwrap_or("row");
        let day = &row.created_at[..row.created_at.len().min(10)];
        let reads =
            if row.opened { format!("read {}x", row.access_count) } else { "unopened".to_string() };
        out(&format!("  {label:<6} {}  {day}  {reads}", short_id(&row.id)));
        let content =
            if row.opened { row.content.clone() } else { "(not opened for this caller)".to_string() };
        out(&format!("         {content}"));
    }
    if let Some(p) = &item.proposal {
        if let Some(pc) = &p.proposed_content {
            out(&format!("  proposed: {pc}"));
        }
        for f in &p.fields {
            out(&format!("  {}: {}", f.label, f.value));
        }
    }
    if item.verdicts.is_empty() {
        out("  (read only)");
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

// ---- key press to decision ----

/// `Answer::Act` carries a resolved verdict and, for `s`/`o`, the row to keep. Everything else the
/// server needs for a merge or a delete comes from a later prompt, which is why those two answers
/// carry no request of their own.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Act { verdict: wire::Verdict, keep: Option<String> },
    NeedsText,
    NeedsDelete,
    Skip,
    Quit,
    Again,
}

/// One trimmed line. Anything but a single character in the item's list is `Again`, so a line left
/// over from a pasted merge redraws the prompt rather than deciding the next item.
pub fn answer_to_decision(item: &wire::ReviewItem, answer: &str) -> Answer {
    let answer = answer.trim();
    if answer.chars().count() != 1 {
        return Answer::Again;
    }
    let ch = answer.chars().next().expect("one character checked above");
    if ch == 'n' {
        return Answer::Skip;
    }
    if ch == 'q' {
        return Answer::Quit;
    }
    let has = |v: wire::Verdict| item.verdicts.contains(&v);
    let pair = item.rows.len() > 1;
    match ch {
        's' if pair && has(wire::Verdict::Supersede) => Answer::Act {
            verdict: wire::Verdict::Supersede,
            keep: item.rows.get(1).map(|r| r.id.clone()),
        },
        'o' if pair && has(wire::Verdict::Supersede) => Answer::Act {
            verdict: wire::Verdict::Supersede,
            keep: item.rows.first().map(|r| r.id.clone()),
        },
        'm' if has(wire::Verdict::Merge) => Answer::NeedsText,
        'k' if has(wire::Verdict::KeepBoth) => {
            Answer::Act { verdict: wire::Verdict::KeepBoth, keep: None }
        }
        'd' if has(wire::Verdict::Delete) => Answer::NeedsDelete,
        'c' if has(wire::Verdict::Confirm) => {
            Answer::Act { verdict: wire::Verdict::Confirm, keep: None }
        }
        'a' if has(wire::Verdict::Apply) => Answer::Act { verdict: wire::Verdict::Apply, keep: None },
        'x' if has(wire::Verdict::Dismiss) => {
            Answer::Act { verdict: wire::Verdict::Dismiss, keep: None }
        }
        _ => Answer::Again,
    }
}

/// The key line drawn from what this item takes, in the table's own order, `n` and `q` always last.
pub fn key_line(verdicts: &[wire::Verdict], pair: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if pair && verdicts.contains(&wire::Verdict::Supersede) {
        parts.push("s keep newer");
        parts.push("o keep older");
    }
    if verdicts.contains(&wire::Verdict::Merge) {
        parts.push("m merge");
    }
    if verdicts.contains(&wire::Verdict::KeepBoth) {
        parts.push("k keep both");
    }
    if verdicts.contains(&wire::Verdict::Delete) {
        parts.push("d delete");
    }
    if verdicts.contains(&wire::Verdict::Confirm) {
        parts.push("c confirm");
    }
    if verdicts.contains(&wire::Verdict::Apply) {
        parts.push("a apply");
    }
    if verdicts.contains(&wire::Verdict::Dismiss) {
        parts.push("x dismiss");
    }
    parts.push("n skip");
    parts.push("q quit");
    parts.join("   ")
}

/// `m`: read lines until a blank one and join them with a space. `None` when nothing was typed,
/// which aborts to the key line. Reading to the blank line is what stops a two-line paste leaving
/// its second line to be eaten as the next key press.
pub fn merge_text(read: &mut impl FnMut() -> std::io::Result<String>) -> Result<Option<String>> {
    let mut lines: Vec<String> = Vec::new();
    loop {
        let raw = read().map_err(|e| err(format!("cannot read the merged text: {e}")))?;
        let line = raw.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            break;
        }
        lines.push(line.to_string());
    }
    if lines.is_empty() {
        Ok(None)
    } else {
        Ok(Some(lines.join(" ")))
    }
}

/// `d`: which row on a pair, then the first eight characters of the id typed back.
pub fn delete_target(item: &wire::ReviewItem, which: &str) -> Option<String> {
    if item.rows.len() <= 1 {
        return item.rows.first().map(|r| r.id.clone());
    }
    match which.trim() {
        "o" => item.rows.first().map(|r| r.id.clone()),
        "n" => item.rows.get(1).map(|r| r.id.clone()),
        _ => None,
    }
}

pub fn delete_confirmed(id: &str, typed: &str) -> bool {
    let want: String = id.chars().take(8).collect();
    typed.trim() == want
}

/// `None` on a zero-byte read (stdin closed), so a caller mid-loop can end at the tally rather than
/// treat a closed stream as a typed "no" and spin back to the key line forever.
fn confirm_delete(
    read_line: &mut impl FnMut() -> std::io::Result<String>,
    id: &str,
) -> Result<Option<bool>> {
    crate::prompt(&format!("type the first 8 characters of {id} to confirm: "));
    let line = read_line().map_err(|e| err(format!("cannot read the confirmation: {e}")))?;
    if line.is_empty() {
        return Ok(None);
    }
    Ok(Some(delete_confirmed(id, line.trim())))
}

// ---- the tally ----

/// One count per verdict plus `read` and `skipped`, and `dismissed_in_ledger` from the envelope,
/// which the tally prints beside the rest.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    pub read: usize,
    pub superseded: usize,
    pub merged: usize,
    pub kept_both: usize,
    pub deleted: usize,
    pub confirmed: usize,
    pub applied: usize,
    pub dismissed: usize,
    pub skipped: usize,
    pub dismissed_in_ledger: i64,
}

pub fn tally(t: &Tally) -> String {
    let mut parts: Vec<String> = Vec::new();
    if t.superseded > 0 {
        parts.push(format!("{} superseded", t.superseded));
    }
    if t.merged > 0 {
        parts.push(format!("{} merged", t.merged));
    }
    if t.kept_both > 0 {
        parts.push(format!("{} kept", t.kept_both));
    }
    if t.deleted > 0 {
        parts.push(format!("{} deleted", t.deleted));
    }
    if t.confirmed > 0 {
        parts.push(format!("{} confirmed", t.confirmed));
    }
    if t.applied > 0 {
        parts.push(format!("{} applied", t.applied));
    }
    if t.dismissed > 0 {
        parts.push(format!("{} dismissed", t.dismissed));
    }
    if t.skipped > 0 {
        parts.push(format!("{} skipped", t.skipped));
    }
    let mut line = format!("{} read", t.read);
    if !parts.is_empty() {
        line.push_str(&format!(": {}", parts.join(", ")));
    }
    if t.dismissed_in_ledger > 0 {
        line.push_str(&format!(", {} dismissed in the ledger", t.dismissed_in_ledger));
    }
    line
}

/// The server's one-line answer to a decision, printed after every act.
pub fn one_line(d: &wire::Decided) -> String {
    match d.verdict {
        wire::Verdict::Supersede => {
            let ids = key_pair_ids(&d.key);
            let retired = d.superseded.first().cloned().unwrap_or_default();
            let kept = ids.into_iter().find(|id| id != &retired).unwrap_or_default();
            format!("retired {retired} into {kept}")
        }
        wire::Verdict::Merge => {
            let written = d.written.as_deref().unwrap_or("");
            let mut line = format!("wrote {written} and retired {}", d.superseded.len());
            if !d.unfinished.is_empty() {
                line.push_str(&format!("; unfinished: {}", d.unfinished.join(", ")));
            }
            line
        }
        wire::Verdict::KeepBoth => {
            if d.already_dismissed {
                "kept both (already dismissed)".to_string()
            } else {
                "kept both".to_string()
            }
        }
        wire::Verdict::Delete => format!("deleted {}", d.deleted.first().cloned().unwrap_or_default()),
        wire::Verdict::Confirm => "confirmed".to_string(),
        wire::Verdict::Apply => d.proposal_state.clone().unwrap_or_else(|| "applied".to_string()),
        wire::Verdict::Dismiss => d.proposal_state.clone().unwrap_or_else(|| "dismissed".to_string()),
    }
}

fn key_pair_ids(key: &str) -> Vec<String> {
    key.split(':').skip(1).map(|s| s.to_string()).collect()
}

// ---- flags that decide without prompting (spec 6.2) ----

/// One resolved action from a single flag. `flag_decision` refuses two action flags at once and
/// a `--merge` with no `--content`, since nothing in the engine writes that text.
#[derive(Debug)]
pub struct FlagAction {
    pub key: String,
    pub verdict: wire::Verdict,
    pub keep: Option<String>,
    pub id: Option<String>,
    pub content: Option<String>,
    pub tags: Option<Vec<String>>,
    pub occurred_at: Option<String>,
    pub reason: Option<String>,
    pub needs_delete_confirmation: bool,
}

/// `--supersede`, `--merge`, `--keep-both`, `--confirm`, `--delete`, `--apply` and `--dismiss` are
/// mutually exclusive; a flag not present here answers `None` and the caller falls through to the
/// interactive loop or `--json`.
pub fn flag_decision(args: &Args) -> Result<Option<FlagAction>> {
    const ACTION_FLAGS: [&str; 7] =
        ["supersede", "merge", "keep-both", "confirm", "delete", "apply", "dismiss"];
    let present: Vec<&str> = ACTION_FLAGS.into_iter().filter(|f| args.present(f)).collect();
    if present.len() > 1 {
        return Err(err(format!(
            "one action flag per call; got --{}",
            present.join(" and --")
        )));
    }
    let Some(flag) = present.first().copied() else {
        return Ok(None);
    };

    let action = match flag {
        "supersede" => {
            let raw = args.value("supersede").ok_or_else(|| err("--supersede needs <old>,<new>"))?;
            let (old, new) = split_pair(raw, "--supersede")?;
            FlagAction {
                key: format!("conflict:{old}:{new}"),
                verdict: wire::Verdict::Supersede,
                keep: Some(new.to_string()),
                id: None,
                content: None,
                tags: None,
                occurred_at: None,
                reason: None,
                needs_delete_confirmation: false,
            }
        }
        "merge" => {
            let raw = args.value("merge").ok_or_else(|| err("--merge needs <id> or <a>,<b>"))?;
            let content = args.value("content").ok_or_else(|| {
                err("--merge needs --content; the text comes from you, nothing in the engine \
writes it")
            })?;
            let key = match split_pair(raw, "--merge") {
                Ok((a, b)) => format!("conflict:{a}:{b}"),
                Err(_) => format!("stale:{}", raw.trim()),
            };
            let occurred_at = match args.value("occurred-at") {
                Some(raw) => {
                    Some(parse_two_date_forms("occurred_at", raw).map_err(err)?.to_rfc3339())
                }
                None => None,
            };
            FlagAction {
                key,
                verdict: wire::Verdict::Merge,
                keep: None,
                id: None,
                content: Some(content.to_string()),
                tags: args.comma_list(&["tags"]),
                occurred_at,
                reason: None,
                needs_delete_confirmation: false,
            }
        }
        "keep-both" => {
            let raw = args.value("keep-both").ok_or_else(|| err("--keep-both needs <a>,<b>"))?;
            let (a, b) = split_pair(raw, "--keep-both")?;
            FlagAction {
                key: format!("conflict:{a}:{b}"),
                verdict: wire::Verdict::KeepBoth,
                keep: None,
                id: None,
                content: None,
                tags: None,
                occurred_at: None,
                reason: None,
                needs_delete_confirmation: false,
            }
        }
        "confirm" => {
            let id = args.value("confirm").ok_or_else(|| err("--confirm needs <id>"))?;
            FlagAction {
                key: format!("stale:{id}"),
                verdict: wire::Verdict::Confirm,
                keep: None,
                id: None,
                content: None,
                tags: None,
                occurred_at: None,
                reason: None,
                needs_delete_confirmation: false,
            }
        }
        "delete" => {
            let id = args.value("delete").ok_or_else(|| err("--delete needs <id>"))?;
            FlagAction {
                key: format!("stale:{id}"),
                verdict: wire::Verdict::Delete,
                keep: None,
                id: Some(id.to_string()),
                content: None,
                tags: None,
                occurred_at: None,
                reason: args.value("reason").map(str::to_string),
                needs_delete_confirmation: !args.present("yes"),
            }
        }
        "apply" => {
            let raw = args.value("apply").ok_or_else(|| err("--apply needs <origin>:<id>"))?;
            FlagAction {
                key: format!("proposal:{raw}"),
                verdict: wire::Verdict::Apply,
                keep: None,
                id: None,
                content: None,
                tags: None,
                occurred_at: None,
                reason: None,
                needs_delete_confirmation: false,
            }
        }
        "dismiss" => {
            let raw = args.value("dismiss").ok_or_else(|| err("--dismiss needs <origin>:<id>"))?;
            FlagAction {
                key: format!("proposal:{raw}"),
                verdict: wire::Verdict::Dismiss,
                keep: None,
                id: None,
                content: None,
                tags: None,
                occurred_at: None,
                reason: None,
                needs_delete_confirmation: false,
            }
        }
        _ => unreachable!("ACTION_FLAGS and this match must name the same flags"),
    };
    Ok(Some(action))
}

fn split_pair<'a>(raw: &'a str, flag: &str) -> Result<(&'a str, &'a str)> {
    let mut parts = raw.splitn(2, ',');
    let a = parts.next().map(str::trim).filter(|s| !s.is_empty());
    let b = parts.next().map(str::trim).filter(|s| !s.is_empty());
    match (a, b) {
        (Some(a), Some(b)) => Ok((a, b)),
        _ => Err(err(format!("{flag} needs <a>,<b>"))),
    }
}

async fn run_flag_decision(
    c: &Client,
    action: FlagAction,
    json: bool,
    read_line: &mut impl FnMut() -> std::io::Result<String>,
) -> Result<()> {
    if action.needs_delete_confirmation {
        let id = action.id.clone().unwrap_or_default();
        if confirm_delete(read_line, &id)? != Some(true) {
            out("aborted, nothing deleted");
            return Ok(());
        }
    }
    let (body, decided) = post_decision(
        c,
        &action.key,
        action.verdict,
        action.keep.as_deref(),
        action.id.as_deref(),
        action.content.as_deref(),
        action.tags.clone(),
        action.occurred_at.clone(),
        action.reason.as_deref(),
    )
    .await?;
    if json {
        out_json(&body);
    } else {
        out(&one_line(&decided));
    }
    Ok(())
}

// ---- HTTP ----

fn sources_arg(args: &Args) -> Option<Vec<String>> {
    if let Some(list) = args.comma_list(&["source"]) {
        return Some(list);
    }
    if args.present("stale") {
        return Some(vec!["stale".to_string()]);
    }
    if args.present("conflicts") {
        return Some(vec!["conflict".to_string()]);
    }
    None
}

fn queue_path(args: &Args, offset: i64, limit: i64) -> String {
    let mut qp = vec![format!("limit={limit}"), format!("offset={offset}")];
    if let Some(sources) = sources_arg(args) {
        qp.push(format!("source={}", sources.join(",")));
    }
    if let Some(days) = args.value("days") {
        qp.push(format!("days={days}"));
    }
    if let Some(ms) = args.value("min-similarity") {
        qp.push(format!("min_similarity={ms}"));
    }
    format!("/admin/review/queue?{}", qp.join("&"))
}

/// The server's own `error` code beside its `detail`, so `source_not_filled`,
/// `verdict_not_for_source` and `not_a_queue_key` read as different failures rather than the same
/// prose. Neither key present falls back to the whole body, since that shape is the bug to see.
fn refusal_text(status: u16, body: &Value) -> String {
    let code = body.get("error").and_then(Value::as_str);
    let detail = body.get("detail").and_then(Value::as_str);
    match (code, detail) {
        (Some(code), Some(detail)) => format!("({status}, {code}): {detail}"),
        (Some(code), None) => format!("({status}, {code})"),
        (None, Some(detail)) => format!("({status}): {detail}"),
        (None, None) => format!("({status}): {}", compact(body)),
    }
}

async fn fetch_queue(c: &Client, args: &Args, offset: i64, limit: i64) -> Result<wire::ReviewQueue> {
    let path = queue_path(args, offset, limit);
    let (status, body) = c.http_get(&path).await?;
    if status == 404 {
        return Err(err("this server has no review queue route; upgrade it"));
    }
    if status != 200 {
        return Err(err(format!("review queue failed {}", refusal_text(status, &body))));
    }
    typed(&body, "review queue")
}

async fn run_json(c: &Client, args: &Args) -> Result<()> {
    let limit = args.int("limit", 50).clamp(1, 200);
    let offset = args.int("offset", 0).max(0);
    let path = queue_path(args, offset, limit);
    let (status, body) = c.http_get(&path).await?;
    if status == 404 {
        return Err(err("this server has no review queue route; upgrade it"));
    }
    if status != 200 {
        return Err(err(format!("review queue failed {}", refusal_text(status, &body))));
    }
    out_json(&body);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn post_decision(
    c: &Client,
    key: &str,
    verdict: wire::Verdict,
    keep: Option<&str>,
    id: Option<&str>,
    content: Option<&str>,
    tags: Option<Vec<String>>,
    occurred_at: Option<String>,
    reason: Option<&str>,
) -> Result<(Value, wire::Decided)> {
    let req = wire::DecisionRequest { key, verdict, keep, id, content, tags, occurred_at, reason };
    let (status, body) = c
        .http_request(
            reqwest::Method::POST,
            "/admin/review/decide",
            Some(serde_json::to_value(&req).unwrap()),
        )
        .await?;
    if status != 200 {
        return Err(err(format!("decide failed {}", refusal_text(status, &body))));
    }
    let decided = typed(&body, "decide")?;
    Ok((body, decided))
}

// ---- the ledger, spec 2.2 ----

async fn run_dismissed(c: &Client, args: &Args) -> Result<()> {
    let limit = args.int("limit", 50);
    let (status, body) = c.http_get(&format!("/admin/review/dismissed?limit={limit}")).await?;
    if status != 200 {
        return Err(err(format!("dismissed listing failed {}", refusal_text(status, &body))));
    }
    // `src/http/review.rs` answers `{"pairs": [...]}`; a rename there should fail loud here, not
    // read as an empty ledger.
    let Some(list_value) = body.get("pairs") else {
        return Err(err("dismissed listing is not the expected shape"));
    };
    let rows: Vec<wire::DismissedPair> = typed(list_value, "dismissed listing")?;
    out(&format!("dismissed pairs: {}", rows.len()));
    for p in &rows {
        out(&format!("  {}  {}  dismissed by {} ({})", p.lo_id, p.hi_id, p.dismissed_by, p.dismissed_at));
        for r in &p.rows {
            out(&format!(
                "    {}  [{}]  {}",
                r.id,
                r.namespace,
                r.content.chars().take(80).collect::<String>()
            ));
        }
    }
    Ok(())
}

async fn run_undismiss(c: &Client, raw: &str) -> Result<()> {
    let (a, b) = split_pair(raw, "--undismiss")?;
    let path = format!("/admin/review/dismissed/{}/{}", urlencode(a), urlencode(b));
    let (status, body) = c.http_request(reqwest::Method::DELETE, &path, None).await?;
    if status != 200 {
        return Err(err(format!("undismiss failed {}", refusal_text(status, &body))));
    }
    let removed = body.get("removed").and_then(Value::as_bool).unwrap_or(false);
    if removed {
        out(&format!("{a} and {b} are back in the queue"));
    } else {
        out("nothing to undismiss for that pair");
    }
    Ok(())
}

// ---- --dates and --registry, moved here unchanged (spec 6) ----

async fn run_dates(c: &Client, args: &Args) -> Result<()> {
    let limit = args.int("limit", 25);
    let (status, body) = c.http_get(&format!("/admin/review/dates?limit={limit}")).await?;
    if status != 200 {
        return Err(err(format!("date review failed ({status}): {}", compact(&body))));
    }
    let review: wire::DateReview = typed(&body, "date review")?;
    let ready = review.rows.iter().filter(|r| r.proposed.is_some()).count();
    out(&format!(
        "undated facts whose own text names a day: {} ({ready} with one date, {} with more)",
        review.rows.len(),
        review.rows.len() - ready
    ));
    for r in &review.rows {
        match &r.proposed {
            Some(day) => out(&format!(
                "  {}  [{}]  {day}\n      {}",
                r.id,
                r.namespace,
                r.content.chars().take(96).collect::<String>()
            )),
            None => out(&format!(
                "  {}  [{}]  names {} days: {}\n      {}",
                r.id,
                r.namespace,
                r.ambiguous.len(),
                r.ambiguous.join(", "),
                r.content.chars().take(96).collect::<String>()
            )),
        }
    }
    if ready > 0 {
        out("");
        out("Nothing was written. Fill one with:");
        out("  lumberroom fill-date <id> <YYYY-MM-DD>");
    }
    Ok(())
}

async fn run_registry(c: &Client, args: &Args) -> Result<()> {
    let limit = args.int("limit", 25);
    let (status, body) = c.http_get(&format!("/admin/review/registry?limit={limit}")).await?;
    if status != 200 {
        return Err(err(format!("registry review failed ({status}): {}", compact(&body))));
    }
    let review: wire::RegistryReview = typed(&body, "registry review")?;
    out(&format!("registry due for review: {}", review.due_for_review.len()));
    for e in &review.due_for_review {
        out(&format!("  {} {}:{}", e.namespace, e.kind, e.key));
    }
    out(&format!("non-canonical registry keys: {}", review.non_canonical.len()));
    for e in &review.non_canonical {
        out(&format!("  {} {}:{}", e.namespace, e.kind, e.key));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, opened: bool) -> wire::ReviewRow {
        wire::ReviewRow {
            id: id.to_string(),
            namespace: "user:me".to_string(),
            sensitivity: "open".to_string(),
            content: "some fact".to_string(),
            opened,
            created_at: "2026-09-01T00:00:00Z".to_string(),
            occurred_at: None,
            access_count: 0,
            last_accessed_at: None,
            last_confirmed_at: None,
        }
    }

    fn conflict_item(key: &str, verdicts: Vec<wire::Verdict>) -> wire::ReviewItem {
        wire::ReviewItem {
            key: key.to_string(),
            source: wire::Source::Conflict,
            namespace: "user:me".to_string(),
            rows: vec![row("older00001111", true), row("newer00002222", true)],
            similarity: Some(0.93),
            age_days: None,
            proposal: None,
            verdicts,
        }
    }

    fn stale_item(key: &str, verdicts: Vec<wire::Verdict>) -> wire::ReviewItem {
        wire::ReviewItem {
            key: key.to_string(),
            source: wire::Source::Stale,
            namespace: "user:me".to_string(),
            rows: vec![row("stale0000aaaa", true)],
            similarity: None,
            age_days: Some(400),
            proposal: None,
            verdicts,
        }
    }

    #[test]
    fn the_key_line_draws_only_the_verdicts_the_item_takes_and_read_only_draws_n_and_q() {
        assert_eq!(key_line(&[], false), "n skip   q quit");
        let all = vec![
            wire::Verdict::Supersede,
            wire::Verdict::Merge,
            wire::Verdict::KeepBoth,
            wire::Verdict::Delete,
        ];
        assert_eq!(
            key_line(&all, true),
            "s keep newer   o keep older   m merge   k keep both   d delete   n skip   q quit"
        );
        // Not a pair: s/o never draw even though the verdict list carries Supersede.
        assert_eq!(key_line(&all, false), "m merge   k keep both   d delete   n skip   q quit");
    }

    #[test]
    fn s_keeps_the_newer_row_and_o_keeps_the_older() {
        let item = conflict_item("conflict:a:b", vec![wire::Verdict::Supersede]);
        assert_eq!(
            answer_to_decision(&item, "s"),
            Answer::Act { verdict: wire::Verdict::Supersede, keep: Some("newer00002222".into()) }
        );
        assert_eq!(
            answer_to_decision(&item, "o"),
            Answer::Act { verdict: wire::Verdict::Supersede, keep: Some("older00001111".into()) }
        );
    }

    #[test]
    fn a_merged_text_of_two_lines_is_read_whole_and_leaves_nothing_on_the_stream() {
        let mut lines = vec!["first line\n", "second line\n", "\n", "q\n"].into_iter();
        let mut read = move || Ok(lines.next().unwrap().to_string());
        let text = merge_text(&mut read).unwrap();
        assert_eq!(text.as_deref(), Some("first line second line"));
        // The blank line was consumed; the next read is the caller's next prompt, untouched.
        assert_eq!(read().unwrap(), "q\n");
    }

    #[test]
    fn an_empty_merged_text_aborts_to_the_key_line() {
        let mut lines = vec!["\n"].into_iter();
        let mut read = move || Ok(lines.next().unwrap().to_string());
        assert_eq!(merge_text(&mut read).unwrap(), None);
    }

    #[test]
    fn a_key_line_answer_longer_than_one_character_decides_nothing() {
        let item = stale_item("stale:a", vec![wire::Verdict::Confirm]);
        assert_eq!(answer_to_decision(&item, "cc"), Answer::Again);
        assert_eq!(answer_to_decision(&item, ""), Answer::Again);
    }

    #[test]
    fn a_delete_confirmation_needs_the_first_eight_characters_of_the_id() {
        assert!(delete_confirmed("9f1c2b4e-0000-4a1b", "9f1c2b4e"));
        assert!(!delete_confirmed("9f1c2b4e-0000-4a1b", "9f1c2b4"));
        assert!(!delete_confirmed("9f1c2b4e-0000-4a1b", "0000-4a1b"));
    }

    fn args_from(argv: &[&str]) -> Args {
        Args::parse(argv.iter().map(|s| s.to_string()))
    }

    #[test]
    fn two_action_flags_are_a_usage_error() {
        let args = args_from(&["review", "--confirm", "abc", "--delete", "abc"]);
        assert!(flag_decision(&args).is_err());
    }

    #[test]
    fn merge_without_content_names_the_decision_that_the_text_comes_from_the_caller() {
        let args = args_from(&["review", "--merge", "abc"]);
        let e = flag_decision(&args).unwrap_err();
        assert!(e.message.contains("the text comes from you"));
    }

    #[test]
    fn a_delete_reason_is_carried_onto_the_flag_action_and_absent_elsewhere() {
        let args = args_from(&["review", "--delete", "abc", "--reason", "dup"]);
        let action = flag_decision(&args).unwrap().unwrap();
        assert_eq!(action.reason.as_deref(), Some("dup"));

        let args = args_from(&["review", "--delete", "abc"]);
        let action = flag_decision(&args).unwrap().unwrap();
        assert_eq!(action.reason, None);

        let args = args_from(&["review", "--confirm", "abc", "--reason", "ignored"]);
        let action = flag_decision(&args).unwrap().unwrap();
        assert_eq!(action.reason, None, "reason is delete's own flag, not every verdict's");
    }

    #[test]
    fn the_merge_line_prints_every_unfinished_id() {
        let d = wire::Decided {
            key: "conflict:a:b".to_string(),
            verdict: wire::Verdict::Merge,
            written: Some("2b7c".to_string()),
            superseded: vec!["a".to_string()],
            deleted: vec![],
            end_left_open: false,
            unfinished: vec!["b".to_string()],
            already_dismissed: false,
            proposal_state: None,
        };
        assert_eq!(one_line(&d), "wrote 2b7c and retired 1; unfinished: b");
    }

    #[test]
    fn the_tally_counts_every_verdict_the_skips_and_the_ledger_total() {
        let t = Tally {
            read: 39,
            superseded: 22,
            merged: 3,
            kept_both: 6,
            skipped: 2,
            dismissed_in_ledger: 12,
            ..Tally::default()
        };
        assert_eq!(
            tally(&t),
            "39 read: 22 superseded, 3 merged, 6 kept, 2 skipped, 12 dismissed in the ledger"
        );
    }

    /// Drives the interactive loop end to end over a canned `read_line`, against a hand-rolled
    /// mock of `/admin/review/queue` and `/admin/review/decide`: the paste-then-quit shape is the
    /// one thing a unit test on `answer_to_decision` alone cannot catch, because it needs the
    /// second item's key line to survive the first item's merged text.
    mod loop_end_to_end {
        use super::*;
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        struct Recorded {
            decides: Vec<Value>,
            merged_already: bool,
        }

        fn find_header_end(buf: &[u8]) -> Option<usize> {
            buf.windows(4).position(|w| w == b"\r\n\r\n")
        }

        fn queue_body(merged_already: bool) -> Value {
            let item1 = serde_json::json!({
                "key": "conflict:a:b",
                "source": "conflict",
                "namespace": "user:me",
                "rows": [
                    { "id": "aaaaaaaa-0000", "namespace": "user:me", "sensitivity": "open",
                      "content": "older text", "opened": true, "created_at": "2026-09-01T00:00:00Z",
                      "access_count": 1 },
                    { "id": "bbbbbbbb-0000", "namespace": "user:me", "sensitivity": "open",
                      "content": "newer text", "opened": true, "created_at": "2026-09-02T00:00:00Z",
                      "access_count": 0 },
                ],
                "similarity": 0.9,
                "verdicts": ["merge"],
            });
            let item2 = serde_json::json!({
                "key": "stale:c",
                "source": "stale",
                "namespace": "user:me",
                "rows": [
                    { "id": "cccccccc-0000", "namespace": "user:me", "sensitivity": "open",
                      "content": "stale text", "opened": true, "created_at": "2026-01-01T00:00:00Z",
                      "access_count": 3 },
                ],
                "age_days": 400,
                "verdicts": ["confirm"],
            });
            let items = if merged_already { vec![item2] } else { vec![item1, item2] };
            serde_json::json!({
                "items": items,
                "sources": { "conflict": true, "stale": true, "proposal": [] },
                "refused": {},
                "dismissed": 0,
                "stale_days": 180,
                "min_similarity": 0.85,
                "limit": 50,
                "offset": 0,
                "has_more": false,
            })
        }

        async fn serve_one(socket: &mut tokio::net::TcpStream, state: &Mutex<Recorded>) {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(at) = find_header_end(&buf) {
                    break at;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let length = headers
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buf.len() < header_end + 4 + length {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let request_line = headers.lines().next().unwrap_or_default().to_string();
            let path = request_line.split(' ').nth(1).unwrap_or("");
            let body_str =
                String::from_utf8_lossy(&buf[header_end + 4..header_end + 4 + length]).to_string();

            let answer = if path.starts_with("/admin/review/queue") {
                let merged_already = state.lock().unwrap().merged_already;
                queue_body(merged_already).to_string()
            } else if path.starts_with("/admin/review/decide") {
                let decision: Value = serde_json::from_str(&body_str).unwrap();
                let mut st = state.lock().unwrap();
                st.decides.push(decision.clone());
                st.merged_already = true;
                drop(st);
                serde_json::json!({
                    "key": decision["key"],
                    "verdict": "merge",
                    "written": "2b7c0000",
                    "superseded": ["aaaaaaaa-0000", "bbbbbbbb-0000"],
                })
                .to_string()
            } else {
                panic!("unexpected path {path}");
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                 {}\r\nconnection: close\r\n\r\n{answer}",
                answer.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }

        #[tokio::test]
        async fn a_two_line_paste_merges_once_and_leaves_the_second_item_undecided() {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = Arc::new(Mutex::new(Recorded { decides: Vec::new(), merged_already: false }));
            let server_state = state.clone();
            let server = tokio::spawn(async move {
                // One queue read, one decide, one queue re-read after the decision, one more
                // queue read is never sent because the loop quits on item2's key line.
                for _ in 0..3 {
                    let Ok((mut socket, _)) = listener.accept().await else { break };
                    serve_one(&mut socket, &server_state).await;
                }
            });

            let env: HashMap<String, String> = HashMap::from([
                ("LUMBERROOM_URL".to_string(), format!("http://127.0.0.1:{port}")),
                ("LUMBERROOM_TOKEN".to_string(), "t".to_string()),
            ]);
            let file = crate::config::FileConfig::empty(std::env::temp_dir().join(format!(
                "lumberroom-review-loop-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
            let resolved = crate::config::resolve(&env, &file, None, None, None, false, None);
            let client = Client::new(resolved, file).unwrap();
            let args = Args::parse(["review"].into_iter().map(str::to_string));

            let mut lines =
                vec!["m\n", "first line\n", "second line\n", "\n", "q\n"].into_iter();
            let mut read = move || Ok(lines.next().unwrap_or("").to_string());

            run(&client, &args, &mut read).await.unwrap();
            drop(server.await);

            let st = state.lock().unwrap();
            assert_eq!(st.decides.len(), 1, "exactly one decision was posted");
            assert_eq!(st.decides[0]["verdict"], "merge");
            assert_eq!(st.decides[0]["content"], "first line second line");
            assert_eq!(st.decides[0]["key"], "conflict:a:b");
        }
    }
}
