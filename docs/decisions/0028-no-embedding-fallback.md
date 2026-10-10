# 28. No embedding fallback

**Date:** 10 October 2026 · **Status:** accepted, implemented · **Decided by:** the owner

## Decision

The server never embeds with a model other than the one configured. A local model that fails to load
stops the boot. A failed embed fails the request. `EMBED_ALLOW_FALLBACK`, `AppState.degraded_embedder`, the
`embedder_degraded` key and 503 clause in `/readyz`, the console's fallback badge, and the private-write
and restore guards against the hash sketch are removed. Setting `EMBED_ALLOW_FALLBACK` to anything but
false stops the boot with a message that names the removal.

## Context

A vector is comparable only with vectors from the same model. A fallback embedder writes rows that no
existing row can be compared with: search cannot rank them against the store and dedupe cannot see
their neighbours. Rows are marked, not repaired. `memory.embedding_model` has carried the embedder id
since the first migration, and the write and archive paths stamp it, so fallback rows read
`hash-v1-768`, and `DEPLOY.md` has the query that finds them. Nothing re-embeds them. Until
something does, search ranks them beside real vectors.

The engine's own comment on `warm_embedder` already called mixed embedders a recall loss, and the
default refused to start for that reason. The setting existed for operators who preferred a degraded
server to a stopped one.

The owner ruled on 10 October 2026: no embedding fallback of any kind.

## What lost

**Keep the hash fallback and warn.** The server would stay up, `/readyz` would answer 503, and the
console would show a badge. It lost because the damage lands at write time, not at read time. Every
write during the window stores a vector that stays wrong after the model returns, and the warning
cannot undo it. The two guards added for the window (refuse private writes, refuse restore) show the
shape: each closes one door while the rest stay open.

## What it costs, accepted

- With `EMBED_PROVIDER=local`, a model that fails to load stops the boot. A box with a corrupt model
  cache serves nothing until an operator fixes it. Compose restarts the container, so the failure
  shows as a restart loop in `docker compose ps` and the cause in `docker compose logs server`.
- With `EMBED_PROVIDER=openai`, the boot builds an HTTP client and makes no call, so a down endpoint
  does not stop it. Each write and search that needs a vector fails with an error, and `/readyz`
  checks only the database, so it stays green. An operator checks a remote embedder with a
  `memory_search` or a `curl` to the endpoint's `/embeddings` route. A boot probe was rejected: a
  sidecar embedder that starts after the server would put compose into a restart loop.
- `/readyz` loses `embedder_degraded`. `scripts/deploy-check.sh` and any client that read the key must
  stop reading it.
- An `.env` that sets `EMBED_ALLOW_FALLBACK=true` no longer boots. `false`, `0`, `no`, `off` in any
  case, empty and unset pass. Compose forwards the variable for one release so the refusal reaches
  the server.

## What this is not for

**It does not remove `EMBED_PROVIDER=hash`.** That setting is the operator's explicit choice and the
test suite's embedder. It never runs by accident, so the vectors it writes are the ones the operator
asked for. The `.env.example` line marks it as tests only. It never suits private content: its
vector is a token sketch stored in the clear.

It does not stop a remote embedder from failing at request time. A failed embed fails that request and
nothing else.

## Reversal condition

Reverse this if a deployment needs the server to stay up with search degraded more than it needs the
store to stay comparable, and an owner decides that for a named tenant. The route back is a fallback
whose rows an automatic re-embed sweep repairs: the planned server-managed migration (decision 0027,
not yet written). `embedding_model` already marks the rows. Without the sweep, a fallback is the
mismatch this record removes.
