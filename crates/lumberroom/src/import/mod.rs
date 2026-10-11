//! `lumberroom import`. Bringing a memory out of somebody else's product.
//!
//! Separate from `ingest`, which reads transcripts this machine already owns. Import starts from a
//! file the owner fetched out of ChatGPT or claude.ai, and the two halves of that arrive by
//! different routes: conversations come in an export archive, and a memory store comes either in
//! the same archive, as claude.ai's `memories` part does, or through the assistant itself, which is
//! what `dump_prompt` is for.
//!
//! The split decides the bill. Conversations go through extraction and spend provider tokens. A
//! memory dump is already a list of facts, so it reaches the proposal queue with no model in the
//! path and no key configured.
//!
//! Nothing here hands the server a transcript. `ingest::mod` states the reason the client is its own
//! binary, and import keeps to it: the parse happens on the machine holding the file, and only
//! proposals cross the wire.

pub mod dump;
pub mod dump_prompt;
pub mod judge;
pub mod junk;
pub mod manifest;
pub mod parts;
pub mod plan;

use std::path::PathBuf;

use crate::args::Args;
use crate::client::{err, Client, Result};
use crate::ingest::api;
use crate::{out, read_line};
use manifest::Manifest;
use parts::State;

pub const SUBCOMMANDS: &str = "prompt, claude, memory-dump";

pub async fn dispatch(c: &Client, args: &Args, sub: &str) -> Result<()> {
    match sub {
        "prompt" => {
            out(dump_prompt::DUMP_PROMPT);
            Ok(())
        }
        "claude" => claude(args).await,
        "memory-dump" => memory_dump(c, args).await,
        other => Err(err(format!("unknown import subcommand `{other}`. Available: {SUBCOMMANDS}"))),
    }
}

/// `lumberroom import claude --manifest <path> [--dir <path>]`
///
/// Reads the directory and reports what is there. No path is guessed: a downloads directory holds
/// many archives, and importing the wrong one is undone by hand.
async fn claude(args: &Args) -> Result<()> {
    let manifest_path = args.value("manifest").ok_or_else(|| {
        err("import claude needs --manifest <path to the export manifest json>, \
             and optionally --dir <where the parts were saved>")
    })?;
    let manifest_path = PathBuf::from(manifest_path);
    let m = Manifest::read(&manifest_path)?;

    // Beside the manifest unless told otherwise, so a second run finds the first run's files
    // without the owner having to remember a path.
    let dir = match args.value("dir") {
        Some(d) => PathBuf::from(d),
        None => manifest_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("claude-export"),
    };

    let survey = parts::survey(&m, &dir);
    out(&format!("{} parts named, looking in {}", m.data_files.len(), dir.display()));
    for (part, state) in &survey {
        let line = match state {
            State::Present { bytes, .. } => {
                format!("  have      {} ({bytes} bytes)", part.category)
            }
            State::NotAnArchive { bytes, .. } => {
                format!("  not a zip {} ({bytes} bytes on disk)", part.category)
            }
            State::Missing => format!("  missing   {}", part.category),
        };
        out(&line);
    }

    if survey.iter().any(|(_, s)| !s.is_ready()) {
        out("");
        out(&parts::download_instructions(&survey, &dir, &manifest_path));
        // Not an error. Nothing broke, the owner has a step to take, and a failing exit code would
        // stop a script that should carry on once the files land.
        return Ok(());
    }

    out("");
    out(&format!("Every part is in {}.", dir.display()));
    Ok(())
}

