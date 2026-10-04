//! Output rendering, kept pure so it can be tested without a server.
//!
//! Every line here matches `bin/lumberroom.mjs` character for character, padding included. The owner
//! reads these and `scripts/policy-test.sh` greps them, so a widened column is a broken script.

use crate::wire::{
    AliasRecord, ClientRecord, ClientStatsRow, HistoryEntry, Memory, StatsTotals, ToolStatsRow,
};

/// JavaScript's `padEnd`/`padStart`: pad to width, never truncate.
fn pad_end(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

fn pad_start(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - len))
    }
}

/// A JS number interpolated into a string: an integral float loses its `.0`.
pub fn number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

pub fn search_line(score: f64, namespace: &str, content: &str) -> String {
    format!("{score:.3}  [{namespace}] {content}")
}

pub fn write_line(deduplicated: bool, id: &str, namespace: &str) -> String {
    let verb = if deduplicated { "exists" } else { "written" };
    format!("{verb} {id} in {namespace}")
}

pub fn totals_line(t: &StatsTotals) -> String {
    let rate = t.unprompted_rate.map(number).unwrap_or_else(|| "n/a".to_string());
    format!(
        "totals: {} calls, {} failed, unprompted {} ({rate})",
        t.calls, t.failures, t.unprompted
    )
}

pub fn tool_stats_line(row: &ToolStatsRow) -> String {
    let p50 = row.p50_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string());
    let p95 = row.p95_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string());
    format!(
        "  {} {} calls  {} unprompted  p50 {p50}ms  p95 {p95}ms  [{}]",
        pad_end(&row.tool, 18),
        pad_start(&row.calls.to_string(), 4),
        pad_start(&row.unprompted.to_string(), 4),
        row.client
    )
}

pub fn client_stats_line(row: &ClientStatsRow) -> String {
    let ratio =
        row.write_to_read_ratio.map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".to_string());
    let unprompted = row
        .unprompted_write_rate
        .map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_else(|| "n/a".to_string());
    format!(
        "  {} calls {}  reads {}  writes {}  write/read {ratio}  unprompted-write {unprompted}",
        pad_end(&row.client, 18),
        pad_start(&row.calls.to_string(), 4),
        pad_start(&row.reads.to_string(), 4),
        pad_start(&row.writes.to_string(), 4),
    )
}

pub fn client_line(c: &ClientRecord) -> String {
    let consent = if c.consented_at.is_some() { "consented" } else { "pending consent" };
    let revoked = if c.revoked_at.is_some() { "  REVOKED" } else { "" };
    // An older server sends no `label`; the registered name is the best it can offer.
    let name = c.label.as_deref().unwrap_or(&c.client_name);
    // Show the registered name only when the owner named the client, so the line still says what
    // the client calls itself on the consent page.
    let registered = match &c.owner_label {
        Some(_) => format!("  (registered as {})", c.client_name),
        None => String::new(),
    };
    format!("{}  {name}  via {}  {consent}{revoked}{registered}", c.client_id, c.registered_via)
}

/// The candidate line `forget` prints before it asks for confirmation.
///
/// The number is what `--pick` takes, and the score is what tells the reader where the list stops
/// being an answer and starts being the ranker filling its limit. A lookup by id has no score, so
/// it prints none rather than a made-up one.
pub fn candidate_line(
    n: usize,
    id: &str,
    namespace: &str,
    content: &str,
    score: Option<f64>,
) -> String {
    let score = match score {
        Some(s) => format!("  {s:.3}"),
        None => String::new(),
    };
    format!(
        "  {n:>2}.{score}  {id}  [{namespace}]  {}",
        content.chars().take(100).collect::<String>()
    )
}

/// One namespace, one directory. A colon in a path is legal on Linux and a nuisance everywhere.
pub fn obsidian_dir(namespace: &str) -> String {
    namespace.replace([':', '/'], "-")
}

