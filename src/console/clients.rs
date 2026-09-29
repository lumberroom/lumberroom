//! Clients in the console: who may reach this store, what they may do, and how to stop them.
//!
//! Until this page, adding a surface meant editing `AUTH_TOKENS` on the box and restarting, or
//! waiting for a client to register itself and then approving it at the consent screen. Neither is
//! something to do from a phone, and the first is the reason a store ends up with one credential
//! doing everything: the cost of a second one is an SSH session.
//!
//! # These are OAuth clients, not bearer tokens
//!
//! A client created here goes in `oauth_client` beside the ones that register themselves, and it
//! completes the OAuth flow to get its own tokens. Nothing here mints a bearer token, and
//! `AUTH_TOKENS` is untouched: a credential the owner put in `.env` keeps working exactly as it
//! did, because auth modes compose rather than exclude.
//!
//! # Creating a client consents to it
//!
//! `set_client_grant` writes `consented_at` alongside the grant, and that is the right reading
//! here. Consent exists so a grant is one the owner approved rather than one a client asked for;
//! an owner filling in this form behind his own password has approved it more directly than a
//! consent screen ever asks. A client that registers itself still goes to that screen, because
//! nobody approved that one yet.
//!
//! # The secret is shown once
//!
//! Confidential clients get a secret at creation and the store keeps only `hash_token` of it, so
//! nothing can show it again. Losing it means issuing another client. That is stricter than
//! `AUTH_TOKENS`, which holds its tokens in plaintext in `.env`, and the difference is deliberate:
//! a console reachable from a browser must not be a place every credential can be read out of.
//!
//! # Access is editable after the fact
//!
//! Most clients arrive by registering themselves and consenting, so the grant they end up with is
//! whatever the consent screen offered on the day. Until this page the only ways to change it were
//! to send the client back through the flow, which only the client can start, or to revoke it and
//! issue another. Each client here carries its own copy of the same form the create form uses, and
//! saving it writes the grant through `set_client_grant`.
//!
//! A change lands on the client's next call. `OpaqueTokenAuthenticator` reads the grant off the
//! client row for every request rather than off the token, so nothing has to reconnect and no
//! token has to be reissued.
//!
//! # Scope is a list of namespaces, not a grammar
//!
//! The grant language is globs with a ceiling each, and asking for it in a text box is how a form
//! gets answered by copying whatever the last client had. The shape says what a client may do and
//! how deep it may see; the scope says where. Picking `project:sivella` and `user:me` off the list
//! rewrites the shape's `*` into those two names at the level the shape already chose, so the
//! common request, one client that only sees one project, is two clicks and no syntax.
//!
//! The namespaces offered are the ones the store actually holds, plus any the client already
//! reaches, so a grant written before a namespace was emptied still shows its own name ticked. The
//! text box beside them takes anything the list cannot: a namespace with nothing in it yet, or a
//! glob like `project:*`.
//!
//! # Revoking is one click and no confirmation
//!
//! Deleting a memory asks first because the owner can miss what is gone. Revoking a client is the
//! opposite: the cost of a mistake is a surface that stops working and says so, and the cost of
//! hesitating is a credential still live while you look for the confirm button.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::Deserialize;

use super::{closed, data, page, redirect, trimmed, Console};
use crate::authserver::pages::escape;
use crate::authserver::session::{OwnerSession, Sessions};
use crate::console::pages::{self, Health, Tab};
use crate::domain::oauth::{client_name_display, hash_token};
use crate::domain::policy::NamespaceGrant;
use crate::domain::presets::Preset;
use crate::domain::types::Sensitivity;
use crate::ports::oauth::{ClientGrantUpdate, NewOauthClient, OauthClientRecord};

const NEW_ACTION: &str = "client-new";
const REVOKE_ACTION: &str = "client-revoke";
const ACCESS_ACTION: &str = "client-access";
/// One target for the create form, because it decides nothing that already exists.
const NEW_TARGET: &str = "new";

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    done: Option<String>,
}

/// A submitted form, read straight off the body.
///
/// `Form<T>` is `serde_urlencoded` underneath, and a repeated key is not a sequence to it: the
/// namespace boxes all post as `ns`, and serde would take one of them and drop the rest. This is
/// the only form in the console that repeats a key, so it is the only one that reads its own body.
struct Posted(Vec<(String, String)>);

