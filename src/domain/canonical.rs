//! Canonical registry keys (Phase 2 spec §4).
//!
//! Six writers without a scheme produce `desktop.gpu`, `machines.desktop.gpu` and
//! `hardware.desktop.gpu` for one fact. The PRD is explicit that preventing that beats cleaning it
//! up, so a key is checked on write against a fixed shape and a closed domain vocabulary.
//!
//! Rejection alone is not enough. A model that gets a bare error invents a third variant rather
//! than the canonical name, which is why every rejection carries the closest valid key and why the
//! caller records the rejected guess as an alias. That turns each naming mistake into a one-line
//! redirect instead of a duplicate fact.
//!
//! Three outcomes, and the line between them is the whole design:
//!
//! 1. **Repaired silently.** Surrounding whitespace, upper case, repeated dots, dash runs, and
//!    separator characters. These are mechanical: every variant lands on one key, no word changes,
//!    and there is nothing for the caller to decide.
//! 2. **Rejected with a suggestion.** A domain outside the vocabulary, a singular domain, a
//!    pluralised attribute, a missing domain. Each of these repairs guesses at intent, and a guess
//!    belongs in the alias table where it is visible, not applied silently to a write. Accepting a
//!    near-miss quietly is how the scheme stops meaning anything.
//! 3. **Rejected with nothing.** Empty, one segment, five segments, non-ascii, a leading or
//!    trailing dot. There is no reading of these with one plausible answer, and a wrong suggestion
//!    recorded as an alias is worse than no suggestion.
//!
//! No I/O. The alias table, the registry rows and the transport all live elsewhere.

use crate::domain::errors::{DomainError, Result};

/// The closed first-segment vocabulary. Adding to it is a deliberate act, not a side effect of a
/// model guessing.
pub const DOMAINS: &[&str] =
    &["machines", "services", "credentials", "routes", "datasets", "people", "accounts"];

/// The coarse categories 'kind' may take.
///
/// Narrower than `DOMAINS` on purpose: `kind` is the Phase 1 column and its five values are what
/// the tool schema and the stored rows already use. `people` and `accounts` keys have no kind yet,
/// and inventing one here would put a value in the column that nothing else knows about.
pub const KINDS: &[&str] = &["host", "service", "credential-ref", "model-route", "dataset"];

const MIN_SEGMENTS: usize = 2;
const MAX_SEGMENTS: usize = 4;

/// How far a first segment may be from a vocabulary word and still be read as a typo of it.
///
/// One, counting an adjacent swap as one. Two admits `houses` as `routes` and `counts` as
/// `accounts`, which are confident wrong answers rather than typos.
const MAX_TYPO: usize = 1;

/// Words a writer reaches for that mean a domain but are not one: singular forms, the `kind` word
/// leaking into the key position, and the plain synonyms.
///
/// The singulars are also within `MAX_TYPO` of their plurals, so they would be caught anyway. They
/// are listed because the rule is "plural domain", not "close enough to a plural domain", and
/// `person` to `people` is four edits away and reachable no other way.
const DOMAIN_ALIASES: &[(&str, &str)] = &[
    ("machine", "machines"),
    ("host", "machines"),
    ("hosts", "machines"),
    ("hardware", "machines"),
    ("server", "machines"),
    ("servers", "machines"),
    ("service", "services"),
    ("credential", "credentials"),
    ("credential-ref", "credentials"),
    ("creds", "credentials"),
    ("secret", "credentials"),
    ("secrets", "credentials"),
    ("route", "routes"),
    ("model-route", "routes"),
    ("dataset", "datasets"),
    ("data", "datasets"),
    ("person", "people"),
    ("account", "accounts"),
];

/// Attributes that belong to exactly one domain, used to put a domain in front of a key that has
/// none.
///
/// Deliberately short. `location`, `path`, `item`, `model`, `url` and `username` are all missing
/// because each is plausible under two or more domains: `credentials.postgres.location` and
/// `datasets.tidewallet.location` both exist, and `routes.coding.model` reads exactly like the model
/// name of a machine. An entry here is a claim that no second reading exists.
const ATTRIBUTE_DOMAINS: &[(&str, &str)] = &[
    ("os", "machines"),
    ("gpu", "machines"),
    ("cpu", "machines"),
    ("ram", "machines"),
    ("disk", "machines"),
    ("arch", "machines"),
    ("kernel", "machines"),
    ("hostname", "machines"),
    ("endpoint", "services"),
    ("port", "services"),
    ("dsn", "services"),
    ("email", "people"),
    ("phone", "people"),
];

