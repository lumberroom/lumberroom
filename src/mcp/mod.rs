//! MCP surface: twelve tools, seven open to every client and five behind a capability each.
//!
//! `src/mcp/capability.rs` holds which grant opens which tool, and `list_tools` filters on it so a
//! client never sees a tool it cannot call. The filter shapes what a model tries; every service
//! checks the grant again on the call, which is what refuses a client that names a tool anyway.
//!
//! Descriptions say what a tool does, what comes back and when it applies. They give no orders
//! about behaviour: when to read or write memory is the owner's rule, kept in the owner's own agent
//! configuration (docs/connect-*.md carries the snippets), and a server that issues those orders
//! itself fails connector directory review. Every tool also carries a title and the hints a
//! client reads to decide what runs without a prompt; `tests/mcp_tool_annotations.rs` fails a tool
//! that lacks them. Signatures extend and never rename: a client pinned to an older argument list
//! keeps working, and a renamed tool is a tool the model has to be told about again.

pub mod tools;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ResultType, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

use crate::config::Config;
use crate::crypto::kek::KeyProvider;
use crate::domain::errors::DomainError;
use crate::domain::types::{Invocation, Principal, ToolCall};
use crate::ports::{Embedder, IngestRepository, OauthStore};
use crate::services::{bootstrap, forget, registry, search, write, Ctx, Repos};

pub mod capability;
pub mod extra_tools;
pub mod views;

pub const SERVER_NAME: &str = "lumberroom";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a client reads at `initialize`. Facts about the server and its tools, with no orders.
pub const SERVER_INSTRUCTIONS: &str = "Durable memory for this user, shared across the agents and \
machines they use. context_bootstrap returns what is already known: the user's preferences, the \
active project and the infrastructure registry. memory_search finds facts by meaning, and \
registry_get looks up an exact operational value. memory_write records one durable fact per call: \
a decision, preference, constraint or convention, with its numbers and identifiers. Which tools \
appear depends on what the client's credential grants.";

/// Every tool the server registers, as `tools/list` would show it to a credential that holds every
/// grant. Exists so a test can read the annotations without a database or a request.
pub fn tool_definitions() -> Vec<rmcp::model::Tool> {
    (Lumberroom::tool_router() + Lumberroom::extra_tool_router()).list_all()
}

/// Registered only for a principal whose grant carries `may_delete`, so it does not appear in
/// `tools/list` for anyone else. Named here because both the filter and the guard read it.
pub const FORGET_TOOL: &str = "memory_forget";

/// Shared, request-independent state.
pub struct AppState {
    pub cfg: Arc<Config>,
    /// The ports, not the Postgres struct: everything below this line is written against traits.
    pub repos: Repos,
    /// Held beside `repos` because the authorization server's router needs the store directly and
    /// `Repos` carries only what a service uses.
    pub oauth: Arc<dyn OauthStore>,
    /// The proposal queue. Beside `repos` for the same reason `oauth` is: ingestion is an operator
    /// surface with no tool behind it, so the tool path would carry a field it never reads.
    pub ingest: Arc<dyn IngestRepository>,
    /// The cleanup queue, beside `repos` for the reason `ingest` is: a periodic pass and the queue
    /// it fills are operator surfaces with no tool behind them.
    pub cleanup: Arc<dyn crate::ports::CleanupRepository>,
    /// Names that denote the same subject. Beside `repos` for the reason `ingest` is: search
    /// reaches it through a service rather than through this field.
    ///
    /// `alias_set` and `alias_list` are tools now, behind `registryWrite` and ordinary read. An
    /// alias is a naming fact of the same class as a registry key, and a model that notices a
    /// rename and cannot record it has noticed nothing anyone can use.
    pub aliases: Arc<dyn crate::ports::AliasRepository>,
    pub embedder: Arc<dyn Embedder>,
    pub degraded_embedder: bool,
    /// `None` when KEK_PROVIDER=none. A write at `private` is then refused rather than stored in
    /// plaintext.
    pub keys: Option<Arc<dyn KeyProvider>>,
    /// Set by the composition root's boot check against `kek_state`. False means private writes stay
    /// refused, which is why `/readyz` reports it: a server that silently refuses every private
    /// write looks healthy otherwise.
    pub kek_verified: bool,
    /// Proposal producers wired for `review_queue`. The engine ships none; a downstream server
    /// fills this to add a `proposal:` source.
    pub proposals: Vec<Arc<dyn crate::services::review_queue::ProposalSource>>,
}

