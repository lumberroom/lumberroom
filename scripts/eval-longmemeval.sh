#!/bin/sh
# Runs LongMemEval-S against a scratch server and a scratch database, never the owner's live
# store. That is the one property this script exists to guarantee, so read the rest with that in
# mind: every host, port and database name below is chosen to stay off 127.0.0.1:8787 and off the
# `lumberroom` database.
#
#   ./scripts/eval-longmemeval.sh --protocol session-as-document --limit 20 --out report.json
#   ./scripts/eval-longmemeval.sh --dataset /path/to/longmemeval_s_cleaned.json --resume
#
# Flags, all optional:
#   --dataset PATH   the LongMemEval-S JSON file. Default: ./longmemeval_s_cleaned.json,
#                     the name the fetch command below leaves it under.
#   --protocol NAME   session-as-document (default, comparable to agentmemory's published run)
#                     or chunked (not comparable; see docs/eval-longmemeval.md).
#   --limit N         stop after N questions, for a smoke run before the full 500.
#   --resume          skip a question whose namespace already holds rows.
#   --out PATH        where the JSON report is written.
#   --port N          the scratch server's port. Default 8788, never 8787.
#   --isolate         delete each question's haystack once it is scored, so the next question meets
#                       an empty store. This is the configuration comparable to a published run
#                       that built a fresh index per question, and it says the least about scale.
#   --corpus-wide     search without the namespace filter, so every question competes against every
#                       session in the corpus. The hardest of the three and the most realistic.
#   --keep            leave the lumberroom_eval database in place after the run. Without this flag the
#                     script drops it on exit; with it, drop it later with:
#                       docker compose exec db dropdb -U <POSTGRES_USER> lumberroom_eval
#
# LUMBERROOM_CLI, if set, is a path to an already-built lumberroom binary and the harness runs on the host
# through it, talking to the scratch server's mapped port. Unset, the harness runs inside the
# builder image (build it once: docker build -t lumberroom-builder -f Dockerfile.builder .), which is
# slower on a cold cache but needs nothing installed on the host.
#
# The dataset is not checked in. Fetch it with:
#   curl -L -o longmemeval_s_cleaned.json \
#     https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json

set -e
cd "$(dirname "$0")/.."
REPO_DIR="$PWD"
[ -f .env ] && { set -a; . ./.env; set +a; }

USAGE="usage: eval-longmemeval.sh [--dataset PATH] [--protocol session-as-document|chunked]
                            [--limit N] [--isolate] [--corpus-wide]
                            [--resume] [--out PATH] [--port N] [--keep]

Runs LongMemEval-S against a scratch server on port 8788 (default) and a scratch database
named lumberroom_eval, both torn down or dropped on exit unless --keep is given. Never touches
127.0.0.1:8787 or the lumberroom database. See the top of this file for the full flag reference."

PORT="${LUMBERROOM_EVAL_PORT:-8788}"
DATASET="${LUMBERROOM_EVAL_DATASET:-$REPO_DIR/longmemeval_s_cleaned.json}"
PROTOCOL=""
LIMIT=""
OUT=""
RESUME=0
# Isolation deletes each question's haystack once it is scored, reproducing the fresh index per
# question a published run used. It removes every distractor the rest of the corpus supplies, so it
# measures ranking rather than scale. Corpus-wide drops the namespace filter and is the hardest of
# the three.
ISOLATE=0
CORPUS_WIDE=0
# Prepending each session's date is a deviation from the published protocol, which carried none.
DATES_IN_TEXT=0
ONLY_TYPE=""
# linear adds the two arms' raw scores, which is what ships. rrf fuses their ranks.
FUSION=""
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --dataset) DATASET="$2"; shift 2 ;;
    --protocol) PROTOCOL="$2"; shift 2 ;;
    --limit) LIMIT="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --resume) RESUME=1; shift ;;
    --isolate) ISOLATE=1; shift ;;
    --corpus-wide) CORPUS_WIDE=1; shift ;;
    --dates-in-text) DATES_IN_TEXT=1; shift ;;
    --type) ONLY_TYPE="$2"; shift 2 ;;
    --fusion) FUSION="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help)
      echo "$USAGE"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      echo "$USAGE" >&2
      exit 1
      ;;
  esac
done

[ -f "$DATASET" ] || {
  echo "dataset not found at $DATASET" >&2
  echo "fetch it with:" >&2
  echo "  curl -L -o \"$DATASET\" https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json" >&2
  exit 1
}
echo "dataset: $DATASET"

