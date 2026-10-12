#!/usr/bin/env bash
# One gate slot: a Postgres container and an arm server, plus a text-channel proxy when asked.
#
#   slot.sh up --dir D --name A1-structured --image IMG --checkout DIR --port 8841 --client gate-A1 \
#               [--channel text --proxy-port 8941] [--settings-sql FILE] [--extra-env FILE] \
#               [--archive-client NAME]
#   slot.sh load-template D DUMP [NAME]  restore a pg_dump (custom format) into a template database
#   slot.sh freeze-template D [NAME]     copy the live database into a template
#   slot.sh dump D OUT                   pg_dump the live database (custom format) to OUT on the host
#   slot.sh wipe D                       empty live database, server restarted so it migrates afresh
#   slot.sh reset D [NAME]               fresh database from a template, settings reapplied, restart
#   slot.sh psql D SQL                   run SQL against the live database
#   slot.sh down D                       remove the containers, the network and the proxy
#
# NAME picks one of several templates in a slot, such as `review-1`; without it the default template
# serves. Each review task runs on a template holding only its own fixture rows. `dump` and
# `load-template` carry a template built once to every slot, so every slot holds the same bytes and
# the same fixture ids. A dump holds corpus content: write it under LR_GATE_ROOT, never into a checkout.
#
# Network shape. The database sits only on a per-slot `--internal` network, so it
# cannot reach out and nothing outside can reach it. Docker refuses to publish a port from a
# container attached only to an internal network, so the arm server is created on the default
# bridge (which carries the `-p 127.0.0.1:PORT:8787` publish), then joins the internal network
# before it starts. The slot environment holds the gate token and the database URL and no
# billing, SMTP, LLM, GitHub or OpenRouter credential, so the bridge leg carries nothing worth
# stealing and the server can fetch its local embedding model on first boot.
#
# `--extra-env` appends a file of KEY=VALUE lines to the slot environment, such as the TENANT_ID a
# multi-tenant build reads to bind static tokens to one tenant. `--archive-client` adds a second
# grant that reads `*` at `sealed`, because `archive export` refuses any narrower one
# (src/services/archive.rs). Its token goes to `archive.token` and never into `mcp.json`, so no
# session holds it; use it for `archive export` alone.
#
# The live database is named `lumberroom`, so a settings.sql written for that name applies
# unchanged. The default template database is `lrgate_template`, a named one `lrgate_template_<name>`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PG_IMAGE="${GATE_PG_IMAGE:-pgvector/pgvector:pg16}"
PG_USER=lumberroom
LIVE_DB=lumberroom
TEMPLATE_DB=lrgate_template

die() { echo "slot.sh: $*" >&2; exit 1; }

template_db() { # template_db [NAME]: the database name, from a name safe to splice into SQL
  local name="${1:-}"
  if [[ -z $name ]]; then echo "$TEMPLATE_DB"; return; fi
  [[ $name =~ ^[a-z0-9-]+$ ]] || die "template name $name: use lowercase letters, digits and hyphens"
  echo "${TEMPLATE_DB}_${name//-/_}"
}

stop_live() { # stop_live APP DB: stop the server and close every session on the live database
  docker stop "$1" >/dev/null
  pg "$2" postgres -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$LIVE_DB' AND pid <> pg_backend_pid()" >/dev/null
}
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
  local extra_env="" archive_client=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dir) dir="$2";; --name) name="$2";; --image) image="$2";; --checkout) checkout="$2";;
      --port) port="$2";; --client) client="$2";; --channel) channel="$2";;
      --proxy-port) proxy_port="$2";; --settings-sql) settings="$2";;
      --extra-env) extra_env="$2";; --archive-client) archive_client="$2";;
      *) die "unknown option $1";;
    esac
    shift 2
  done
  [[ -n $dir && -n $name && -n $image && -n $checkout && -n $port && -n $client ]] || die "up needs --dir --name --image --checkout --port --client"
  [[ $channel == structured || $channel == text ]] || die "--channel is structured or text"
  [[ $channel == structured || -n $proxy_port ]] || die "the text channel needs --proxy-port"
  [[ -d $checkout/client ]] || die "--checkout $checkout holds no client/ directory"
  [[ -z $extra_env || -f $extra_env ]] || die "no extra env file at $extra_env"
  [[ -z $archive_client || $archive_client != "$client" ]] || die "--archive-client must differ from --client"
  mkdir -p "$dir"; chmod 700 "$dir"

  local net="lrgate-$name" db="lrgate-$name-db" app="lrgate-$name-app"
  local token pgpass grants archive_token=""
  token="$(openssl rand -hex 32)"
  pgpass="$(openssl rand -hex 16)"
  grants="{\"client\":\"$client\",\"token\":\"$token\",\"read\":[{\"namespace\":\"*\",\"max\":\"open\"}],\"write\":[{\"namespace\":\"*\",\"max\":\"open\"}],\"registryWrite\":true,\"mayDelete\":true,\"mayReadHistory\":true}"
  if [[ -n $archive_client ]]; then
    archive_token="$(openssl rand -hex 32)"
    grants="$grants,{\"client\":\"$archive_client\",\"token\":\"$archive_token\",\"read\":[{\"namespace\":\"*\",\"max\":\"sealed\"}],\"write\":[{\"namespace\":\"*\",\"max\":\"open\"}],\"mayReadHistory\":true}"
  fi

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
AUTH_TOKENS=[$grants]
PUBLIC_URL=http://127.0.0.1:$port
PORT=8787
SEARCH_USAGE_WEIGHT=0
ENV
    if [[ -n $extra_env ]]; then cat "$extra_env" >> "$dir/slot.env"; fi
    if [[ -n $archive_token ]]; then printf '%s\n' "$archive_token" > "$dir/archive.token"; fi
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
  local db tpl; db="$(jget "$dir" db_container)"; tpl="$(template_db "${3:-}")"
  pg "$db" postgres -c "DROP DATABASE IF EXISTS $tpl"
  pg "$db" postgres -c "CREATE DATABASE $tpl"
  pg "$db" "$tpl" -c "CREATE EXTENSION IF NOT EXISTS vector"
  # A dump's row policies can name roles this engine never creates, and a restore without them
  # fails at the first CREATE POLICY. LR_GATE_RESTORE_ROLES lists them, space-separated. NOLOGIN:
  # nothing connects as them here; the slot's server connects as the slot's own user.
  local role
  for role in ${LR_GATE_RESTORE_ROLES:-}; do
    [[ $role =~ ^[a-z_][a-z0-9_]*$ ]] || die "bad role name in LR_GATE_RESTORE_ROLES: $role"
    pg "$db" postgres -c "DO \$\$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '$role') THEN CREATE ROLE $role NOLOGIN NOSUPERUSER NOBYPASSRLS; END IF; END \$\$"
  done
  docker cp "$dump" "$db:/tmp/corpus.dump"
  # A dump of a database whose public schema was recreated carries `CREATE SCHEMA public`, which
  # fails on the schema every new database already has. Skip that one TOC entry and restore
  # everything else as dumped.
  # Two steps, not a pipe: the image's sh has no pipefail, and a failed listing must stop here.
  docker exec "$db" sh -c 'pg_restore -l /tmp/corpus.dump > /tmp/corpus.toc &&
    grep -v "^[0-9]*; [0-9]* [0-9]* SCHEMA - public " /tmp/corpus.toc > /tmp/corpus.list'
  docker exec "$db" pg_restore -U "$PG_USER" -d "$tpl" --no-owner --no-privileges --exit-on-error \
    -L /tmp/corpus.list /tmp/corpus.dump
  docker exec "$db" rm -f /tmp/corpus.dump /tmp/corpus.toc /tmp/corpus.list
  echo "template $tpl restored from $dump"
}