impl Posted {
    fn parse(body: &[u8]) -> Self {
        Self(
            url::form_urlencoded::parse(body)
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    fn one(&self, key: &str) -> &str {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or_default()
    }

    fn all(&self, key: &str) -> Vec<&str> {
        self.0.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).collect()
    }

    /// A browser posts a ticked box and omits an unticked one, so presence is the answer.
    fn on(&self, key: &str) -> bool {
        self.0.iter().any(|(k, v)| k == key && !v.is_empty())
    }
}

/// The five capability flags, moved around as one value so a sixth is a field here rather than
/// five call sites that each forgot a different one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub registry_write: bool,
    pub sealed_capable: bool,
    pub may_delete: bool,
    pub may_ingest: bool,
    pub may_read_history: bool,
}

impl Caps {
    fn of(c: &OauthClientRecord) -> Self {
        Self {
            registry_write: c.registry_write,
            sealed_capable: c.sealed_capable,
            may_delete: c.may_delete,
            may_ingest: c.may_ingest,
            may_read_history: c.may_read_history,
        }
    }

    fn posted(form: &Posted) -> Self {
        Self {
            registry_write: form.on("registry_write"),
            sealed_capable: form.on("sealed_capable"),
            may_delete: form.on("may_delete"),
            may_ingest: form.on("may_ingest"),
            may_read_history: form.on("may_read_history"),
        }
    }
}

/// `*@sealed, project:*@open` into grants. A bare glob means a ceiling of open, which is the same
/// reading `AUTH_TOKENS` gives a bare string, so the two surfaces cannot disagree.
pub fn parse_grants(raw: &str) -> Result<Vec<NamespaceGrant>, String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        let (ns, max) = match t.split_once('@') {
            Some((ns, m)) => {
                let max = match m.trim() {
                    "open" => Sensitivity::Open,
                    "private" => Sensitivity::Private,
                    "sealed" => Sensitivity::Sealed,
                    other => {
                        return Err(format!(
                            "{other:?} is not a level. Use open, private or sealed."
                        ))
                    }
                };
                (ns.trim(), max)
            }
            None => (t, Sensitivity::Open),
        };
        if ns.is_empty() {
            return Err(format!("{t:?} names no namespace"));
        }
        out.push(NamespaceGrant::new(ns.to_ascii_lowercase(), max));
    }
    Ok(out)
}

/// Grants back into the string the advanced field shows, so the form opens saying what the client
/// holds today.
fn grants_text(g: &[NamespaceGrant]) -> String {
    g.iter().map(|n| format!("{}@{}", n.namespace, n.max.as_str())).collect::<Vec<_>>().join(", ")
}

/// The level a shape grants on one side, which is the level a scoped namespace inherits. Every
/// preset states one `*` per side, so this is a max over one element in practice and over the
/// whole side if a preset ever states two.
fn ceiling(side: &[NamespaceGrant]) -> Option<Sensitivity> {
    side.iter().map(|g| g.max).max()
}

/// The shape's globs, narrowed to the namespaces the owner picked. An empty side stays empty: a
/// read-only client scoped to one project still writes nothing.
fn narrowed(side: &[NamespaceGrant], chosen: &[String]) -> Vec<NamespaceGrant> {
    match ceiling(side) {
        None => Vec::new(),
        Some(max) => chosen.iter().map(|ns| NamespaceGrant::new(ns.clone(), max)).collect(),
    }
}

/// The namespaces a scoped grant covers: the boxes that were ticked, plus whatever was typed
/// beside them.
fn chosen_namespaces(form: &Posted) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let typed = form.one("more");
    for raw in form.all("ns").into_iter().chain(typed.split(',')) {
        let ns = raw.trim().to_ascii_lowercase();
        if ns.is_empty() || out.contains(&ns) {
            continue;
        }
        out.push(ns);
    }
    out
}

/// The grant a submitted form asks for.
///
/// Three layers, each one narrower than the last. The shape decides everything. The scope narrows
/// where the shape applies. The advanced view replaces both, and only when its own box is ticked:
/// a form that merged them would let a checkbox nobody saw clear a capability the shape granted.
fn grant_of(form: &Posted) -> Result<ClientGrantUpdate, String> {
    let asked = form.one("preset").trim();
    let preset = Preset::parse(asked)
        .ok_or_else(|| format!("{asked:?} is not one of the shapes on this form"))?;
    let shape = preset.shape();

    if form.on("advanced") {
        let read = match form.one("read").trim() {
            "" => shape.read,
            raw => parse_grants(raw)?,
        };
        let write = match form.one("write").trim() {
            "" => shape.write,
            raw => parse_grants(raw)?,
        };
        let caps = Caps::posted(form);
        return Ok(ClientGrantUpdate {
            profile: Some(format!("{} (adjusted)", preset.as_str())),
            read,
            write,
            registry_write: caps.registry_write,
            sealed_capable: caps.sealed_capable,
            may_delete: caps.may_delete,
            may_ingest: caps.may_ingest,
            may_read_history: caps.may_read_history,
        });
    }

    let (read, write, profile) = if form.one("scope") == "chosen" {
        let chosen = chosen_namespaces(form);
        if chosen.is_empty() {
            return Err("Pick at least one namespace, or set the scope back to every namespace."
                .to_string());
        }
        (
            narrowed(&shape.read, &chosen),
            narrowed(&shape.write, &chosen),
            format!("{} (scoped)", preset.as_str()),
        )
    } else {
        (shape.read, shape.write, preset.as_str().to_string())
    };

    Ok(ClientGrantUpdate {
        profile: Some(profile),
        read,
        write,
        registry_write: shape.registry_write,
        sealed_capable: shape.sealed_capable,
        may_delete: shape.may_delete,
        may_ingest: shape.may_ingest,
        may_read_history: shape.may_read_history,
    })
}

