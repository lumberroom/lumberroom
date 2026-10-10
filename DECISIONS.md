# Decisions

The PRD left three questions open before coding (§10) and named OAuth as the schedule risk (§9).
Here is what was decided, what it was measured against, and where the build departs from the PRD.

Phases 2, 3 and 4 reversed some of what follows. Where that happened the original text stays and
carries a marker naming the record that replaced it and the date the position changed. Editing the
history to read consistently would hide the reasoning that made the first choice look right, and
that reasoning is the only reason this file exists.

---

## 1. Embedding model: bge-base, self-hosted, q8

> **Read the numbers below as Phase 1's measurement, not as the current runtime.** They were taken
> under the Node build with Transformers.js at q8. The Rust build embeds through `fastembed`, which
> resolves `Qdrant/bge-base-en-v1.5-onnx-Q` instead of `Xenova/bge-base-en-v1.5`: same model family,
> same 768 dimensions, and not provably the same quantised weights. [docs/traps.md](docs/traps.md) carries
> that finding and what it means for a store written by both.

**Decided:** `Xenova/bge-base-en-v1.5` through Transformers.js, q8 quantized, 768 dimensions,
running in the server process. Configurable with `EMBED_PROVIDER`.

The PRD asked to confirm it runs acceptably on an Ampere A1 before committing. Measured in a
`linux/arm64` container, the same architecture as the A1:

| | load (warm cache) | one short text | batch of 3 | RSS |
|---|---|---|---|---|
| fp32 | 1375 ms | 21 to 38 ms | 82 ms | 918 MB |
| **q8** | **~600 ms** | **11 to 16 ms** | **45 ms** | **349 MB** |

q8 wins on every axis that matters on a small box. Cold start including the download was 17s,
which is why the weights are baked into the image at build time: a first-request download on a
VM with locked-down egress is a silent failure that looks like a hung tool call.

**Why not OpenAI:** one less running process, but one more external dependency, one more key to
rotate, and 100 to 300 ms of network latency on every search. The local path costs about 350 MB
of RAM on a box that has 24 GB.

**The switch is a config flip.** `EMBED_PROVIDER=openai` uses `text-embedding-3-small` with
`dimensions: 768`, so the schema does not change. Switching does require re-embedding what is
already stored; `DEPLOY.md` has the procedure. Every row records its `embedding_model`, so a
mixed store is detectable rather than mysterious.

A third provider, `hash`, is deterministic and needs no weights. It exists so the test suite runs
in three seconds without downloading anything. Its retrieval quality is word overlap only, so it
is never a production setting. It used to stand in as an emergency fallback under
`EMBED_ALLOW_FALLBACK=true`; decision 0028 removed that.

---

## 2. Auth: bearer tokens now, Logto as a first-class flip

> **Superseded in part on 19 August 2026.** lumberroom issues its own OAuth tokens.
> [Decision 0002](docs/decisions/0002-built-in-oauth-server.md) reverses "the server never issues
> tokens" and "No hand-rolled OAuth, in either mode" directly, and adds `AUTH_MODE=oauth` as a third
> mode: RFC 8414 discovery, RFC 7591 registration, PKCE with S256 only, an owner login and consent
> screen, and opaque access tokens stored as hashes.
> [Decision 0003](docs/decisions/0003-grants-in-the-database.md) reverses the last paragraph's
> "editing `AUTH_TOKENS`" in part, and only in part: an OAuth client's grant is a row in
> `oauth_client` that takes effect on the next request, while a static bearer client's grant stays
> authoritative in `AUTH_TOKENS` and no code path copies one into the other. What survives untouched
> is `oidc` mode, which validates an external issuer's JWTs and issues nothing, and the reason token
> mode shipped first. The text below is the Phase 1 record and stays as it was written.

**Decided:** ship `AUTH_MODE=token` as the deploy path and `AUTH_MODE=oidc` as a tested
alternative, with a Logto compose profile and a written switch procedure.

This is a deliberate deviation from PRD §3, which specifies Logto validating every request.
PRD §9 also names the OAuth integration as the schedule risk, and that judgement was right: a
Logto tenant needs DNS, its own certificates, an admin console pass, and a client registration
before a single memory can be written. Token mode gets the loop running tonight and rests on one
verified fact: `claude mcp add --transport http <name> <url> --header "Authorization: Bearer ..."`
is supported by the installed CLI.

What is not deviated from: the server never issues tokens. In `oidc` mode it validates JWTs
against the Logto JWKS with issuer and audience checks, serves RFC 9728 protected resource
metadata, and answers a rejected token with a `WWW-Authenticate` header pointing at that
metadata. No hand-rolled OAuth, in either mode.

Both modes resolve a token to the same `Principal`: a client identity and a pair of namespace
glob lists. PRD §8 asks for exactly that, so Phase 2 adds a per-client namespace denial by
editing `AUTH_TOKENS` rather than by rewriting the authorization path. The enforcement is live:
reads narrow silently to what a client may see, writes outside a grant fail loudly with 403, and
both are covered by tests.

