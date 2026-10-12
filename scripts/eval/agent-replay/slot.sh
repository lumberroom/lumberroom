#!/usr/bin/env bash
# One gate slot: a Postgres container and an arm server, plus a text-channel proxy when asked.
#
#   slot.sh up --dir D --name A1-structured --image IMG --checkout DIR --port 8841 --client gate-A1 \
#               [--channel text --proxy-port 8941] [--settings-sql FILE]
#   slot.sh load-template D DUMP     restore a pg_dump (custom format) into the template database
#   slot.sh freeze-template D        copy the live database into the template (engine path)
#   slot.sh reset D                  fresh database from the template, settings reapplied, restart
#   slot.sh psql D SQL               run SQL against the live database
#   slot.sh down D                   remove the containers, the network and the proxy
#
# Network shape (spec G2, Slots). The database sits only on a per-slot `--internal` network, so it
# cannot reach out and nothing outside can reach it. Docker refuses to publish a port from a
# container attached only to an internal network, so the arm server is created on the default
# bridge (which carries the `-p 127.0.0.1:PORT:8787` publish), then joins the internal network
# before it starts. The slot environment holds the gate token and the database URL and no
# Razorpay, SMTP, LLM, GitHub or OpenRouter credential, so the bridge leg carries nothing worth
# stealing and the server can fetch its local embedding model on first boot.
#
# The live database is named `lumberroom`, as in production, so a settings.sql written for that
# name applies unchanged. The template database is `lrgate_template`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PG_IMAGE="${GATE_PG_IMAGE:-pgvector/pgvector:pg16}"
PG_USER=lumberroom
LIVE_DB=lumberroom
TEMPLATE_DB=lrgate_template

die() { echo "slot.sh: $*" >&2; exit 1; }
need_dir() { [[ -f "$1/slot.json" ]] || die "$1 holds no slot.json; run 'up' first"; }
jget() { python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1/slot.json" "$2"; }

pg() { # pg CONTAINER DBNAME [psql args...]
  local c="$1" d="$2"; shift 2
  docker exec -i "$c" psql -U "$PG_USER" -d "$d" -v ON_ERROR_STOP=1 "$@"
}

wait_pg() {
  for _ in $(seq 1 60); do
    docker exec "$1" pg_isready -U "$PG_USER" -d postgres >/dev/null 2>&1 && return 0
    sleep 1
  done
  die "postgres in $1 did not come up"
}

wait_app() { # wait_app PORT
  for _ in $(seq 1 120); do
    curl -fsS "http://127.0.0.1:$1/healthz" >/dev/null 2>&1 && return 0
    sleep 1
  done
  die "the arm server on port $1 did not answer /healthz"
}

cmd_up() {
  local dir="" name="" image="" checkout="" port="" client="" channel="structured" proxy_port="" settings=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dir) dir="$2";; --name) name="$2";; --image) image="$2";; --checkout) checkout="$2";;
      --port) port="$2";; --client) client="$2";; --channel) channel="$2";;
      --proxy-port) proxy_port="$2";; --settings-sql) settings="$2";;
      *) die "unknown option $1";;
    esac
    shift 2
  done
  [[ -n $dir && -n $name && -n $image && -n $checkout && -n $port && -n $client ]] || die "up needs --dir --name --image --checkout --port --client"
  [[ $channel == structured || $channel == text ]] || die "--channel is structured or text"
  [[ $channel == structured || -n $proxy_port ]] || die "the text channel needs --proxy-port"
  [[ -d $checkout/client ]] || die "--checkout $checkout holds no client/ directory"
  mkdir -p "$dir"; chmod 700 "$dir"

  local net="lrgate-$name" db="lrgate-$name-db" app="lrgate-$name-app"
  local token pgpass
  token="$(openssl rand -hex 32)"
  pgpass="$(openssl rand -hex 16)"

  docker network inspect "$net" >/dev/null 2>&1 || docker network create --internal "$net" >/dev/null

  docker run -d --name "$db" --network "$net" --network-alias db --shm-size 1gb \
    -e POSTGRES_USER="$PG_USER" -e POSTGRES_PASSWORD="$pgpass" -e POSTGRES_DB=postgres \
    "$PG_IMAGE" postgres -c shared_buffers=256MB -c max_connections=50 >/dev/null
  wait_pg "$db"

  # 0600 before the secrets land in it.
  ( umask 077
    cat > "$dir/slot.env" <<ENV
DATABASE_URL=postgres://$PG_USER:$pgpass@db:5432/$LIVE_DB
AUTH_MODE=token
AUTH_TOKENS=[{"client":"$client","token":"$token","read":[{"namespace":"*","max":"open"}],"write":[{"namespace":"*","max":"open"}],"registryWrite":true,"mayDelete":true,"mayReadHistory":true}]
PUBLIC_URL=http://127.0.0.1:$port
PORT=8787
SEARCH_USAGE_WEIGHT=0
ENV
  )

  # Created on the bridge so the publish is legal, joined to the internal network, then started.
  docker create --name "$app" --network bridge -p "127.0.0.1:$port:8787" --env-file "$dir/slot.env" "$image" >/dev/null
  docker network connect --alias app "$net" "$app"
  # An empty live database lets the server boot and run its migrations. The engine template path
  # builds on that; the fork path replaces it with `load-template` then `reset`.
  pg "$db" postgres -c "CREATE DATABASE $LIVE_DB"

  local mcp_port="$port"
  if [[ $channel == text ]]; then mcp_port="$proxy_port"; fi
  ( umask 077
    cat > "$dir/mcp.json" <<JSON
{"mcpServers":{"lumberroom":{"type":"http","url":"http://127.0.0.1:$mcp_port/mcp","headers":{"Authorization":"Bearer $token"}}}}
JSON
  )
  python3 -I - "$dir" "$name" "$channel" "$port" "$client" "$db" "$app" "$checkout" "$settings" <<'PY'