/// How a stored profile reads back onto the form: which shape it started as, and which of the two
/// narrowing layers wrote it.
///
/// `set_client_grant` writes `read-write`, `read-write (scoped)` or `read-write (adjusted)`, so the
/// suffix is the answer. A profile from before presets existed, or one naming a shape no longer in
/// `Preset::ALL`, reads as adjusted: the grant itself holds the truth in that case and the form
/// should open showing it rather than quietly offering to overwrite it with a shape.
fn stored_preset(profile: Option<&str>) -> (Preset, bool, bool) {
    let Some(raw) = profile else { return (Preset::ReadWrite, false, true) };
    let base = raw.split(" (").next().unwrap_or(raw).trim();
    match Preset::parse(base) {
        Some(p) => (p, raw.contains("(scoped)"), raw.contains("(adjusted)")),
        None => (Preset::ReadWrite, false, true),
    }
}

pub async fn index(
    State(app): State<Console>,
    headers: HeaderMap,
    Query(q): Query<IndexQuery>,
) -> Response {
    let session = match app.guard(&headers, "/console/clients") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    listing(&app, sessions, &session, None, None, StatusCode::OK, q.done.as_deref()).await
}

pub async fn create(State(app): State<Console>, headers: HeaderMap, body: Bytes) -> Response {
    let session = match app.guard(&headers, "/console/clients") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let form = Posted::parse(&body);
    if !sessions.console_csrf_ok(&session, NEW_ACTION, NEW_TARGET, form.one("csrf")) {
        tracing::warn!("console client refused: the form token did not match");
        return stale();
    }

    let name = trimmed(form.one("name")).unwrap_or("unnamed client").to_string();
    let grant = match grant_of(&form) {
        Ok(g) => g,
        Err(e) => {
            return listing(&app, sessions, &session, None, Some(&e), StatusCode::BAD_REQUEST, None)
                .await
        }
    };

    let redirect_uris: Vec<String> = form
        .one("redirect_uris")
        .split(|c| c == ',' || c == '\n')
        .filter_map(|s| trimmed(s).map(str::to_string))
        .collect();
    // The same checks `/oauth/register` runs, count and length included. A URI typed here reaches
    // `/authorize` by the same path a registered one does, so a client the owner issued by hand
    // should not be the one way a fragment, a plain-http host or an oversized list gets stored.
    if !redirect_uris.is_empty() {
        if let Err(e) = crate::domain::oauth::validate_redirect_uris(&redirect_uris) {
            let message = e.client_message().to_string();
            return listing(
                &app,
                sessions,
                &session,
                None,
                Some(&message),
                StatusCode::BAD_REQUEST,
                None,
            )
            .await;
        }
    }

    let client_id = match crate::domain::oauth::random_token(24) {
        Ok(id) => id,
        Err(e) => {
            return listing(
                &app,
                sessions,
                &session,
                None,
                Some(&e.log_message()),
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
            )
            .await
        }
    };
    // A secret only when asked for. A client that runs in a browser cannot keep one, and handing it
    // a secret it will publish is worse than the public client PKCE already secures.
    let secret = if form.on("confidential") {
        match crate::domain::oauth::random_token(32) {
            Ok(s) => Some(s),
            Err(e) => {
                return listing(
                    &app,
                    sessions,
                    &session,
                    None,
                    Some(&e.log_message()),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                )
                .await
            }
        }
    } else {
        None
    };

    let record = NewOauthClient {
        client_id: client_id.clone(),
        secret_hash: secret.as_deref().map(hash_token),
        client_name: name.clone(),
        redirect_uris,
        grant_types: vec!["authorization_code".into(), "refresh_token".into()],
        software_id: None,
        software_version: None,
        // "manual" is the column's own word for a credential the owner issued rather than one a
        // client asked for, and the consent screen reads it.
        registered_via: "manual".to_string(),
    };
    if let Err(e) = app.state.oauth.register_client(record).await {
        return listing(
            &app,
            sessions,
            &session,
            None,
            Some(&e.log_message()),
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
        )
        .await;
    }
    // The grant, which also records consent. A client registered and left ungranted would sit in
    // the list looking created and reach nothing.
    if let Err(e) = app.state.oauth.set_client_grant(&client_id, grant).await {
        return listing(
            &app,
            sessions,
            &session,
            None,
            Some(&e.log_message()),
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
        )
        .await;
    }

    listing(
        &app,
        sessions,
        &session,
        Some(Issued { client_id, secret, name }),
        None,
        StatusCode::OK,
        None,
    )
    .await
}

