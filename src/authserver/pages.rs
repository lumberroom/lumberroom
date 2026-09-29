//! The two pages the owner ever sees: sign in, and decide what a client may reach.
//!
//! Server-rendered, no JavaScript, no external assets, inline CSS. A consent screen that depends on
//! a CDN is a consent screen that renders blank on a network that blocks the CDN, and the owner reads
//! this page while deciding whether to hand a browser tool their memory.
//!
//! Every interpolated value goes through `escape`. `client_name`, `software_id` and the redirect URI
//! arrive in an unauthenticated dynamic-registration request, so they are attacker-controlled text
//! rendered on a page that has the owner's session cookie. That is the one XSS this server can
//! actually be handed.

use crate::console::pages::FAVICON;
use crate::domain::oauth::{
    claimed_known_client, client_name_display, redirect_destination, GrantProfile,
    RedirectDestination,
};

/// The mark, above the heading on all three pages.
///
/// It points at `/console/logo.svg`, which the console serves outside its session guard for exactly
/// this: the owner reads these two screens before any session exists. One path rather than a second
/// copy of the file, because a mark that drifts between the consent screen and the console is a
/// mark the owner has to think about.
const MARK: &str =
    "<img class=\"glyph\" src=\"/console/logo.svg\" width=\"32\" height=\"32\" alt=\"\">";

/// The hidden fields that carry an authorization request through the login and consent POSTs.
///
/// They are hidden fields rather than a server-side pending-request table because the request has to
/// survive a restart between the redirect and the Allow, and because every value in it is
/// re-validated against the client record on the way back in. Nothing here is trusted for having
/// been on the page.
pub struct FlowFields<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub code_challenge_method: &'a str,
    pub response_type: &'a str,
    pub state: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub resource: Option<&'a str>,
}

/// What the owner is shown about the client asking for access.
pub struct ClientView<'a> {
    pub client_name: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub software_id: Option<&'a str>,
    /// True when the client registered itself through RFC 7591 rather than being issued by hand.
    pub self_registered: bool,
    /// The profile this client already holds, when it is being re-consented.
    pub current_profile: Option<&'a str>,
}

/// The stylesheet. `include_str!` in a release build, read from disk on every render in a
/// development one, so an edit to `auth.css` shows up on a browser refresh instead of a recompile
/// and a restart. `CARGO_MANIFEST_DIR` is baked in at compile time and the dev container
/// bind-mounts the repository at that same path, so the file the running server reads is the file
/// being edited. A read that fails falls back to the compiled-in copy rather than serving an
/// unstyled page.
///
/// `[profile.dev-release]` sets `debug-assertions = true` to keep this arm switched on; it
/// inherits from release, where the flag is off.
const STYLE: &str = include_str!("auth.css");

#[cfg(debug_assertions)]
fn style() -> std::borrow::Cow<'static, str> {
    match std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/authserver/auth.css")) {
        Ok(css) => std::borrow::Cow::Owned(css),
        Err(_) => std::borrow::Cow::Borrowed(STYLE),
    }
}

#[cfg(not(debug_assertions))]
fn style() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed(STYLE)
}

/// HTML-escape for both text and attribute contexts.
///
/// Quotes and apostrophes are escaped too, because the same function fills `value="..."` on every
/// hidden field. Escaping only `<` and `&` is what makes an attacker-supplied client name able to
/// close an attribute and open an event handler.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"robots\" content=\"noindex,nofollow\">{FAVICON}\
<title>{}</title><style>{style}</style></head>\n<body><main>{body}</main></body></html>\n",
        escape(title),
        style = style()
    )
}

fn hidden(name: &str, value: &str) -> String {
    format!("<input type=\"hidden\" name=\"{}\" value=\"{}\">", escape(name), escape(value))
}