[ -n "${POSTGRES_PASSWORD:-}" ] || {
  echo "POSTGRES_PASSWORD is not set. Copy .env.example to .env and fill it in first." >&2
  exit 1
}
POSTGRES_USER="${POSTGRES_USER:-lumberroom}"
NETWORK="${LUMBERROOM_DOCKER_NETWORK:-lumberroom_default}"
SERVER_NAME="${LUMBERROOM_EVAL_SERVER_NAME:-lumberroom-eval-server}"
# A function rather than a `docker compose -f ...` string in a variable: POSIX sh has no arrays,
# so a multi-word command stored as a plain string breaks the moment REPO_DIR has a space in it.
compose() {
  docker compose -f "$REPO_DIR/docker-compose.yml" "$@"
}

docker image inspect lumberroom-builder >/dev/null 2>&1 || {
  echo "the lumberroom-builder image is not built. Build it once with:" >&2
  echo "  docker build -t lumberroom-builder -f Dockerfile.builder ." >&2
  exit 1
}

# Resolve to an absolute path, creating the parent directory for an output file that does not
# exist yet. Needed because the harness may run inside a container that only sees what is
# mounted, and a relative path means something different on each side of that boundary.
abs_path() {
  d=$(dirname "$1")
  b=$(basename "$1")
  mkdir -p "$d"
  (cd "$d" && printf '%s/%s\n' "$(pwd)" "$b")
}
DATASET="$(abs_path "$DATASET")"
[ -z "$OUT" ] || OUT="$(abs_path "$OUT")"

echo "bringing up the compose database (reusing it if already running)..."
compose up -d db >/dev/null

echo "waiting for postgres..."
i=0
until compose exec -T db pg_isready -U "$POSTGRES_USER" >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    echo "postgres did not become ready within 60s" >&2
    exit 1
  fi
  sleep 1
done

echo "creating database lumberroom_eval inside the existing postgres container, if absent..."
exists=$(compose exec -T db psql -U "$POSTGRES_USER" -d postgres -tAc \
  "SELECT 1 FROM pg_database WHERE datname = 'lumberroom_eval'")
if [ "$exists" != "1" ]; then
  compose exec -T db psql -U "$POSTGRES_USER" -d postgres -c "CREATE DATABASE lumberroom_eval" >/dev/null
fi

# A fresh credential per run, generated here and never read from the owner's .env. The compact
# grant form (no read/write lists) means unrestricted at every namespace and every sensitivity
# level, which is what a client scoped to "read and write at '*'" means on this server; see the
# AUTH_TOKENS comment in .env.example for why that form reaches further than a bare `"*"` glob.
TOKEN="$(openssl rand -hex 32)"
# mayDelete is on because --isolate deletes each haystack once it is scored, which is how the
# comparable configuration reproduces a fresh index per question. The credential is generated here,
# lives for the run and dies with the database, so it is never a grant on anything the owner keeps.
AUTH_TOKENS_JSON="[{\"client\":\"eval\",\"token\":\"$TOKEN\",\"mayDelete\":true}]"

docker rm -f "$SERVER_NAME" >/dev/null 2>&1 || true