/// The note body, frontmatter included.
///
/// An empty `content` means the row came back unopened rather than blank: `write::run` refuses an
/// empty fact, so nothing in the store has one. The placeholder says so instead of writing a note
/// that looks like a bug.
pub fn obsidian_note(m: &Memory) -> String {
    let tags = if m.tags.is_empty() {
        "[]".to_string()
    } else {
        let quoted: Vec<String> =
            m.tags.iter().map(|t| serde_json::Value::String(t.clone()).to_string()).collect();
        format!("[{}]", quoted.join(", "))
    };
    let body = if m.content.is_empty() {
        "*(no plaintext content at this sensitivity level)*"
    } else {
        m.content.as_str()
    };
    format!(
        "---\nid: {}\nnamespace: {}\nsensitivity: {}\nsource_client: {}\ncreated_at: {}\ntags: {tags}\n---\n{body}\n",
        m.id, m.namespace, m.sensitivity, m.source_client, m.created_at
    )
}

/// A date the owner reads at a glance rather than an ISO instant. Falls back to the raw string on
/// a parse failure, the same defensiveness `review` uses for dates it does not reformat: a bad
/// value should still print rather than vanish.
fn pretty_date(iso: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => {
            let dt = dt.with_timezone(&chrono::Utc);
            let midnight = dt.time() == chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap();
            if midnight {
                dt.format("%-d %b %Y").to_string()
            } else {
                dt.format("%-d %b %Y %H:%M UTC").to_string()
            }
        }
        Err(_) => iso.to_string(),
    }
}

/// Orders a supersession chain oldest first by following `superseded_by` links rather than by
/// date. S4 lets a later approval state an older fact, so link order and date order can disagree,
/// and the link is the ground truth: it is what the chain actually is.
///
/// Falls back to date order (`occurred_at`, else `created_at`) when the entries do not form one
/// clean chain: a malformed or partial response should still render something a person can read
/// rather than nothing, which is this system's worst failure.
pub fn order_chain(entries: &[HistoryEntry]) -> Vec<&HistoryEntry> {
    if entries.is_empty() {
        return Vec::new();
    }
    let by_id: std::collections::HashMap<&str, &HistoryEntry> =
        entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let successors: std::collections::HashSet<&str> =
        entries.iter().filter_map(|e| e.superseded_by.as_deref()).collect();
    let roots: Vec<&HistoryEntry> =
        entries.iter().filter(|e| !successors.contains(e.id.as_str())).collect();

    let Some(root) = one_root(&roots) else {
        let mut all: Vec<&HistoryEntry> = entries.iter().collect();
        by_date(&mut all);
        return all;
    };

    let mut ordered: Vec<&HistoryEntry> = Vec::with_capacity(entries.len());
    let mut seen = std::collections::HashSet::new();
    let mut current = root;
    loop {
        if !seen.insert(current.id.as_str()) {
            break; // a cycle in bad data; stop rather than loop forever
        }
        ordered.push(current);
        match current.superseded_by.as_deref().and_then(|id| by_id.get(id)) {
            Some(next) => current = next,
            None => break,
        }
    }
    if ordered.len() < entries.len() {
        let mut rest: Vec<&HistoryEntry> =
            entries.iter().filter(|e| !seen.contains(e.id.as_str())).collect();
        by_date(&mut rest);
        ordered.extend(rest);
    }
    ordered
}

fn one_root<'a>(roots: &[&'a HistoryEntry]) -> Option<&'a HistoryEntry> {
    match roots {
        [root] => Some(root),
        _ => None,
    }
}

fn by_date(list: &mut [&HistoryEntry]) {
    list.sort_by(|a, b| date_key(a).cmp(&date_key(b)));
}

fn date_key(e: &HistoryEntry) -> String {
    e.occurred_at.clone().unwrap_or_else(|| e.created_at.clone())
}

