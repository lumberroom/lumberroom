# 28. No embedding fallback

**Date:** 10 October 2026 · **Status:** accepted, implemented · **Decided by:** the owner

## Decision

The server never embeds with a model other than the one configured. A model that fails to load stops
the boot. A failed embed fails the request. `EMBED_ALLOW_FALLBACK`, `AppState.degraded_embedder`, the
`embedder_degraded` key and 503 clause in `/readyz`, the console's fallback badge, and the private-write
and restore guards against the hash sketch are removed. Setting `EMBED_ALLOW_FALLBACK` to anything but
false stops the boot with a message that names the removal.

## Context

A vector is comparable only with vectors from the same model. A fallback embedder writes rows that no
existing row can be compared with: search cannot rank them against the store, dedupe cannot see their
neighbours, and nothing marks which rows they are. The engine's own comment on `warm_embedder` already
called mixed embedders a recall loss, and the default refused to start for that reason. The setting
existed for operators who preferred a degraded server to a stopped one.

The embedding migration (`feat/embeddinggemma-2`) makes the cost worse. It stores a model id beside
each slot and treats a slot as holding one named model. A hash sketch stamped into that slot breaks the
claim the migration's sweep, flip and retire logic rest on.

The owner ruled on 10 October 2026: no embedding fallback of any kind.

## What lost

**Keep the hash fallback and warn.** The server would stay up, `/readyz` would answer 503, and the
console would show a badge. It lost because the damage lands at write time, not at read time. Every
write during the window stores a vector that stays wrong after the model returns, and the warning
cannot undo it. The two guards added for the window (refuse private writes, refuse restore) show the
shape: each closes one door while the rest stay open.

## What it costs, accepted

- A model that fails to load stops the boot. A box with a corrupt model cache or an unreachable
  embedder endpoint serves nothing until an operator fixes it. Compose restarts the container, so the
  failure shows as a restart loop in `docker compose ps` and the cause in `docker compose logs server`.
- `/readyz` loses `embedder_degraded`. `scripts/deploy-check.sh` and any client that read the key must
  stop reading it.
- An `.env` that sets `EMBED_ALLOW_FALLBACK=true` no longer boots. `false`, `0`, empty and unset pass.

## What this is not for

**It does not remove `EMBED_PROVIDER=hash`.** That setting is the operator's explicit choice and the
test suite's embedder. It never runs by accident, so the vectors it writes are the ones the operator
asked for. The `.env.example` line still marks it as tests and emergencies only.

It does not stop a remote embedder from failing at request time. A failed embed fails that request and
nothing else.

## Reversal condition

Reverse this if a deployment needs the server to stay up with search degraded more than it needs the
store to stay comparable, and an owner decides that for a named tenant. The route back is a fallback
that is visible in the data: a model id on every vector, which the embedding migration adds, plus a
sweep that re-embeds rows written under the fallback. Without both, a fallback is the mismatch this
record removes.
