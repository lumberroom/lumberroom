#!/bin/sh
# Build lumberroom for every target this machine can produce today, and write the binaries, a
# gzipped tar archive of each, and a SHA256SUMS file into a directory it names. This script never
# publishes anything: no git tag, no git push, no `gh release`. That last step stays a human
# decision once the binaries exist.
#
#   ./scripts/cli-release.sh              # dry run: prints the plan, builds nothing
#   ./scripts/cli-release.sh --build      # builds every target this machine can reach
#   ./scripts/cli-release.sh --build --linux-only
#   ./scripts/cli-release.sh --build --out /path/to/dir
#
# Each target produces two artifacts: the bare binary (lumberroom-X.Y.Z-<target>, for a container
# that wants one file with nothing to untar) and an archive of the same name with .tar.gz appended.
# The archive holds the binary renamed to the plain `lumberroom` PATH name, plus LICENSE and
# README.md from the repository root, because that is the shape Homebrew's formula expects: it
# downloads and unpacks the tarball, and whatever comes out lands on PATH under that name.
# SHA256SUMS covers both the bare binaries and the archives, since a formula needs the archive's
# checksum and this file is where every checksum here already lives.
#
# Two build paths, because they need different toolchains:
#
#   linux (musl)   goes through the lumberroom-builder image, the same one ./scripts/cargo.sh uses.
#                  musl-tools and the x86_64 mingw-free cross gcc are not baked into that image
#                  today, so this script installs them inside the container on every run. The
#                  scout's run measured 188s total for the apt install plus both builds (59.1s
#                  and 68.4s), so roughly 60s of that is the repeated toolchain install, paid
#                  again each time. Baking it into Dockerfile.builder would remove the repeat
#                  cost; that file belongs to another owner, so it stays a wire-in rather than an
#                  edit here.
#   darwin         needs a host Rust toolchain. There is no cross-compiler for it in the
#                  container (no osxcross, no zig here), so this script shells out to the host's
#                  own `cargo` and `rustup`, not docker. On a machine with neither, both darwin
#                  targets are skipped with the command that would unblock them, and the script
#                  still exits 0: a partial release for the targets that ARE buildable here is the
#                  point of this script running locally at all, with CI (cli-release.yml) covering
#                  the rest.
#
# Windows is never attempted. crates/lumberroom resolves its config path from $HOME with no
# %USERPROFILE% fallback and `restrict()` no-ops outside unix, so a Windows binary today would
# write an unprotected token file. Fix config.rs first.
#
# Archiving runs on whatever tar is on this host's PATH: bsdtar on macOS, GNU tar on Linux. Both
# get --format=ustar plus zeroed owner/group and a fixed mtime on every staged file, and the
# archive is piped through `gzip -n` so the gzip header carries no timestamp or filename either;
# that makes two runs of the same commit produce identical bytes ON THE SAME TAR. It was verified
# by running bsdtar twice in a row and comparing output byte for byte. It was NOT verified across
# bsdtar and GNU tar: pax/ustar implementations differ enough between libarchive and GNU tar
# (padding, extended-header handling) that --format=ustar narrows the gap but does not close it
# for certain, and this machine has no GNU tar to check against. Treat cross-flavour byte parity
# as unverified, not achieved.
set -e
cd "$(dirname "$0")/.."

BUILD=0
OUT=""
DO_LINUX=1
DO_DARWIN=1
VERSION=""

usage() {
  cat <<'EOF'
usage: scripts/cli-release.sh [--build] [--out DIR] [--version V] [--linux-only] [--darwin-only]

  --build        actually run the builds. Without it, prints the plan and touches nothing.
  --out DIR      where binaries and SHA256SUMS land. Default: dist/cli-release-<version>.
  --version V    override the version string used in artifact names. Default: the version in
                 crates/lumberroom/Cargo.toml.
  --linux-only   skip the darwin leg (the two musl targets only).
  --darwin-only  skip the linux leg (the two darwin targets only, host toolchain required).
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --build) BUILD=1 ;;
    --out) OUT="$2"; shift ;;
    --out=*) OUT="${1#--out=}" ;;
    --version) VERSION="$2"; shift ;;
    --version=*) VERSION="${1#--version=}" ;;
    --linux-only) DO_DARWIN=0 ;;
    --darwin-only) DO_LINUX=0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unrecognised argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [ -z "$VERSION" ]; then
  VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' crates/lumberroom/Cargo.toml | head -1)
fi
[ -n "$VERSION" ] || { echo "could not read the version from crates/lumberroom/Cargo.toml" >&2; exit 2; }

[ -n "$OUT" ] || OUT="dist/cli-release-$VERSION"

LINUX_TARGETS="aarch64-unknown-linux-musl x86_64-unknown-linux-musl"
DARWIN_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"

# sha256sum on Linux (the CI runners), shasum -a 256 on macOS (this machine). Neither is
# guaranteed on every box, so fail with a clear message rather than a bare "command not found".
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$@"
  else
    echo "cli-release.sh: need sha256sum or shasum on PATH" >&2
    exit 2
  fi
}

echo "lumberroom release, version $VERSION"
echo "output directory: $OUT"
echo