/// Attribute names already known to be singular, which is the only way the "singular attribute"
/// rule can be enforced at all.
///
/// English plurals are not decidable from spelling: `address`, `status` and `os` itself all end in
/// `s` while being singular, so a rule against a trailing `s` would reject the spec's own first
/// example. What is decidable is whether dropping the `s` lands on a name already in use, which
/// catches `oss` for `os` and `endpoints` for `endpoint`. An attribute outside this list is
/// accepted as written, because the alternative is refusing keys for a rule the server cannot
/// actually evaluate.
const KNOWN_ATTRIBUTES: &[&str] = &[
    "os", "gpu", "cpu", "ram", "disk", "arch", "kernel", "hostname", "host", "endpoint", "port",
    "dsn", "url", "email", "phone", "location", "path", "item", "model", "version", "region",
    "size", "key", "token", "owner", "id", "name",
];

/// Normalised canonical key, or a rejection whose message carries the closest valid key so the
/// caller can retry without a round trip through the user.
pub fn validate_key(key: &str) -> Result<String> {
    let normalised = normalise(key);
    match judge(&normalised) {
        None => Ok(normalised),
        Some(reason) => Err(rejection(key, reason)),
    }
}

/// The nearest valid key, or None when nothing plausible is close. Used both in the rejection
/// message and to record the rejected guess as an alias.
pub fn suggest_key(key: &str) -> Option<String> {
    // Same normalisation the validator ran, so a key the validator accepts cannot get a suggestion
    // that disagrees with what would have been stored.
    let mut segments = repairable_segments(key)?;
    let last = segments.len() - 1;

    // Attribute before domain: de-pluralising first is what lets `desktop.gpus` find the `gpu`
    // hint and come back as `machines.desktop.gpu`.
    if let Some(singular) = singular_attribute(&segments[last]) {
        segments[last] = singular.to_string();
    }

    if !DOMAINS.contains(&segments[0].as_str()) {
        match domain_alias(&segments[0]).or_else(|| nearest_domain(&segments[0])) {
            Some(domain) => segments[0] = domain.to_string(),
            None => {
                // The first segment says nothing usable, so the attribute has to. With two
                // segments the caller wrote entity and attribute and the domain is missing; with
                // three or more the first segment is in the domain slot and is simply wrong.
                let inferred = attribute_domain(&segments[segments.len() - 1])?;
                if segments.len() == MIN_SEGMENTS {
                    segments.insert(0, inferred.to_string());
                } else {
                    segments[0] = inferred.to_string();
                }
            }
        }
    }

    let candidate = segments.join(".");
    // A suggestion that would itself be rejected is worse than none: the caller writes it into the
    // alias table as a canonical target.
    is_canonical(&candidate).then_some(candidate)
}

pub fn validate_kind(kind: &str) -> Result<String> {
    let normalised = normalise_kind(kind);
    if KINDS.contains(&normalised.as_str()) {
        return Ok(normalised);
    }
    // A writer that pluralises the kind means the same category. The vocabulary is five words, so
    // this is the only repair worth making before naming them all.
    if let Some(singular) = normalised.strip_suffix('s') {
        if KINDS.contains(&singular) {
            return Ok(singular.to_string());
        }
    }
    Err(DomainError::validation(format!(
        "invalid registry kind {kind:?}. Use one of {}.",
        KINDS.join(", ")
    )))
}

/// Does this key already satisfy the scheme? Cheap check for the read path.
pub fn is_canonical(key: &str) -> bool {
    judge(key).is_none()
}