/// Writes a new grant onto a client that already exists.
///
/// The same `set_client_grant` the consent screen calls, so a client the owner narrows here and a
/// client that re-consents end up in one state, and what a client reaches is one row rather than a
/// history of how it got there.
pub async fn access(
    State(app): State<Console>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let session = match app.guard(&headers, "/console/clients") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let form = Posted::parse(&body);
    if !sessions.console_csrf_ok(&session, ACCESS_ACTION, &id, form.one("csrf")) {
        tracing::warn!("console client access refused: the form token did not match");
        return stale();
    }

    let grant = match grant_of(&form) {
        Ok(g) => g,
        Err(e) => {
            return listing(&app, sessions, &session, None, Some(&e), StatusCode::BAD_REQUEST, None)
                .await
        }
    };

    // Revoked stays revoked. `set_client_grant` writes `consented_at`, so letting this through
    // would leave a row that is revoked and freshly approved at once, and the page would have to
    // pick which of the two to believe.
    let refusal = match app.state.oauth.find_client(&id).await {
        Ok(Some(c)) if c.revoked_at.is_none() => None,
        Ok(Some(_)) => Some((
            "That client is revoked. Issue a new one rather than reviving this row.".to_string(),
            StatusCode::BAD_REQUEST,
        )),
        Ok(None) => Some(("There is no client with that id.".to_string(), StatusCode::NOT_FOUND)),
        Err(e) => Some((e.client_message().to_string(), StatusCode::INTERNAL_SERVER_ERROR)),
    };
    if let Some((message, status)) = refusal {
        return listing(&app, sessions, &session, None, Some(&message), status, None).await;
    }

    match app.state.oauth.set_client_grant(&id, grant).await {
        Ok(()) => redirect("/console/clients?done=access"),
        Err(e) => {
            let health = app.health();
            page(
                StatusCode::INTERNAL_SERVER_ERROR,
                pages::notice(
                    "that grant was not written",
                    &e.client_message(),
                    None,
                    Some(&health),
                ),
            )
        }
    }
}

pub async fn revoke(
    State(app): State<Console>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let session = match app.guard(&headers, "/console/clients") {
        Ok(s) => s,
        Err(response) => return response,
    };
    let Some(sessions) = app.sessions.as_ref() else {
        return closed();
    };
    let form = Posted::parse(&body);
    if !sessions.console_csrf_ok(&session, REVOKE_ACTION, &id, form.one("csrf")) {
        tracing::warn!("console client revoke refused: the form token did not match");
        return stale();
    }
    match app.state.oauth.revoke_client(&id).await {
        Ok(true) => redirect("/console/clients?done=revoked"),
        Ok(false) => redirect("/console/clients?done=already-revoked"),
        Err(e) => {
            let health = app.health();
            page(
                StatusCode::INTERNAL_SERVER_ERROR,
                pages::notice(
                    "that client was not revoked",
                    &e.client_message(),
                    None,
                    Some(&health),
                ),
            )
        }
    }
}

/// What a creation produced, shown once and never stored anywhere this page can read.
pub struct Issued {
    pub client_id: String,
    pub secret: Option<String>,
    pub name: String,
}

fn stale() -> Response {
    page(
        StatusCode::BAD_REQUEST,
        pages::notice(
            "that form is out of date",
            "The page it came from was drawn for an older sign-in. Open the clients page again and \
             repeat the change.",
            None,
            None,
        ),
    )
}