plan_linux() {
  echo "linux (musl), via the lumberroom-builder image:"
  for t in $LINUX_TARGETS; do
    echo "  - $t"
  done
  if ! docker image inspect lumberroom-builder >/dev/null 2>&1; then
    echo "  lumberroom-builder image not found locally. Build it first:"
    echo "    docker build -t lumberroom-builder -f Dockerfile.builder ."
  fi
}

plan_darwin() {
  echo "darwin, via the host toolchain:"
  for t in $DARWIN_TARGETS; do
    echo "  - $t"
  done
  if ! command -v cargo >/dev/null 2>&1; then
    echo "  no host cargo found. Install rustup, then:"
    for t in $DARWIN_TARGETS; do
      echo "    rustup target add $t"
    done
  fi
}

if [ "$DO_LINUX" = 1 ]; then plan_linux; fi
if [ "$DO_DARWIN" = 1 ]; then plan_darwin; fi

if [ "$BUILD" = 0 ]; then
  echo
  echo "dry run only. Pass --build to actually build."
  exit 0
fi

mkdir -p "$OUT"
PRODUCED=""

# One packaging implementation, in scripts/package-archive.sh, because CI needs the identical bytes
# and a Homebrew formula pins a sha256 per archive. Two callers whose archives differ by a timestamp
# produce two hashes for one build.
package_archive() {
  archive="$(scripts/package-archive.sh "$1" "$2" "$VERSION" "$OUT")"
  PRODUCED="$PRODUCED $archive"
}

build_linux() {
  echo
  echo "== linux (musl) =="
  if ! docker image inspect lumberroom-builder >/dev/null 2>&1; then
    echo "skip: lumberroom-builder image not found. docker build -t lumberroom-builder -f Dockerfile.builder ." >&2
    return 0
  fi
  # One container, both targets: paying the apt-get and rustup-target-add cost once rather than
  # twice. CC_x86_64_unknown_linux_musl and the matching linker point rustc's C compile step (ring
  # needs a C compiler; see docs/decisions/0012) at a real gcc while the link itself still uses
  # Rust's self-contained musl libc, which is what let the scout's x86_64 leg produce a working
  # static-pie despite crossing arches on an arm64 host.
  #
  # The same uid scripts/cargo.sh uses, because this container writes into the shared
  # lumberroom-cargo registry. A root run here would leave root-owned crate sources in a volume
  # every later non-root build reads, and the ownership marker would not notice.
  # musl-tools and the cross gcc now come from Dockerfile.builder, so nothing in here needs root.
  # `rustup target add` writes into RUSTUP_HOME, which the rust image leaves world-writable.
  docker run --rm \
    -v "$PWD:/app" -w /app \
    -v lumberroom-cargo:/usr/local/cargo/registry \
    -e CARGO_TERM_COLOR=never \
    -e BUILDER_UID="$(id -u)" -e BUILDER_GID="$(id -g)" \
    lumberroom-builder sh -c '
      set -e
      rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl >/dev/null
      CC_aarch64_unknown_linux_musl=musl-gcc \
        cargo build --release --locked -p lumberroom --target aarch64-unknown-linux-musl
      CC_x86_64_unknown_linux_musl=x86_64-linux-gnu-gcc \
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-gnu-gcc \
        cargo build --release --locked -p lumberroom --target x86_64-unknown-linux-musl
    '
  for t in $LINUX_TARGETS; do
    src="target/$t/release/lumberroom"
    if [ -f "$src" ]; then
      dest="$OUT/lumberroom-$VERSION-$t"
      cp "$src" "$dest"
      chmod 755 "$dest"
      PRODUCED="$PRODUCED $dest"
      package_archive "$dest" "$t"
    else
      echo "warning: expected $src, not found after build" >&2
    fi
  done
}

build_darwin() {
  echo
  echo "== darwin =="
  if ! command -v cargo >/dev/null 2>&1; then
    echo "skip: no host cargo. Install rustup: https://rustup.rs" >&2
    return 0
  fi
  for t in $DARWIN_TARGETS; do
    if ! rustup target list --installed 2>/dev/null | grep -qx "$t"; then
      echo "skip: $t not installed. rustup target add $t" >&2
      continue
    fi
    cargo build --release --locked -p lumberroom --target "$t"
    src="target/$t/release/lumberroom"
    if [ -f "$src" ]; then
      dest="$OUT/lumberroom-$VERSION-$t"
      cp "$src" "$dest"
      chmod 755 "$dest"
      PRODUCED="$PRODUCED $dest"
      package_archive "$dest" "$t"
    else
      echo "warning: expected $src, not found after build" >&2
    fi
  done
}

if [ "$DO_LINUX" = 1 ]; then build_linux; fi
if [ "$DO_DARWIN" = 1 ]; then build_darwin; fi

if [ -z "$PRODUCED" ]; then
  echo
  echo "nothing built. See the skip messages above." >&2
  exit 1
fi

( cd "$OUT" && sha256 $(for p in $PRODUCED; do basename "$p"; done) > SHA256SUMS )

echo
echo "produced:"
for p in $PRODUCED; do
  size=$(wc -c < "$p" | tr -d ' ')
  echo "  $p ($size bytes)"
done
echo "  $OUT/SHA256SUMS"
echo
cat "$OUT/SHA256SUMS"