**To switch:** [deploy/logto.md](deploy/logto.md).

---

## 3. Logto over Ory Hydra

> **Narrowed on 19 August 2026.** This still decides which external issuer `oidc` mode targets, and
> that part is intact. It no longer decides how the product authenticates by default:
> [decision 0002](docs/decisions/0002-built-in-oauth-server.md) puts lumberroom's own authorization server
> on the path the browser surfaces take.

**Decided:** Logto, as the PRD recommends. It ships a compose profile here. Nothing in the server
is Logto-specific: point `OIDC_ISSUER` and `OIDC_JWKS_URI` at Hydra and it validates Hydra's
tokens. `OIDC_CLIENT_CLAIM` handles the claim naming difference between authorization servers.

---

## Departures from the PRD, and why

**Search reaches beyond the default namespace set.** PRD §5 fixes the default at `user:me`,
`global`, and the active project. Those three are still the primary set and still rank first, but
other project namespaces are now scanned at a 0.85 score penalty. This came out of a real failure:
a model asked "what embedding model does this use" without passing `project`, and was told nothing
was known while the fact sat one namespace away. Silent recall failure is the worst outcome this
system can produce. `SEARCH_INCLUDE_ALL_PROJECTS=false` restores strict behaviour.

**The bootstrap digest spans every readable namespace.** Same failure, same fix. Recent writes
come from anywhere the client may read, and namespaces the digest did not print get named with
their counts so a model knows where to look. A model files a fact under the project it is talking
about, which is not always the directory it is sitting in.

**Exact duplicates collapse on write.** Agents restate the same fact across sessions. Without
this, the digest fills with the same sentence eight times. Identical content in the same namespace
returns the existing id and reports `deduplicated: true`.

> **Widened on 19 August 2026.** Phase 4 reaches past exact match. At or above `DEDUPE_THRESHOLD`
> (0.97) a write collapses into the existing row; between `CONFLICT_THRESHOLD` (0.90) and that line
> it stores and hands the caller the older row as a possible conflict. A guard on digits,
> identifiers and negations blocks a collapse where the two texts disagree about a value, and a
> collapse across two sensitivity levels is refused. Both thresholds are design targets rather than
> observations, and the Phase 4 spec's calibration procedure has not been run.

**Lexical blending.** Vector search alone misses short factual queries ("ssh port", "which
region"). Results are ranked by cosine similarity plus a capped `ts_rank`, weight 0.35. Same tool
signature, better recall.

> **Narrowed on 19 August 2026.** The lexical half now covers `sensitivity = 'open'` only. Migration
> `20260819000004_sensitivity.sql` builds the GIN index with a `WHERE` predicate, so a private row is
> reachable by meaning and not by exact phrase. The reasoning, including what the owner gives up:
> [decision 0005](docs/decisions/0005-private-drops-lexical-search.md).

**One extra column.** `memory.embedding_model` records which model produced each vector. Without
it, a provider change leaves a store that retrieves badly for no visible reason.

**Registry writes are an operator action.** PRD §5 fixes the tool surface at four, so registry
entries are written through `lumberroom registry set`, which posts to an authenticated admin endpoint
subject to the same namespace grants. Agents read the registry; they do not edit it.

> **Superseded in part on 19 August 2026.** The tool surface is five. Phase 4 added
> `memory_forget`, and it is opt-in per client: `src/mcp/mod.rs` filters it out of `tools/list` for
> any principal whose grant lacks `mayDelete`, so a client the owner has not granted deletion to
> cannot see that the tool exists. The registry half stands. Writes still go through the admin
> endpoint, and `registry_get` is still the only registry tool a model gets.

---

## Things worth knowing before you use it

**A careful model refuses to record authorization-shaped claims.** The first version of the
done-when test stated "the deploy runbook is signed off under codename X". The model declined to
write it, on the grounds that a future session could read it as authorization. That is correct
behaviour, and it means the write path works best for preferences, decisions, conventions, and
operational facts. Claims that grant permission need a human in the loop, which is a Phase 2
conversation about provenance and confidence.

**Injected memory reads as untrusted content to a careful model.** In one run, a fresh session
recovered the fact and then flagged the nickname as possibly injected. The hook preamble now
states where the digest came from and every line carries its namespace and date. Provenance is
the answer here, not louder instructions.

**Stateless transport.** No session ids, so a server restart does not kill a connected client.
Each request builds a transport and an MCP server instance, which costs a fraction of a
millisecond and removes a whole class of "reconnect after deploy" failures.

Phase 4 added an `X-Session-Id` request header so `tool_calls` can group one conversation's calls.
It is correlation, nothing authorizes on it, and the server keeps no state keyed to it, so the
paragraph above still holds.
