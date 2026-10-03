# Documentation index

One line per file. Start with the top-level entry points before anything under here: `README.md`
and `CONTRIBUTING.md` at the repository root, then `docs/decisions/README.md`.

## Start here

- [`faq.md`](faq.md): the questions people actually ask, with short answers and a pointer to the
  long one.
- [`managing.md`](managing.md): running a live store. Approving a client, changing what one may
  reach, deciding the two queues, correcting a fact.
- [`traps.md`](traps.md): findings that cost real time to get, with the evidence and what to do
  instead.
- [`architecture.md`](architecture.md): the ports-and-adapters shape the service should keep, and
  where it currently falls short of that.
- [`connect-claude-code.md`](connect-claude-code.md): wiring Claude Code on a Mac to a lumberroom
  deployment: credential, MCP registration, hook, write rule, and the commands that prove the loop.
- [`connect-agents-md.md`](connect-agents-md.md): wiring Codex, Hermes and every other `AGENTS.md`
  reader: where each one reads the file, the snippet that carries the write rule, and the
  credential behind it.
- [`permissions.md`](permissions.md): writing an `AUTH_TOKENS` grant, and the asymmetry in how
  namespace globs and sensitivity ceilings interact.
- [`ingestion.md`](ingestion.md): the ingestion pipeline: implemented, not yet run against real
  transcripts.
- [`ingestion-mode-a.md`](ingestion-mode-a.md): choosing between the two ways extraction can run,
  and what has actually run as of writing.
- [`ingestion-providers.md`](ingestion-providers.md): how `lumberroom ingest extract` reaches a
  model, and which of its settings are measured rather than assumed.
- [`importing.md`](importing.md): bringing a memory out of ChatGPT or claude.ai, the two routes it
  arrives by, and the dump prompt `lumberroom import prompt` prints.
- [`cleanup-schedule.md`](cleanup-schedule.md): the two processes and no cron daemon behind the
  cleanup pass.
- [`benchmarks.md`](benchmarks.md): every retrieval number the project has, with the configuration
  that produced it and what the number does not say.
- [`eval-longmemeval.md`](eval-longmemeval.md): running LongMemEval-S against lumberroom, and what the
  score does and does not measure.
- [`rust-spike-findings.md`](rust-spike-findings.md): what the pre-rewrite spike established
  against a live Postgres on the deploy architecture, before the rewrite started.

## Decisions

Numbered records of choices that shape the build, in the shape `docs/decisions/0001` sets: the
decision, the context that forced it, what lost and why, and the reversal condition.

- [`decisions/README.md`](decisions/README.md): what a decision record is for and when to write
  one.
- [`decisions/0001-rust-rewrite.md`](decisions/0001-rust-rewrite.md): rewriting the service in
  Rust.
- [`decisions/0002-built-in-oauth-server.md`](decisions/0002-built-in-oauth-server.md): building
  the OAuth 2.1 authorization server into lumberroom rather than depending on an external one.
- [`decisions/0003-grants-in-the-database.md`](decisions/0003-grants-in-the-database.md): OAuth
  client grants live in Postgres; environment clients stay in the environment.
- [`decisions/0004-kek-provider.md`](decisions/0004-kek-provider.md): the key-encryption key sits
  behind a provider, and the local providers defend less than a KMS.
- [`decisions/0005-private-drops-lexical-search.md`](decisions/0005-private-drops-lexical-search.md):
  private content drops out of lexical search.
- [`decisions/0006-console-decides-the-queue.md`](decisions/0006-console-decides-the-queue.md):
  the console decides the review queue, the same call the CLI makes.
- [`decisions/0007-longmemeval-as-the-retrieval-gate.md`](decisions/0007-longmemeval-as-the-retrieval-gate.md):
  LongMemEval-S recall as the standing retrieval gate.
- [`decisions/0008-valid-time.md`](decisions/0008-valid-time.md): a memory carries two clocks, and
  only one of them exists today.