impl FlowFields<'_> {
    /// The authorization request, re-posted verbatim. An absent optional field is omitted rather
    /// than sent empty, so what comes back deserialises to the same `None` it arrived as.
    fn hidden_inputs(&self) -> String {
        let mut out = String::new();
        out.push_str(&hidden("client_id", self.client_id));
        out.push_str(&hidden("redirect_uri", self.redirect_uri));
        out.push_str(&hidden("code_challenge", self.code_challenge));
        out.push_str(&hidden("code_challenge_method", self.code_challenge_method));
        out.push_str(&hidden("response_type", self.response_type));
        for (name, value) in
            [("state", self.state), ("scope", self.scope), ("resource", self.resource)]
        {
            if let Some(v) = value {
                out.push_str(&hidden(name, v));
            }
        }
        out
    }
}

/// The password form. Shown when there is no live session, and again with a message when a password
/// was wrong or the attempt was throttled.
pub fn login(flow: &FlowFields, client_name: &str, error: Option<&str>) -> String {
    let banner = match error {
        Some(message) => format!("<p class=\"error\">{}</p>", escape(message)),
        None => String::new(),
    };
    shell(
        "lumberroom: sign in",
        &format!(
            "{MARK}<p class=\"mark\">lumberroom</p>\
<h1>Sign in to lumberroom</h1>\
<p class=\"lede\"><b>{client}</b> is asking for access to your memory. Sign in to decide what it \
may reach.</p>\
{banner}\
<form method=\"post\" action=\"/oauth/login\">{fields}\
<label class=\"field\" for=\"password\">Owner password</label>\
<input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"current-password\" \
autofocus required>\
<p class=\"actions\"><button type=\"submit\">Sign in</button></p></form>\
<p class=\"foot\">Signing in does not grant anything. The next page is where you choose.</p>",
            // Cleaned again here: a row stored before the invisible-character list grew can still
            // hold a bidi override.
            client = escape(&client_name_display(client_name)),
            banner = banner,
            fields = flow.hidden_inputs(),
        ),
    )
}

/// The consent screen. The only place in this server that turns a registered client into a client
/// holding a grant.
pub fn consent(
    flow: &FlowFields,
    client: &ClientView,
    csrf: &str,
    default_profile: GrantProfile,
) -> String {
    let mut choices = String::new();
    for profile in [GrantProfile::Full, GrantProfile::Standard, GrantProfile::Narrow] {
        let checked = if profile == default_profile { " checked" } else { "" };
        choices.push_str(&format!(
            "<label class=\"choice\"><input type=\"radio\" name=\"profile\" value=\"{value}\"{checked}>\
<b>{name}</b><span>{describe}</span></label>",
            value = escape(profile.as_str()),
            checked = checked,
            name = escape(&title_case(profile.as_str())),
            describe = escape(profile.describe()),
        ));
    }

    let (destination, origin) = if client.self_registered {
        self_registered_notice(client)
    } else {
        (String::new(), String::new())
    };

    let again = match client.current_profile {
        Some(profile) => format!(
            "<p class=\"lede\">This client already holds the <b>{}</b> grant. Choosing again \
replaces it.</p>",
            escape(profile)
        ),
        None => String::new(),
    };

    let software = match client.software_id {
        Some(id) if !id.is_empty() => {
            format!("<dt>Software id</dt><dd>{}</dd>", escape(id))
        }
        _ => "<dt>Software id</dt><dd>not declared</dd>".to_string(),
    };

    shell(
        "lumberroom: grant access",
        &format!(
            "{MARK}<p class=\"mark\">lumberroom</p>\
<h1>Give <b>{client}</b> access to lumberroom?</h1>{destination}\
<p class=\"lede\">It will be able to read and write memories inside the boundary you pick, on \
every surface, until you revoke it.</p>\
{origin}{again}\
<dl><dt>Client name</dt><dd>{client}</dd>\
<dt>Sends the code back to</dt><dd>{redirect}</dd>\
{software}\
<dt>Client id</dt><dd>{client_id}</dd>\
<dt>Registered</dt><dd>{registered}</dd></dl>\
<form method=\"post\" action=\"/oauth/consent\">{fields}{csrf}\
<fieldset><legend>What it may reach</legend>{choices}</fieldset>\
<p class=\"actions\"><button type=\"submit\" name=\"action\" value=\"allow\">Allow</button>\
<button type=\"submit\" name=\"action\" value=\"deny\" class=\"secondary\">Deny</button></p></form>\
<p class=\"foot\">Change or revoke this at <code>/console/clients</code> whenever you like. <code>lumberroom clients</code> lists what is registered.</p>",
            client = escape(&client_name_display(client.client_name)),
            destination = destination,
            origin = origin,
            again = again,
            redirect = escape(client.redirect_uri),
            software = software,
            client_id = escape(client.client_id),
            registered = if client.self_registered {
                "by itself, through dynamic client registration"
            } else {
                "by you"
            },
            fields = flow.hidden_inputs(),
            csrf = hidden("csrf", csrf),
            choices = choices,
        ),
    )
}

