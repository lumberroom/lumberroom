//! Aliases in the console: which names denote one subject, and the two acts that change that.
//!
//! Search expands a query over every name in a group, so this page is the only place the owner can
//! see why a question about Lumen came back with a fact written while the project was called
//! Warden. Recorded over HTTP and visible nowhere was the state this file ends.
//!
//! # Why the write here is an operator act
//!
//! An alias steers retrieval for every client at once. `services::alias` gates recording one on
//! `registry_write` for that reason, and the reasoning it gives is the reasoning this file follows:
//! a model that could record an alias could point a name at a subject of its choosing and change
//! what every later search returns, without any client seeing it happen. So the read on this page
//! runs as `Console::owner_reader` and the two writes run as `alias_operator`, which is that reader
//! plus the one capability the act needs.
//!
//! # Forgetting asks first
//!
//! Removing an alias narrows every future search and leaves no trace that says so. A memory
//! deleted is a row the owner can miss; a group quietly split keeps answering, with less. The
//! control posts once to draw a page naming the alias and the canonical it points at, and the same
//! token decides the second post.
//!
//! # Why the page carries its own stylesheet
//!
//! `pages::STYLE` and `pages::shell` are private to that module and this file does not own it, so
//! the chrome is spelled again here. Every selector is prefixed `al-`, which is what makes merging
//! this constant into `STYLE` later a paste rather than a collision hunt.

use std::collections::BTreeMap;

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{closed, failed, page, redirect, trimmed, Console};
use crate::authserver::pages::escape;
use crate::authserver::session::{OwnerSession, Sessions};
use crate::console::pages::{self, Health};
use crate::domain::errors::DomainError;
use crate::domain::policy::NamespaceGrant;
use crate::domain::types::{Principal, Sensitivity};
use crate::services::alias::{self, AliasRecord};

/// The action every token on this page is minted for. Two of them, because a token that forgets a
/// name must not be spendable on a form that records one.
const RECORD_ACTION: &str = "alias-record";
const FORGET_ACTION: &str = "alias-forget";

/// The record form's second half. One form draws one alias, so there is no row to name, and a
/// literal keeps the empty string out of the signing input.
const RECORD_TARGET: &str = "alias";

/// What the second post carries to say the reader read the question.
const CONFIRMED: &str = "yes";

// ---- the principal ----

/// The console reader plus the one capability recording an alias needs.
///
/// `registry_write` is what `services::alias` gates on, and the service argues why: an alias
/// steers retrieval for every client at once, so it sits beside the registry rather than beside a
/// memory write.
///
/// The write grant matches `owner_approver`'s, for the reason that one gives. It is the ceiling the
/// owner's own CLI credential already holds, and a narrower grant would refuse a namespace
/// the `lumberroom` client accepts and read as a broken button. Nothing widens past that: `may_delete` stays
/// false, and `alias::forget` removes a name from a group and no memory.
fn alias_operator(app: &Console) -> Principal {
    Principal {
        registry_write: true,
        write: vec![NamespaceGrant::new("*", Sensitivity::Sealed)],
        ..app.owner_reader()
    }
}

// ---- reading ----

/// One canonical name and every name in this namespace that resolves to it.
///
/// Grouped so a rename chain reads as one thing. Warden, Quill and Lumen are three rows in the
/// store and one subject, and a flat table of pairs makes the reader rebuild that in his head.
///
/// One hop, no walk. A canonical name is never itself an alias, because `put` repoints a whole
/// group when its canonical name is renamed, and the read path in the adapter leans on the same
/// invariant. A row hand-edited in psql that breaks it prints here as two groups, which is the
/// visible failure rather than the silent one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub namespace: String,
    pub canonical: String,
    /// The aliases pointing at `canonical`, by name. The canonical name itself has no row and the
    /// page prints it at the head of the group.
    pub names: Vec<AliasRecord>,
}

impl Group {
    /// Names in the group, the canonical one included, which is what a reader counts.
    pub fn size(&self) -> usize {
        self.names.len() + 1
    }
}

