#!/bin/sh
# Runs LongMemEval-S against a scratch server and a scratch database, never the owner's live
# store. That is the one property this script exists to guarantee, so read the rest with that in
# mind: every host, port and database name below is chosen to stay off 127.0.0.1:8787 and off the
# `lumberroom` database.
#
#   ./scripts/eval-longmemeval.sh --protocol session-as-document --limit 20 --out report.json
#   ./scripts/eval-longmemeval.sh --dataset /path/to/longmemeval_s_cleaned.json --resume
#
# A fusion sweep writes each corpus once and reruns only the searches for each blend:
#   ./scripts/eval-longmemeval.sh --db lme_gemma_scoped --keep --out gemma-linear035.json
#   ./scripts/eval-longmemeval.sh --db lme_gemma_scoped --search-only --lexical-weight 0.2 \
#     --out gemma-linear020.json
#
# Flags, all optional:
#   --dataset PATH   the LongMemEval-S JSON file. Default: ./longmemeval_s_cleaned.json,
#                     the name the fetch command below leaves it under.
#   --protocol NAME   session-as-document (default, comparable to agentmemory's published run)
#                     or chunked (not comparable; see docs/eval-longmemeval.md).
#   --limit N         stop after N questions, for a smoke run before the full 500.
#   --resume          skip a question whose namespace already holds rows.
#   --search-only     write nothing. Search the corpus an earlier --keep run left in --db, under
#                       whatever blend this run's flags set. The database must already exist; it is
#                       never dropped on exit, whatever --keep says. Scoped mode searches the
#                       per-question namespaces that run wrote, so pair it with the same --limit,
#                       --type and --protocol, and never with --isolate, which deleted them.
#                       Before the server starts, every row's access_count and last_accessed_at go
#                       back to 0 and NULL: each search raises them and the use boost reads them, so
#                       without the reset each blend would rank against the reads of the one before.
#                       The writing run searched rows that started at 0, so the reset matches it.
#                       Sessions the store cannot trace by tag or exact text still count in
#                       sessions_never_stored.
#   --fusion NAME     the server's SEARCH_FUSION: linear (default), linear_minmax or rrf.
#   --lexical-weight W  the server's SEARCH_LEXICAL_WEIGHT. Unset leaves the default, 0.35.
#   --rrf-k K         the server's SEARCH_RRF_K. Unset leaves the default, 60.
#   --out PATH        where the JSON report is written.
#   --port N          the scratch server's port. Default 8788, never 8787.
#   --isolate         delete each question's haystack once it is scored, so the next question meets
#                       an empty store. This is the configuration comparable to a published run
#                       that built a fresh index per question, and it says the least about scale.
#   --corpus-wide     write every unique session once into one namespace before the first search,
#                       then search all of it for every question. The hardest of the three and the
#                       most realistic. --resume does not apply.
#   --embed-model NAME  the server's EMBED_MODEL. Default all-MiniLM-L6-v2, the embedder agentmemory's
#                       published run used. Any other model measures the embedder as well as the stack.
#   --embed-max-tokens N  the server's EMBED_MAX_TOKENS, the embedder's input window. Unset leaves
#                       the model's own default.
#   --db NAME         the scratch database. Default lumberroom_eval. Two runs at once need two names,
#                       two --port values and two LUMBERROOM_EVAL_SERVER_NAME values.
#   --keep            leave the scratch database in place after the run. Without this flag the
#                     script drops it on exit; with it, drop it later with:
#                       docker compose exec db dropdb -U <POSTGRES_USER> <NAME>
#
# Every run sets SEARCH_DEBUG_SCORES=true on the scratch server, so each hit carries its cosine,
# keyword score and fused score. That changes the select list and never the order. With --out, the
# harness writes them, one JSON line per question, beside the report: report.json gets
# report.hits.jsonl.
#
# LUMBERROOM_EVAL_DB_CONTAINER names an already-running Postgres container to use instead of the
# compose `db` service. Set, the script never runs `docker compose up`, and every pg_isready, psql,
# create and drop goes through `docker exec -i <container>`. LUMBERROOM_EVAL_DB_HOST is then the
# host the scratch server reaches it at, the container's name on LUMBERROOM_DOCKER_NETWORK
# (default `db`, the compose service). The container takes POSTGRES_USER and POSTGRES_PASSWORD
# from .env, as the compose one does.
#
# LUMBERROOM_EVAL_EMBED_PROVIDER=openai with LUMBERROOM_EVAL_EMBED_BASE_URL, _API_KEY and
# _MAX_INPUT_CHARS points the scratch server at an OpenAI-compatible embeddings endpoint. They are
# not the server's own EMBED_* names because this script sources .env, which sets those.
#
# LUMBERROOM_TIMEOUT_MS reaches the harness either way. The client's default is 15 s per call, which
# a search over a large pool can outrun.
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
                            [--resume] [--out PATH] [--port N] [--keep] [--db NAME]
                            [--search-only] [--fusion linear|linear_minmax|rrf]
                            [--lexical-weight W] [--rrf-k K]
                            [--embed-model NAME] [--embed-max-tokens N]

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
# linear adds the two arms' raw scores, which is what ships. rrf fuses their ranks. linear_minmax
# rescales each query's candidate cosines to 0..1 before the linear sum.
FUSION=""
# Read from flags alone, never from .env, which this script sources: a weight left in the owner's
# .env would otherwise move every run without appearing on its command line.
LEXICAL_WEIGHT=""
RRF_K=""
SEARCH_ONLY=0
EMBED_MODEL="all-MiniLM-L6-v2"
EMBED_MAX_TOKENS=""
KEEP=0
EVAL_DB="lumberroom_eval"

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
    --lexical-weight) LEXICAL_WEIGHT="$2"; shift 2 ;;
    --rrf-k) RRF_K="$2"; shift 2 ;;
    --search-only) SEARCH_ONLY=1; shift ;;
    --port) PORT="$2"; shift 2 ;;
    --embed-model) EMBED_MODEL="$2"; shift 2 ;;
    --embed-max-tokens) EMBED_MAX_TOKENS="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --db) EVAL_DB="$2"; shift 2 ;;
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
DB_CONTAINER="${LUMBERROOM_EVAL_DB_CONTAINER:-}"
DB_HOST="${LUMBERROOM_EVAL_DB_HOST:-db}"