cmd_freeze_template() {
  local dir="$1"; need_dir "$dir"
  local db app tpl; db="$(jget "$dir" db_container)"; app="$(jget "$dir" app_container)"
  tpl="$(template_db "${2:-}")"
  stop_live "$app" "$db"
  pg "$db" postgres -c "DROP DATABASE IF EXISTS $tpl"
  pg "$db" postgres -c "CREATE DATABASE $tpl TEMPLATE $LIVE_DB"
  docker start "$app" >/dev/null
  wait_app "$(jget "$dir" port)"
  echo "template $tpl frozen from the live database"
}

cmd_dump() {
  local dir="$1" out="$2"; need_dir "$dir"
  [[ -n $out ]] || die "dump needs an output path"
  local db; db="$(jget "$dir" db_container)"
  ( umask 077; docker exec "$db" pg_dump -U "$PG_USER" -d "$LIVE_DB" -Fc > "$out" )
  echo "live database dumped to $out"
}

cmd_wipe() {
  local dir="$1"; need_dir "$dir"
  local db app; db="$(jget "$dir" db_container)"; app="$(jget "$dir" app_container)"
  stop_live "$app" "$db"
  pg "$db" postgres -c "DROP DATABASE IF EXISTS $LIVE_DB"
  pg "$db" postgres -c "CREATE DATABASE $LIVE_DB"
  # The server runs its migrations on boot, which leaves the empty store a restore requires.
  docker start "$app" >/dev/null
  wait_app "$(jget "$dir" port)"
  echo "live database emptied"
}

cmd_reset() {
  local dir="$1"; need_dir "$dir"
  local db app port settings tpl
  db="$(jget "$dir" db_container)"; app="$(jget "$dir" app_container)"
  port="$(jget "$dir" port)"; settings="$(jget "$dir" settings_sql)"; tpl="$(template_db "${2:-}")"
  stop_live "$app" "$db"
  # Two separate -c strings: Postgres rejects DROP DATABASE inside a transaction block.
  pg "$db" postgres -c "DROP DATABASE IF EXISTS $LIVE_DB"
  pg "$db" postgres -c "CREATE DATABASE $LIVE_DB TEMPLATE $tpl"
  # pg_dump carries no ALTER DATABASE ... SET, and a clone loses them. Without the hnsw
  # settings a filtered search returns zero rows. Applied to the live database, so a settings.sql
  # that names its target with current_database() sets the database the server reads.
  if [[ -n $settings ]]; then pg "$db" "$LIVE_DB" < "$settings"; fi
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

sub="${1:-}"; [[ -n $sub ]] || die "usage: slot.sh up|load-template|freeze-template|dump|wipe|reset|psql|down ..."
shift
case "$sub" in
  up) cmd_up "$@";;
  load-template) cmd_load_template "$@";;
  freeze-template) cmd_freeze_template "$@";;
  dump) cmd_dump "$@";;
  wipe) cmd_wipe "$@";;
  reset) cmd_reset "$@";;
  psql) cmd_psql "$@";;
  down) cmd_down "$@";;
  *) die "unknown subcommand $sub";;
esac