async fn listing(
    app: &Console,
    sessions: &Sessions,
    session: &OwnerSession,
    issued: Option<Issued>,
    error: Option<&str>,
    status: StatusCode,
    done: Option<&str>,
) -> Response {
    let clients = app.state.oauth.list_clients(true).await.unwrap_or_default();
    // The namespaces to offer. A store that will not answer costs the picker its list and nothing
    // else: the text box beside it still takes any namespace by name.
    let namespaces: Vec<String> = match data::readable(&app.ctx()).await {
        Ok(rows) => rows.into_iter().map(|r| r.namespace).collect(),
        Err(e) => {
            tracing::warn!(
                error = %e.log_message(),
                "the clients page could not list namespaces for the scope picker"
            );
            Vec::new()
        }
    };
    let health = app.health();
    let csrf = |action: &str, target: &str| sessions.console_csrf(session, action, target);
    page(
        status,
        html(
            View {
                clients: &clients,
                namespaces: &namespaces,
                health: &health,
                issued: issued.as_ref(),
                error,
                done,
            },
            &csrf,
        ),
    )
}

/// Everything the page draws itself from, so a test draws it without a server.
pub struct View<'a> {
    pub clients: &'a [OauthClientRecord],
    /// The namespaces the scope picker offers. Empty is fine and means typing one.
    pub namespaces: &'a [String],
    pub health: &'a Health,
    pub issued: Option<&'a Issued>,
    pub error: Option<&'a str>,
    pub done: Option<&'a str>,
}

pub fn html(v: View<'_>, csrf: &dyn Fn(&str, &str) -> String) -> String {
    let mut body = String::from("<main class=\"page\">");

    body.push_str(&format!(
        "<div class=\"pagehead\"><h2>Clients</h2>\
<span class=\"when\">{} registered</span></div>\
<p class=\"hint\">What each surface may reach. A change here lands on that client's next call: \
nothing reconnects and no token is reissued.</p>",
        v.clients.len()
    ));

    if let Some(line) = v.done.and_then(done_line) {
        body.push_str(&format!("<p class=\"done\">{}</p>", escape(line)));
    }
    if let Some(e) = v.error {
        body.push_str(&format!("<p class=\"wrerr\">{}</p>", escape(e)));
    }

    // The one time the secret is readable. Above the list, because scrolling past it is how it gets
    // lost.
    if let Some(i) = v.issued {
        body.push_str(&format!(
            "<div class=\"note\"><div class=\"big2\">{name} is ready</div>\
<p>Client id</p><p><code>{id}</code></p>",
            name = escape(&i.name),
            id = escape(&i.client_id)
        ));
        match &i.secret {
            Some(s) => body.push_str(&format!(
                "<p>Client secret</p><p><code>{}</code></p>\
<p>Copy it now. The store keeps only a hash of it, so nothing here can show it again. Losing it \
means issuing another client.</p>",
                escape(s)
            )),
            None => body.push_str(
                "<p>A public client, so there is no secret. It proves itself with PKCE, which is \
                 what a client that cannot keep a secret should use.</p>",
            ),
        }
        body.push_str("</div>");
    }

    if v.clients.is_empty() {
        body.push_str(
            "<div class=\"none\"><p class=\"big\">Nothing reaches this store yet.</p>\
<p>Point a client at this server and approve it when it asks, or issue one below.</p></div>",
        );
    } else {
        for c in v.clients {
            body.push_str(&client_card(c, v.namespaces, csrf));
        }
    }
    body.push_str(&new_form(v.namespaces, csrf));
    body.push_str("</main>");

    pages::shell("lumberroom: clients", Tab::Clients, Some(v.health), &body)
}

/// The word in the URL, said as a sentence. The URL carries the word rather than the sentence
/// because a redirect target ends up in browser history and in a proxy log.
fn done_line(word: &str) -> Option<&'static str> {
    match word {
        "revoked" => Some(
            "Revoked. Its tokens went with it, so the surface holding them fails its next call.",
        ),
        "already-revoked" => Some("Nothing changed. That client was already revoked."),
        "access" => Some("Saved. The new grant decides that client's next call."),
        _ => None,
    }
}