import json, sys
d, name, channel, port, client, db, app, checkout, settings = sys.argv[1:]
json.dump({"name": name, "arm": name.rsplit("-", 1)[0], "channel": channel, "port": int(port),
           "client": client, "db_container": db, "app_container": app, "db_user": "lumberroom",
           "db_name": "lumberroom", "checkout": checkout, "settings_sql": settings,
           "mcp_config": f"{d}/mcp.json", "server": "lumberroom"},
          open(f"{d}/slot.json", "w"), indent=1)
PY
  if [[ $channel == text ]]; then
    nohup python3 -I "$SCRIPT_DIR/strip_proxy.py" --listen "127.0.0.1:$proxy_port" \
      --upstream "http://127.0.0.1:$port" >"$dir/proxy.log" 2>&1 &
    echo $! > "$dir/proxy.pid"
  fi
  docker start "$app" >/dev/null
  wait_app "$port"
  echo "slot $name is up on 127.0.0.1:$port"
}

cmd_load_template() {
  local dir="$1" dump="$2"; need_dir "$dir"
  [[ -f $dump ]] || die "no dump at $dump"
  local db; db="$(jget "$dir" db_container)"
  pg "$db" postgres -c "DROP DATABASE IF EXISTS $TEMPLATE_DB"
  pg "$db" postgres -c "CREATE DATABASE $TEMPLATE_DB"
  pg "$db" "$TEMPLATE_DB" -c "CREATE EXTENSION IF NOT EXISTS vector"
  docker cp "$dump" "$db:/tmp/corpus.dump"
  docker exec "$db" pg_restore -U "$PG_USER" -d "$TEMPLATE_DB" --no-owner --no-privileges --exit-on-error /tmp/corpus.dump
  docker exec "$db" rm -f /tmp/corpus.dump
  echo "template restored from $dump"
}

cmd_freeze_template() {
  local dir="$1"; need_dir "$dir"
  local db app; db="$(jget "$dir" db_container)"; app="$(jget "$dir" app_container)"
  docker stop "$app" >/dev/null
  pg "$db" postgres -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$LIVE_DB' AND pid <> pg_backend_pid()" >/dev/null
  pg "$db" postgres -c "DROP DATABASE IF EXISTS $TEMPLATE_DB"
  pg "$db" postgres -c "CREATE DATABASE $TEMPLATE_DB TEMPLATE $LIVE_DB"
  docker start "$app" >/dev/null
  wait_app "$(jget "$dir" port)"
  echo "template frozen from the live database"
}

cmd_reset() {
  local dir="$1"; need_dir "$dir"
  local db app port settings
  db="$(jget "$dir" db_container)"; app="$(jget "$dir" app_container)"
  port="$(jget "$dir" port)"; settings="$(jget "$dir" settings_sql)"
  docker stop "$app" >/dev/null
  pg "$db" postgres -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$LIVE_DB' AND pid <> pg_backend_pid()" >/dev/null
  # Two separate -c strings: Postgres rejects DROP DATABASE inside a transaction block.
  pg "$db" postgres -c "DROP DATABASE IF EXISTS $LIVE_DB"
  pg "$db" postgres -c "CREATE DATABASE $LIVE_DB TEMPLATE $TEMPLATE_DB"
  # pg_dump carries no ALTER DATABASE ... SET, and a clone loses them. Without the hnsw
  # settings a filtered search returns zero rows.
  if [[ -n $settings ]]; then pg "$db" postgres < "$settings"; fi
  docker start "$app" >/dev/null
  wait_app "$port"
}

cmd_psql() { local dir="$1"; need_dir "$dir"; pg "$(jget "$dir" db_container)" "$LIVE_DB" -At -c "$2"; }

cmd_down() {
  local dir="$1"; need_dir "$dir"
  local name; name="$(jget "$dir" name)"
  if [[ -f $dir/proxy.pid ]]; then kill "$(cat "$dir/proxy.pid")" 2>/dev/null || true; rm -f "$dir/proxy.pid"; fi
  docker rm -f -v "lrgate-$name-app" "lrgate-$name-db" >/dev/null 2>&1 || true
  docker network rm "lrgate-$name" >/dev/null 2>&1 || true
  echo "slot $name is down"
}

sub="${1:-}"; [[ -n $sub ]] || die "usage: slot.sh up|load-template|freeze-template|reset|psql|down ..."
shift
case "$sub" in
  up) cmd_up "$@";;
  load-template) cmd_load_template "$@";;
  freeze-template) cmd_freeze_template "$@";;
  reset) cmd_reset "$@";;
  psql) cmd_psql "$@";;
  down) cmd_down "$@";;
  *) die "unknown subcommand $sub";;
esac