/// The destination line under the headline and the warning, for a client that registered itself.
///
/// The name in the headline is whatever the registrant typed, so an attacker can register "Claude"
/// with its own redirect URI and the headline reads "Give Claude access". The redirect host is the
/// one fact on this page the registrant cannot dress up, since the code lands there, so it goes
/// next to the headline and decides whether the owner is warned. The name only sharpens the
/// wording of a warning the host has already earned.
///
/// Only loopback draws no warning. A known host such as claude.ai serves many accounts, so it
/// proves the service and not the person: anyone can add a connector in their own account that
/// points here, then send the victim the authorize link. The recognised host gets a short warning
/// about that link and no mismatch alarm.
fn self_registered_notice(client: &ClientView) -> (String, String) {
    let destination = redirect_destination(client.redirect_uri);
    let target = match &destination {
        RedirectDestination::Host { host, .. } => format!("<b>{}</b>", escape(host)),
        RedirectDestination::App { scheme } => format!("<b>{}</b>", escape(scheme)),
        RedirectDestination::Unreadable => "<b>an address this server cannot read</b>".to_string(),
    };
    let (line, place) = match &destination {
        RedirectDestination::App { .. } => (
            format!("Codes go to the app on this device that claims {target}"),
            format!("the app that claims {target}"),
        ),
        _ => (format!("Codes go to {target}"), format!("the app at {target}")),
    };
    let line = format!("<p class=\"dest\">{line}</p>");

    if destination.is_local() {
        return (line, String::new());
    }

    if destination.recognised() {
        let warning =
            "<p class=\"warn\">Allow only if you pressed Connect in your own account just \
now. A link someone sent you connects their account.</p>"
                .to_string();
        return (line, warning);
    }

    let (class, mismatch) = match claimed_known_client(client.client_name) {
        Some(brand) => (
            "warn alarm",
            format!(
                "This client calls itself {brand}, but {target} is not where {brand} sends its \
codes. "
            ),
        ),
        None => ("warn", String::new()),
    };
    // Registration lets plain http through only to loopback, which returned above, or to the
    // owner's own network, so the sentence can name the local network.
    let cleartext = if destination.is_plain_http() {
        " Anyone on your local network could be listening at that address, and the code travels there unencrypted."
    } else {
        ""
    };
    let warning = format!(
        "<p class=\"{class}\">{mismatch}Whoever registered this client chose the name <b>{name}</b>, \
and this server cannot vouch for it. Allow only if you started this connection from \
{place}.{cleartext}</p>",
        name = escape(&client_name_display(client.client_name)),
    );
    (line, warning)
}