/// One client: a line that says who and what, and the controls that change it.
///
/// The grant reads as a sentence rather than a table. Four labelled rows per client turned a list
/// of three into a page you scroll, and what the owner is scanning for is the one client whose
/// reach looks wrong.
fn client_card(
    c: &OauthClientRecord,
    namespaces: &[String],
    csrf: &dyn Fn(&str, &str) -> String,
) -> String {
    let revoked = c.revoked_at.is_some();
    let state = if revoked {
        "<span class=\"pill off\">revoked</span>"
    } else if c.consented_at.is_some() {
        ""
    } else {
        "<span class=\"pill warn\">awaiting consent</span>"
    };
    let origin = if c.registered_via == "manual" { "issued here" } else { "registered itself" };
    let last = match c.last_used_at {
        Some(t) => format!("last used {}", t.format("%-d %b")),
        None => "never used".to_string(),
    };

    let side = |label: &str, g: &[NamespaceGrant]| -> String {
        if g.is_empty() {
            return format!("<span class=\"gk\">{label}</span><span class=\"gv\">nothing</span>");
        }
        let chips: String = g
            .iter()
            .map(|n| {
                format!(
                    "<span class=\"chip\">{}<em>@{}</em></span>",
                    escape(&n.namespace),
                    escape(n.max.as_str())
                )
            })
            .collect();
        format!("<span class=\"gk\">{label}</span>{chips}")
    };
    let caps: String = capability_labels(Caps::of(c))
        .into_iter()
        .map(|l| format!("<span class=\"chip cap\">{l}</span>"))
        .collect();

    // A revoked client keeps its row and loses its controls: it is a record of what happened.
    let controls = if revoked {
        String::new()
    } else {
        format!(
            "<div class=\"cli-acts\">{editor}\
<form method=\"post\" action=\"/console/clients/{id}/revoke\">\
<input type=\"hidden\" name=\"csrf\" value=\"{token}\">\
<button type=\"submit\" class=\"danger\">Revoke</button></form></div>",
            editor = access_form(c, namespaces, csrf),
            id = escape(&c.client_id),
            token = escape(&csrf(REVOKE_ACTION, &c.client_id)),
        )
    };

    format!(
        "<article class=\"cli{gone}\">\
<div class=\"cli-head\"><span class=\"cli-name\">{name}</span>{state}\
<span class=\"cli-meta\">{origin} &middot; {last} &middot; <code>{id}</code></span></div>\
<div class=\"cli-grant\">{read}{write}{caps}</div>{controls}</article>",
        gone = if revoked { " gone" } else { "" },
        // Cleaned at render as well as at registration: a row stored before the invisible list grew
        // still holds a bidi override that would reorder the card around it.
        name = escape(&client_name_display(&c.client_name)),
        state = state,
        origin = origin,
        last = escape(&last),
        id = escape(&c.client_id),
        read = side("reads", &c.read),
        write = side("writes", &c.write),
        caps = caps,
        controls = controls,
    )
}

/// The capabilities a grant holds, in the words the tool descriptions use.
fn capability_labels(caps: Caps) -> Vec<&'static str> {
    let mut out = Vec::new();
    if caps.registry_write {
        out.push("registryWrite");
    }
    if caps.sealed_capable {
        out.push("sealedCapable");
    }
    if caps.may_delete {
        out.push("mayDelete");
    }
    if caps.may_ingest {
        out.push("mayIngest");
    }
    if caps.may_read_history {
        out.push("mayReadHistory");
    }
    out
}

/// The grant in one line a person can scan. The card draws the same thing in chips; this is what a
/// log line gets.
pub fn grant_line(c: &OauthClientRecord) -> String {
    let side = |label: &str, g: &[NamespaceGrant]| -> String {
        if g.is_empty() {
            return format!("{label} nothing");
        }
        let names: Vec<String> =
            g.iter().map(|n| format!("{}@{}", n.namespace, n.max.as_str())).collect();
        format!("{label} {}", names.join(" "))
    };
    let caps = capability_labels(Caps::of(c));
    let caps = if caps.is_empty() { "no capabilities".to_string() } else { caps.join(", ") };
    format!("{}, {}, {caps}", side("reads", &c.read), side("writes", &c.write))
}

/// One client's own copy of the access form, folded away until it is wanted.
fn access_form(
    c: &OauthClientRecord,
    namespaces: &[String],
    csrf: &dyn Fn(&str, &str) -> String,
) -> String {
    let (preset, scoped, adjusted) = stored_preset(c.profile.as_deref());
    let waiting = c.consented_at.is_none();
    // Everything the client already reaches, so a namespace it holds is ticked even after the last
    // row under that name was retired and the store stopped listing it.
    let held: Vec<String> = c
        .read
        .iter()
        .chain(c.write.iter())
        .map(|g| g.namespace.clone())
        .filter(|n| n != "*")
        .collect();

    format!(
        "<details class=\"cli-edit\"{open}><summary>{summary}</summary>\
<form method=\"post\" action=\"/console/clients/{id}/access\">\
<input type=\"hidden\" name=\"csrf\" value=\"{token}\">\
{controls}\
<div class=\"send\"><button type=\"submit\" class=\"go\">{submit}</button>\
<span class=\"hint\">{note}</span></div></form></details>",
        open = if waiting { " open" } else { "" },
        summary = if waiting { "Approve" } else { "Change access" },
        id = escape(&c.client_id),
        token = escape(&csrf(ACCESS_ACTION, &c.client_id)),
        controls = grant_controls(Chosen {
            preset,
            scoped,
            adjusted,
            read: &grants_text(&c.read),
            write: &grants_text(&c.write),
            caps: Caps::of(c),
            held: &held,
            namespaces,
            form_id: &c.client_id,
        }),
        submit = if waiting { "Approve this client" } else { "Save" },
        note = if waiting {
            "Approving is what gives it a grant. Until then every call it makes is refused."
        } else {
            "It applies on the next call."
        },
    )
}