#[derive(Clone)]
pub struct Lumberroom {
    state: Arc<AppState>,
    tool_router: ToolRouter<Lumberroom>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BootstrapArgs {
    /// Absolute path or slug of the project you are working in, so its memory is promoted.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    // No occurred_before or occurred_after. A range asks the caller to invent two instants where
    // `as_of` asks for one it was given, and two guesses compound.
    /// What you want to know, in natural language. Full sentences retrieve better than keywords.
    pub query: String,
    /// The namespaces to search. When absent, the search covers the user namespace, global and the
    /// project's namespace when project is set, as primary hits, then every other namespace the
    /// credential can read at a lower rank unless the operator turned that wider reach off. A
    /// non-empty list searches only the listed namespaces and their aliases, so it narrows the
    /// search. Namespaces the credential cannot read are left out either way.
    #[serde(default)]
    pub namespaces: Option<Vec<String>>,
    /// Maximum rows. Default 8.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Slug or path of the current project. Its namespace joins the user namespace and global in
    /// the default set, so its facts rank as primary hits. When the operator turned off the wider
    /// reach, a project not named here or in namespaces is not searched at all. It has no effect
    /// when namespaces is set.
    #[serde(default)]
    pub project: Option<String>,
    /// true includes facts that a later correction replaced, which answers "what did we believe
    /// before". Off by default, because a superseded fact read as current is worse than a missing
    /// one. It needs a credential that may read history, and it cannot be set beside as_of.
    #[serde(default)]
    pub include_superseded: Option<bool>,
    /// What the store held at this instant, as a date, `2026-03-01`, read as midnight UTC, or a
    /// full RFC 3339 instant. The filter drops every fact that started after the instant, so an
    /// instant earlier than a fact's start hides that fact and the search answers "nothing is
    /// known" about something the store holds. An instant the person named reflects what they
    /// asked; one worked out from the question is a guess. A wrong guess can hide a fact that holds
    /// now or return one a later correction retired, and leaving as_of absent risks neither. When
    /// absent, the search answers as of now, which is what almost every question wants. It needs a credential that may read history, and it cannot be set
    /// beside include_superseded.
    #[serde(default)]
    pub as_of: Option<String>,
    /// Keeps only facts carrying every one of these tags. A fact filed without one of them is
    /// dropped, so a tag the fact was never filed under makes the search report nothing known about
    /// something the store holds. Absent or empty, no tag filter applies.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// The durable fact, self-contained, so it still makes sense in six months with no surrounding
    /// conversation. A fact that names its subject and keeps its numbers, identifiers, paths, dates
    /// and the cause or reversal condition it turns on stays usable; the trail of how it came to be
    /// believed adds nothing a later reader needs.
    pub content: String,
    /// 'user:me' for facts about the person, 'project:<slug>' for one codebase, 'global' for facts
    /// true everywhere, 'personal:<slug>' such as 'personal:finance' for a private area of life.
    /// 'credentials:<slug>' is not writable here: those hold client-encrypted items.
    pub namespace: String,
    /// Short lowercase labels used for filtering later.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// id of a memory this one replaces, when correcting a fact that changed.
    #[serde(default)]
    pub supersedes: Option<String>,
    /// 'open' or 'private'. Raises the level above the namespace default and can never lower it,
    /// so passing 'open' for a namespace that classifies private changes nothing.
    #[serde(default)]
    pub sensitivity: Option<String>,
    /// When this fact became true in the world. Two forms are accepted: a date, `2026-03-01`, read
    /// as midnight UTC, or a full RFC 3339 instant, `2026-03-01T09:30:00Z`. A bare month or year
    /// has no form here, so "since March" leaves the argument absent rather than carrying a day
    /// nobody stated. The argument holds a time that was stated rather than worked out: by the
    /// user, as in "we moved to Postgres 16 on 4 June 2026", or by the fact itself naming the day
    /// an event happened, as in "the regulator approved it on 19 August 2026". A date inferred from
    /// context is a guess the store then reports as fact. A stated time left out is lost: the row
    /// then reads as true since the store heard it. By default a date within the last day is
    /// refused unless the fact's text states it, because the store already records when it heard
    /// the fact, so today's date alone fails the write. A future date is refused.
    pub occurred_at: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegistryArgs {
    /// 'host', 'service', 'credential-ref', 'model-route' or 'dataset'.
    pub kind: String,
    /// Exact key. This lookup does not guess or fuzzy-match.
    pub key: String,
    /// Where to look. When absent, the lookup checks the project's namespace, then the user
    /// namespace, then global, and the first match answers.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Slug or path of the current project. When namespace is absent, its namespace is checked
    /// first, so a project override beats a global default.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForgetArgs {
    /// id of the memory to delete, as returned by memory_search or memory_write.
    pub id: String,
    /// Why it is being deleted, recorded with the deletion. Required: a delete with no reason is
    /// indistinguishable from a mistake a month later.
    pub reason: String,
    /// true lists what would go and deletes nothing, so the person can see the exact memory before
    /// a delete removes it. A delete on an instruction that did not name this exact memory can
    /// remove the wrong one, and nothing restores it.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[tool_router]
impl Lumberroom {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state, tool_router: Self::tool_router() + Self::extra_tool_router() }
    }

    #[tool(
        name = "context_bootstrap",
        title = "Load session context",
        description = "Returns what is already known about this user: standing preferences, the \
active project's memory and the infrastructure registry, in one call. It suits the start of a \
session, or the moment before asking a question an earlier session may have answered. project, a \
path or slug, promotes that project's memory.",
        annotations(
            title = "Load session context",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn context_bootstrap(
        &self,
        Parameters(args): Parameters<BootstrapArgs>,
        rc: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.run("context_bootstrap", None, &rc, |ctx| async move {
            let digest = bootstrap::run(&ctx, args.project.as_deref()).await?;
            let text = digest.text.clone();
            Ok((text, serde_json::to_value(&digest).unwrap_or_default()))
        })
        .await
    }

    #[tool(
        name = "memory_search",
        title = "Search memory",
        description = "Semantic search over durable memory. It applies when a task depends on a \
past decision, a preference, a host, a credential location, or how something is usually done. \
Queries retrieve best as full sentences. Superseded facts are excluded, so each hit is what is \
believed now; include_superseded and as_of change that. Each hit's source is the name of the app \
that wrote it. tags keeps only facts carrying all of them.",
        annotations(title = "Search memory", read_only_hint = true, open_world_hint = false)
    )]
    async fn memory_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
        rc: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.run("memory_search", None, &rc, |ctx| async move {
            let as_of = match args.as_of.as_deref() {
                Some(raw) => Some(tools::parse_as_of(raw)?),
                None => None,
            };
            let result = search::run_tagged(
                &ctx,
                &args.query,
                args.namespaces,
                args.limit,
                args.project.as_deref(),
                args.include_superseded,
                // Reversed on 25 August 2026, and the reversal is narrow. What this surface refused
                // was a model turning a question into a date; what it takes now is an instant the
                // person named, which the description says in the words the model reads. The store
                // records no difference between the two, so this argument is the caller's assertion
                // and the wording is the only thing holding the line. `services::search` gates it on
                // `may_read_history` before the statement runs, as it always did.
                as_of,
                // All of them, normalised the way a write stores them. Absent and empty both mean
                // no filter, which is the search this tool ran before the argument existed.
                &args.tags.unwrap_or_default(),
            )
            .await?;
            let json = serde_json::to_value(&result).unwrap_or_default();
            Ok((serde_json::to_string_pretty(&json).unwrap_or_default(), json))
        })
        .await
    }

    // The `possible_conflicts` sentence is what makes supersession happen at all. The store cannot
    // decide whether a near-identical older fact was replaced or merely restated, and the model in
    // the conversation is the only party that knows; without this line the candidates come back and
    // nothing acts on them, and the store accumulates two versions of one fact (Phase 4 §1).
    #[tool(
        name = "memory_write",
        title = "Save a memory",
        description = "Records one durable fact: a decision, a stated preference, a constraint, a \
host or service detail, a convention. Content that names its subject and keeps the numbers, \
identifiers, paths, dates and the cause or reversal condition stays usable in six months, when a \
reader sees only the row. Each call stores one row; transient chatter or a file's contents stored \
here come back in every later search. The response can carry possible_conflicts, older facts close \
to the new one, and the new row is already stored either way. When one of them states the old \
version of the fact just written, a second call with the same content and supersedes set to that \
memory's id retires it in favour of the row the first call stored, which leaves one live row for \
the fact. The old row stays readable in memory_history. \
possible_conflicts can also list a different fact that only sounds similar.",
        annotations(
            title = "Save a memory",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn memory_write(
        &self,
        Parameters(args): Parameters<WriteArgs>,
        rc: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let namespace = Some(args.namespace.clone());
        self.run("memory_write", namespace, &rc, |ctx| async move {
            // Parsed here rather than before `self.run` so a malformed date travels the same
            // path as every other validation refusal and is recorded as a failed tool call.
            let occurred_at = match args.occurred_at.as_deref() {
                Some(raw) => Some(tools::parse_occurred_at(raw)?),
                None => None,
            };
            let result = write::run(
                &ctx,
                &args.content,
                &args.namespace,
                args.tags,
                args.supersedes.as_deref(),
                args.sensitivity.as_deref(),
                occurred_at,
            )
            .await?;
            let json = serde_json::to_value(&result).unwrap_or_default();
            Ok((serde_json::to_string_pretty(&json).unwrap_or_default(), json))
        })
        .await
    }

    #[tool(
        name = "registry_get",
        title = "Look up a registry value",
        description = "Exact lookup of a known operational value: a host, a service endpoint, \
where a credential lives, a model route, a dataset. It does not guess or fuzzy-match, which suits a \
value where a wrong address would break something. Returns found:false when nothing is recorded. \
source is the name of the app that wrote the value.",
        annotations(
            title = "Look up a registry value",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn registry_get(
        &self,
        Parameters(args): Parameters<RegistryArgs>,
        rc: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let namespace = args.namespace.clone();
        self.run("registry_get", namespace, &rc, |ctx| async move {
            let result = registry::get(
                &ctx,
                &args.kind,
                &args.key,
                args.namespace.as_deref(),
                args.project.as_deref(),
            )
            .await?;
            let view = views::registry_get(&ctx, result).await;
            let json = serde_json::to_value(&view).unwrap_or_default();
            Ok((serde_json::to_string_pretty(&json).unwrap_or_default(), json))
        })
        .await
    }

    #[tool(
        name = "memory_forget",
        title = "Delete a memory",
        description = "Deletes one memory permanently, by id. The delete cannot be undone and \
leaves no copy: for a private memory the key that opens it goes with the row. For a fact that \
changed, memory_write with supersedes keeps the history instead. reason is recorded with the \
deletion. dry_run true lists what would go and deletes nothing. The tool appears only for a \
credential granted mayDelete.",
        annotations(
            title = "Delete a memory",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_forget(
        &self,
        Parameters(args): Parameters<ForgetArgs>,
        rc: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.run(FORGET_TOOL, None, &rc, |ctx| async move {
            let result =
                forget::by_id(&ctx, &args.id, Some(&args.reason), args.dry_run.unwrap_or(false))
                    .await?;
            let view = views::forget(&ctx, result).await;
            let text = view.text.clone();
            Ok((text, serde_json::to_value(&view).unwrap_or_default()))
        })
        .await
    }
}

impl Lumberroom {
    /// One place that resolves the caller, times the call, records it, and turns a domain error
    /// into a tool error rather than a transport failure.
    async fn run<F, Fut>(
        &self,
        tool: &'static str,
        namespace: Option<String>,
        rc: &RequestContext<RoleServer>,
        f: F,
    ) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(Ctx) -> Fut,
        Fut: std::future::Future<
            Output = crate::domain::errors::Result<(String, serde_json::Value)>,
        >,
    {
        let started = std::time::Instant::now();

        // Fail closed: a request that reached a tool without an authenticated principal is a bug
        // in the middleware, and guessing an identity here would be the wrong recovery.
        let principal = match request_principal(rc) {
            Some(p) => p,
            None => {
                return Ok(tool_error(
                    tool,
                    &DomainError::forbidden(
                        "request reached a tool without an authenticated client",
                    ),
                ))
            }
        };
        let parts = rc.extensions.get::<axum::http::request::Parts>();
        let invocation = parts
            .and_then(|p| p.extensions.get::<Invocation>())
            .copied()
            .unwrap_or(Invocation::Model);
        let session_id =
            parts.and_then(|p| p.extensions.get::<SessionId>()).and_then(|s| s.0.clone());

        let ctx = Ctx {
            cfg: Arc::clone(&self.state.cfg),
            repos: self.state.repos.clone(),
            embedder: Arc::clone(&self.state.embedder),
            keys: self.state.keys.clone(),
            kek_verified: self.state.kek_verified,
            principal: principal.clone(),
            invocation,
            session_id: session_id.clone(),
        };

        let outcome = f(ctx).await;
        let latency_ms = started.elapsed().as_millis() as i32;

        self.state.repos.tool_calls.record(ToolCall {
            client: principal.client.clone(),
            tool: tool.to_string(),
            succeeded: outcome.is_ok(),
            unprompted: invocation.is_unprompted(),
            latency_ms,
            session_id,
            namespace,
        });

        match outcome {
            Ok((text, structured)) => {
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                // Models read the text; the CLI and any dashboard read the structured payload.
                result.structured_content = Some(structured);
                Ok(result)
            }
            Err(e) => {
                tracing::warn!(tool, client = %principal.client, error = %e.log_message(), "tool failed");
                Ok(tool_error(tool, &e))
            }
        }
    }
}

/// Per-client session correlation, put on the request by the HTTP layer.
///
/// A newtype rather than a bare `Option<String>` in the extensions map, because extensions are keyed
/// by type and a second `Option<String>` inserted anywhere would silently overwrite this one.
#[derive(Clone, Debug, Default)]
pub struct SessionId(pub Option<String>);

/// rmcp injects the whole `http::request::Parts` into the request context, so anything an axum
/// middleware inserted lives one level in rather than on the context directly.
fn request_principal(rc: &RequestContext<RoleServer>) -> Option<Principal> {
    rc.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|p| p.extensions.get::<Principal>())
        .cloned()
}

fn tool_error(tool: &str, e: &DomainError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{tool} failed: {}",
        e.client_message()
    ))])
}

