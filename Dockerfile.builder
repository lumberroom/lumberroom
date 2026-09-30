# Reusable build environment. Keeps the toolchain and system libraries out of every
# throwaway container: ONNX Runtime links libstdc++, and sqlx/reqwest link OpenSSL.
FROM rust:1.98.0-slim
# rustfmt and clippy because rust-toolchain.toml lists them. An image without them sends rustup to
# the network at the start of every scripts/cargo.sh run to fetch clippy into a container that is
# about to be deleted.
RUN rustup component add rustfmt clippy
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates g++ curl xz-utils mold \
      musl-tools gcc-x86-64-linux-gnu \
 && rm -rf /var/lib/apt/lists/*

# mold, because linking is the part of `cargo test` that cannot be parallelised away here:
# scripts/cargo.sh forces `-j 1` for tests, so the test binaries link one after another. cargo.sh
# selects mold for its own runs through RUSTFLAGS, and reads the label below to refuse an image
# built before mold was in it.
#
# Measured 30 September 2026 in this image on lumberroom-cloud, whose suite is the larger one: a
# 180MB test binary linked in 19.8 to 27.5s with GNU ld, 3.2 to 5.2s with lld and 1.8 to 4.7s with
# mold, three runs each, and its full test build went from 19m 51s to 7m 10s. Nobody has timed this
# repository's own build with and without it.
#
# Not an ENV RUSTFLAGS here. That would reach every container on this image, including
# scripts/cli-release.sh's static musl builds of the shipped CLI, which should not change linker as
# a side effect of a test-speed change. lumberroom-cloud builds the same `lumberroom-builder` tag
# from its own Dockerfile.builder, so whichever checkout builds last decides what the image holds.
# Both files install the same packages and carry the same label, and the label is how cargo.sh
# tells.
LABEL lumberroom.linker=mold

# musl-tools and the x86_64 cross gcc are here for scripts/cli-release.sh, which used to apt-get
# them inside the container on every release build. The scout measured roughly 60s of the 188s run
# on that repeat. Baking them in also takes the last root-only step out of that script, which is
# what lets it run under the same non-root uid as everything else. `rustup target add` stays in the
# script, which keeps the cross targets out of an image every test run pulls.

# watchexec drives the `dev` service in docker-compose.yml: it watches the bind-mounted sources and
# restarts the debug binary when one changes. Rust has no hot reload of its own, and cargo has no
# watch mode, so a running server picks up an edit by being killed and started again.
#
# A prebuilt musl binary rather than `cargo install watchexec-cli`, which compiles a second
# dependency tree from source every time this image is rebuilt. The checksum is verified because
# this is a release artifact fetched over the network into a toolchain image. The published
# .sha256 holds a bare hash with no filename, so the two-space form sha256sum -c wants is built here.
#
# `uname -m` rather than the TARGETARCH build arg: TARGETARCH is only populated under BuildKit, and
# this image is built by hand as often as by compose. uname reports aarch64 and x86_64, which is
# already the spelling the release assets use.
ARG WATCHEXEC_VERSION=2.5.1
RUN arch="$(uname -m)" \
 && base="watchexec-${WATCHEXEC_VERSION}-${arch}-unknown-linux-musl" \
 && url="https://github.com/watchexec/watchexec/releases/download/v${WATCHEXEC_VERSION}/${base}.tar.xz" \
 && curl -fsSL -o /tmp/we.tar.xz "$url" \
 && curl -fsSL -o /tmp/we.sha256 "${url}.sha256" \
 && echo "$(cat /tmp/we.sha256)  /tmp/we.tar.xz" | sha256sum -c - \
 && tar -xJf /tmp/we.tar.xz -C /tmp \
 && install -m 0755 "/tmp/${base}/watchexec" /usr/local/bin/watchexec \
 && rm -rf /tmp/we.tar.xz /tmp/we.sha256 "/tmp/${base}" \
 && watchexec --version


# ── cargo does not run as root here ───────────────────────────────────────────────────────────────
#
# Root ignores permission bits, which makes every test that asserts a refusal from the filesystem
# pass whatever the code does. One of them had been inert for months while the gate reported green.
# scripts/lib/builder-entrypoint.sh drops to BUILDER_UID before it execs anything, and carries the
# rest of the reasoning.
#
# /home/builder is 0777 because the uid is not known until the container starts, and a throwaway
# build container with one user in it has nothing to protect there. It holds no cache: CARGO_HOME
# and RUSTUP_HOME arrive world-writable from the rust image and stay where they are.
RUN mkdir -p /home/builder && chmod 0777 /home/builder
# Set here as well as in the entrypoint, so `docker run --user N` gets a writable HOME too. That
# path skips the entrypoint's drop and would otherwise inherit HOME=/root, which is 0700.
ENV HOME=/home/builder
COPY scripts/lib/builder-entrypoint.sh /usr/local/bin/builder-entrypoint
RUN chmod 0755 /usr/local/bin/builder-entrypoint

WORKDIR /app
ENTRYPOINT ["/usr/local/bin/builder-entrypoint"]