/// What the controls open with.
struct Chosen<'a> {
    preset: Preset,
    scoped: bool,
    adjusted: bool,
    read: &'a str,
    write: &'a str,
    caps: Caps,
    /// Namespaces this grant already names, ticked whether or not the store still lists them.
    held: &'a [String],
    namespaces: &'a [String],
    /// Distinguishes this card's Reads/Writes inputs from every other card's, so a label's `for`
    /// points at one input: the form draws one copy per client plus one for the create form, and a
    /// fixed id would repeat across the page and steal focus for the wrong client.
    form_id: &'a str,
}

/// The shape, the scope and the advanced view, drawn the same wherever a grant is chosen.
///
/// The create form and every client's form ask the same three questions, and a page that drew them
/// twice would answer them differently the first time one of them changed.
fn grant_controls(c: Chosen<'_>) -> String {
    let mut shapes = String::from("<div class=\"shapes\">");
    for p in Preset::ALL {
        shapes.push_str(&format!(
            "<label class=\"shape\"><input type=\"radio\" name=\"preset\" value=\"{value}\"{on}>\
<span class=\"sh-t\">{title}</span><span class=\"sh-d\">{detail}</span></label>",
            value = p.as_str(),
            on = if p == c.preset { " checked" } else { "" },
            title = escape(p.title()),
            detail = escape(p.detail()),
        ));
    }
    shapes.push_str("</div>");

    // The list, plus anything this client reaches that the list has forgotten.
    let mut offered: Vec<&str> = c.namespaces.iter().map(String::as_str).collect();
    for ns in c.held {
        if !offered.iter().any(|o| o == ns) {
            offered.push(ns);
        }
    }
    let boxes: String = offered
        .iter()
        .map(|ns| {
            format!(
                "<label class=\"nsbox\"><input type=\"checkbox\" name=\"ns\" value=\"{ns}\"{on}>\
<span>{ns}</span></label>",
                ns = escape(ns),
                on = if c.held.iter().any(|h| h == ns) { " checked" } else { "" },
            )
        })
        .collect();
    // What the picker cannot offer: a namespace with nothing in it yet, or a glob.
    let typed = c
        .held
        .iter()
        .filter(|h| !c.namespaces.iter().any(|n| n == *h))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");

    let check = |name: &str, label: &str, on: bool, detail: &str| -> String {
        format!(
            "<label class=\"check\"><input type=\"checkbox\" name=\"{name}\" value=\"1\"{on}>\
<span class=\"ch-t\">{label}</span><span class=\"ch-d\">{detail}</span></label>",
            on = if on { " checked" } else { "" },
        )
    };

    format!(
        "{shapes}\
<div class=\"scope\">\
<label class=\"opt\"><input type=\"radio\" name=\"scope\" value=\"all\"{all}>\
<span>Everywhere</span></label>\
<label class=\"opt\"><input type=\"radio\" name=\"scope\" value=\"chosen\"{some}>\
<span>Only these namespaces</span></label>\
<div class=\"nspick\">{boxes}\
<input type=\"text\" name=\"more\" value=\"{typed}\" placeholder=\"project:new-thing, personal:*\" \
autocomplete=\"off\" aria-label=\"Other namespaces\">\
<p class=\"hint\">The shape above decides how deep this client sees. These decide where.</p></div>\
</div>\
<details class=\"adv\"{open}><summary>Write the grant out instead</summary>\
{use_fields}\
<div class=\"pair\">\
<div class=\"f\"><label for=\"adv-read-{form_id}\">Reads</label>\
<input id=\"adv-read-{form_id}\" type=\"text\" name=\"read\" value=\"{read}\" \
placeholder=\"*@sealed\" autocomplete=\"off\">\
</div>\
<div class=\"f\"><label for=\"adv-write-{form_id}\">Writes</label>\
<input id=\"adv-write-{form_id}\" type=\"text\" name=\"write\" value=\"{write}\" \
placeholder=\"*@open\" autocomplete=\"off\">\
</div></div>\
<p class=\"hint\">Comma separated globs, each with a ceiling after an @. A bare glob means open, \
the same reading AUTH_TOKENS gives a bare string.</p>\
<div class=\"caps\">{registry}{sealed}{ingest}{history}{delete}</div>\
</details>",
        shapes = shapes,
        all = if c.scoped { "" } else { " checked" },
        some = if c.scoped { " checked" } else { "" },
        boxes = boxes,
        typed = escape(&typed),
        open = if c.adjusted { " open" } else { "" },
        use_fields = check(
            "advanced",
            "Write the grant by hand",
            c.adjusted,
            "Ticked, the capability boxes below decide and the scope above is ignored. An \
empty Reads or Writes field falls back to the shape's grant for that side.",
        ),
        read = escape(c.read),
        write = escape(c.write),
        form_id = escape(c.form_id),
        registry = check(
            "registry_write",
            "registryWrite",
            c.caps.registry_write,
            "Writes the registry, which holds credential locations.",
        ),
        sealed = check(
            "sealed_capable",
            "sealedCapable",
            c.caps.sealed_capable,
            "Asserts it decrypts locally. Without it a sealed row is ciphertext.",
        ),
        ingest = check(
            "may_ingest",
            "mayIngest",
            c.caps.may_ingest,
            "Fills the proposal queue and reaches the cleanup routes.",
        ),
        history = check(
            "may_read_history",
            "mayReadHistory",
            c.caps.may_read_history,
            "Reads retired facts, which can be more revealing than what replaced them.",
        ),
        delete = check(
            "may_delete",
            "mayDelete",
            c.caps.may_delete,
            "Deletes memories. No shape grants this.",
        ),
    )
}