/// One line of a chain: the value, the period it held, and how it ended. Reads as a sentence
/// scanning down the list, which is the point: "port is 8080: 1 Aug 2026 until 20 Aug 2026,
/// superseded" followed by "port is 8787: since 20 Aug 2026, current".
pub fn history_line(e: &HistoryEntry) -> String {
    let period = match (&e.occurred_at, &e.occurred_until) {
        (Some(s), Some(u)) => format!("{} until {}", pretty_date(s), pretty_date(u)),
        (Some(s), None) => format!("since {}", pretty_date(s)),
        (None, Some(u)) => format!("until {} (start unknown)", pretty_date(u)),
        (None, None) => "always".to_string(),
    };
    let ending = if e.superseded_by.is_some() {
        // A superseded row with no end is the same-day case: the successor started on the day the
        // predecessor did, so closing the period would have written `[T, T)`, which reads as never
        // true. Saying "superseded" alone leaves the owner to notice a missing date on a line that
        // otherwise looks finished, and the as-of read they cannot see is the one that suffers.
        if e.occurred_at.is_some() && e.occurred_until.is_none() {
            "superseded, end date unknown"
        } else {
            "superseded"
        }
    } else if e.occurred_until.is_some() {
        "ended, nothing replaced it"
    } else {
        "current"
    };
    format!("{}: {period}, {ending}", e.content)
}

/// The full chain, oldest first, one rendered line per row.
pub fn history_lines(entries: &[HistoryEntry]) -> Vec<String> {
    order_chain(entries).into_iter().map(history_line).collect()
}