/// `lumberroom import memory-dump --file <path> [--submit] [--drop-junk|--keep-all] [--delete-source]`
///
/// Reads, sifts, and reports. `--submit` is what writes to the queue, and defaulting the other way
/// would put a hundred rows in somebody's queue because they wanted to see what a file held.
///
/// **Two things send the dump's content off this machine, and neither waits for `--submit`.** The
/// tripwire scan posts it to the owner's own lumberroom server, which is where the credential rules
/// live. The judgement pass posts it to a model provider, which is somebody else's computer, and it
/// runs by default. `--keep-all` is the flag that stops it. Saying "nothing leaves the machine
/// without --submit" would be false, and a person deciding whether to run this on a memory export
/// is exactly who a false claim there would hurt.
///
/// `--delete-source` removes the file, and only once the post came back having landed something. It
/// refuses without `--submit`, because a dry run posts nothing and the file would be the only copy
/// of a dump that never reached the queue.
async fn memory_dump(c: &Client, args: &Args) -> Result<()> {
    let path = args
        .value("file")
        .ok_or_else(|| err("import memory-dump needs --file <path to the saved dump>"))?;
    let policy = junk::Policy::from_flags(args.present("drop-junk"), args.present("keep-all"))
        .map_err(err)?;
    let submit = args.present("submit");
    let delete_source = deletes_source(args.present("delete-source"), submit).map_err(err)?;

    let text =
        std::fs::read_to_string(&path).map_err(|e| err(format!("cannot read {path}: {e}")))?;
    let parsed = dump::parse(&text);
    let source = std::path::Path::new(&path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());

    out(&format!("{} entries parsed from {}", parsed.entries.len(), path));
    for (section, n) in &parsed.per_section {
        out(&format!("  {section:<14} {n}"));
    }
    if !parsed.unrecognised.is_empty() {
        out(&format!("  {} lines were not entries", parsed.unrecognised.len()));
        for (line, snippet) in parsed.unrecognised.iter().take(5) {
            out(&format!("    line {line}: {snippet}"));
        }
    }
    if !parsed.future_dated.is_empty() {
        out(&format!(
            "  {} lines carried a date after today, imported without one",
            parsed.future_dated.len()
        ));
        for (line, date) in parsed.future_dated.iter().take(5) {
            out(&format!("    line {line}: {date}"));
        }
        out("  Fix the date in the file and import again if any of those are wrong.");
    }

    let mut plan = plan::sift(parsed.entries.clone());
    // The tripwire is the server's, and reaching it needs a credential. Posting without it is not
    // an option, so `--submit` fails on a scan that cannot run. Reading a file is not posting, so a
    // dry run says the check is missing and carries on: needing a live token to answer "what is in
    // this file" would push people toward reading dumps in an editor instead, which is how a key
    // ends up pasted somewhere by hand.
    let mut tripwire_ran = true;
    if let Err(e) = plan::tripwire(c, &mut plan).await {
        if submit {
            return Err(e);
        }
        tripwire_ran = false;
        out(&format!("\nThe credential tripwire could not run: {}", e.message));
        out("Counts below have not been checked for secrets. --submit will refuse until it can run.");
    }

    if policy.wants_a_model() {
        if tripwire_ran {
            match assess(args, &mut plan, policy).await {
                Ok(()) => {}
                // A provider that cannot be reached keeps every line. It is not a verdict, and it is
                // not a reason to throw away a parsed dump.
                Err(e) => {
                    out("");
                    out(&judge::describe_failure(&e));
                }
            }
        } else {
            // The tripwire is what decides whether a line holds a credential, and the judgement pass
            // hands every line to a third party. Running the second without the first would send an
            // unscanned API key to a model provider, which is the one outcome this import must never
            // produce, and it would do it on a run the owner thought was a read.
            out("");
            out("Skipped the judgement pass: it sends every line to a model provider, and the \
                 credential tripwire has not run over them. Fix the credential and run again, or \
                 pass --keep-all to import without it.");
        }
    }

    // Reported after the judgement pass so `judged` is populated. Before it, that list is always
    // empty and printing it here was dead code.
    out("");
    let checked = if tripwire_ran { "" } else { ", tripwire not run" };
    out(&format!("{} kept, {} dropped{checked}", plan.kept.len(), plan.dropped_total()));
    report_drops("duplicate", &plan.duplicates);
    report_drops("structural", &plan.structural);
    report_drops("tripwire", &plan.refused);
    report_drops("judged", &plan.judged);

    let inferred = plan.kept.iter().filter(|e| e.confidence == dump::Confidence::Inferred).count();
    let dated = plan.kept.iter().filter(|e| e.occurred_at.is_some()).count();
    let mut namespaces: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for e in &plan.kept {
        *namespaces.entry(e.namespace.as_str()).or_insert(0) += 1;
    }
    out("");
    out(&format!("  {inferred} inferred, {} stated", plan.kept.len() - inferred));
    out(&format!("  {dated} carry a date and would land as valid time"));
    out("");
    for (ns, n) in &namespaces {
        out(&format!("  {ns:<24} {n}"));
    }

    out("");
    match parsed.ending {
        dump::Ending::Complete => out("The dump says COMPLETE."),
        dump::Ending::MoreRemains => out(
            "The dump says MORE REMAINS. Ask the assistant to continue and import the rest as its \
             own file.",
        ),
        dump::Ending::Truncated => {
            out("No completion marker, so this dump was cut off.");
            if let Some(partial) = &parsed.dropped_partial {
                out(&format!("Dropped the last entry, which may be half a fact: {partial}"));
            }
            out("Ask the assistant to continue and import the rest as its own file.");
        }
    }

    if !submit {
        out("");
        out(&format!(
            "Nothing was posted. Add --submit to put these {} in the queue.",
            plan.kept.len()
        ));
        return Ok(());
    }
    if plan.kept.is_empty() {
        out("");
        out("Nothing left to post.");
        return Ok(());
    }

    let run_id = api::open_run(c, "memory-dump", plan::scope(&source, &parsed)).await?;
    let reqs = plan::requests(&plan.kept, &source, run_id);
    let posted = api::post_proposals(c, "memory-dump", &reqs).await?;
    out("");
    out(&format!(
        "run {run_id}: {} new, {} reinforced, {} already known, {} refused, {} blocked",
        posted.proposals_new,
        posted.proposals_reinforced,
        posted.confirmations,
        posted.refused,
        posted.blocked
    ));
    out("Review them with: lumberroom ingest list --state proposed");

    // Only after the post returned, and only when it landed something. A run that refused or blocked
    // every line leaves the owner with nothing in the queue, and deleting the file then would take
    // the only copy of a dump they still have to fix and retry.
    if delete_source {
        let landed = posted.proposals_new + posted.proposals_reinforced + posted.confirmations;
        let fixable = fixable_refusals(&posted.outcomes);
        if landed == 0 {
            out("");
            out(&format!(
                "Kept {path}: nothing reached the queue, so the file is still the only copy."
            ));
        } else if !fixable.is_empty() {
            let rules: Vec<String> =
                fixable.iter().map(|(rule, n)| format!("{rule} x{n}")).collect();
            out("");
            out(&format!(
                "Kept {path}: the server refused lines an edit can fix ({}), and the file is the \
                 only copy of them. Fix those lines and import the file again; the lines that \
                 landed come back as reinforced or already known.",
                rules.join(", ")
            ));
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    out("");
                    out(&format!("Deleted {path}."));
                }
                // The import worked. Saying so and carrying on beats a non-zero exit that reads as
                // "the import failed" for a file the owner can delete themselves.
                Err(e) => {
                    out("");
                    out(&format!("Imported, but could not delete {path}: {e}"));
                }
            }
        }
    }
    Ok(())
}