cleanup() {
  status=$?
  echo "tearing down the eval server..."
  docker rm -f "$SERVER_NAME" >/dev/null 2>&1 || true
  if [ "$KEEP" -eq 1 ]; then
    echo "left lumberroom_eval in place for inspection. Drop it with:"
    echo "  docker compose exec db dropdb -U $POSTGRES_USER lumberroom_eval"
  else
    echo "dropping database lumberroom_eval..."
    compose exec -T db psql -U "$POSTGRES_USER" -d postgres -c "DROP DATABASE IF EXISTS lumberroom_eval" >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

# Five settings on the scratch server that a reader has to know about to read the number
# honestly, each a deliberate departure from the owner's real deployment defaults:
#
#   EMBED_PROVIDER=local, EMBED_MODEL=all-MiniLM-L6-v2, EMBED_DIM=768
#     all-MiniLM-L6-v2 is the model agentmemory's published numbers were produced with. It embeds
#     at 384 dimensions and the store zero-pads that into the 768-dim column the schema already
#     has; cosine is invariant under zero padding, so this needs no second column and no second
#     database. Running any other embedder here would measure the embedder, not the search stack.
#   SENSITIVITY_TRIPWIRE=false
#     LongMemEval's synthetic sessions contain API-key-shaped and token-shaped text by design,
#     which is exactly what the tripwire exists to refuse. A refused write removes a session from
#     the haystack for a reason that has nothing to do with retrieval, which is the same failure
#     mode as a write-ceiling truncation below and just as fatal to the number.
#   WRITE_MAX_CONTENT_CHARS=200000
#     The comparable protocol writes a whole haystack session as one document, and a real session
#     rendered whole runs past the default 8000-char write ceiling. Raised well past the longest
#     session in the set rather than tuned to it, so nothing here is silently truncated either.
#   SEARCH_INCLUDE_ALL_PROJECTS=false
#     Each question's haystack lives in its own project: namespace (see question_namespace in
#     crates/lumberroom/src/eval/mod.rs). Leaving this at the server default of true would let one
#     question's search see every other question's sessions, which is not what a per-question
#     recall number is supposed to measure.
#   PUBLIC_URL=http://<container>:<port>
#     rmcp validates the Host header against an allowlist derived from this, defaulting to loopback
#     only. The harness reaches the server by container name, so without this every tool call comes
#     back 403 while /healthz and /readyz answer normally.
#
#   AUTH_MODE=token
#     The eval has no need for OAuth, and a static token keeps the run's own authorization out of
#     the variables being measured.
echo "starting the eval server on port $PORT (builds first if the target directory is cold)..."
docker run -d --name "$SERVER_NAME" --network "$NETWORK" \
  -v "$REPO_DIR:/app" \
  -v lumberroom-cargo:/usr/local/cargo/registry \
  -v lumberroom-eval-models:/models \
  -w /app \
  -p "127.0.0.1:${PORT}:${PORT}" \
  -e CARGO_TERM_COLOR=never \
  -e PORT="$PORT" \
  -e HOST=0.0.0.0 \
  -e TENANT_ID=eval \
  -e DATABASE_URL="postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@db:5432/lumberroom_eval" \
  -e PUBLIC_URL="http://${SERVER_NAME}:${PORT}" \
  -e AUTH_MODE=token \
  -e "AUTH_TOKENS=$AUTH_TOKENS_JSON" \
  -e EMBED_PROVIDER=local \
  -e EMBED_MODEL=all-MiniLM-L6-v2 \
  -e EMBED_DIM=768 \
  -e SENSITIVITY_TRIPWIRE=false \
  -e WRITE_MAX_CONTENT_CHARS=200000 \
  -e SEARCH_INCLUDE_ALL_PROJECTS=false \
  -e SEARCH_FUSION="$FUSION" \
  -e KEK_PROVIDER=none \
  -e MODEL_CACHE_DIR=/models \
  -e BUILDER_UID="$(id -u)" -e BUILDER_GID="$(id -g)" \
  -e BUILDER_OWN=/models \
  lumberroom-builder cargo run --release --bin lumberroom-server >/dev/null

echo "waiting for the eval server to become ready..."
i=0
until curl -sf "http://127.0.0.1:${PORT}/readyz" >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -ge 300 ]; then
    echo "the eval server did not become ready within 600s. Last log lines:" >&2
    docker logs --tail 80 "$SERVER_NAME" >&2 || true
    exit 1
  fi
  sleep 2
done
echo "eval server ready on port $PORT"

# Build the lumberroom argument list once; both branches below consume the same one.
set -- eval-longmemeval --dataset "$DATASET"
[ -z "$PROTOCOL" ] || set -- "$@" --protocol "$PROTOCOL"
[ -z "$LIMIT" ] || set -- "$@" --limit "$LIMIT"
[ -z "$OUT" ] || set -- "$@" --out "$OUT"
[ "$RESUME" -eq 1 ] && set -- "$@" --resume
[ "$ISOLATE" -eq 1 ] && set -- "$@" --isolate
[ "$CORPUS_WIDE" -eq 1 ] && set -- "$@" --corpus-wide
[ "$DATES_IN_TEXT" -eq 1 ] && set -- "$@" --dates-in-text
[ -z "$ONLY_TYPE" ] || set -- "$@" --type "$ONLY_TYPE"

if [ -n "${LUMBERROOM_CLI:-}" ]; then
  echo "running the harness through $LUMBERROOM_CLI..."
  LUMBERROOM_URL="http://127.0.0.1:${PORT}/mcp" LUMBERROOM_TOKEN="$TOKEN" "$LUMBERROOM_CLI" "$@"
else
  echo "running the harness inside the builder image..."
  # The dataset and, if given, the output file may sit outside the repo (the scratchpad case),
  # so their directories are mounted at their own host paths rather than assumed to fall under
  # /app. Mounting a path already inside /app twice is harmless; Docker just resolves the same
  # bind for that subtree.
  DATASET_DIR="$(dirname "$DATASET")"
  MOUNTS="-v $DATASET_DIR:$DATASET_DIR:ro"
  if [ -n "$OUT" ]; then
    OUT_DIR="$(dirname "$OUT")"
    MOUNTS="$MOUNTS -v $OUT_DIR:$OUT_DIR"
  fi
  # shellcheck disable=SC2086
  docker run --rm --network "$NETWORK" \
    -v "$REPO_DIR:/app" \
    -v lumberroom-cargo:/usr/local/cargo/registry \
    $MOUNTS \
    -w /app \
    -e CARGO_TERM_COLOR=never \
    -e LUMBERROOM_URL="http://${SERVER_NAME}:${PORT}/mcp" \
    -e LUMBERROOM_TOKEN="$TOKEN" \
    -e BUILDER_UID="$(id -u)" -e BUILDER_GID="$(id -g)" \
    lumberroom-builder cargo run --release -p lumberroom -- "$@"
fi