/// The silent half. Every repair here is mechanical: no word changes and no segment appears or
/// disappears, so the result is the one key every spelling of the same intent lands on.
fn normalise(key: &str) -> String {
    let trimmed = key.trim();

    // An underscore is either a word joiner or a segment separator, and structure decides which.
    // With no dot in the key the underscores carry the whole shape, so they are separators
    // (`machines_desktop_os`). With a dot present the dots carry it, so an underscore or a space
    // joins words inside a segment (`machines.desktop.gpu memory` is one attribute, not two
    // segments). Reading a space as a segment break when dots are already doing that job would
    // invent a segment the caller never wrote.
    let joiners_separate = !trimmed.contains('.');
    let mut flat = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        if c == '_' || c.is_whitespace() {
            flat.push(if joiners_separate { '.' } else { '-' });
        } else {
            flat.push(c.to_ascii_lowercase());
        }
    }

    // Collapse dot runs in place rather than by dropping empty segments. Dropping them would erase
    // a leading or trailing dot, which is the one separator mistake that must not be forgiven:
    // `.machines.desktop.os` has a hole where a segment should be, and whether the caller meant to
    // remove the dot or fill the hole is not knowable.
    let mut collapsed = String::with_capacity(flat.len());
    let mut previous_dot = false;
    for c in flat.chars() {
        if c == '.' && previous_dot {
            continue;
        }
        previous_dot = c == '.';
        collapsed.push(c);
    }

    collapsed.split('.').map(tidy_segment).collect::<Vec<_>>().join(".")
}

/// Collapse dash runs and drop edge dashes, which is what a trimmed or double-joined word leaves
/// behind. `is_canonical` refuses both shapes for the same reason: if the cheap read-path check
/// accepted a key that `normalise` would have changed, the same fact would sit under two keys.
fn tidy_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut previous_dash = false;
    for c in segment.chars() {
        if c == '-' && previous_dash {
            continue;
        }
        previous_dash = c == '-';
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

fn normalise_kind(kind: &str) -> String {
    let mut out = String::with_capacity(kind.trim().len());
    let mut previous_dash = false;
    for c in kind.trim().chars() {
        let mapped = if c == '_' || c.is_whitespace() { '-' } else { c.to_ascii_lowercase() };
        if mapped == '-' && previous_dash {
            continue;
        }
        previous_dash = mapped == '-';
        out.push(mapped);
    }
    out.trim_matches('-').to_string()
}

/// Every rule, in one place, so `validate_key` and `is_canonical` cannot drift apart. `None` means
/// the key is canonical; the string is what to tell the caller.
fn judge(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        return Some("the key is empty");
    }
    let segments: Vec<&str> = key.split('.').collect();
    if segments.len() < MIN_SEGMENTS || segments.len() > MAX_SEGMENTS {
        return Some("a key has two to four dot-separated segments");
    }
    if !segments.iter().all(|s| is_valid_segment(s)) {
        return Some(
            "each segment is lowercase [a-z0-9-], and may not be empty or start or end with a dash",
        );
    }
    if !DOMAINS.contains(&segments[0]) {
        return Some("the first segment is not one of the known domains");
    }
    if singular_attribute(segments[segments.len() - 1]).is_some() {
        return Some("the last segment is a plural of an attribute already in use");
    }
    None
}

fn is_valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !segment.starts_with('-')
        && !segment.ends_with('-')
        && !segment.contains("--")
}

/// The segments a repair can work from, or `None` when the shape itself is the problem. Anything
/// rejected here is rejected with no suggestion.
fn repairable_segments(key: &str) -> Option<Vec<String>> {
    let normalised = normalise(key);
    if normalised.is_empty() {
        return None;
    }
    let segments: Vec<&str> = normalised.split('.').collect();
    if segments.len() < MIN_SEGMENTS || segments.len() > MAX_SEGMENTS {
        return None;
    }
    if !segments.iter().all(|s| is_valid_segment(s)) {
        return None;
    }
    Some(segments.into_iter().map(str::to_string).collect())
}

/// The matched constant, not a slice of the input, so a caller can rewrite the segment it came
/// from without holding a borrow of it.
fn singular_attribute(segment: &str) -> Option<&'static str> {
    let stripped = segment.strip_suffix('s')?;
    KNOWN_ATTRIBUTES.iter().find(|a| **a == stripped).copied()
}

fn domain_alias(segment: &str) -> Option<&'static str> {
    DOMAIN_ALIASES.iter().find(|(wrong, _)| *wrong == segment).map(|(_, right)| *right)
}

fn attribute_domain(segment: &str) -> Option<&'static str> {
    ATTRIBUTE_DOMAINS.iter().find(|(attr, _)| *attr == segment).map(|(_, domain)| *domain)
}

/// The single closest vocabulary word within `MAX_TYPO`, or `None`. A tie is `None`: two words
/// equally close means the input picks neither.
fn nearest_domain(segment: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    let mut tied = false;
    for &domain in DOMAINS {
        let distance = edit_distance(segment, domain);
        match best {
            Some((best_distance, _)) if distance > best_distance => {}
            Some((best_distance, _)) if distance == best_distance => tied = true,
            _ => {
                best = Some((distance, domain));
                tied = false;
            }
        }
    }
    match best {
        Some((distance, domain)) if distance <= MAX_TYPO && !tied => Some(domain),
        _ => None,
    }
}