/// Issuing a client by hand, folded away. Most clients register themselves, so this is the rarer
/// job and it was taking two thirds of the page.
fn new_form(namespaces: &[String], csrf: &dyn Fn(&str, &str) -> String) -> String {
    format!(
        "<details class=\"issue\"><summary>Issue a client by hand</summary>\
<p class=\"hint\">For a surface that cannot register itself, or one you want waiting before it \
first calls. A client issued here is consented to already.</p>\
<form method=\"post\" action=\"/console/clients/new\">\
<input type=\"hidden\" name=\"csrf\" value=\"{token}\">\
<div class=\"pair\">\
<div class=\"f\"><label for=\"c-name\">Name</label>\
<input id=\"c-name\" type=\"text\" name=\"name\" placeholder=\"claude-desktop\" \
autocomplete=\"off\">\
<p class=\"hint\">A label in this list, never an identity.</p></div>\
<div class=\"f\"><label for=\"c-uris\">Redirect URIs</label>\
<input id=\"c-uris\" type=\"text\" name=\"redirect_uris\" \
placeholder=\"https://claude.ai/api/mcp/auth_callback\" autocomplete=\"off\">\
<p class=\"hint\">Comma separated, matched exactly. A trailing slash is a different address.</p>\
</div></div>\
<label class=\"check\"><input type=\"checkbox\" name=\"confidential\" value=\"1\">\
<span class=\"ch-t\">Issue a client secret</span>\
<span class=\"ch-d\">For a client that runs on a server and can keep one. A browser cannot, and \
should stay public: PKCE binds its exchange. Shown once, stored as a hash.</span></label>\
{controls}\
<div class=\"send\"><button type=\"submit\" class=\"go\">Create it</button>\
<a href=\"/console/reading\">Back to reading</a></div>\
</form></details>",
        token = escape(&csrf(NEW_ACTION, NEW_TARGET)),
        controls = grant_controls(Chosen {
            preset: Preset::ReadWrite,
            scoped: false,
            adjusted: false,
            read: "",
            write: "",
            caps: Caps::default(),
            held: &[],
            namespaces,
            form_id: "new",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn named(name: &str) -> OauthClientRecord {
        OauthClientRecord {
            client_id: "c1".into(),
            secret_hash: None,
            client_name: name.into(),
            redirect_uris: vec!["https://tool.example/cb".into()],
            grant_types: vec!["authorization_code".into()],
            registered_via: "dcr".into(),
            software_id: None,
            read: Vec::new(),
            write: Vec::new(),
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
            consented_at: None,
            profile: None,
            created_at: Utc::now(),
            last_used_at: None,
            revoked_at: Some(Utc::now()),
        }
    }

    #[test]
    fn a_stored_name_prints_without_its_bidi_override_and_stays_escaped() {
        let card = client_card(&named("<b>Evil\u{202E}gnp.exe</b>\n"), &[], &|_, _| String::new());
        assert!(!card.contains('\u{202E}'), "{card}");
        assert!(
            card.contains("<span class=\"cli-name\">&lt;b&gt;Evilgnp.exe&lt;/b&gt;</span>"),
            "{card}"
        );
    }
}