/// Any failure that must not be reported by redirecting. Everything before the redirect URI has been
/// matched against the client record lands here, because reporting those by redirect is the
/// open-redirect path.
pub fn error_page(title: &str, detail: &str) -> String {
    shell(
        &format!("lumberroom: {title}"),
        &format!(
            "{MARK}<p class=\"mark\">lumberroom</p><h1>{}</h1><p class=\"lede\">{}</p>\
<p class=\"foot\">Nothing was granted, and no code was issued. Start the connection again from \
the client.</p>",
            escape(title),
            escape(detail)
        ),
    )
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow() -> FlowFields<'static> {
        FlowFields {
            client_id: "abc123",
            redirect_uri: "https://claude.ai/api/mcp/auth_callback",
            code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            code_challenge_method: "S256",
            response_type: "code",
            state: Some("st-1"),
            scope: None,
            resource: Some("https://lumberroom.example/mcp"),
        }
    }

    fn view() -> ClientView<'static> {
        ClientView {
            client_name: "Claude",
            client_id: "abc123",
            redirect_uri: "https://claude.ai/api/mcp/auth_callback",
            software_id: Some("anthropic-claude"),
            self_registered: true,
            current_profile: None,
        }
    }

    #[test]
    fn a_stored_name_with_a_bidi_override_renders_without_it_on_every_page() {
        let dirty = "Cl\u{202E}au\u{3164}de";
        let consent_page = render(&registered(dirty, "https://tool.example/cb"));
        let login_page = login(&flow(), dirty, None);
        for page in [consent_page, login_page] {
            assert!(!page.contains('\u{202E}'));
            assert!(!page.contains('\u{3164}'));
            assert!(page.contains("<b>Claude</b>"));
        }
    }

    #[test]
    fn a_client_name_carrying_markup_cannot_close_a_tag() {
        let hostile = "<script>alert(1)</script>";
        let page = login(&flow(), hostile, None);
        assert!(!page.contains("<script>alert"));
        assert!(page.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_client_name_carrying_a_quote_cannot_escape_an_attribute() {
        let hostile = "\" onfocus=\"alert(1)";
        let mut f = flow();
        f.client_id = hostile;
        let page = login(&f, "x", None);
        assert!(!page.contains("onfocus=\"alert"));
        assert!(page.contains("&quot; onfocus=&quot;"));
    }

    #[test]
    fn every_page_is_self_contained() {
        let pages = [
            login(&flow(), "Claude", None),
            consent(&flow(), &view(), "tok", GrantProfile::Standard),
            error_page("bad request", "no"),
        ];
        for page in pages {
            assert!(!page.contains("<script"), "no JavaScript");
            assert!(!page.contains("http://"), "no external assets");
            assert_eq!(
                page.matches("src=").count(),
                page.matches("src=\"/console/logo.svg\"").count(),
                "nothing is fetched except the mark"
            );
            assert!(page.starts_with("<!doctype html>"));
        }
    }

    #[test]
    fn the_login_form_carries_the_authorize_request_forward() {
        let page = login(&flow(), "Claude", None);
        assert!(page.contains("name=\"client_id\" value=\"abc123\""));
        assert!(page.contains("name=\"code_challenge_method\" value=\"S256\""));
        assert!(page.contains("name=\"state\" value=\"st-1\""));
        assert!(page.contains("name=\"resource\" value=\"https://lumberroom.example/mcp\""));
        assert!(page.contains("action=\"/oauth/login\""));
    }

    #[test]
    fn an_absent_optional_parameter_is_omitted_rather_than_sent_empty() {
        // scope is None in the fixture. An empty `scope=` would come back as Some(""), which is a
        // different authorization request from the one the client made.
        let page = login(&flow(), "Claude", None);
        assert!(!page.contains("name=\"scope\""));
    }

    #[test]
    fn the_login_page_shows_a_message_when_one_is_given() {
        let page = login(&flow(), "Claude", Some("that password is wrong"));
        assert!(page.contains("that password is wrong"));
        assert!(login(&flow(), "Claude", None).matches("class=\"error\"").count() == 0);
    }

    #[test]
    fn the_consent_page_offers_all_three_profiles_with_their_descriptions() {
        let page = consent(&flow(), &view(), "tok", GrantProfile::Standard);
        for profile in [GrantProfile::Full, GrantProfile::Standard, GrantProfile::Narrow] {
            assert!(page.contains(&format!("value=\"{}\"", profile.as_str())));
            assert!(page.contains(&escape(profile.describe())));
        }
    }

    #[test]
    fn the_consent_page_preselects_the_configured_default() {
        let page = consent(&flow(), &view(), "tok", GrantProfile::Narrow);
        assert!(page.contains("value=\"narrow\" checked"));
        assert!(!page.contains("value=\"full\" checked"));
    }

    #[test]
    fn the_consent_page_carries_the_csrf_token_and_both_actions() {
        let page = consent(&flow(), &view(), "tok-9", GrantProfile::Full);
        assert!(page.contains("name=\"csrf\" value=\"tok-9\""));
        assert!(page.contains("name=\"action\" value=\"allow\""));
        assert!(page.contains("name=\"action\" value=\"deny\""));
    }

    #[test]
    fn the_consent_page_says_when_a_client_registered_itself() {
        let page = consent(&flow(), &view(), "tok", GrantProfile::Standard);
        assert!(page.contains("by itself, through dynamic client registration"));
        assert!(page.contains("anthropic-claude"));
        assert!(page.contains("https://claude.ai/api/mcp/auth_callback"));

        let mut issued = view();
        issued.self_registered = false;
        let page = consent(&flow(), &issued, "tok", GrantProfile::Standard);
        assert!(!page.contains("by itself"));
        assert!(page.contains("by you"));
    }

    /// A self-registered client sending its codes to `redirect_uri`, under `name`.
    fn registered(name: &'static str, redirect_uri: &'static str) -> ClientView<'static> {
        ClientView { client_name: name, redirect_uri, ..view() }
    }

    fn render(client: &ClientView) -> String {
        consent(&flow(), client, "tok", GrantProfile::Standard)
    }

    const UNVOUCHED: &str = "this server cannot vouch for it";
    const MISMATCH: &str = "is not where";
    const FORWARDED: &str = "A link someone sent you connects their account";

    #[test]
    fn the_destination_host_sits_under_the_headline_for_a_self_registered_client() {
        let page = render(&registered("Zed", "https://zed.example/oauth/cb"));
        let headline = page.find("<h1>").unwrap();
        let destination = page.find("class=\"dest\"").expect("destination line");
        let details = page.find("<dl>").unwrap();
        assert!(headline < destination && destination < details);
        assert!(page.contains("Codes go to <b>zed.example</b>"));
    }

    #[test]
    fn a_known_client_host_still_warns_that_a_forwarded_link_connects_someone_elses_account() {
        let page = render(&view());
        assert!(page.contains("Codes go to <b>claude.ai</b>"));
        assert!(page.contains("class=\"warn"));
        assert!(!page.contains("class=\"warn alarm"), "the host is real, so no mismatch alarm");
        assert!(page.contains(FORWARDED));
        assert!(!page.contains(UNVOUCHED), "the host is not in doubt, the sender is");
    }

    #[test]
    fn a_loopback_destination_draws_no_warning() {
        let page = render(&registered("lumberroom CLI", "http://127.0.0.1:53682/callback"));
        assert!(page.contains("Codes go to <b>127.0.0.1</b>"));
        assert!(!page.contains("class=\"warn"));
    }

    #[test]
    fn an_unknown_host_draws_a_warning_that_names_the_host() {
        let page = render(&registered("Zed", "https://zed.example/oauth/cb"));
        assert!(page.contains("class=\"warn"));
        assert!(page.contains(UNVOUCHED));
        assert!(page.contains("the app at <b>zed.example</b>"));
        assert!(!page.contains(MISMATCH), "Zed claims no known client");
    }

    #[test]
    fn a_known_host_used_as_a_prefix_is_an_unknown_host() {
        let page = render(&registered("Zed", "https://claude.ai.attacker.example/cb"));
        assert!(page.contains("Codes go to <b>claude.ai.attacker.example</b>"));
        assert!(page.contains(UNVOUCHED));
    }

    #[test]
    fn an_ip_address_destination_draws_a_warning() {
        let page = render(&registered("Zed", "https://203.0.113.7/cb"));
        assert!(page.contains("Codes go to <b>203.0.113.7</b>"));
        assert!(page.contains(UNVOUCHED));
    }

    const CLEARTEXT: &str = "the code travels there unencrypted";

    #[test]
    fn a_plain_http_lan_destination_warns_that_the_code_travels_unencrypted() {
        let page = render(&registered("OpenWebUI", "http://192.168.1.10:3000/oauth/callback"));
        assert!(page.contains("Codes go to <b>192.168.1.10</b>"));
        assert!(page.contains("class=\"warn"));
        assert!(page.contains(UNVOUCHED));
        assert!(page.contains(CLEARTEXT));
        let named = render(&registered("OpenWebUI", "http://nas.local:3000/oauth/callback"));
        assert!(named.contains(CLEARTEXT));
    }

    #[test]
    fn only_a_plain_http_lan_destination_carries_the_unencrypted_line() {
        for uri in [
            "https://192.168.1.10:3000/oauth/callback",
            "https://zed.example/oauth/cb",
            "http://127.0.0.1:53682/callback",
            "com.example.zed:/oauth2redirect",
        ] {
            let page = render(&registered("Zed", uri));
            assert!(!page.contains(CLEARTEXT), "{uri}");
        }
        assert!(!render(&view()).contains(CLEARTEXT), "a known https host");
    }

    #[test]
    fn a_private_use_scheme_draws_a_warning_that_names_the_scheme() {
        let page = render(&registered("Zed", "com.example.zed:/oauth2redirect"));
        assert!(page.contains("<b>com.example.zed</b>"));
        assert!(page.contains(UNVOUCHED));
    }

    #[test]
    fn a_name_borrowed_from_a_known_client_is_called_out_against_its_host() {
        let page = render(&registered("Claude", "https://attacker.example/cb"));
        assert!(page.contains(UNVOUCHED));
        assert!(page.contains(MISMATCH));
        assert!(page.contains("<b>attacker.example</b> is not where Claude"));
    }

    #[test]
    fn a_borrowed_name_hidden_behind_case_and_invisible_characters_is_still_called_out() {
        let page = render(&registered("my CHAT\u{200B}GPT helper", "https://attacker.example/cb"));
        assert!(page.contains("is not where ChatGPT"));
    }

    #[test]
    fn a_client_the_owner_issued_draws_no_warning_wherever_it_sends_codes() {
        let mut issued = registered("Claude", "https://attacker.example/cb");
        issued.self_registered = false;
        let page = render(&issued);
        assert!(!page.contains("class=\"warn"));
        assert!(!page.contains("class=\"dest\""));
    }

    #[test]
    fn a_hostile_host_is_escaped_in_the_destination_and_the_warning() {
        // `&`, `'` and `"` are legal in a URL host, and the page puts the host in text and could
        // one day put it in an attribute.
        let page = render(&registered("Claude", "https://a&b'c\"d.example/cb"));
        assert!(!page.contains("a&b'c\"d.example"));
        assert!(page.contains("Codes go to <b>a&amp;b&#39;c&quot;d.example</b>"));
        assert!(page.contains("<b>a&amp;b&#39;c&quot;d.example</b> is not where Claude"));
    }

    #[test]
    fn a_hostile_name_is_escaped_inside_the_warning() {
        let page =
            render(&registered("<img src=x onerror=alert(1)>Claude", "https://x.example/cb"));
        assert!(!page.contains("<img src=x"));
        assert!(page.contains("&lt;img src=x onerror=alert(1)&gt;Claude"));
    }

    #[test]
    fn a_client_being_reconsented_is_told_what_it_already_holds() {
        let mut again = view();
        again.current_profile = Some("narrow");
        let page = consent(&flow(), &again, "tok", GrantProfile::Standard);
        assert!(page.contains("already holds"));
        assert!(page.contains("<b>narrow</b>"));
    }

    #[test]
    fn an_undeclared_software_id_is_shown_as_such_rather_than_as_a_gap() {
        let mut anonymous = view();
        anonymous.software_id = None;
        let page = consent(&flow(), &anonymous, "tok", GrantProfile::Standard);
        assert!(page.contains("not declared"));
    }
}