/// Damerau-Levenshtein. The transposition step is there because a swapped pair (`rotues`,
/// `serivces`) is the most common way a word gets mistyped and plain Levenshtein scores it 2,
/// which would put every real typo out of budget.
fn edit_distance(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut grid = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in grid.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        grid[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let substitution = usize::from(a[i - 1] != b[j - 1]);
            let mut best =
                (grid[i - 1][j] + 1).min(grid[i][j - 1] + 1).min(grid[i - 1][j - 1] + substitution);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(grid[i - 2][j - 2] + 1);
            }
            grid[i][j] = best;
        }
    }
    grid[a.len()][b.len()]
}

/// The refusal names the scheme rather than saying "invalid", and appends the closest key when
/// there is one, because that is what lets the caller retry in the same turn.
fn rejection(key: &str, reason: &'static str) -> DomainError {
    let mut message = format!(
        "invalid registry key {key:?}: {reason}. Use <domain>.<entity>.<attribute>, \
two to four lowercase [a-z0-9-] segments, where <domain> is one of {}.",
        DOMAINS.join(", ")
    );
    if let Some(suggestion) = suggest_key(key) {
        message.push_str(&format!(" Closest valid key: {suggestion:?}."));
    }
    DomainError::validation(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key in the spec §4 table, which is the definition of correct.
    const SPEC_EXAMPLES: &[&str] = &[
        "machines.desktop.os",
        "machines.desktop.gpu",
        "services.postgres.endpoint",
        "credentials.postgres.location",
        "routes.coding.model",
        "datasets.tidewallet.location",
    ];

    #[test]
    fn accepts_the_spec_examples_unchanged() {
        for key in SPEC_EXAMPLES {
            assert_eq!(validate_key(key).unwrap(), *key);
            assert!(is_canonical(key), "{key} should already be canonical");
        }
    }

    #[test]
    fn accepts_two_and_four_segments_and_refuses_one_and_five() {
        assert!(validate_key("machines.hostname").is_ok());
        assert!(validate_key("machines.desktop.gpu.vram").is_ok());
        assert!(validate_key("machines").is_err());
        assert!(validate_key("machines.desktop.gpu.vram.size").is_err());
    }

    #[test]
    fn repairs_case_and_surrounding_whitespace_silently() {
        assert_eq!(validate_key("  Machines.Desktop.OS  ").unwrap(), "machines.desktop.os");
    }

    #[test]
    fn repairs_repeated_dots_and_dash_runs_silently() {
        assert_eq!(validate_key("machines..desktop..os").unwrap(), "machines.desktop.os");
        assert_eq!(validate_key("machines.desktop.--os").unwrap(), "machines.desktop.os");
    }

    #[test]
    fn a_key_without_dots_reads_its_underscores_as_segment_separators() {
        assert_eq!(validate_key("machines_desktop_os").unwrap(), "machines.desktop.os");
        assert_eq!(validate_key("MACHINES_DESKTOP_OS").unwrap(), "machines.desktop.os");
        assert_eq!(validate_key("machines desktop os").unwrap(), "machines.desktop.os");
    }

    #[test]
    fn a_key_with_dots_reads_its_underscores_as_word_joiners() {
        // Dots already carry the structure here, so an underscore or a space joins words inside one
        // segment. Pinning this because it is a choice: the other reading would silently add a
        // segment the caller did not write.
        assert_eq!(validate_key("machines.desktop_os").unwrap(), "machines.desktop-os");
        assert_eq!(
            validate_key("machines.desktop.gpu memory").unwrap(),
            "machines.desktop.gpu-memory"
        );
    }

    #[test]
    fn a_repair_the_validator_makes_silently_is_the_same_key_suggest_offers() {
        for key in ["Machines.Desktop.OS", "machines_desktop_os", "machines.desktop_os"] {
            let accepted = validate_key(key).unwrap();
            assert_eq!(suggest_key(key), Some(accepted), "suggestion disagreed for {key}");
        }
    }

    #[test]
    fn a_missing_domain_is_refused_and_the_attribute_supplies_one() {
        assert!(validate_key("desktop.gpu").is_err());
        assert_eq!(suggest_key("desktop.gpu").as_deref(), Some("machines.desktop.gpu"));
        assert_eq!(suggest_key("postgres.endpoint").as_deref(), Some("services.postgres.endpoint"));
        assert_eq!(suggest_key("dana.email").as_deref(), Some("people.dana.email"));
    }

    #[test]
    fn a_singular_domain_is_refused_and_pluralised() {
        assert!(validate_key("machine.desktop.gpu").is_err());
        assert_eq!(suggest_key("machine.desktop.gpu").as_deref(), Some("machines.desktop.gpu"));
        assert_eq!(
            suggest_key("credential.postgres.location").as_deref(),
            Some("credentials.postgres.location")
        );
        // Four edits from its plural, so only the alias table reaches it.
        assert_eq!(suggest_key("person.dana.email").as_deref(), Some("people.dana.email"));
    }

    #[test]
    fn a_domain_outside_the_vocabulary_is_refused_and_mapped() {
        assert!(validate_key("hardware.desktop.gpu").is_err());
        assert_eq!(suggest_key("hardware.desktop.gpu").as_deref(), Some("machines.desktop.gpu"));
        assert_eq!(
            suggest_key("hosts.desktop.location").as_deref(),
            Some("machines.desktop.location")
        );
        assert_eq!(
            suggest_key("data.tidewallet.location").as_deref(),
            Some("datasets.tidewallet.location")
        );
    }

    #[test]
    fn a_pluralised_attribute_is_refused_and_made_singular() {
        assert!(validate_key("machines.desktop.oss").is_err());
        assert_eq!(suggest_key("machines.desktop.oss").as_deref(), Some("machines.desktop.os"));
        assert_eq!(
            suggest_key("services.postgres.endpoints").as_deref(),
            Some("services.postgres.endpoint")
        );
    }

    #[test]
    fn an_attribute_that_only_looks_plural_is_left_alone() {
        // 'address', 'status' and 'os' all end in s while being singular, which is why the rule
        // asks whether dropping the s lands on a name in use rather than whether an s is present.
        for key in ["machines.desktop.address", "machines.desktop.status", "machines.desktop.os"] {
            assert_eq!(validate_key(key).unwrap(), key);
        }
    }

    #[test]
    fn a_mistyped_domain_is_read_as_the_word_it_is_one_edit_from() {
        assert_eq!(suggest_key("machnes.desktop.gpu").as_deref(), Some("machines.desktop.gpu"));
        assert_eq!(suggest_key("rotues.coding.model").as_deref(), Some("routes.coding.model"));
        assert_eq!(
            suggest_key("serivces.postgres.port").as_deref(),
            Some("services.postgres.port")
        );
        assert_eq!(
            suggest_key("credentails.postgres.location").as_deref(),
            Some("credentials.postgres.location")
        );
    }

    #[test]
    fn a_word_two_edits_away_is_not_a_typo_of_a_domain() {
        // 'houses' scores 2 against 'routes' and 'counts' scores 2 against 'accounts'. Suggesting
        // either would record a confident wrong answer as an alias.
        assert_eq!(nearest_domain("houses"), None);
        assert_eq!(nearest_domain("counts"), None);
        assert_eq!(nearest_domain("hardware"), None);
    }

    #[test]
    fn two_mistakes_in_one_key_are_repaired_together() {
        assert_eq!(suggest_key("desktop.gpus").as_deref(), Some("machines.desktop.gpu"));
        assert_eq!(suggest_key("machine_desktop_oss").as_deref(), Some("machines.desktop.os"));
        assert_eq!(suggest_key("Hardware.Desktop.GPUs").as_deref(), Some("machines.desktop.gpu"));
    }

    #[test]
    fn a_structurally_broken_key_is_refused_with_no_suggestion() {
        for bad in [
            "",
            "   ",
            "machines",
            "gpu",
            "machinesdesktopos",
            "machines.desktop.gpu.vram.size",
            "machines.büro.os",
            ".machines.desktop.os",
            "machines.desktop.os.",
            "...",
            "machines.desktop.os;drop table registry",
        ] {
            assert!(validate_key(bad).is_err(), "should have refused {bad:?}");
            assert_eq!(suggest_key(bad), None, "should not have guessed at {bad:?}");
        }
    }

    #[test]
    fn a_first_segment_with_nothing_behind_it_gets_no_suggestion() {
        // Nothing in the vocabulary is close and the attribute belongs to no single domain, so any
        // answer here is invention.
        for bad in ["widgets.gizmo.thing", "postgres.location", "coding.model", "foo.bar.baz"] {
            assert!(validate_key(bad).is_err(), "should have refused {bad:?}");
            assert_eq!(suggest_key(bad), None, "should not have guessed at {bad:?}");
        }
    }

    #[test]
    fn an_attribute_shared_by_two_domains_infers_neither() {
        assert_eq!(attribute_domain("location"), None);
        assert_eq!(attribute_domain("model"), None);
        assert_eq!(attribute_domain("path"), None);
    }

    #[test]
    fn the_refusal_message_carries_the_closest_key_and_the_vocabulary() {
        let e = validate_key("hardware.desktop.gpu").unwrap_err();
        let message = e.client_message();
        assert!(message.contains("machines.desktop.gpu"), "no closest key in {message:?}");
        assert!(message.contains("credentials"), "no vocabulary in {message:?}");

        let e = validate_key("foo.bar.baz").unwrap_err();
        assert!(
            !e.client_message().contains("Closest"),
            "offered a closest key when there was none"
        );
    }

    #[test]
    fn every_accepted_key_is_canonical_and_survives_a_second_pass() {
        // The read path skips normalisation when `is_canonical` says yes. If the two ever disagree,
        // one fact ends up stored under two keys, which is the failure the scheme exists to stop.
        for input in [
            "machines.desktop.os",
            "  Machines.Desktop.OS  ",
            "machines_desktop_os",
            "machines..desktop..os",
            "machines.desktop.--os",
            "machines.desktop_ram.size",
            "machines.desktop.gpu memory",
            "MACHINES.hostname",
        ] {
            let first = validate_key(input).unwrap();
            assert!(is_canonical(&first), "{first} accepted but not canonical");
            assert_eq!(
                validate_key(&first).unwrap(),
                first,
                "{first} did not survive a second pass"
            );
        }
    }

    #[test]
    fn a_suggestion_is_never_something_the_validator_would_refuse() {
        for input in [
            "desktop.gpu",
            "machine.desktop.gpu",
            "hardware.desktop.gpu",
            "machines.desktop.oss",
            "Machines.Desktop.OS",
            "machines_desktop_os",
            "",
            "machines",
            "machines.desktop.gpu.vram.size",
            ".machines.desktop.os",
            "widgets.gizmo.thing",
        ] {
            if let Some(suggestion) = suggest_key(input) {
                assert_eq!(
                    validate_key(&suggestion).unwrap(),
                    suggestion,
                    "suggested {suggestion} for {input}, which the validator refuses"
                );
            }
        }
    }

    #[test]
    fn the_repair_tables_only_ever_point_into_the_vocabulary() {
        for &(wrong, right) in DOMAIN_ALIASES {
            assert!(DOMAINS.contains(&right), "{right} is not a domain");
            assert!(!DOMAINS.contains(&wrong), "{wrong} is a domain, so this alias never fires");
        }
        for &(attribute, domain) in ATTRIBUTE_DOMAINS {
            assert!(DOMAINS.contains(&domain), "{domain} is not a domain");
            // The hint is looked up after de-pluralisation, so it has to be a name that step knows.
            assert!(
                KNOWN_ATTRIBUTES.contains(&attribute),
                "{attribute} hints a domain but is not a known attribute"
            );
        }
    }

    #[test]
    fn accepts_the_five_kinds() {
        for kind in KINDS {
            assert_eq!(validate_kind(kind).unwrap(), *kind);
        }
    }

    #[test]
    fn repairs_kind_case_and_separators() {
        assert_eq!(validate_kind("  Credential_Ref ").unwrap(), "credential-ref");
        assert_eq!(validate_kind("Model Route").unwrap(), "model-route");
    }

    #[test]
    fn accepts_a_pluralised_kind_as_the_category_it_names() {
        assert_eq!(validate_kind("hosts").unwrap(), "host");
        assert_eq!(validate_kind("datasets").unwrap(), "dataset");
    }

    #[test]
    fn refuses_a_kind_outside_the_vocabulary_and_names_the_set() {
        let e = validate_kind("machine").unwrap_err();
        assert!(e.client_message().contains("credential-ref"), "the message must list the set");
        for bad in ["", "gpu", "credentials", "person"] {
            assert!(validate_kind(bad).is_err(), "should have refused {bad:?}");
        }
    }
}
