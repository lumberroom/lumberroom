# Architecture

Ports and adapters, checked against the Rust tree in `src/` rather than described in the abstract.
This document predates decision
[`0001-rust-rewrite`](decisions/0001-rust-rewrite.md) in an earlier form that described a TypeScript
target shape (`app.ts`, `tools/*.ts`, `interfaces/`); that shape was retired with the rewrite and
this page now tracks the Rust service it produced.

---

## Why the shape matters

Two goals, both stated rather than inferred.

**Storage must be swappable.** Not because Postgres is wrong for this workload, but because being
locked to it by accident is a bad position to hold. The lock-in to avoid is SQL written inside
request handlers.

**The service must be maintainable.** One rule carries it: domain and services never import from
adapters. A service asks a `MemoryRepository` for rows and does not know Postgres exists, which is
what makes a second storage implementation possible.

---

## The tree

```
src/domain/     types, errors, namespace rules, the two-axis policy model, canonical registry keys,
                the credential tripwire. No I/O anywhere in here.
src/ports/      one file per port: memory, registry, alias, cleanup, embedder, ingest, oauth,
                sealed, tool_calls. The contract adapters implement and services consume.
src/services/   the use cases: bootstrap, search, write, registry, forget, review, export, recall,
                alias, cleanup, history, ingest, eval, conflicts (the sweeper that records
                conflict pairs after a row commits).
src/adapters/   postgres (the only module containing SQL; `conflict_wake` holds the LISTEN
                connection the sweeper wakes on), embedding, auth.
src/authserver/ the built-in OAuth 2.1 authorization server: routes, consent pages, session, limiter.
src/crypto/     envelope encryption and the KEK provider.
src/mcp/        tool registration and the tool descriptions.
src/http/       axum routes; the MCP transport mounts at /mcp.
src/console/    the operator web console: mod.rs (router), pages.rs (HTML), data.rs, aliases.rs,
                clients.rs, cleanup.rs.
src/bin/        prefetch.rs, the model-download step the container build runs separately.
migrations/     SQL, applied at boot by sqlx.
```

Verified against `src/` directly: this is the actual module list, not a target.

## The dependency rule

**Domain and services never import from adapters.** Checked with `grep -rn "^use crate::adapters"
src/domain` (nothing) and the same over `src/services` (every hit is one of two files). The
exceptions are deliberate and narrow:

- `adapters::auth`: pure grant arithmetic over a `Principal` (`can_read`, `can_write`,
  `assert_writable`, `filter_readable`). Every service file that touches policy imports it; there is
  no port for it because it has no I/O to abstract behind one.
- `crypto`: key material a service has to reason about to refuse a write it cannot honour.
  `services/mod.rs` imports `crypto::envelope::SealedContent` and `crypto::kek::KeyProvider`;
  `services/ingest.rs` imports `crypto::Digester` and re-exports `crypto::digest::normalise`.

No other adapter import appears in `src/domain` or `src/services`. The rule holds as stated, not as
aspiration.

### What a port looks like

```rust
pub trait MemoryRepository: Send + Sync {
    async fn search(&self, q: SearchQuery) -> Result<Vec<SearchHit>>;
    async fn insert(&self, m: NewMemory) -> Result<Memory>;
    async fn find_by_id(&self, tenant: &str, id: Uuid) -> Result<Option<Memory>>;
    async fn digest(&self, q: DigestQuery) -> Result<DigestData>;
    async fn namespace_counts(&self, tenant: &str) -> Result<HashMap<String, i64>>;
}
```

`SearchQuery` carries namespaces, an embedding, a limit and the ranking weights. It carries no SQL
and no table name. Hybrid ranking, the blend of vector distance and lexical rank, sits behind the
port because it is expressed differently by every storage engine: the port promises ranked results,
the adapter decides how.

### What a service looks like

