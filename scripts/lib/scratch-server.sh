#!/usr/bin/env bash
# Sourced by a gate script, never executed. Stands up a throwaway lumberroom server against its own
# database so a gate that writes nonce-tagged rows cannot write them into the owner's store.
#
# scripts/correction-test.sh ran against 127.0.0.1:8787 on 21 August 2026 and left four rows in
# user:me. Two of them survived as live memories and reached the next session's digest as facts
# about the owner. The port and the database name are checked here rather than trusted, because
# both are one flag away from being the real ones.
#
# The caller sets these before calling scratch_start:
#
#   SCRATCH_DB       database name. Refused when it equals POSTGRES_DB.
#   SCRATCH_PORT     host port. Refused when it is 8787.
#   SCRATCH_NAME     container name.
#   SCRATCH_TOKENS   the AUTH_TOKENS JSON the gate needs.
#   SCRATCH_EMBED    hash | local. Default hash. A gate whose assertions turn on similarity
#                    between two texts needs local; one that only moves rows around does not.
#   SCRATCH_KEK      unset for KEK_PROVIDER=none, or "env" to generate a key and hold it in the
#                    container's environment. A gate that writes a private or sealed row needs it.
#
# After scratch_start returns:
#
#   SCRATCH_URL           reachable from this host
#   SCRATCH_INTERNAL_URL  reachable from a container on the same docker network
#
# scratch_stop removes the container and drops the database. Call it from the gate's own trap.

scratch_require() {
  command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; return 1; }
  command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; return 1; }
  command -v openssl >/dev/null 2>&1 || { echo "openssl is required" >&2; return 1; }

  [ -n "${SCRATCH_DB:-}" ] || { echo "scratch-server: SCRATCH_DB is unset" >&2; return 1; }
  [ -n "${SCRATCH_PORT:-}" ] || { echo "scratch-server: SCRATCH_PORT is unset" >&2; return 1; }
  [ -n "${SCRATCH_NAME:-}" ] || { echo "scratch-server: SCRATCH_NAME is unset" >&2; return 1; }
  [ -n "${SCRATCH_TOKENS:-}" ] || { echo "scratch-server: SCRATCH_TOKENS is unset" >&2; return 1; }

  # The two values a mistake costs the owner his own store.
  [ "$SCRATCH_PORT" = "8787" ] && {
    echo "8787 is the owner's live server. Pick another port." >&2; return 1; }
  [ "$SCRATCH_DB" = "${POSTGRES_DB:-lumberroom}" ] && {
    echo "the scratch database is the owner's database ($SCRATCH_DB). Refusing." >&2; return 1; }

  [ -n "${POSTGRES_PASSWORD:-}" ] || {
    echo "POSTGRES_PASSWORD is not set. Copy .env.example to .env and fill it in first." >&2
    return 1
  }
  docker image inspect lumberroom-server:0.4.0 >/dev/null 2>&1 || {
    echo "the lumberroom-server:0.4.0 image is not built. Build it once with:  docker compose build server" >&2
    return 1
  }
  return 0
}

scratch_compose() { docker compose -f "$SCRATCH_REPO_DIR/docker-compose.yml" "$@"; }

