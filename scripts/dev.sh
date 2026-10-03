#!/bin/sh
# The local development loop.
#
#   ./scripts/dev.sh            start it, follow the log
#   ./scripts/dev.sh -d         start it and detach
#   ./scripts/dev.sh --down     stop it
#
# Rust has no hot reload and cargo has no watch mode, so an edit reaches the running server by
# killing it and starting it again. watchexec does that inside the `dev` compose service; this
# script is the two setup steps that service cannot do for itself, plus the command to start it.
#
# It is deliberately thin. Everything about how the server runs (ports, volumes, environment) lives
# in docker-compose.yml next to the services it resembles, not in here.
set -e
cd "$(dirname "$0")/.."

DB="${DEV_POSTGRES_DB:-lumberroom_dev}"
MODELS_VOLUME=lumberroom-dev-models
SERVER_IMAGE="${DEV_SEED_IMAGE:-lumberroom-server:0.4.0}"

if [ "$1" = "--down" ]; then
  exec docker compose --profile dev down dev
fi

# .env is gitignored, so it does not follow a git worktree. Compose reads it from the project
# directory and there is nothing useful this script can do without it.
if [ ! -r .env ]; then
  echo "dev.sh: no readable .env here." >&2
  echo "  In a worktree:  ln -s ../../../.env .env" >&2
  exit 1
fi

# Postgres has to be up before either step below, and compose brings it up healthy.
docker compose up -d db >/dev/null

# sqlx migrates at boot but never creates the database, so the first run needs this. Its own
# database rather than the real store: a dev-loop restart should not be able to write into it.
if ! docker compose exec -T db psql -U "${POSTGRES_USER:-lumberroom}" -lqt \
  | cut -d'|' -f1 | tr -d ' ' | grep -qx "$DB"; then
  echo "dev.sh: creating database $DB"
  docker compose exec -T db psql -U "${POSTGRES_USER:-lumberroom}" -c "CREATE DATABASE $DB"
fi

# The weights, copied out of the server image rather than downloaded. 209MB either way, and one of
# them needs the network and four minutes. EMBED_PROVIDER stays `local` in the dev loop on purpose:
# a hash embedder boots faster and returns different vectors from the ones production returns, which
# is the wrong thing to be debugging against.
if ! docker volume inspect "$MODELS_VOLUME" >/dev/null 2>&1 \
  || [ -z "$(docker run --rm -v "$MODELS_VOLUME":/m alpine ls /m 2>/dev/null)" ]; then
  if ! docker image inspect "$SERVER_IMAGE" >/dev/null 2>&1; then
    echo "dev.sh: $SERVER_IMAGE is not built, so there is nothing to copy the weights from." >&2
    echo "  Build it first:  docker compose build server" >&2
    exit 1
  fi
  echo "dev.sh: seeding $MODELS_VOLUME from $SERVER_IMAGE"
  # chmod after the copy: the dev service runs as the uid that owns the target volume now rather
  # than as root, and root's copy is otherwise readable only by accident. Read is all it needs.
  docker run --rm --user root --entrypoint sh -v "$MODELS_VOLUME":/dst "$SERVER_IMAGE" \
    -c 'cp -a /models/. /dst/ && chmod -R a+rX /dst'
fi

if [ "$1" = "-d" ]; then
  exec docker compose --profile dev up -d dev
fi

exec docker compose --profile dev up dev