/// Server refusal rules an edit to the line can clear, as the server names them in
/// `services::ingest`. A line refused under one of these exists only in the source file.
///
/// A tripwire rule is left out on purpose. Its line holds a credential, no edit makes it
/// storable, and keeping the file for it keeps the credential on disk.
const FIXABLE_RULES: [&str; 4] =
    ["content_too_long", "invalid_namespace", "namespace_outside_grant", "supersedes_not_writable"];

/// Each fixable refusal rule in a post's outcomes with its count, in rule order. Empty means
/// nothing the owner could fix and retry was refused.
fn fixable_refusals(outcomes: &[api::FactOutcome]) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for outcome in outcomes {
        if let api::FactOutcome::Refused { rule } = outcome {
            if FIXABLE_RULES.contains(&rule.as_str()) {
                *counts.entry(rule.clone()).or_insert(0) += 1;
            }
        }
    }
    counts.into_iter().collect()
}

/// Whether this run deletes the file when it is done, or refuses the combination outright.
///
/// Refused rather than ignored. The two readings of `--delete-source` on a dry run are "delete it
/// anyway" and "do nothing", and a flag that silently means the second is a flag somebody will
/// eventually trust to mean the first.
fn deletes_source(delete_source: bool, submit: bool) -> std::result::Result<bool, &'static str> {
    match (delete_source, submit) {
        (true, false) => Err(
            "--delete-source needs --submit. A dry run posts nothing, so deleting the file would \
             throw away the only copy of a dump that never reached the queue",
        ),
        (wanted, _) => Ok(wanted),
    }
}