- [`decisions/0009-aliases-are-query-expansion.md`](decisions/0009-aliases-are-query-expansion.md):
  two names for one thing is an alias, and for retrieval that means query expansion.
- [`decisions/0010-registry-history.md`](decisions/0010-registry-history.md): the registry keeps
  what it replaces.
- [`decisions/0011-cleanup-proposes.md`](decisions/0011-cleanup-proposes.md): the cleanup pass
  proposes, and the model only ever sees open rows.
- [`decisions/0012-cli-distribution.md`](decisions/0012-cli-distribution.md): four raw binaries off
  a git tag, built from two places.
- [`decisions/0013-delete-splices-the-chain.md`](decisions/0013-delete-splices-the-chain.md): a
  delete splices the supersession chain, and revives a predecessor only under the caller's grant.

## Product requirements

- [`prd/system-prd.md`](prd/system-prd.md): the system PRD, all phases.
- [`prd/phase-1-prd.md`](prd/phase-1-prd.md): the Phase 1 PRD: walking skeleton, single-tenant,
  tier 0.

## Specs

- [`specs/phase-2-surfaces.md`](specs/phase-2-surfaces.md): every surface connected: a fact told
  to one client shows up in another the next day.
- [`specs/phase-3-policy-encryption.md`](specs/phase-3-policy-encryption.md): permissions and
  encryption: one client provably cannot see what another can.
- [`specs/phase-4-quality.md`](specs/phase-4-quality.md): quality: a correction made once does not
  resurface as a contradiction later.
- [`specs/phase-6-ingestion.md`](specs/phase-6-ingestion.md): ingestion: one week of transcripts
  produces a proposal list the owner can read.
- [`specs/phase-7-valid-time.md`](specs/phase-7-valid-time.md): the implementation plan for
  decision 0008.

## Research

Commissioned to answer one question before a phase, rather than assumed.

- [`research/prior-art.md`](research/prior-art.md): the unresearched risk the system PRD named
  before Phase 2: how lumberroom compares to Supermemory and similar systems.
- [`research/data-layer.md`](research/data-layer.md): ORM, query builder, or neither, evaluated
  against the five query shapes this service actually needs.
- [`research/pgvector-at-scale.md`](research/pgvector-at-scale.md): pgvector in production, past
  the point where a laptop benchmark on random vectors stops telling the truth.
- [`research/encryption-and-sensitivity.md`](research/encryption-and-sensitivity.md): what each
  sensitivity level actually protects, commissioned for Phase 3.
- [`research/client-capabilities.md`](research/client-capabilities.md): what each client surface
  can actually do, feeding the Phase 2 spec.
- [`research/recall-monitor.md`](research/recall-monitor.md): what the recall monitor measures,
  written after its first re-run since its exact arm was fixed.

## Results

- [`results/longmemeval-scoped-20260820.json`](results/longmemeval-scoped-20260820.json): raw
  LongMemEval-S scores, scoped mode, 20 August 2026: 500 questions, recall and NDCG at several
  depths.

## Design

The memory bank console's design record: an information architecture pass, a rejection, the spec
that answered it, and the visual direction that shipped.

- [`design/README.md`](design/README.md): how the three documents fit together and what shipped.
- [`design/console-ia.md`](design/console-ia.md): information architecture and flows, the first
  pass, written before Phase 3 landed.
- [`design/console-spec.md`](design/console-spec.md): the console specification, revised against
  five reviews and feeding five competing visual directions.
- [`design/console-visual.md`](design/console-visual.md): visual and interaction direction: the
  design serves the task, no marketing surface exists.
- [`design/console-preview.html`](design/console-preview.html): a rendered preview of the chosen
  direction.
- [`design/style-palette.html`](design/style-palette.html): style proposal: command-first.
- [`design/style-map.html`](design/style-map.html): style proposal: policy map.
- [`design/style-notebook.html`](design/style-notebook.html): style proposal: the notebook.
- [`design/style-triage.html`](design/style-triage.html): style proposal: triage inbox.
- [`design/style-operations.html`](design/style-operations.html): style proposal: operations
  console.
