# syntax=docker/dockerfile:1
# Multi-arch: builds and runs on linux/arm64 (Oracle Ampere A1) and linux/amd64.
#
# Two build requirements the spike established the hard way (docs/rust-spike-findings.md):
#   g++      ONNX Runtime links libstdc++, and rust:slim ships gcc without it. Without this the
#            build dies at the link step with "cannot find -lstdc++".
#   network  `ort` downloads a prebuilt ONNX Runtime, and the prefetch downloads model weights.

FROM rust:1-slim AS base
WORKDIR /build
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates g++ \
 && rm -rf /var/lib/apt/lists/*

# ── the client, built apart from the server ──────────────────────────────────────────────────────
#
# crates/lumberroom has no path dependency on the root crate, which is what makes this split real: the
# stage never copies src/, so editing the server cannot invalidate it. Before this, `--workspace`
# rebuilt both binaries every time a console file changed, and the client is not what changed.
#
# The stub src/ exists because the workspace root is itself a package and cargo will not read the
# manifest without one. It carries none of the trap the old stub did: no real source is ever copied
# into this stage, so there is nothing whose mtime could be compared against it.
FROM base AS cli
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN mkdir -p src/bin \
 && echo 'fn main() {}' > src/main.rs \
 && echo 'fn main() {}' > src/bin/prefetch.rs \
 && echo '' > src/lib.rs

# The build stamp. `:-unknown` covers `--build-arg LUMBERROOM_BUILD_SHA=`, which passes an empty
# string through and would otherwise stamp a binary with "".
#
# A changed sha re-runs this RUN, which weakens the cache split the comment above describes. Leave
# it: the sources under the cache mount have not changed, so cargo relinks at most and the stage
# costs seconds. What it buys is a label naming the commit on the client image too, which is the
# image the cleanup daemon runs.
#
# The client binary itself carries no stamp today. build.rs belongs to the root package and
# crates/lumberroom has no build script and no path dependency on the root crate, so nothing in
# the client reads these. The environment is here so a build script added later needs no Dockerfile
# change to be stamped.
ARG LUMBERROOM_BUILD_SHA=unknown
ARG LUMBERROOM_BUILD_TAG=unknown
ARG LUMBERROOM_BUILT_AT=unknown
RUN --mount=type=cache,target=/build/target,id=lumberroom-target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,id=lumberroom-registry,sharing=locked \
    LUMBERROOM_BUILD_SHA="${LUMBERROOM_BUILD_SHA:-unknown}" \
    LUMBERROOM_BUILD_TAG="${LUMBERROOM_BUILD_TAG:-unknown}" \
    LUMBERROOM_BUILT_AT="${LUMBERROOM_BUILT_AT:-unknown}" \
    cargo build --release --locked -p lumberroom \
 && test "$(stat -c%s target/release/lumberroom)" -gt 5000000 \
 && mkdir -p /out && cp target/release/lumberroom /out/

FROM base AS builder

# Everything the build reads, in one layer. COPY of a directory is recursive by construction, so
# nothing here has to enumerate src/'s subdirectories.
#
# build.rs has to be here. Cargo detects a build script by its presence, so leaving it out gives a
# build with no script at all: the stamp still reaches `option_env!` through rustc's environment,
# and cargo never learns the environment is an input, so the next build reuses yesterday's stamp out
# of the cache mount and the binary claims a commit it was not built from.
COPY Cargo.toml Cargo.lock build.rs ./
COPY crates ./crates
COPY src ./src
COPY migrations ./migrations

# One build, with target/ and the registry carried between builds by BuildKit cache mounts.
#
# This replaces two things that used to fight each other. A stub `fn main() {}` was built first to
# cache the dependency compile in an image layer; then `COPY src` brought the real sources in with
# their host mtimes, which were older than that stub's artifacts, so cargo's mtime freshness check
# skipped the real build and the image shipped the stub: a 325KB binary and an empty /models, past
# every check in the file. `find src crates -exec touch` was the fix, and it forced a full rebuild
# of the whole workspace on every single build. The optimisation and its patch cancelled out.
#
# A cache mount is what the stub was approximating, done properly. With no stub there is no artifact
# that is newer than the sources for a spurious reason: what the cache holds corresponds to the last
# real source state, so comparing mtimes means what it is supposed to mean and the touch can go.
#
# `sharing=locked` because two concurrent builds writing one target/ corrupt it, which is the same
# reason scripts/cargo.sh serialises.
#
# **A cache mount is not part of the image.** Nothing under /build/target survives this RUN, so the
# binaries are copied to /out inside it, and the runtime stage takes them from there. Copying them
# from /build/target in a later stage finds an empty directory and fails the build, which is the one
# way to get this wrong.
#
# --workspace, because the root is a package and a bare `cargo build` at a workspace root builds
# only that one. Without it the client is never built and the size assertion passes on the binary that
# did get built.
#
# The stamp goes in as environment on the build command, and build.rs declares it as an input so a
# changed sha recompiles the crate that reads it. `/readyz` reports the three values back, which is
# how you tell a container running the image you just built from one `docker restart` brought back
# on the old one. Unset means `unknown`, and so does empty: `--build-arg LUMBERROOM_BUILD_SHA=` passes
# an empty string that the ARG default does not catch.
ARG LUMBERROOM_BUILD_SHA=unknown
ARG LUMBERROOM_BUILD_TAG=unknown
ARG LUMBERROOM_BUILT_AT=unknown
RUN --mount=type=cache,target=/build/target,id=lumberroom-server-target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,id=lumberroom-registry,sharing=locked \
    LUMBERROOM_BUILD_SHA="${LUMBERROOM_BUILD_SHA:-unknown}" \
    LUMBERROOM_BUILD_TAG="${LUMBERROOM_BUILD_TAG:-unknown}" \
    LUMBERROOM_BUILT_AT="${LUMBERROOM_BUILT_AT:-unknown}" \
    cargo build --release --locked -p lumberroom-server \
 && test "$(stat -c%s target/release/lumberroom-server)" -gt 5000000 \
 && mkdir -p /out \
 && cp target/release/lumberroom-server target/release/prefetch /out/

# Bake the weights in: a first-request download on a VM with restricted egress is a silent
# failure that looks like a hung tool call.
ARG EMBED_PROVIDER=local
ARG EMBED_MODEL=Xenova/bge-base-en-v1.5
ENV MODEL_CACHE_DIR=/models
# The download goes to a cache mount and is copied into the image from there. This layer sits after
# the build, so any server change invalidated it and re-downloaded 209MB of weights: 21s of every
# build, spent fetching bytes that had not changed. The cache makes the second build a local copy.
RUN --mount=type=cache,target=/model-cache,id=lumberroom-models,sharing=locked \
    MODEL_CACHE_DIR=/model-cache EMBED_PROVIDER=$EMBED_PROVIDER EMBED_MODEL=$EMBED_MODEL /out/prefetch \
 && mkdir -p /models && cp -a /model-cache/. /models/

# /models measures ~209MB against an expected ~110MB. An earlier finding attributed this to Docker COPY
# dereferencing the HuggingFace hub cache's blobs/+snapshots/ symlinks into duplicate copies.
# Verified against the built image and that is not what is happening: `snapshots/` is 20KB of
# intact symlinks, `blobs/` holds the real 209MB alone, no duplication. The actual cause is that
# `EmbeddingModel::BGEBaseENV15Q` in src/adapters/embedding/local.rs is already the most-quantized
# variant fastembed exposes for this model, and it resolves to Qdrant/bge-base-en-v1.5-onnx-Q's
# `model_optimized.onnx`, which is ~218MB, almost exactly 2 bytes/parameter (fp16), not the
# ~1 byte/parameter (int8) an equivalent q8 export would be. There is no COPY-layer fix for that;
# it is a model-selection question for whoever owns src/adapters/embedding. Left as-is here rather
# than guessed at. See docs/traps.md for this finding in full.

FROM debian:trixie-slim AS runtime
# libstdc++6 for ONNX Runtime, libssl3 for TLS to a JWKS endpoint, curl for the healthcheck.
# uid and gid are both pinned to 10001 (rather than letting useradd pick a group id, which on
# debian:trixie-slim lands at 999 and drifts across base-image updates) so install.sh can chown
# the KEK file to a fixed, known uid:gid on the host and have it read as owner-only once bind-
# mounted in. src/crypto/kek.rs refuses a key file readable by group or other; see the KEK note
# at the top of docker-compose.yml for why this is a bind mount rather than a Compose secret.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 libstdc++6 curl \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 lumberroom \
 && useradd --system --create-home --uid 10001 --gid 10001 lumberroom

WORKDIR /app
COPY --from=builder /out/lumberroom-server /usr/local/bin/lumberroom-server
COPY --from=builder /models /models
# The licence travels with the binary: a pulled image is a redistribution.
COPY LICENSE NOTICE THIRD-PARTY-NOTICES.md /usr/share/doc/lumberroom-server/
RUN chown -R lumberroom:lumberroom /models

# What this image was built from, readable with `docker inspect` without starting anything. The
# binary reports the same three values on /readyz, and those are the ones that answer the question
# that matters: what the running container is serving, rather than what is on disk.
ARG LUMBERROOM_BUILD_SHA=unknown
ARG LUMBERROOM_BUILD_TAG=unknown
ARG LUMBERROOM_BUILT_AT=unknown
LABEL org.opencontainers.image.title="lumberroom-server" \
      org.opencontainers.image.source="https://github.com/lumberroom/lumberroom" \
      org.opencontainers.image.revision="${LUMBERROOM_BUILD_SHA:-unknown}" \
      org.opencontainers.image.version="${LUMBERROOM_BUILD_TAG:-unknown}" \
      org.opencontainers.image.created="${LUMBERROOM_BUILT_AT:-unknown}"

USER lumberroom
ENV MODEL_CACHE_DIR=/models \
    HOST=0.0.0.0 \
    PORT=8787 \
    RUST_BACKTRACE=1
EXPOSE 8787

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT}/healthz" > /dev/null || exit 1

CMD ["lumberroom-server"]

# ── the client, as its own image ─────────────────────────────────────────────────────────────────
#
# The cleanup daemon runs `lumberroom cleanup daemon`, and that is not the server. Carrying the client
# in the server image put a binary there that the server never executes, and made one image the
# answer to two different questions.
#
# It shares nothing with the runtime stage above on purpose. The client embeds no model, links no
# ONNX Runtime and reads no key, so it needs neither the 209MB of weights nor libstdc++: a debian
# base, ca-certificates for TLS to a provider, and a 5MB binary.
#
# `docker compose --profile cleanup up -d` builds this target. Releasing the client for a person to
# run on a laptop is a different job again, and wants cross-platform artefacts from CI rather than
# an image.
FROM debian:trixie-slim AS cli-runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 lumberroom \
 && useradd --system --create-home --uid 10001 --gid 10001 lumberroom

WORKDIR /app
COPY --from=cli /out/lumberroom /usr/local/bin/lumberroom
COPY LICENSE NOTICE THIRD-PARTY-NOTICES.md /usr/share/doc/lumberroom/

# The client has no /readyz to report from, so the label is the only place its commit is written
# down. `docker inspect lumberroom:0.1.0` reads it.
ARG LUMBERROOM_BUILD_SHA=unknown
ARG LUMBERROOM_BUILD_TAG=unknown
ARG LUMBERROOM_BUILT_AT=unknown
LABEL org.opencontainers.image.title="lumberroom" \
      org.opencontainers.image.source="https://github.com/lumberroom/lumberroom" \
      org.opencontainers.image.revision="${LUMBERROOM_BUILD_SHA:-unknown}" \
      org.opencontainers.image.version="${LUMBERROOM_BUILD_TAG:-unknown}" \
      org.opencontainers.image.created="${LUMBERROOM_BUILT_AT:-unknown}"

USER lumberroom
ENV RUST_BACKTRACE=1
# No default command. Every use names one, and a daemon that starts by accident is worse than a
# container that refuses to start at all.