fn report_drops(label: &str, drops: &[plan::Dropped]) {
    if drops.is_empty() {
        return;
    }
    out(&format!("  {label}: {}", drops.len()));
    for d in drops.iter().take(5) {
        out(&format!("    [{}] {}", d.why, first_line(&d.content, 88)));
    }
    if drops.len() > 5 {
        out(&format!("    and {} more", drops.len() - 5));
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max {
        return line.to_string();
    }
    line.chars().take(max).collect::<String>() + "..."
}

/// The judgement pass, and the confirmation in front of it.
///
/// `--drop-junk` skips the question. Nothing else does: this is the only place in the import that
/// removes a fact on a model's say-so, and a wrong deletion here leaves no gap anybody notices.
async fn assess(args: &Args, plan: &mut plan::PlanReport, policy: junk::Policy) -> Result<()> {
    if plan.kept.is_empty() {
        return Ok(());
    }
    // Read from disk the same way `ingest extract` does. The key lives in the CLI's own config at
    // 0600 and reaches nothing else: not `.env`, not an argument, not this process's command line.
    let name = args.value("provider").unwrap_or("anthropic");
    let env = crate::config::ProcessEnv;
    let file = crate::config::FileConfig::load(crate::config::config_path(&env));
    let provider =
        crate::ingest::provider::resolve(name, &file, args.value("model"), args.value("base-url"))?;
    let http = reqwest::Client::new();

    let numbered: Vec<(usize, String)> =
        plan.kept.iter().enumerate().map(|(i, e)| (i, e.content.clone())).collect();

    let mut verdicts: Vec<judge::Verdict> = Vec::new();
    let mut usage = crate::ingest::Usage::default();
    for batch in numbered.chunks(judge::BATCH) {
        let (mut v, u) = judge::assess_batch(&http, &provider, batch, 120).await?;
        verdicts.append(&mut v);
        usage.requests += u.requests;
        usage.prompt_tokens += u.prompt_tokens;
        usage.completion_tokens += u.completion_tokens;
    }

    out("");
    out(&format!(
        "the judgement pass read {} lines in {} requests, {} prompt and {} completion tokens",
        numbered.len(),
        usage.requests,
        usage.prompt_tokens,
        usage.completion_tokens
    ));

    if verdicts.is_empty() {
        out("It flagged nothing.");
        return Ok(());
    }

    out(&format!("It flagged {} of {}:", verdicts.len(), numbered.len()));
    for v in &verdicts {
        out(&format!("  [{}] {}", v.why, first_line(&plan.kept[v.index].content, 84)));
    }

    if policy != junk::Policy::DropWithoutAsking {
        out("");
        crate::prompt("Drop these? Everything else is kept. [y/N] ");
        let answer = read_line().map_err(|e| err(format!("could not read your answer: {e}")))?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            out("Kept them. Nothing was dropped by the model.");
            return Ok(());
        }
    }

    let flagged: std::collections::HashSet<usize> = verdicts.iter().map(|v| v.index).collect();
    let reasons: std::collections::HashMap<usize, String> =
        verdicts.into_iter().map(|v| (v.index, v.why)).collect();
    let mut kept = Vec::with_capacity(plan.kept.len());
    for (i, entry) in std::mem::take(&mut plan.kept).into_iter().enumerate() {
        if flagged.contains(&i) {
            plan.judged.push(plan::Dropped {
                content: entry.content,
                why: reasons.get(&i).cloned().unwrap_or_default(),
            });
        } else {
            kept.push(entry);
        }
    }
    plan.kept = kept;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{deletes_source, fixable_refusals};
    use crate::ingest::api::FactOutcome;

    fn refused(rule: &str) -> FactOutcome {
        FactOutcome::Refused { rule: rule.into() }
    }

    /// A line over the server's length cap is refused at post and exists nowhere else, so the file
    /// has to survive for the owner to shorten it and post again.
    #[test]
    fn a_line_over_the_length_cap_keeps_the_source() {
        let kept = fixable_refusals(&[refused("content_too_long"), refused("content_too_long")]);
        assert_eq!(kept, vec![("content_too_long".to_string(), 2)]);
    }

    #[test]
    fn every_rule_an_edit_can_fix_keeps_the_source() {
        for rule in [
            "content_too_long",
            "invalid_namespace",
            "namespace_outside_grant",
            "supersedes_not_writable",
        ] {
            assert!(!fixable_refusals(&[refused(rule)]).is_empty(), "{rule} would delete the file");
        }
    }

    /// A tripwire refusal means the line holds a credential. Keeping the file for it keeps the
    /// credential on disk, and no edit to the line makes it storable.
    #[test]
    fn a_tripwire_refusal_alone_lets_the_source_go() {
        assert!(
            fixable_refusals(&[refused("pem_private_key"), refused("aws_access_key")]).is_empty()
        );
    }

    #[test]
    fn deleting_the_source_needs_a_submit_and_says_why() {
        assert_eq!(deletes_source(false, false), Ok(false));
        assert_eq!(deletes_source(false, true), Ok(false));
        assert_eq!(deletes_source(true, true), Ok(true));

        let refused = deletes_source(true, false).expect_err("a dry run must not delete");
        assert!(refused.contains("needs --submit"), "{refused}");
        // The message has to name what is lost. "Invalid combination" tells the owner nothing about
        // why the safe answer is to keep the file.
        assert!(refused.contains("only copy"), "{refused}");
    }
}
