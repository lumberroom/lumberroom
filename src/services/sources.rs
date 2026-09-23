//! The readable name of whoever wrote a row, for the MCP tools to print (decision 0020).
//!
//! `source_client` stores `Principal.client`, and for a built-in OAuth client that is a random
//! 32-character `client_id`. An agent reading `via l9Bo4qkodrWSqZQs3ZredgblRz22g9tw` cannot tell
//! which app wrote the fact and has no tool that would tell it. The MCP tools print the name the
//! client registered with instead. Storage, export and the archive keep the id, because a client
//! can be renamed and an audit wants the value that held at write time.
//!
//! The name is the client's own choice, approved by the owner at consent. Two approved clients
//! whose names compare equal each carry the date they were added, so a second "Codex" never reads
//! as the first.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use unicode_normalization::UnicodeNormalization;

use super::Ctx;
use crate::config::AuthMode;
use crate::ports::oauth::OauthClientRecord;
use crate::ports::OauthStore;

/// What a client with a blank or wholly invisible name reads as. The same words registration
/// substitutes for a missing `client_name`.
const UNNAMED: &str = "unnamed client";

/// Readable name for each stored `source_client`. A value no OAuth client matches maps to itself.
///
/// Matching is exact on `client_id`, so a static token labelled with something that looks like an
/// id keeps its label. Revoked clients are read too: a fact outlives the credential that wrote it,
/// and a revoked writer is still the app that wrote it.
///
/// It never fails a read. A client table that cannot be read logs a warning and every value prints
/// as stored: an id on the page beats a search or a digest that did not arrive.
pub async fn labels(ctx: &Ctx, stored: &[String]) -> HashMap<String, String> {
    resolve(ctx.repos.oauth.as_deref(), stored).await
}

/// The store `labels` reads, or `None` outside `AUTH_MODE=oauth`.
///
/// Without the built-in authorization server no request can arrive as an OAuth client, and the
/// only rows it could name came from an earlier configuration. Skipping the table keeps a token
/// deployment off a query it has no use for.
pub fn label_store(mode: AuthMode, store: &Arc<dyn OauthStore>) -> Option<Arc<dyn OauthStore>> {
    (mode == AuthMode::Oauth).then(|| Arc::clone(store))
}

async fn resolve(store: Option<&dyn OauthStore>, stored: &[String]) -> HashMap<String, String> {
    let identity = || stored.iter().map(|s| (s.clone(), s.clone())).collect();
    let Some(store) = store else { return identity() };
    if stored.is_empty() {
        return HashMap::new();
    }
    // Every client rather than the ones asked about. Whether a name needs a date depends on the
    // other clients sharing it, and a label that changed with the rows on the page would name one
    // app two ways.
    let clients = match store.list_clients(true).await {
        Ok(clients) => clients,
        Err(e) => {
            tracing::warn!(
                error = %e.log_message(),
                "could not read OAuth clients; sources print as stored"
            );
            return identity();
        }
    };
    let named = name_clients(&clients);
    stored.iter().map(|s| (s.clone(), named.get(s).cloned().unwrap_or_else(|| s.clone()))).collect()
}

/// One label per `client_id`, unique among the approved clients.
///
/// A duplicate carries the date the client was added, `created_at`, never its approval date.
/// `set_client_grant` rewrites `consented_at` on every grant edit, so a label keyed on it would
/// change each time the owner edited that client's access. `created_at` never moves.
///
/// Only an approved client counts toward a duplicate. Registration is open to anyone who can reach
/// the server, and a client nobody approved holds no grant and cannot have written a row; letting
/// it count would let a stranger push a date onto the owner's own "Codex" by registering the name.
/// An unapproved client that shares a name with an approved one reads `Codex (not approved)`, so
/// the two never print the same.
fn name_clients(clients: &[OauthClientRecord]) -> HashMap<String, String> {
    // Compared in the printed form, so a blank name and a literal "unnamed client" are one name.
    let key = |c: &OauthClientRecord| comparable(&display(&c.client_name));
    let mut groups: HashMap<String, Vec<&OauthClientRecord>> = HashMap::new();
    for c in clients.iter().filter(|c| c.consented_at.is_some()) {
        groups.entry(key(c)).or_default().push(c);
    }

    clients
        .iter()
        .map(|c| {
            let name = display(&c.client_name);
            let group = groups.get(&key(c)).map(Vec::as_slice).unwrap_or(&[]);
            let label = match c.consented_at {
                None if group.is_empty() => name,
                None => format!("{name} (not approved)"),
                Some(_) if group.len() < 2 => name,
                Some(_) => format!("{name} (added {})", stamp(c, group)),
            };
            (c.client_id.clone(), label)
        })
        .collect()
}