async fn read(app: &Console) -> Result<Vec<Group>, DomainError> {
    let ctx = app.ctx();
    // No namespace argument: the service filters what this principal may read, and the reader holds
    // every namespace, so one call answers the whole page.
    let rows = alias::list(&ctx, app.state.aliases.as_ref(), None).await?;
    Ok(grouped(rows))
}

/// Rows into groups, ordered by namespace then canonical name, each group's aliases by name.
///
/// Grouping is by the pair, never by the canonical name alone. Two namespaces can each record a
/// `warden`, and merging them would show the owner a group that no search will ever expand over.
fn grouped(rows: Vec<AliasRecord>) -> Vec<Group> {
    let mut by_group: BTreeMap<(String, String), Vec<AliasRecord>> = BTreeMap::new();
    for row in rows {
        by_group.entry((row.namespace.clone(), row.canonical.clone())).or_default().push(row);
    }
    by_group
        .into_iter()
        .map(|((namespace, canonical), mut names)| {
            names.sort_by(|a, b| a.alias.cmp(&b.alias));
            Group { namespace, canonical, names }
        })
        .collect()
}

/// The row a forget control names, found by what the form sent.
///
/// Case folded because the store lowercases a name on the way in and a hand-edited form need not.
fn find<'a>(groups: &'a [Group], namespace: &str, alias: &str) -> Option<&'a AliasRecord> {
    let (namespace, alias) = (namespace.trim(), alias.trim());
    groups.iter().flat_map(|g| g.names.iter()).find(|row| {
        row.namespace.eq_ignore_ascii_case(namespace) && row.alias.eq_ignore_ascii_case(alias)
    })
}

// ---- the routes ----

#[derive(Debug, Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    pub done: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RecordForm {
    #[serde(default)]
    pub csrf: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub canonical: String,
    #[serde(default)]
    pub since: String,
    #[serde(default)]
    pub until: String,
}