```rust
pub struct WriteService<M: MemoryRepository, E: Embedder> {
    memories: M,
    embedder: E,
}

impl<M: MemoryRepository, E: Embedder> WriteService<M, E> {
    pub async fn write(&self, principal: &Principal, input: WriteInput) -> Result<WriteResult> {
        // ...
    }
}
```

Constructor arguments, no container. Tests hand a service fakes; `main.rs` hands it the Postgres
adapter.

## The console

`src/console/` is the operator surface, mounted beside the MCP transport rather than folded into it.
Routes in `src/console/mod.rs::router`: login, reading, namespace, fact detail, search, write
(compose), registry, queue (approve/reject/unreject), cleanup (index/apply/reject/resolve/unreject),
clients (create/access/revoke), aliases (record/forget). That is eleven distinct screens plus their
mutating actions, up from the handful the console started with; `pages.rs` renders each as a
self-contained HTML string, and a test (`tests/console.rs`,
`every_page_is_self_contained`) asserts that shape holds. It depends on `services/` the same way the
MCP tool layer does: through the service constructors, never through `adapters::postgres` directly.

## Prod readiness

The gaps, checked against the current tree rather than assumed carried over from the pre-rewrite
plan.

**Closed since the pre-rewrite version of this document.** An error taxonomy exists:
`domain::errors::Kind` (`Validation`, `NotFound`, `Forbidden`, `Conflict`, `Unavailable`, `Internal`)
maps to an HTTP status in one place, `Kind::http_status`. A statement timeout is set:
`src/adapters/postgres/mod.rs:54` runs `SET statement_timeout = '30s'` on every pooled connection.

**Still open, checked by grep against `src/http/mod.rs` and the rest of `src/`.**

**Request correlation.** No request id is generated, carried, or logged. Two concurrent calls still
interleave in the logs with no way to separate them.

**Metrics.** `/statsz` (`src/http/mod.rs`) answers product questions about model behaviour: counts,
recent tool calls. It does not answer operational ones: error rates, latency distributions, pool
saturation, embedder health. No `/metrics` endpoint exists.

**An audit trail that can answer questions.** `tool_calls` records that a call happened and whether
it succeeded. It does not record which row was written or deleted, or why a request was refused.

**Backpressure.** No queue limit is set on the pool beyond its size, so load turns into unbounded
waiting rather than a fast failure.

Each of these is implemented-or-not as stated above; none of them is a measured runtime figure, so
there is no gate that would "verify" a gap being open beyond reading the code, which is what this
section did.

## Testing, per layer

Checked against `find tests -name '*.rs'` (ten integration files) and `grep -c` for `#[test]` and
`#[tokio::test]` inside `src/domain` and `src/services` (285 hits combined).

- **domain**: pure functions, no fixtures. Namespace grammar, grant matching, ranking arithmetic,
  the credential tripwire.
- **services**: fake repositories where a real one is not needed, real Postgres in the integration
  suite for the rest. Dedupe, supersession, grant narrowing, digest assembly live here.
- **adapters/postgres**: exercised through the integration tests, one transaction per test where the
  harness allows it. This is the layer a second storage implementation would have to satisfy.
- **interfaces** (`src/mcp`, `src/http`, `src/console`): the wire. Auth rejection, status codes, MCP
  result shapes, console page self-containment, truncation limits.

## Storage decision

Postgres 16 with pgvector; the reasoning is in [`docs/research/`](research/) and confirmed in
[`decisions/0001-rust-rewrite.md`](decisions/0001-rust-rewrite.md), which names `sqlx`'s
compile-time SQL verification as one of the three reasons for the rewrite itself. The port boundary
above is what keeps that reversible: a different engine means a second `adapters/` implementation
satisfying the same port contracts, not a rewrite of the services.

The query construction inside the Postgres adapter is an adapter-local choice; `sqlx`'s `query`/
`query_as` with `.bind()`, no macros, no query builder. It should not appear in a port signature, a
service, or a domain type. If it does, the abstraction has leaked and the portability it was meant to
buy is gone.