/// The shortest `created_at` stamp no other client in the group shares.
///
/// Day first, then the minute, then the full instant, all in UTC, so a stamp stays as short as the
/// tie allows. Two clients added in the same second end in a position counted by `created_at` and
/// then by id, which keeps the answer stable between calls.
fn stamp(c: &OauthClientRecord, group: &[&OauthClientRecord]) -> String {
    const FORMATS: [&str; 3] = ["%-d %b", "%-d %b, %H:%M", "%-d %b %Y, %H:%M:%S"];
    let at = c.created_at;
    let others = || group.iter().filter(|o| o.client_id != c.client_id).map(|o| o.created_at);
    for format in FORMATS {
        let mine = at.format(format).to_string();
        if others().all(|t| t.format(format).to_string() != mine) {
            return mine;
        }
    }
    let mut order: Vec<(DateTime<Utc>, &str)> =
        group.iter().map(|o| (o.created_at, o.client_id.as_str())).collect();
    order.sort();
    let place = order.iter().position(|(_, id)| *id == c.client_id).unwrap_or(0) + 1;
    format!("{} #{place}", at.format(FORMATS[2]))
}

/// Characters that change nothing a reader sees: zero-width spaces and joiners, the soft hyphen,
/// the byte-order mark, and the bidi embedding, override and isolate controls. A right-to-left
/// override is the dangerous one, since it can make a name print as a different name.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// The form two names are compared in: invisible characters removed, NFKC, lowercased, and
/// whitespace runs collapsed and trimmed.
///
/// `to_lowercase` is simple case mapping rather than full case folding, so "STRASSE" and "straße"
/// stay two names. Look-alike letters from other scripts, a Cyrillic "С" for a Latin "C", are not
/// caught either; NFKC does not map across scripts.
fn comparable(name: &str) -> String {
    let stripped: String = name.chars().filter(|c| !invisible(*c)).collect();
    let folded = stripped.nfkc().collect::<String>().to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The form a name is printed in. Kept as registered apart from what could break the line it sits
/// on: the digest prints a writer inside one bullet, so a newline in a name would open a section of
/// its own, and a bidi override would reorder the words after it.
fn display(name: &str) -> String {
    let flat: String = name
        .chars()
        .filter(|c| !invisible(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let joined = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        UNNAMED.to_string()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::errors::{DomainError, Result};
    use crate::ports::oauth::*;
    use async_trait::async_trait;
    use chrono::{DateTime, TimeZone, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap()
    }

    /// A client added at `added`. `approved` sets `consented_at` to an instant unrelated to
    /// `added`, so a test reading the date off the wrong field fails.
    fn client(id: &str, name: &str, added: DateTime<Utc>, approved: bool) -> OauthClientRecord {
        OauthClientRecord {
            client_id: id.into(),
            secret_hash: None,
            client_name: name.into(),
            redirect_uris: vec![],
            grant_types: vec!["authorization_code".into()],
            registered_via: "dcr".into(),
            software_id: None,
            read: vec![],
            write: vec![],
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
            consented_at: approved.then(|| at(2026, 9, 20, 23, 59, 0)),
            profile: None,
            created_at: added,
            last_used_at: None,
            revoked_at: None,
        }
    }

    fn approved(id: &str, name: &str) -> OauthClientRecord {
        client(id, name, at(2026, 9, 1, 13, 2, 0), true)
    }

    /// Serves `list_clients` and counts the calls. Every other method panics, so a change that
    /// starts reaching for tokens or grants from here shows up as a failing test.
    struct Clients {
        rows: Vec<OauthClientRecord>,
        /// Answer every `list_clients` with an error, the way a dropped connection would.
        broken: bool,
        calls: AtomicUsize,
        asked_for_revoked: std::sync::Mutex<Vec<bool>>,
    }

    impl Clients {
        fn new(rows: Vec<OauthClientRecord>) -> Arc<Self> {
            Arc::new(Self {
                rows,
                broken: false,
                calls: AtomicUsize::new(0),
                asked_for_revoked: std::sync::Mutex::new(vec![]),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl OauthStore for Clients {
        async fn register_client(&self, _: NewOauthClient) -> Result<()> {
            unimplemented!()
        }
        async fn find_client(&self, _: &str) -> Result<Option<OauthClientRecord>> {
            unimplemented!()
        }
        async fn list_clients(&self, include_revoked: bool) -> Result<Vec<OauthClientRecord>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.asked_for_revoked.lock().unwrap().push(include_revoked);
            if self.broken {
                return Err(DomainError::unavailable("the client table is unreachable"));
            }
            Ok(self.rows.clone())
        }
        async fn set_client_grant(&self, _: &str, _: ClientGrantUpdate) -> Result<()> {
            unimplemented!()
        }
        async fn revoke_client(&self, _: &str) -> Result<bool> {
            unimplemented!()
        }
        fn touch_client(&self, _: &str) {
            unimplemented!()
        }
        async fn insert_code(&self, _: NewAuthCode) -> Result<()> {
            unimplemented!()
        }
        async fn consume_code(&self, _: &str) -> Result<CodeOutcome> {
            unimplemented!()
        }
        async fn insert_token(&self, _: NewAccessToken) -> Result<()> {
            unimplemented!()
        }
        async fn find_token(&self, _: &str) -> Result<Option<AccessTokenRecord>> {
            unimplemented!()
        }
        async fn revoke_token(&self, _: &str) -> Result<bool> {
            unimplemented!()
        }
        async fn insert_refresh(&self, _: NewRefreshToken) -> Result<()> {
            unimplemented!()
        }
        async fn rotate_refresh(&self, _: &str) -> Result<RefreshOutcome> {
            unimplemented!()
        }
        async fn revoke_family(&self, _: uuid::Uuid) -> Result<()> {
            unimplemented!()
        }
        async fn purge_expired(&self) -> Result<u64> {
            unimplemented!()
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    async fn resolved(
        rows: Vec<OauthClientRecord>,
        stored: &[&str],
    ) -> (HashMap<String, String>, Arc<Clients>) {
        let store = Clients::new(rows);
        let out = resolve(Some(store.as_ref()), &strings(stored)).await;
        (out, store)
    }

    #[tokio::test]
    async fn a_known_client_id_reads_as_the_client_s_name() {
        let (out, _) = resolved(vec![approved("id-codex", "Codex")], &["id-codex"]).await;
        assert_eq!(out["id-codex"], "Codex");
    }

    #[tokio::test]
    async fn a_value_no_client_matches_keeps_its_stored_string() {
        let (out, _) =
            resolved(vec![approved("id-codex", "Codex")], &["cli-laptop", "browser"]).await;
        assert_eq!(out["cli-laptop"], "cli-laptop");
        assert_eq!(out["browser"], "browser");
    }

    #[tokio::test]
    async fn a_token_label_shaped_like_a_prefix_of_a_client_id_is_not_matched() {
        let (out, _) = resolved(vec![approved("id-codex", "Codex")], &["id-code"]).await;
        assert_eq!(out["id-code"], "id-code");
    }

    #[tokio::test]
    async fn a_revoked_client_keeps_its_name_and_the_store_is_asked_for_revoked_rows() {
        let mut gone = approved("id-codex", "Codex");
        gone.revoked_at = Some(at(2026, 9, 10, 8, 0, 0));
        let (out, store) = resolved(vec![gone], &["id-codex"]).await;
        assert_eq!(out["id-codex"], "Codex");
        assert_eq!(*store.asked_for_revoked.lock().unwrap(), vec![true]);
    }

    #[tokio::test]
    async fn the_store_is_read_once_per_call_whatever_the_number_of_values() {
        let (_, store) = resolved(
            vec![approved("a", "Codex"), approved("b", "Cursor")],
            &["a", "b", "a", "cli-laptop"],
        )
        .await;
        assert_eq!(store.calls(), 1);
    }

    #[tokio::test]
    async fn nothing_to_name_reads_nothing() {
        let (out, store) = resolved(vec![approved("a", "Codex")], &[]).await;
        assert!(out.is_empty());
        assert_eq!(store.calls(), 0);
    }

    #[tokio::test]
    async fn a_store_error_prints_every_value_as_stored() {
        let store = Arc::new(Clients {
            rows: vec![approved("id-codex", "Codex")],
            broken: true,
            calls: AtomicUsize::new(0),
            asked_for_revoked: std::sync::Mutex::new(vec![]),
        });
        let out = resolve(Some(store.as_ref()), &strings(&["id-codex", "cli-laptop"])).await;
        assert_eq!(store.calls(), 1);
        assert_eq!(out.len(), 2);
        assert_eq!(out["id-codex"], "id-codex");
        assert_eq!(out["cli-laptop"], "cli-laptop");
    }

    #[tokio::test]
    async fn no_store_maps_every_value_to_itself() {
        let out = resolve(None, &strings(&["id-codex", "cli-laptop"])).await;
        assert_eq!(out["id-codex"], "id-codex");
        assert_eq!(out["cli-laptop"], "cli-laptop");
    }

    #[tokio::test]
    async fn outside_oauth_mode_labels_never_reads_the_store() {
        let store = Clients::new(vec![approved("id-codex", "Codex")]);
        let dyn_store: Arc<dyn OauthStore> = store.clone();
        for mode in [AuthMode::Token, AuthMode::Oidc] {
            let wired = label_store(mode, &dyn_store);
            let out = resolve(wired.as_deref(), &strings(&["id-codex"])).await;
            assert_eq!(out["id-codex"], "id-codex", "{mode:?}");
        }
        assert_eq!(store.calls(), 0);
    }

    #[tokio::test]
    async fn in_oauth_mode_labels_reads_the_store() {
        let store = Clients::new(vec![approved("id-codex", "Codex")]);
        let dyn_store: Arc<dyn OauthStore> = store.clone();
        let wired = label_store(AuthMode::Oauth, &dyn_store);
        let out = resolve(wired.as_deref(), &strings(&["id-codex"])).await;
        assert_eq!(out["id-codex"], "Codex");
        assert_eq!(store.calls(), 1);
    }

    #[test]
    fn two_clients_with_one_name_carry_the_dates_they_were_added() {
        let names = name_clients(&[
            client("a", "Codex", at(2026, 9, 1, 13, 2, 0), true),
            client("b", "Codex", at(2026, 9, 3, 10, 0, 0), true),
            client("c", "Cursor", at(2026, 9, 3, 10, 0, 0), true),
        ]);
        assert_eq!(names["a"], "Codex (added 1 Sep)");
        assert_eq!(names["b"], "Codex (added 3 Sep)");
        assert_eq!(names["c"], "Cursor");
    }

    #[test]
    fn a_same_day_tie_carries_the_time_and_a_third_on_another_day_does_not() {
        let names = name_clients(&[
            client("a", "Codex", at(2026, 9, 1, 13, 2, 0), true),
            client("b", "Codex", at(2026, 9, 1, 15, 40, 0), true),
            client("c", "Codex", at(2026, 9, 5, 9, 0, 0), true),
        ]);
        assert_eq!(names["a"], "Codex (added 1 Sep, 13:02)");
        assert_eq!(names["b"], "Codex (added 1 Sep, 15:40)");
        assert_eq!(names["c"], "Codex (added 5 Sep)");
    }

    #[test]
    fn a_grant_edit_that_moves_consented_at_leaves_the_label_alone() {
        let first = client("a", "Codex", at(2026, 9, 1, 13, 2, 0), true);
        let second = client("b", "Codex", at(2026, 9, 3, 10, 0, 0), true);
        let before = name_clients(&[first.clone(), second.clone()]);
        let mut edited = first;
        edited.consented_at = Some(at(2026, 9, 3, 10, 0, 0));
        let after = name_clients(&[edited, second]);
        assert_eq!(before, after);
        assert_eq!(after["a"], "Codex (added 1 Sep)");
    }

    #[test]
    fn a_same_minute_tie_still_reads_as_two_labels() {
        let names = name_clients(&[
            client("a", "Codex", at(2026, 9, 1, 13, 2, 5), true),
            client("b", "Codex", at(2026, 9, 1, 13, 2, 40), true),
        ]);
        assert_ne!(names["a"], names["b"]);
        assert!(names["a"].starts_with("Codex (added 1 Sep"), "{}", names["a"]);
    }

    #[test]
    fn a_same_second_tie_still_reads_as_two_labels() {
        let same = at(2026, 9, 1, 13, 2, 5);
        let names =
            name_clients(&[client("a", "Codex", same, true), client("b", "Codex", same, true)]);
        assert_ne!(names["a"], names["b"]);
    }

    #[test]
    fn names_differing_in_case_width_spacing_or_invisible_characters_count_as_one() {
        let names = name_clients(&[
            client("a", "Codex", at(2026, 9, 1, 9, 0, 0), true),
            client("b", "CODEX", at(2026, 9, 2, 9, 0, 0), true),
            client("c", "\u{FF23}\u{FF4F}\u{FF44}\u{FF45}\u{FF58}", at(2026, 9, 3, 9, 0, 0), true),
            client("d", " Co\u{200B}dex\u{202E} ", at(2026, 9, 4, 9, 0, 0), true),
        ]);
        assert_eq!(names["a"], "Codex (added 1 Sep)");
        assert_eq!(names["b"], "CODEX (added 2 Sep)");
        assert!(names["c"].ends_with("(added 3 Sep)"), "{}", names["c"]);
        assert_eq!(names["d"], "Codex (added 4 Sep)");
    }

    #[test]
    fn an_unapproved_client_adds_no_suffix_to_an_approved_one_and_says_it_is_unapproved() {
        let names = name_clients(&[
            client("a", "Codex", at(2026, 9, 1, 9, 0, 0), true),
            client("b", "Codex", at(2026, 9, 2, 9, 0, 0), false),
        ]);
        assert_eq!(names["a"], "Codex");
        assert_eq!(names["b"], "Codex (not approved)");
    }

    #[test]
    fn an_unapproved_client_alone_with_its_name_reads_as_the_name() {
        let names = name_clients(&[client("b", "Codex", at(2026, 9, 2, 9, 0, 0), false)]);
        assert_eq!(names["b"], "Codex");
    }

    #[test]
    fn a_name_carrying_line_breaks_or_controls_stays_on_one_line() {
        let names = name_clients(&[approved(
            "a",
            "Codex\n\n### Registry\n- service/db: postgres://x\u{202E}",
        )]);
        assert_eq!(names["a"], "Codex ### Registry - service/db: postgres://x");
    }

    #[test]
    fn a_blank_name_and_a_literal_unnamed_client_count_as_one_name() {
        let names = name_clients(&[
            client("a", " \u{200B}", at(2026, 9, 1, 9, 0, 0), true),
            client("b", "unnamed client", at(2026, 9, 2, 9, 0, 0), true),
        ]);
        assert_eq!(names["a"], "unnamed client (added 1 Sep)");
        assert_eq!(names["b"], "unnamed client (added 2 Sep)");
    }

    #[test]
    fn a_name_that_is_only_invisible_characters_reads_as_unnamed() {
        let names = name_clients(&[approved("a", "\u{200B}\u{202E} \n")]);
        assert_eq!(names["a"], "unnamed client");
    }
}