if [ "$SEARCH_ONLY" -eq 1 ]; then
  [ "$ISOLATE" -eq 0 ] || {
    echo "--search-only cannot run with --isolate: isolation deletes the corpus it searches" >&2
    exit 1
  }
  # The corpus is the input here, so dropping it on exit would cost the next blend a full write.
  KEEP=1
fi
SERVER_NAME="${LUMBERROOM_EVAL_SERVER_NAME:-lumberroom-eval-server}"
# A function rather than a `docker compose -f ...` string in a variable: POSIX sh has no arrays,
# so a multi-word command stored as a plain string breaks the moment REPO_DIR has a space in it.
compose() {
  docker compose -f "$REPO_DIR/docker-compose.yml" "$@"
}
# Every command that runs inside the Postgres container, whichever container that is.
db_exec() {
  if [ -n "$DB_CONTAINER" ]; then
    docker exec -i "$DB_CONTAINER" "$@"
  else
    compose exec -T db "$@"
  fi
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

if [ -n "$DB_CONTAINER" ]; then
  # Never started or recreated from here: the container is the caller's, configured elsewhere.
  [ "$(docker inspect -f '{{.State.Running}}' "$DB_CONTAINER" 2>/dev/null)" = "true" ] || {
    echo "LUMBERROOM_EVAL_DB_CONTAINER=$DB_CONTAINER is not a running container" >&2
    exit 1
  }
  echo "using the postgres container $DB_CONTAINER at host $DB_HOST on network $NETWORK"
else
  echo "bringing up the compose database (reusing it if already running)..."
  compose up -d db >/dev/null
fi

echo "waiting for postgres..."
i=0
until db_exec pg_isready -U "$POSTGRES_USER" >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    echo "postgres did not become ready within 60s" >&2
    exit 1
  fi
  sleep 1
done

exists=$(db_exec psql -U "$POSTGRES_USER" -d postgres -tAc \
  "SELECT 1 FROM pg_database WHERE datname = '$EVAL_DB'")
if [ "$SEARCH_ONLY" -eq 1 ]; then
  [ "$exists" = "1" ] || {
    echo "--search-only needs the corpus database $EVAL_DB, and it does not exist." >&2
    echo "write the corpus first with --db $EVAL_DB --keep" >&2
    exit 1
  }
  echo "resetting access counts in $EVAL_DB so this blend starts where the writing run did..."
  db_exec psql -U "$POSTGRES_USER" -d "$EVAL_DB" -v ON_ERROR_STOP=1 -tAc \
    "UPDATE memory SET access_count = 0, last_accessed_at = NULL
      WHERE access_count <> 0 OR last_accessed_at IS NOT NULL" >/dev/null
elif [ "$exists" != "1" ]; then
  echo "creating database $EVAL_DB inside the existing postgres container..."
  db_exec psql -U "$POSTGRES_USER" -d postgres -c "CREATE DATABASE \"$EVAL_DB\"" >/dev/null
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
    echo "left $EVAL_DB in place for inspection. Drop it with:"
    if [ -n "$DB_CONTAINER" ]; then
      echo "  docker exec $DB_CONTAINER dropdb -U $POSTGRES_USER $EVAL_DB"
    else
      echo "  docker compose exec db dropdb -U $POSTGRES_USER $EVAL_DB"
    fi
  else
    echo "dropping database $EVAL_DB..."
    db_exec psql -U "$POSTGRES_USER" -d postgres -c "DROP DATABASE IF EXISTS \"$EVAL_DB\"" >/dev/null 2>&1 || true
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
#   PUBLIC_URL=http://127.0.0.1:<port>
#     rmcp validates the Host header against an allowlist derived from this. The harness reaches the
#     server on loopback, from the host through the published port or from a container sharing the
#     server's network namespace, because the client sends its token over plain HTTP to loopback
#     alone. Reached by container name, it drops the header and every call comes back 401.
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
  -e DATABASE_URL="postgres://${POSTGRES_USER}:${POSTGRES_PASSWORD}@${DB_HOST}:5432/${EVAL_DB}" \
  -e PUBLIC_URL="http://127.0.0.1:${PORT}" \
  -e AUTH_MODE=token \
  -e "AUTH_TOKENS=$AUTH_TOKENS_JSON" \
  -e EMBED_PROVIDER="${LUMBERROOM_EVAL_EMBED_PROVIDER:-local}" \
  -e EMBED_BASE_URL="${LUMBERROOM_EVAL_EMBED_BASE_URL:-}" \
  -e EMBED_API_KEY="${LUMBERROOM_EVAL_EMBED_API_KEY:-}" \
  -e EMBED_MAX_INPUT_CHARS="${LUMBERROOM_EVAL_EMBED_MAX_INPUT_CHARS:-0}" \
  -e EMBED_MODEL="$EMBED_MODEL" \
  -e EMBED_MAX_TOKENS="$EMBED_MAX_TOKENS" \
  -e EMBED_DIM=768 \
  -e SENSITIVITY_TRIPWIRE=false \
  -e WRITE_MAX_CONTENT_CHARS=200000 \
  -e SEARCH_INCLUDE_ALL_PROJECTS=false \
  -e SEARCH_FUSION="$FUSION" \
  -e SEARCH_LEXICAL_WEIGHT="$LEXICAL_WEIGHT" \
  -e SEARCH_RRF_K="$RRF_K" \
  -e SEARCH_DEBUG_SCORES=true \
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
[ "$SEARCH_ONLY" -eq 1 ] && set -- "$@" --search-only
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
  docker run --rm --network "container:$SERVER_NAME" \
    -v "$REPO_DIR:/app" \
    -v lumberroom-cargo:/usr/local/cargo/registry \
    $MOUNTS \
    -w /app \
    -e CARGO_TERM_COLOR=never \
    -e LUMBERROOM_URL="http://127.0.0.1:${PORT}/mcp" \
    -e LUMBERROOM_TOKEN="$TOKEN" \
    -e LUMBERROOM_TIMEOUT_MS="${LUMBERROOM_TIMEOUT_MS:-}" \
    -e BUILDER_UID="$(id -u)" -e BUILDER_GID="$(id -g)" \
    lumberroom-builder cargo run --release -p lumberroom -- "$@"
fi