scratch_start() {
  SCRATCH_REPO_DIR="${SCRATCH_REPO_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
  SCRATCH_NETWORK="${LUMBERROOM_DOCKER_NETWORK:-lumberroom_default}"
  SCRATCH_PG_USER="${POSTGRES_USER:-lumberroom}"
  scratch_require || return 1

  echo "bringing up the compose database (reusing it if already running)..."
  scratch_compose up -d db >/dev/null
  local i=0
  until scratch_compose exec -T db pg_isready -U "$SCRATCH_PG_USER" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -ge 60 ] && { echo "postgres did not become ready within 60s" >&2; return 1; }
    sleep 1
  done

  # Dropped and recreated. A gate that inherits the rows of the run before it is asserting against
  # yesterday's state, which is the failure mode this whole file exists to prevent.
  scratch_compose exec -T -e PGOPTIONS="-c client_min_messages=warning" db \
    psql -U "$SCRATCH_PG_USER" -d postgres -c "DROP DATABASE IF EXISTS $SCRATCH_DB" >/dev/null
  scratch_compose exec -T db \
    psql -U "$SCRATCH_PG_USER" -d postgres -c "CREATE DATABASE $SCRATCH_DB" >/dev/null

  local kek_args=(-e KEK_PROVIDER=none)
  if [ "${SCRATCH_KEK:-}" = "env" ]; then
    # 64 hex characters, which is what crypto::kek::decode_key accepts. Generated per run and never
    # written to disk: the store it protects is dropped at the end of the same run.
    kek_args=(-e KEK_PROVIDER=env -e LUMBERROOM_KEK="$(openssl rand -hex 32)" -e KEK_ID=scratch-1)
  fi

  # Named rather than left empty. macOS ships bash 3.2, where "${arr[@]}" on an empty array under
  # set -u is an unbound variable, so an empty array here is a crash at the docker run.
  local embed_args=(-e EMBED_PROVIDER=hash -e EMBED_DIM=768)
  if [ "${SCRATCH_EMBED:-hash}" = "local" ]; then
    # The image carries the weights, so this stays offline. It costs boot time, and a gate asks for
    # it only when an assertion turns on two texts being alike, which hashing cannot answer.
    embed_args=(-e EMBED_PROVIDER=local)
  fi

  docker rm -f "$SCRATCH_NAME" >/dev/null 2>&1 || true
  echo "starting the scratch server on port $SCRATCH_PORT against database $SCRATCH_DB..."
  # PUBLIC_URL names the container because rmcp validates Host against an allowlist derived from
  # it, and a client reaching this server by container name gets a 403 that reads as a connection
  # failure otherwise.
  docker run -d --name "$SCRATCH_NAME" --network "$SCRATCH_NETWORK" \
    -p "127.0.0.1:${SCRATCH_PORT}:${SCRATCH_PORT}" \
    -e PORT="$SCRATCH_PORT" \
    -e HOST=0.0.0.0 \
    -e TENANT_ID=scratch \
    -e DATABASE_URL="postgres://${SCRATCH_PG_USER}:${POSTGRES_PASSWORD}@db:5432/${SCRATCH_DB}" \
    -e PUBLIC_URL="http://${SCRATCH_NAME}:${SCRATCH_PORT}" \
    -e AUTH_MODE=token \
    -e AUTH_TOKENS="$SCRATCH_TOKENS" \
    "${embed_args[@]}" \
    "${kek_args[@]}" \
    lumberroom-server:0.4.0 >/dev/null

  i=0
  until curl -sf "http://127.0.0.1:${SCRATCH_PORT}/readyz" >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 90 ]; then
      echo "the scratch server did not become ready within 180s. Last log lines:" >&2
      docker logs --tail 40 "$SCRATCH_NAME" >&2 || true
      return 1
    fi
    sleep 2
  done

  SCRATCH_URL="http://127.0.0.1:${SCRATCH_PORT}"
  SCRATCH_INTERNAL_URL="http://${SCRATCH_NAME}:${SCRATCH_PORT}"
  export SCRATCH_URL SCRATCH_INTERNAL_URL
}

scratch_stop() {
  docker rm -f "${SCRATCH_NAME:-}" >/dev/null 2>&1 || true
  if [ "${SCRATCH_KEEP:-0}" -eq 1 ]; then
    echo ""
    echo "  left database $SCRATCH_DB in place. Drop it with:"
    echo "    docker compose exec db dropdb -U ${SCRATCH_PG_USER:-lumberroom} $SCRATCH_DB"
  else
    scratch_compose exec -T db psql -U "${SCRATCH_PG_USER:-lumberroom}" -d postgres \
      -c "DROP DATABASE IF EXISTS ${SCRATCH_DB:-}" >/dev/null 2>&1 || true
  fi
}