// `router = self.tool_router` is load-bearing. Bare `#[tool_handler]` dispatches call_tool
// and get_tool through a freshly built `Self::tool_router()`, which carries only the first block,
// so the capability-gated tools would appear in tools/list and answer "tool not found" on every
// call. Reading the field also stops dispatch rebuilding a router per request.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Lumberroom {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    /// The tool list is per client, because `memory_forget` is per grant.
    ///
    /// A model that can silently delete memories is a worse failure than one that hoards them, so
    /// deletion is off unless the owner granted it, and a tool a client may not call should not be
    /// in the list it reads. The service checks the grant again on the call: this filter shapes what
    /// a model tries, and `forget::by_id` is what refuses it.
    ///
    /// `#[tool_handler]` skips generating a method this block already defines, so `call_tool` and
    /// `get_tool` still come from the macro and stay in step with the router.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        rc: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Every tool the caller's grant permits, from the one table `docs/permissions.md` is
        // written against. Four capabilities now rather than one, which is why the cache hint
        // below matters more than it did: a list that varies by four flags handed to the wrong
        // client through a shared proxy leaks more than a delete tool.
        let principal = request_principal(&rc);
        let tools = self
            .tool_router
            .list_all()
            .into_iter()
            .filter(|t| principal.as_ref().is_some_and(|p| capability::permits(p, &t.name)))
            .collect();

        // Cache hints landed in the 2026-07-28 revision. This list depends on the credential, so it
        // is Private with a zero TTL: a public cache entry would hand one client's tool list, delete
        // tool included, to the next client through the same proxy.
        let hints =
            rc.protocol_version().is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: hints.then_some(0),
            cache_scope: hints.then_some(CacheScope::Private),
        })
    }
}
