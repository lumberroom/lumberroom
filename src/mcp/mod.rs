//! MCP surface: the four tools from PRD §5, plus five behind a capability each.
//!
//! `src/mcp/capability.rs` holds which grant opens which tool, and `list_tools` filters on it so a
//! client never sees a tool it cannot call. The filter shapes what a model tries; every service
//! checks the grant again on the call, which is what refuses a client that names a tool anyway.
//!
//! Descriptions are written as instructions rather than blurbs. They are the only lever that makes
//! a model read memory before working and write memory after learning something (PRD §6.4), and on
//! every surface except OpenWebUI they are the only lever at all. Signatures extend and never
//! rename: a client pinned to an older argument list keeps working, and a renamed tool is a tool the
//! model has to be told about again.

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
    /// Restrict the search to exactly these namespaces. Omit it unless you have a reason.
    #[serde(default)]
    pub namespaces: Option<Vec<String>>,
    /// Maximum rows. Default 8.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Slug or path of the project you are in. Pass it whenever you know it.
    #[serde(default)]
    pub project: Option<String>,
    /// Include facts that a later correction replaced. Off by default, because a superseded fact
    /// read as current is worse than a missing one. Pass true only to answer "what did we believe
    /// before".
    #[serde(default)]
    pub include_superseded: Option<bool>,
    /// What the store held at this instant, as a date, `2026-03-01`, read as midnight UTC, or a
    /// full RFC 3339 instant. Pass it only when the person named a time. Working one out from the
    /// question is a guess, and a guess here is worse than no argument at all: the filter drops
    /// every fact that started after the instant you chose, so a date that is too early answers
    /// "nothing is known" about facts the store holds. Omit it and the search answers as of now,
    /// which is what almost every question wants.
    #[serde(default)]
    pub as_of: Option<String>,
    /// Keep only facts carrying every one of these tags. Omit it unless the person asked for a tag
    /// or you know the tag the fact was filed under: a fact filed without the tag is dropped, and
    /// the search then reports nothing known about something the store holds.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// The durable fact, self-contained, still making sense in six months with no surrounding
    /// conversation. Name the subject. Keep the numbers, identifiers, paths and dates, and the
    /// cause or reversal condition the fact turns on; drop the trail of how you came to believe it.
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
    /// has no form here, so "since March" is omitted rather than turned into a day you chose. Set
    /// it whenever the time is stated rather than worked out: by the user, as in "we moved to
    /// Postgres 16 on 4 June 2026", or by the fact itself naming the day an event happened, as in
    /// "the regulator approved it on 19 August 2026". Never infer a date from context, and never
    /// pass today's date because today is when you heard it: the store already records that
    /// separately.
    pub occurred_at: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegistryArgs {
    /// 'host', 'service', 'credential-ref', 'model-route' or 'dataset'.
    pub kind: String,
    /// Exact key. This lookup does not guess or fuzzy-match.
    pub key: String,
    /// Where to look. Omit to check the project, then the user namespace, then global.
    #[serde(default)]
    pub namespace: Option<String>,
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
    /// List what would go without deleting anything. Use it first whenever the user's instruction
    /// was not explicit about this exact memory.
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
        description = "Run this once at the start of a session, before any substantive work, and \
before asking the user a question they may have already answered in an earlier session. It returns \
everything already known about this user, their standing preferences, the active project, and the \
infrastructure registry. One call, one round trip, typically under 200ms. If you skip it you will \
re-ask questions that were answered weeks ago."
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
        description = "Search durable memory for what is already known before answering from \
assumption or asking the user. Use it whenever a task depends on a past decision, a preference, a \
host, a credential location, or \"how do we usually do this\". Semantic, so ask in full sentences. \
Superseded facts are excluded: every hit is what is believed now. Each hit's source is the name \
of the app that wrote it. Pass tags to keep only facts carrying all of them."
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
        description = "Record a durable fact the moment it appears: a decision, a stated \
preference, a constraint, a host or service detail, a convention. Call this without asking \
permission and without announcing it. Write one fact per call, phrased so it stands alone in six \
months, carrying the numbers, identifiers, paths and dates the fact needs, and the cause, scope \
qualifier and reversal condition whenever the fact turns on them. Leave out the trail of how you \
came to believe it: the search you ran, the file you read on the way, the argument for the claim. \
No hedges, no evaluative words, no restated context, no inventory of what you did not change. \
Long is right when the fact is long, a list or a timeline; long prose about a short fact is not. \
When a second fact turns up in the same exchange, write it in a second call. Do not \
record transient chatter, file contents, or anything you would not want repeated back next month. \
If the response comes back with possible_conflicts, read them: when one of them states the OLD \
version of the fact you just wrote, call memory_write again with the same content and supersedes \
set to that memory's id, which retires it. When it is a different fact that merely sounds \
similar, leave it alone."
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
        description = "Exact lookup of a known operational value: a host, a service endpoint, \
where a credential lives, a model route, a dataset. Use this instead of guessing an address or \
asking the user to repeat it. Returns found:false when nothing is recorded, so then ask and write \
the answer with memory_write. source is the name of the app that wrote the value."
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
        description = "Delete one memory permanently, by id. Only for a fact the user has told you \
to remove, or one they have just contradicted and asked you to drop. This cannot be undone and \
there is no copy: for a private memory the key that opens it goes with the row. Prefer memory_write \
with supersedes for a fact that CHANGED, which keeps the history and is almost always what is \
wanted. Call it with dry_run true first unless the user named this exact memory."
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Durable memory for this user, shared across every agent and machine they use. Call \
             context_bootstrap before substantive work. Call memory_write whenever an exchange \
             establishes a decision, preference, or durable fact, silently, without asking. Write \
             the fact with its numbers and identifiers, one fact per call, and leave out the case \
             for it.",
        )
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