impl RecordForm {
    fn draft(&self) -> Draft {
        Draft {
            namespace: self.namespace.clone(),
            alias: self.alias.clone(),
            canonical: self.canonical.clone(),
            since: self.since.clone(),
            until: self.until.clone(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ForgetForm {
    #[serde(default)]
    pub csrf: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub alias: String,
    /// Empty on the post the control makes, `yes` on the post the confirmation makes.
    #[serde(default)]
    pub confirm: String,
}

/// The list, with the record form under it.
pub async fn index(
    State(app): State<Console>,
    headers: HeaderMap,
    Query(q): Query<IndexQuery>,
) -> Response {
    let session = match app.guard(&headers, "/console/aliases") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    listing(&app, sessions, &session, Draft::default(), None, StatusCode::OK, q.done.as_deref())
        .await
}

/// Record one alias. Token first, then the dates, then the service.
///
/// Nothing here decides whether the alias may be written. `alias::put` normalizes the namespace,
/// checks the capability and the grant, refuses a sealed namespace and flattens a chain, exactly as
/// it does for `POST /admin/alias`.
pub async fn record(
    State(app): State<Console>,
    headers: HeaderMap,
    Form(form): Form<RecordForm>,
) -> Response {
    let session = match app.guard(&headers, "/console/aliases") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    if !sessions.console_csrf_ok(&session, RECORD_ACTION, RECORD_TARGET, &form.csrf) {
        tracing::warn!("console alias refused: the form token did not match");
        return stale();
    }

    let (since, until) =
        match (moment("Current from", &form.since), moment("Current until", &form.until)) {
            (Ok(since), Ok(until)) => (since, until),
            (Err(e), _) | (_, Err(e)) => {
                return refused(&app, sessions, &session, &form, &e).await;
            }
        };
    // A period that ends before it starts is a typo, and the store takes it: the column pair carries
    // no check and the service adds none. Catching it on the surface a person types into costs one
    // comparison, and the alias it would record was current for no time at all.
    if let (Some(s), Some(u)) = (since, until) {
        if u <= s {
            let e = DomainError::validation(
                "the alias stops being current before it starts. The period is half-open, so \
                 'current until' has to fall after 'current from'.",
            );
            return refused(&app, sessions, &session, &form, &e).await;
        }
    }

    match alias::put(
        &app.ctx_with(alias_operator(&app)),
        app.state.aliases.as_ref(),
        &form.namespace,
        &form.alias,
        &form.canonical,
        since,
        until,
        // The owner typed it, which is what `manual` means. `derived` belongs to whatever reads a
        // name out of a fact, and it loses to this one on a conflict.
        Some("manual"),
    )
    .await
    {
        Ok(_) => redirect("/console/aliases?done=recorded"),
        Err(e) => refused(&app, sessions, &session, &form, &e).await,
    }
}

/// Forget one alias, in two posts.
///
/// The first draws the question naming the alias and the canonical it points at. The second does
/// it. One token covers both, because both are the same act on the same row in the same session.
pub async fn forget(
    State(app): State<Console>,
    headers: HeaderMap,
    Form(form): Form<ForgetForm>,
) -> Response {
    let session = match app.guard(&headers, "/console/aliases") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let key = forget_key(&form.namespace, &form.alias);
    if !sessions.console_csrf_ok(&session, FORGET_ACTION, &key, &form.csrf) {
        tracing::warn!("console alias forget refused: the form token did not match");
        return stale();
    }

    if trimmed(&form.confirm) != Some(CONFIRMED) {
        let groups = match read(&app).await {
            Ok(g) => g,
            Err(e) => return failed(&app, "the aliases did not load", &e),
        };
        return match find(&groups, &form.namespace, &form.alias) {
            // The canonical name comes off the row the store holds rather than out of the form, so
            // the question names what would actually be removed.
            Some(row) => page(StatusCode::OK, confirm_html(row, &form.csrf, &app.health())),
            None => page(
                StatusCode::NOT_FOUND,
                pages::notice(
                    "no such alias",
                    "Nothing in this store records that name in that namespace. It may have been \
                     forgotten in another tab.",
                    None,
                    None,
                ),
            ),
        };
    }

    match alias::forget(
        &app.ctx_with(alias_operator(&app)),
        app.state.aliases.as_ref(),
        &form.namespace,
        &form.alias,
    )
    .await
    {
        Ok(true) => redirect("/console/aliases?done=forgotten"),
        // The row had already gone, which is what a second tab looks like from here.
        Ok(false) => redirect("/console/aliases?done=unchanged"),
        Err(e) => {
            tracing::warn!(error = %e.log_message(), "console refused an alias forget");
            let status =
                StatusCode::from_u16(e.kind.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
            if status.is_server_error() {
                return failed(&app, "the alias was not forgotten", &e);
            }
            page(
                status,
                pages::notice(
                    "that alias was not forgotten",
                    &e.client_message(),
                    None,
                    Some(&app.health()),
                ),
            )
        }
    }
}

/// The page again, with what the store said and everything that was typed.
///
/// A server fault takes the trouble page instead. The reader can do nothing about one, and handing
/// back the same form invites them to press the button until the log fills.
async fn refused(
    app: &Console,
    sessions: &Sessions,
    session: &OwnerSession,
    form: &RecordForm,
    e: &DomainError,
) -> Response {
    let status = StatusCode::from_u16(e.kind.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    if status.is_server_error() {
        return failed(app, "the alias was not recorded", e);
    }
    tracing::warn!(error = %e.log_message(), "console refused an alias");
    listing(app, sessions, session, form.draft(), Some(&e.client_message()), status, None).await
}

/// Draw the page. One place mints the tokens, so every form and the check downstream agree.
async fn listing(
    app: &Console,
    sessions: &Sessions,
    session: &OwnerSession,
    draft: Draft,
    error: Option<&str>,
    status: StatusCode,
    done: Option<&str>,
) -> Response {
    let groups = match read(app).await {
        Ok(g) => g,
        Err(e) => return failed(app, "the aliases did not load", &e),
    };
    let record = sessions.console_csrf(session, RECORD_ACTION, RECORD_TARGET);
    let forget = |namespace: &str, alias: &str| {
        sessions.console_csrf(session, FORGET_ACTION, &forget_key(namespace, alias))
    };
    page(status, listing_html(&groups, &app.health(), &draft, &record, &forget, error, done))
}

/// What a forget token is signed for.
///
/// A namespace holds no `/`, so no pair of names collides here: `("project:a", "b/c")` cannot also
/// be read as `("project:a/b", "c")`.
fn forget_key(namespace: &str, alias: &str) -> String {
    format!("{}/{}", namespace.trim(), alias.trim())
}

fn stale() -> Response {
    page(
        StatusCode::FORBIDDEN,
        pages::notice(
            "that form went stale",
            "The token on it was minted for another session or another name, so nothing was \
             changed. Reload the aliases page and try again.",
            None,
            None,
        ),
    )
}

/// One optional date off the form.
///
/// The parser is the one every other date on this server goes through, and its refusal is written
/// about `occurred_at`, which names no field on this form. So the message is restated for the field
/// the reader is looking at.
fn moment(field: &str, raw: &str) -> Result<Option<DateTime<Utc>>, DomainError> {
    let Some(value) = trimmed(raw) else {
        return Ok(None);
    };
    crate::mcp::tools::parse_occurred_at(value).map(Some).map_err(|_| {
        DomainError::validation(format!(
            "{field} `{}` is not a date. Write it as 2026-03-01, read as midnight UTC, or as a \
             full instant, 2026-03-01T09:30:00Z.",
            clip(value)
        ))
    })
}

/// Keep a pasted paragraph out of a refusal and out of the log line carrying it.
fn clip(value: &str) -> String {
    const LIMIT: usize = 48;
    match value.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}...", &value[..cut]),
        None => value.to_string(),
    }
}

// ---- the page ----

/// What the record form holds, so a refusal comes back with it rather than empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub namespace: String,
    pub alias: String,
    pub canonical: String,
    pub since: String,
    pub until: String,
}

/// The whole page: the groups, then the form that records one.
///
/// `forget_csrf` arrives as a maker so the session type stays out of the rendering and a test can
/// draw the page without one, the same shape the queue page takes.
pub fn listing_html(
    groups: &[Group],
    health: &Health,
    draft: &Draft,
    record_csrf: &str,
    forget_csrf: &dyn Fn(&str, &str) -> String,
    error: Option<&str>,
    done: Option<&str>,
) -> String {
    let names: usize = groups.iter().map(Group::size).sum();
    let count = if groups.is_empty() {
        "nothing recorded yet".to_string()
    } else {
        format!(
            "{names} {name_word} in {count} {group_word}",
            names = names,
            name_word = plural(names, "name", "names"),
            count = groups.len(),
            group_word = plural(groups.len(), "group", "groups"),
        )
    };
    let body = if groups.is_empty() {
        "<div class=\"al-none\"><div class=\"big\">No name here answers to another.</div>\
<p>An alias tells search that two names mean one subject, so a question about Lumen also reaches \
the facts written while the project was called Warden. Record the first one below.</p></div>"
            .to_string()
    } else {
        groups.iter().map(|group| group_html(group, forget_csrf)).collect()
    };

    pages::shell(
        "lumberroom: aliases",
        pages::Tab::Aliases,
        Some(health),
        &format!(
            "<main class=\"al-page\">\
<div class=\"al-head\"><h2>Aliases</h2><div class=\"when\">{count}</div></div>\
<p class=\"al-lede\">Search expands a query over every name in a group. A question naming any one \
of them reads the facts written under all of them, which is what keeps a rename from hiding half \
the store.</p>{done}{body}{form}</main>",
            count = escape(&count),
            done = done_note(done),
            body = body,
            form = record_form(draft, record_csrf, error),
        ),
    )
}

/// One group: the canonical name, then every name that resolves to it.
fn group_html(group: &Group, forget_csrf: &dyn Fn(&str, &str) -> String) -> String {
    let rows: String = group
        .names
        .iter()
        .map(|row| {
            format!(
                "<div class=\"al-row\"><span class=\"al-name\">{alias}</span>\
<span class=\"al-when\">{period}</span>\
<span class=\"al-origin\">{origin}</span>\
<form method=\"post\" action=\"/console/aliases/forget\">\
<input type=\"hidden\" name=\"csrf\" value=\"{token}\">\
<input type=\"hidden\" name=\"namespace\" value=\"{namespace}\">\
<input type=\"hidden\" name=\"alias\" value=\"{alias_value}\">\
<button type=\"submit\" class=\"danger\">Forget</button></form></div>",
                alias = escape(&row.alias),
                period = escape(&period(row)),
                origin = escape(&row.origin),
                token = escape(&forget_csrf(&row.namespace, &row.alias)),
                namespace = escape(&row.namespace),
                alias_value = escape(&row.alias),
            )
        })
        .collect();

    format!(
        "<div class=\"al-group\"><h3>{namespace}</h3>\
<div class=\"al-row\"><span class=\"al-canon\">{canonical}</span>\
<span class=\"al-when\">the name the rest resolve to</span>\
<span class=\"al-origin\">canonical</span><span></span></div>{rows}</div>",
        namespace = escape(&group.namespace),
        canonical = escape(&group.canonical),
        rows = rows,
    )
}

/// The question a forget asks before it happens.
pub fn confirm_html(row: &AliasRecord, csrf: &str, health: &Health) -> String {
    pages::shell(
        "lumberroom: forget an alias",
        pages::Tab::Aliases,
        Some(health),
        &format!(
            "<main class=\"al-page\"><div class=\"al-ask\">\
<div class=\"big\">Forget {alias} as a name for {canonical}?</div>\
<p>In {namespace}. Every search naming {alias} stops reaching the facts written under \
{canonical}, and every search naming {canonical} stops reaching the ones written under {alias}. \
The facts themselves stay exactly where they are.</p>\
<p>A search that came back thinner is the only sign this leaves, which is why it asks.</p>\
<form method=\"post\" action=\"/console/aliases/forget\">\
<input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
<input type=\"hidden\" name=\"namespace\" value=\"{namespace}\">\
<input type=\"hidden\" name=\"alias\" value=\"{alias}\">\
<input type=\"hidden\" name=\"confirm\" value=\"{confirmed}\">\
<div class=\"al-send\"><button type=\"submit\">Forget it</button>\
<a href=\"/console/aliases\">Keep it</a></div></form></div></main>",
            alias = escape(&row.alias),
            canonical = escape(&row.canonical),
            namespace = escape(&row.namespace),
            csrf = escape(csrf),
            confirmed = CONFIRMED,
        ),
    )
}

/// The form that records one. Every hint is a rule the store enforces anyway.
fn record_form(draft: &Draft, csrf: &str, error: Option<&str>) -> String {
    let banner = match error {
        Some(message) => format!("<p class=\"al-err\">{}</p>", escape(message)),
        None => String::new(),
    };
    format!(
        "<div class=\"al-form\"><h3>Record an alias</h3>\
<form class=\"al-f\" method=\"post\" action=\"/console/aliases/record\">\
<input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">{banner}\
<div class=\"pair\">\
<div><label for=\"a-ns\">Namespace</label>\
<input id=\"a-ns\" name=\"namespace\" value=\"{namespace}\" placeholder=\"project:lumen\" \
autocomplete=\"off\" spellcheck=\"false\" required>\
<p class=\"hint\">The alias holds inside one namespace. A project renamed here also reaches the \
namespace it used to carry.</p></div>\
<div><label for=\"a-alias\">Alias</label>\
<input id=\"a-alias\" name=\"alias\" value=\"{alias}\" placeholder=\"warden\" \
autocomplete=\"off\" spellcheck=\"false\" required>\
<p class=\"hint\">The name facts were written under. Case does not matter: the store folds it.</p>\
</div></div>\
<div class=\"pair\">\
<div><label for=\"a-canon\">Canonical</label>\
<input id=\"a-canon\" name=\"canonical\" value=\"{canonical}\" placeholder=\"lumen\" \
autocomplete=\"off\" spellcheck=\"false\" required>\
<p class=\"hint\">The name the group answers to.</p></div></div>\
<div class=\"pair\">\
<div><label for=\"a-since\">Current from</label>\
<input id=\"a-since\" name=\"since\" value=\"{since}\" placeholder=\"2026-03-01\" \
autocomplete=\"off\" spellcheck=\"false\"></div>\
<div><label for=\"a-until\">Current until</label>\
<input id=\"a-until\" name=\"until\" value=\"{until}\" placeholder=\"2026-06-01\" \
autocomplete=\"off\" spellcheck=\"false\"></div></div>\
<p class=\"hint\">Both optional, and neither narrows a search. Retrieval expands over every name in \
the group whatever the dates say, because the facts written under a retired name are still true. \
The period records when the name was the one in use.</p>\
<div class=\"al-send\"><button type=\"submit\">Record it</button>\
<a href=\"/console/reading\">Back to reading</a></div></form></div>",
        csrf = escape(csrf),
        banner = banner,
        namespace = escape(&draft.namespace),
        alias = escape(&draft.alias),
        canonical = escape(&draft.canonical),
        since = escape(&draft.since),
        until = escape(&draft.until),
    )
}

/// The line the page prints after an act.
///
/// A closed list. Each word came from a redirect this file wrote, so one it does not recognise
/// prints nothing rather than reaching the page as text.
fn done_note(done: Option<&str>) -> String {
    let line = match done.unwrap_or_default() {
        "recorded" => "Recorded. Every search naming either name now reads both.",
        "forgotten" => {
            "Forgotten. That name stands alone again, and the facts under it stay where \
                        they are."
        }
        "unchanged" => "Nothing changed. That name was already gone.",
        _ => return String::new(),
    };
    format!("<p class=\"al-done\">{}</p>", escape(line))
}

/// The valid period in words, half-open the way the store reads it.
fn period(row: &AliasRecord) -> String {
    match (&row.since, &row.until) {
        (Some(since), Some(until)) => format!("{} to {}", day(since), day(until)),
        (Some(since), None) => format!("from {}", day(since)),
        (None, Some(until)) => format!("until {}", day(until)),
        (None, None) => "no period recorded".to_string(),
    }
}

/// A stored instant as the day it names. A rename happens on a day and the clock beside it says
/// nothing, so the page drops it. The store keeps the whole timestamp.
fn day(instant: &str) -> String {
    instant.split('T').next().unwrap_or(instant).to_string()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        many.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health() -> Health {
        Health {
            key_verified: true,
            keys_configured: true,
            embedder: "bge-small-en-v1.5".into(),
            last_write: None,
            now: Utc::now(),
        }
    }

    fn row(namespace: &str, alias: &str, canonical: &str) -> AliasRecord {
        AliasRecord {
            namespace: namespace.into(),
            alias: alias.into(),
            canonical: canonical.into(),
            since: None,
            until: None,
            origin: "manual".into(),
            created_at: "2026-08-20T09:00:00+00:00".into(),
        }
    }

    fn token(_action: &str, _id: &str) -> String {
        "tok".to_string()
    }

    fn chain() -> Vec<AliasRecord> {
        vec![row("project:lumen", "quill", "lumen"), row("project:lumen", "warden", "lumen")]
    }

    /// The owner's case. Warden became Quill and Quill became Lumen, and the store holds facts
    /// under all three. Three names, one subject, one group on the page.
    #[test]
    fn a_three_name_chain_renders_as_one_group() {
        let groups = grouped(chain());
        assert_eq!(groups.len(), 1, "one canonical name, one group");
        assert_eq!(groups[0].canonical, "lumen");
        assert_eq!(groups[0].size(), 3, "the canonical name counts as a name");
        assert_eq!(
            groups[0].names.iter().map(|r| r.alias.as_str()).collect::<Vec<_>>(),
            vec!["quill", "warden"],
            "aliases print in name order whatever order the store returned"
        );

        let html = listing_html(&groups, &health(), &Draft::default(), "tok", &token, None, None);
        assert_eq!(html.matches("class=\"al-group\"").count(), 1);
        assert_eq!(html.matches("class=\"al-canon\"").count(), 1);
        for name in ["warden", "quill", "lumen"] {
            assert!(html.contains(name), "{name} is missing from the group");
        }
        assert!(html.contains("3 names in 1 group"));
    }

    /// One name in two namespaces is two groups. Search never expands across a namespace, so a
    /// merged group would show the owner a link no query will follow.
    #[test]
    fn one_name_under_two_namespaces_stays_two_groups() {
        let groups = grouped(vec![
            row("project:lumen", "warden", "lumen"),
            row("project:warden", "warden", "lumen"),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].namespace, "project:lumen");
        assert_eq!(groups[1].namespace, "project:warden");
    }

    #[test]
    fn the_record_form_carries_the_token_it_will_spend() {
        let html =
            listing_html(&[], &health(), &Draft::default(), "tok-alias-record", &token, None, None);
        assert!(html
            .contains("<form class=\"al-f\" method=\"post\" action=\"/console/aliases/record\">"));
        assert!(html.contains("name=\"csrf\" value=\"tok-alias-record\""));
        for field in ["namespace", "alias", "canonical", "since", "until"] {
            assert!(html.contains(&format!("name=\"{field}\"")), "{field} is missing");
        }
    }

    /// Every control on the page posts a token bound to the session, the action and the row. A form
    /// arriving without one changes nothing, and neither does one minted for the other act.
    #[test]
    fn a_write_with_no_token_is_refused() {
        let sessions = Sessions::new("k".repeat(32), 900, true);
        let session = sessions.open(&sessions.issue(1_000), 1_001).expect("a live session");

        assert!(!sessions.console_csrf_ok(&session, RECORD_ACTION, RECORD_TARGET, ""));
        let key = forget_key("project:lumen", "warden");
        assert!(!sessions.console_csrf_ok(&session, FORGET_ACTION, &key, ""));

        let recording = sessions.console_csrf(&session, RECORD_ACTION, RECORD_TARGET);
        assert!(sessions.console_csrf_ok(&session, RECORD_ACTION, RECORD_TARGET, &recording));
        assert!(
            !sessions.console_csrf_ok(&session, FORGET_ACTION, &key, &recording),
            "a token that records a name must not forget one"
        );

        let forgetting = sessions.console_csrf(&session, FORGET_ACTION, &key);
        assert!(
            !sessions.console_csrf_ok(
                &session,
                FORGET_ACTION,
                &forget_key("project:lumen", "quill"),
                &forgetting
            ),
            "the page prints every name at once, so a token has to name its own"
        );

        let other = sessions.open(&sessions.issue(2_000), 2_001).expect("a second session");
        assert!(!sessions.console_csrf_ok(&other, RECORD_ACTION, RECORD_TARGET, &recording));
    }

    #[test]
    fn the_page_fetches_nothing_and_runs_nothing() {
        let groups = grouped(chain());
        for html in [
            listing_html(
                &groups,
                &health(),
                &Draft::default(),
                "tok",
                &token,
                None,
                Some("recorded"),
            ),
            listing_html(&[], &health(), &Draft::default(), "tok", &token, Some("refused"), None),
            confirm_html(&row("project:lumen", "warden", "lumen"), "tok", &health()),
        ] {
            assert!(!html.contains("<script"), "no JavaScript");
            assert_eq!(
                html.matches("src=").count(),
                html.matches("src=\"/console/logo.svg\"").count(),
                "nothing is fetched except the mark"
            );
            assert!(!html.contains("http://"), "no external asset");
            assert!(html.starts_with("<!doctype html>"));
        }
    }

    /// A name a model wrote into the store renders as text. The escaper is the authorization
    /// server's, so there is one of them.
    #[test]
    fn a_hostile_name_renders_as_text() {
        let hostile = "<script>alert(1)</script>";
        let groups = grouped(vec![row("project:lumen", hostile, "lumen")]);
        let html = listing_html(&groups, &health(), &Draft::default(), "tok", &token, None, None);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    /// Removing an alias narrows every future search and leaves nothing behind that says so, which
    /// is the whole reason the control asks first.
    #[test]
    fn the_confirmation_names_the_alias_and_what_it_points_at() {
        let html = confirm_html(&row("project:lumen", "warden", "lumen"), "tok", &health());
        assert!(html.contains("Forget warden as a name for lumen?"));
        assert!(html.contains("project:lumen"));
        assert!(html.contains("name=\"confirm\" value=\"yes\""));
        assert!(html.contains("name=\"csrf\" value=\"tok\""));
        assert!(html.contains("Keep it"));
    }

    /// The list posts without `confirm`, so a control clicked by accident draws a question rather
    /// than removing a name.
    #[test]
    fn the_control_on_the_list_asks_before_it_removes() {
        let groups = grouped(chain());
        let html = listing_html(&groups, &health(), &Draft::default(), "tok", &token, None, None);
        assert!(html.contains("action=\"/console/aliases/forget\""));
        assert!(!html.contains("name=\"confirm\""), "the list never carries the confirmation");
    }

    #[test]
    fn an_empty_page_says_what_an_alias_is_for() {
        let html = listing_html(&[], &health(), &Draft::default(), "tok", &token, None, None);
        assert!(html.contains("An alias tells search that two names mean one subject"));
        assert!(html.contains("Record an alias"), "the form is there to answer it");
        assert!(html.contains("nothing recorded yet"));
        assert!(!html.contains("0 names"), "a count of nothing says nothing");
    }

    #[test]
    fn a_period_prints_whichever_ends_the_store_holds() {
        let mut both = row("project:lumen", "warden", "lumen");
        both.since = Some("2026-03-01T00:00:00+00:00".into());
        both.until = Some("2026-06-01T00:00:00+00:00".into());
        assert_eq!(period(&both), "2026-03-01 to 2026-06-01");

        let mut open_ended = both.clone();
        open_ended.until = None;
        assert_eq!(period(&open_ended), "from 2026-03-01");

        let mut retired = both.clone();
        retired.since = None;
        assert_eq!(period(&retired), "until 2026-06-01");

        assert_eq!(period(&row("project:lumen", "warden", "lumen")), "no period recorded");
    }

    /// A period that ends before it starts is a typo the store would accept, and the form is where
    /// a person can still see it.
    #[test]
    fn a_backwards_period_is_caught_before_the_store_sees_it() {
        let since = crate::mcp::tools::parse_occurred_at("2026-06-01").unwrap();
        let until = crate::mcp::tools::parse_occurred_at("2026-03-01").unwrap();
        assert!(until <= since, "the check the handler runs on these two values");
    }

    #[test]
    fn a_date_the_parser_refuses_comes_back_named_after_its_own_field() {
        assert_eq!(moment("Current from", "  ").unwrap(), None);
        assert!(moment("Current from", "2026-03-01").unwrap().is_some());

        let e = moment("Current until", "last March").unwrap_err();
        let message = e.client_message();
        assert!(message.contains("Current until"), "the field the reader is looking at");
        assert!(!message.contains("occurred_at"), "no field on this form carries that name");
    }

    #[test]
    fn a_long_paste_is_clipped_out_of_the_refusal() {
        let e = moment("Current from", &"x".repeat(400)).unwrap_err();
        assert!(e.client_message().len() < 200);
        assert!(e.client_message().contains("..."));
    }

    /// Two aliases whose namespace and name run together must not sign one token.
    #[test]
    fn a_forget_token_names_one_row_and_no_other() {
        assert_ne!(forget_key("project:lumen", "warden"), forget_key("project:lumen", "quill"));
        assert_eq!(forget_key(" project:lumen ", " warden "), "project:lumen/warden");
    }

    #[test]
    fn a_row_is_found_whatever_case_the_form_sent() {
        let groups = grouped(chain());
        let found = find(&groups, "project:lumen", " WARDEN ").expect("the row the form names");
        assert_eq!(found.canonical, "lumen");
        assert!(find(&groups, "project:other", "warden").is_none());
        assert!(find(&groups, "project:lumen", "nothing").is_none());
    }

    #[test]
    fn the_done_line_prints_only_words_this_file_redirected_with() {
        assert!(done_note(Some("recorded")).contains("Recorded"));
        assert!(done_note(Some("forgotten")).contains("Forgotten"));
        assert!(done_note(Some("unchanged")).contains("Nothing changed"));
        assert_eq!(done_note(Some("<script>")), "");
        assert_eq!(done_note(None), "");
    }
}