/// One alias, matching the house style of `client_line` and `candidate_line`.
pub fn alias_line(a: &AliasRecord) -> String {
    let period = match (&a.since, &a.until) {
        (None, None) => "always".to_string(),
        (Some(s), None) => format!("since {}", pretty_date(s)),
        (None, Some(u)) => format!("until {}", pretty_date(u)),
        (Some(s), Some(u)) => format!("{} until {}", pretty_date(s), pretty_date(u)),
    };
    format!(
        "  {}  ->  {}  [{}]  {period}  ({})",
        pad_end(&a.alias, 20),
        a.canonical,
        a.namespace,
        a.origin
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_hit_keeps_three_decimals_and_the_namespace_bracket() {
        assert_eq!(
            search_line(0.8127, "user:me", "the owner prefers rust"),
            "0.813  [user:me] the owner prefers rust"
        );
    }

    #[test]
    fn a_deduplicated_write_says_exists() {
        assert_eq!(write_line(true, "abc", "user:me"), "exists abc in user:me");
        assert_eq!(write_line(false, "abc", "user:me"), "written abc in user:me");
    }

    #[test]
    fn tool_rows_keep_their_columns() {
        let row = ToolStatsRow {
            tool: "memory_search".into(),
            client: "claude-code".into(),
            calls: 12,
            unprompted: 3,
            p50_ms: Some(41),
            p95_ms: None,
        };
        assert_eq!(
            tool_stats_line(&row),
            "  memory_search        12 calls     3 unprompted  p50 41ms  p95 -ms  [claude-code]"
        );
    }

    #[test]
    fn client_rows_render_absent_rates_as_n_a() {
        let row = ClientStatsRow {
            client: "openwebui".into(),
            calls: 7,
            reads: 6,
            writes: 1,
            write_to_read_ratio: None,
            unprompted_write_rate: Some(0.5),
        };
        let line = client_stats_line(&row);
        assert!(line.contains("write/read n/a"), "{line}");
        assert!(line.ends_with("unprompted-write 50%"), "{line}");
        assert!(line.starts_with("  openwebui          calls    7"), "{line}");
    }

    #[test]
    fn totals_print_an_absent_rate_as_n_a_and_a_present_one_as_a_bare_number() {
        let t = StatsTotals { calls: 4, failures: 1, unprompted: 2, unprompted_rate: Some(0.5) };
        assert_eq!(totals_line(&t), "totals: 4 calls, 1 failed, unprompted 2 (0.5)");
        let t = StatsTotals { calls: 0, failures: 0, unprompted: 0, unprompted_rate: None };
        assert_eq!(totals_line(&t), "totals: 0 calls, 0 failed, unprompted 0 (n/a)");
        assert_eq!(number(1.0), "1");
    }

    #[test]
    fn a_revoked_client_is_labelled() {
        let c = ClientRecord {
            client_id: "cid".into(),
            client_name: "lumberroom".into(),
            registered_via: "dcr".into(),
            consented_at: None,
            revoked_at: Some("2026-08-01T00:00:00Z".into()),
            owner_label: None,
            label: None,
        };
        assert_eq!(client_line(&c), "cid  lumberroom  via dcr  pending consent  REVOKED");
    }

    #[test]
    fn client_line_prints_the_label_and_the_registered_name() {
        let c = ClientRecord {
            client_id: "cid".into(),
            client_name: "Claude".into(),
            registered_via: "dcr".into(),
            consented_at: Some("2026-08-01T00:00:00Z".into()),
            revoked_at: None,
            owner_label: Some("flow gate".into()),
            label: Some("flow gate".into()),
        };
        assert_eq!(client_line(&c), "cid  flow gate  via dcr  consented  (registered as Claude)");
    }

    #[test]
    fn client_line_falls_back_to_client_name_for_an_older_server() {
        let c = ClientRecord {
            client_id: "cid".into(),
            client_name: "Claude".into(),
            registered_via: "dcr".into(),
            consented_at: Some("2026-08-01T00:00:00Z".into()),
            revoked_at: None,
            owner_label: None,
            label: None,
        };
        assert_eq!(client_line(&c), "cid  Claude  via dcr  consented");
    }

    #[test]
    fn a_namespace_becomes_one_directory() {
        assert_eq!(obsidian_dir("project:lumberroom/core"), "project-lumberroom-core");
    }

    #[test]
    fn frontmatter_quotes_every_tag() {
        let m = Memory {
            id: "1111".into(),
            namespace: "user:me".into(),
            content: "the owner runs arm64".into(),
            tags: vec!["infra".into(), "hardware".into()],
            source_client: "cli".into(),
            sensitivity: "open".into(),
            created_at: "2026-08-20T09:00:00Z".into(),
        };
        let note = obsidian_note(&m);
        assert!(note.starts_with("---\nid: 1111\n"), "{note}");
        assert!(note.contains("tags: [\"infra\", \"hardware\"]"), "{note}");
        // No blank line after the closing fence: node builds the frontmatter as an array ending in
        // an empty string, and the body follows the fence directly.
        assert!(note.ends_with("---\nthe owner runs arm64\n"), "{note}");
    }

    #[test]
    fn an_unopened_row_gets_the_placeholder_body() {
        let m = Memory {
            id: "1".into(),
            namespace: "personal:finance".into(),
            content: String::new(),
            tags: vec![],
            source_client: "cli".into(),
            sensitivity: "private".into(),
            created_at: "2026-08-20T09:00:00Z".into(),
        };
        assert!(obsidian_note(&m).contains("*(no plaintext content at this sensitivity level)*"));
        assert!(obsidian_note(&m).contains("tags: []"));
    }

    #[test]
    fn a_candidate_line_clips_the_content_at_a_hundred_characters() {
        let long = "x".repeat(150);
        let line = candidate_line(1, "id", "global", &long, Some(0.42));
        assert!(line.ends_with(&"x".repeat(100)));
        assert_eq!(line.chars().filter(|c| *c == 'x').count(), 100);
    }

    #[test]
    fn a_candidate_line_carries_the_number_pick_takes_and_the_score_that_ranked_it() {
        let line = candidate_line(3, "abc", "user:me", "a fact", Some(0.7447));
        assert!(line.contains(" 3."), "the --pick number is missing: {line}");
        assert!(line.contains("0.745"), "the score is missing: {line}");
    }

    #[test]
    fn a_candidate_looked_up_by_id_prints_no_score_rather_than_a_made_up_one() {
        let line = candidate_line(1, "abc", "user:me", "a fact", None);
        assert!(!line.contains('.') || !line.contains("0."), "invented a score: {line}");
        assert!(line.contains("abc"));
    }

    fn entry(id: &str, content: &str, occurred_at: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: id.to_string(),
            content: content.to_string(),
            occurred_at: occurred_at.map(str::to_string),
            occurred_until: None,
            superseded_by: None,
            superseded_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn the_port_pair_reads_as_the_owners_sentence() {
        let mut old = entry("1", "port is 8080", Some("2026-08-01T00:00:00Z"));
        old.occurred_until = Some("2026-08-20T00:00:00Z".to_string());
        old.superseded_by = Some("2".to_string());
        let new = entry("2", "port is 8787", Some("2026-08-20T00:00:00Z"));

        assert_eq!(history_line(&old), "port is 8080: 1 Aug 2026 until 20 Aug 2026, superseded");
        assert_eq!(history_line(&new), "port is 8787: since 20 Aug 2026, current");
    }

    #[test]
    fn a_fact_that_ended_with_no_successor_says_so_rather_than_superseded() {
        let mut e = entry("1", "the trial", Some("2026-01-01T00:00:00Z"));
        e.occurred_until = Some("2026-03-01T00:00:00Z".to_string());
        assert_eq!(
            history_line(&e),
            "the trial: 1 Jan 2026 until 1 Mar 2026, ended, nothing replaced it"
        );
    }

    #[test]
    fn a_superseded_row_with_an_unknown_end_says_since_not_a_false_until() {
        // S4's second case: the successor states an older fact, so the link is written but the
        // end date is left unknown rather than guessed from arrival order. The same shape arrives
        // from a dump, where every line in a day carries that day's date and the successor cannot
        // start after the fact it replaces. The line names the gap: `since` alone read as finished
        // and sent the owner looking for an end date that was never written.
        let mut e = entry("1", "port is 8080", Some("2026-08-01T00:00:00Z"));
        e.superseded_by = Some("2".to_string());
        assert_eq!(
            history_line(&e),
            "port is 8080: since 1 Aug 2026, superseded, end date unknown"
        );
    }

    #[test]
    fn an_undated_standing_fact_reads_as_always_current() {
        let e = entry("1", "prefers rust", None);
        assert_eq!(history_line(&e), "prefers rust: always, current");
    }

    #[test]
    fn the_chain_follows_links_rather_than_dates() {
        // z -> y -> x is the true chain, but y is dated after x, so a date sort would put x
        // before y. The chain order must win.
        let mut z = entry("z", "z", Some("2026-08-01T00:00:00Z"));
        z.superseded_by = Some("y".to_string());
        let mut y = entry("y", "y", Some("2026-08-20T00:00:00Z"));
        y.superseded_by = Some("x".to_string());
        let x = entry("x", "x", Some("2026-08-10T00:00:00Z"));

        // Handed in an order that matches neither the chain nor the dates.
        let entries = vec![y, x, z];
        let ordered: Vec<&str> = order_chain(&entries).into_iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ordered, vec!["z", "y", "x"]);
    }

    #[test]
    fn a_broken_chain_falls_back_to_date_order_instead_of_panicking() {
        let a = entry("a", "a", Some("2026-08-01T00:00:00Z"));
        let b = entry("b", "b", Some("2026-08-02T00:00:00Z"));
        let entries = [a, b];
        let ordered: Vec<&str> = order_chain(&entries).into_iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ordered, vec!["a", "b"]);
    }

    #[test]
    fn an_alias_line_shows_the_period_and_the_origin() {
        let a = AliasRecord {
            namespace: "project:lumberroom".into(),
            alias: "warden".into(),
            canonical: "lumen".into(),
            since: None,
            until: Some("2026-05-01T00:00:00Z".into()),
            origin: "manual".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        let line = alias_line(&a);
        assert!(line.contains("warden"), "{line}");
        assert!(line.contains("lumen"), "{line}");
        assert!(line.contains("[project:lumberroom]"), "{line}");
        assert!(line.contains("until 1 May 2026"), "{line}");
        assert!(line.contains("(manual)"), "{line}");
    }
}
