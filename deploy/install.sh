#!/usr/bin/env bash
# One-command deploy for lumberroom on a fresh Linux VM (arm64 or amd64).  [PRD §9]
#
#   git clone <repo> lumberroom && cd lumberroom
#   sudo ./deploy/install.sh --domain memory.example.com --email you@example.com
#   sudo ./deploy/install.sh --domain memory.example.com --email you@example.com --auth-mode oauth
#
# Flags:
#   --domain <fqdn>          enable Caddy + automatic TLS on 443 (omit to run HTTP on 127.0.0.1 only)
#   --behind-proxy           you already run a TLS proxy on this host. Needs --domain. Sets PUBLIC_URL
#                            from it, leaves Caddy off and the firewall alone, and keeps the server on
#                            127.0.0.1:8787 for your proxy to reach. See DEPLOY.md section 3b.
#   --email <addr>           ACME contact address
#   --auth-mode <mode>       token (default) | oauth | oidc. oauth needs --domain: PUBLIC_URL must
#                            be https, and the server refuses to boot otherwise. See deploy/oauth.md.
#   --kek-provider <p>       file (default, and what .env.example ships) | env | none. `file` is what
#                            makes a `private` write possible at all; `none` refuses one rather than
#                            storing plaintext. See .env.example.
#   --build-local            skip the ghcr.io pull and build the image from this checkout instead.
#                            The default already falls back to this when the pull fails, so this
#                            flag is for a working tree ahead of the last release, or a different
#                            EMBED_PROVIDER/EMBED_MODEL than the published image carries.
#   --no-firewall            skip ufw/firewalld changes
#   --no-backups             skip the daily backup cron
#   --dry-run                print every action, change nothing
#   --yes                    never prompt (fails rather than blocks on anything that needs a TTY)
#
# What it does: preflight, .env with generated secrets, the KEK file, pull the image (or build it),
# oauth credentials, start, verify, firewall, backups. It is idempotent: existing secrets in .env are kept, so
# re-running it will not invalidate the token or password your clients already hold, unless you
# pass --auth-mode or --kek-provider explicitly to change them.

set -euo pipefail
# Every secret this script writes (.env, secrets/lumberroom-kek, .env.bak while env_set holds it
# open) lands under this umask before its own chmod runs, closing the window where a loose default
# mode would otherwise apply for the moment between file creation and the explicit chmod below.
umask 077

DOMAIN=""
BEHIND_PROXY=0
EMAIL=""
AUTH_MODE_FLAG=""
KEK_PROVIDER_FLAG=""
BUILD_LOCAL=0
DRY_RUN=0
ASSUME_YES=0
DO_FIREWALL=1
DO_BACKUPS=1
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE=(docker compose)

while [ $# -gt 0 ]; do
  case "$1" in
    --domain) DOMAIN="${2:?}"; shift 2 ;;
    --behind-proxy) BEHIND_PROXY=1; shift ;;
    --email) EMAIL="${2:?}"; shift 2 ;;
    --auth-mode) AUTH_MODE_FLAG="${2:?}"; shift 2 ;;
    --kek-provider) KEK_PROVIDER_FLAG="${2:?}"; shift 2 ;;
    --build-local) BUILD_LOCAL=1; shift ;;
    --no-firewall) DO_FIREWALL=0; shift ;;
    --no-backups) DO_BACKUPS=0; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    -h|--help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 1 ;;
  esac
done

case "$AUTH_MODE_FLAG" in
  ''|token|oauth|oidc) ;;
  *) echo "--auth-mode must be token, oauth or oidc, got: $AUTH_MODE_FLAG" >&2; exit 1 ;;
esac
case "$KEK_PROVIDER_FLAG" in
  ''|none|file|env) ;;
  *) echo "--kek-provider must be none, file or env, got: $KEK_PROVIDER_FLAG" >&2; exit 1 ;;
esac
if [ "$BEHIND_PROXY" = 1 ] && [ -z "$DOMAIN" ]; then
  echo "--behind-proxy needs --domain: PUBLIC_URL is derived from it and there is nothing else to derive it from." >&2
  exit 1
fi
if [ "$AUTH_MODE_FLAG" = "oauth" ] && [ -z "$DOMAIN" ]; then
  echo "--auth-mode oauth needs --domain: the built-in authorization server requires an https PUBLIC_URL, and the server refuses to boot without one." >&2
  exit 1
fi

cd "$REPO_DIR"

say()  { printf '\033[1m%s\033[0m\n' "$*"; }
info() { printf '  %s\n' "$*"; }
warn() { printf '  \033[33mwarning:\033[0m %s\n' "$*"; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
run()  { if [ "$DRY_RUN" = 1 ]; then info "would run: $*"; else "$@"; fi }
ask() {
  # ask <prompt> ; returns 0 for yes
  [ "$ASSUME_YES" = 1 ] && return 0
  [ -t 0 ] || return 1
  local reply
  read -r -p "  $1 [y/N] " reply
  [ "$reply" = y ] || [ "$reply" = Y ]
}
secret() {
  if command -v openssl >/dev/null 2>&1; then openssl rand -hex "$1"
  else head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; fi
}
env_get() {
  # Strips one layer of surrounding single quotes, matching both what env_set writes and how
  # Docker Compose's own .env parser treats a quoted value.
  local v
  v="$(grep -E "^$1=" .env 2>/dev/null | tail -1 | cut -d= -f2-)"
  v="${v%\'}"; v="${v#\'}"
  printf '%s' "$v"
}
env_set() {
  # env_set KEY VALUE always writes VALUE single-quoted. Not just for AUTH_TOKENS: an Argon2 hash
  # (OWNER_PASSWORD_HASH) contains `$` characters that `sh` would try to expand as variables if
  # this file is ever sourced unquoted, the same class of bug as the AUTH_TOKENS trap documented
  # in .env.example but by expansion instead of quote-stripping. Single-quoting every value here
  # closes that off regardless of what future setting needs it. None of the values this script
  # generates contain a single quote; refuse rather than silently write a broken line if one does.
  local key="$1" value="$2" escaped
  case "$value" in *"'"*) die "env_set: refusing a value for $key containing a single quote" ;; esac
  escaped="$(printf '%s' "$value" | sed -e 's/[&#]/\\&/g')"
  sed -i.bak -e "s#^${key}=.*#${key}='${escaped}'#" .env
  rm -f .env.bak
}
# ── preflight ─────────────────────────────────────────────────────────────────
say "1/9 preflight"
[ "$(uname -s)" = Linux ] || warn "this script targets Linux; on macOS use 'docker compose up -d' directly"
info "arch: $(uname -m)"

if ! command -v docker >/dev/null 2>&1; then
  warn "docker is not installed"
  if ask "install Docker from get.docker.com?"; then
    run bash -c 'curl -fsSL https://get.docker.com | sh'
  else
    die "docker is required. Install it, then re-run."
  fi
fi
if ! docker compose version >/dev/null 2>&1; then
  die "the docker compose plugin is missing. Install docker-compose-plugin, then re-run."
fi
if [ "$(id -u)" != 0 ] && ! docker info >/dev/null 2>&1; then
  die "cannot talk to the docker daemon. Run with sudo, or add yourself to the docker group."
fi
TOTAL_MB=$(awk '/MemTotal/ {printf "%d", $2/1024}' /proc/meminfo 2>/dev/null || echo 0)
if [ "$TOTAL_MB" -gt 0 ] && [ "$TOTAL_MB" -lt 1800 ]; then
  warn "only ${TOTAL_MB}MB RAM. The local embedder needs ~400MB; consider EMBED_PROVIDER=openai."
fi
if ! command -v age >/dev/null 2>&1; then
  warn "age is not installed. Backups will refuse to write a plaintext dump until it is."
  if ask "install age now?"; then
    if command -v apt-get >/dev/null 2>&1; then
      run apt-get update -qq
      run apt-get install -y --no-install-recommends age
    else
      warn "no apt-get; install age by hand (https://github.com/FiloSottile/age) before the first backup runs"
    fi
  fi
fi
info "preflight ok"

# ── .env ──────────────────────────────────────────────────────────────────────
say "2/9 configuration"
if [ -f .env ]; then
  info ".env exists; keeping the secrets already in it"
  if [ "$DRY_RUN" != 1 ]; then
    # Chmod first, whether or not anything below rotates: a .env carried over from `cp
    # .env.example .env` by hand, or restored from somewhere that did not preserve mode, sits at
    # the process umask until this runs, and every secret already in the file is exposed to that
    # window otherwise. The mode is read before the chmod so a file that sat open is reported,
    # the way src/crypto/kek.rs reports a loose KEK file: repairing it silently would make a
    # token every local account could read for a week look clean. Placeholders are not secrets,
    # so a fresh copy of .env.example at 0644 is tightened and nothing is said.
    ENV_MODE="$(stat -c %a .env 2>/dev/null || stat -f %Lp .env 2>/dev/null || echo 600)"
    chmod 600 .env
    if [ $((0$ENV_MODE & 077)) -ne 0 ] && ! grep -q CHANGE_ME .env; then
      warn ".env had mode 0$ENV_MODE and must not be readable by group or other. It has been set to 600,"
      warn "but every secret already in it was readable by any local account while it sat that way."
      warn "Treat them as exposed: rotate POSTGRES_PASSWORD and every token in AUTH_TOKENS and"
      warn "LUMBERROOM_CLEANUP_TOKEN, and the KEK if it is in this file, then re-run wire-mac.sh."
    fi
    ROTATED=()
    case "$(env_get POSTGRES_PASSWORD)" in
      *CHANGE_ME*)
        env_set POSTGRES_PASSWORD "$(secret 24)"
        ROTATED+=(POSTGRES_PASSWORD)
        ;;
    esac
    case "$(env_get AUTH_TOKENS)" in
      *CHANGE_ME*)
        CLIENT_TOKEN="$(secret 32)"
        CLEANUP_TOKEN="$(secret 32)"
        env_set AUTH_TOKENS "[{\"client\":\"claude-code-mac\",\"token\":\"$CLIENT_TOKEN\",\"read\":[{\"namespace\":\"*\",\"max\":\"sealed\"}],\"write\":[{\"namespace\":\"*\",\"max\":\"sealed\"}],\"sealedCapable\":true,\"mayDelete\":false,\"registryWrite\":true},{\"client\":\"cleanup\",\"token\":\"$CLEANUP_TOKEN\",\"read\":[{\"namespace\":\"*\",\"max\":\"open\"}],\"write\":[],\"mayIngest\":true}]"
        env_set LUMBERROOM_CLEANUP_TOKEN "$CLEANUP_TOKEN"
        ROTATED+=(AUTH_TOKENS)
        ;;
    esac
    if [ "${#ROTATED[@]}" -gt 0 ]; then
      info "replaced placeholder CHANGE_ME values: ${ROTATED[*]}"
    fi
  fi
else
  if [ "$DRY_RUN" = 1 ]; then
    info "would generate .env from .env.example with fresh secrets"
  else
    cp .env.example .env
    # Before any secret is written, not after: env_set rewrites the file in place with sed -i.bak,
    # so a secret would otherwise sit at the process umask for as long as the five env_set calls
    # below take to run.
    chmod 600 .env
    PG_PW="$(secret 24)"
    CLIENT_TOKEN="$(secret 32)"
    env_set POSTGRES_PASSWORD "$PG_PW"
    # The owner's own client: every namespace at every level, and no delete. The ceilings are typed
    # out because a bare "*" in either list means a ceiling of `open`, which would refuse the first
    # write into personal:finance. .env.example carries this same object and the reasoning behind
    # mayDelete; keep the two in step.
    #
    # env_set quotes this for us; see the AUTH_TOKENS comment in .env.example for why an
    # unquoted JSON value here breaks the moment anything sources .env with `sh`.
    # The cleanup daemon's own client. Minted here rather than left to the owner, because the
    # daemon refuses without it and the failure would land the first time he turns the profile on.
    # Scoped rather than omitted: an omitted read or write list means unrestricted (config.rs), so
    # this grant is read at `open` on every namespace, no write at all, no sealedCapable, no
    # registryWrite, no mayDelete. Read at open over what it cleans and mayIngest; every pair it
    # is handed goes to a provider and the run withholds anything above open, so a higher ceiling
    # buys it nothing. Private duplicates are grouped by the in-server pass, which runs
    # unrestricted and sends nothing anywhere.
    CLEANUP_TOKEN="$(secret 32)"
    env_set AUTH_TOKENS "[{\"client\":\"claude-code-mac\",\"token\":\"$CLIENT_TOKEN\",\"read\":[{\"namespace\":\"*\",\"max\":\"sealed\"}],\"write\":[{\"namespace\":\"*\",\"max\":\"sealed\"}],\"sealedCapable\":true,\"mayDelete\":false,\"registryWrite\":true},{\"client\":\"cleanup\",\"token\":\"$CLEANUP_TOKEN\",\"read\":[{\"namespace\":\"*\",\"max\":\"open\"}],\"write\":[],\"mayIngest\":true}]"
    env_set LUMBERROOM_CLEANUP_TOKEN "$CLEANUP_TOKEN"
    info "generated .env with a new Postgres password and two client tokens"
  fi
fi

if [ -n "$AUTH_MODE_FLAG" ]; then
  if [ "$DRY_RUN" = 1 ]; then
    info "would set AUTH_MODE=$AUTH_MODE_FLAG"
  else
    env_set AUTH_MODE "$AUTH_MODE_FLAG"
    info "AUTH_MODE=$AUTH_MODE_FLAG"
  fi
fi
AUTH_MODE_NOW="${AUTH_MODE_FLAG:-$( [ -f .env ] && env_get AUTH_MODE || echo token )}"
[ -n "$AUTH_MODE_NOW" ] || AUTH_MODE_NOW=token

if [ -n "$DOMAIN" ]; then
  if [ "$DRY_RUN" = 1 ]; then
    info "would set LUMBERROOM_DOMAIN=$DOMAIN, PUBLIC_URL=https://$DOMAIN"
  else
    env_set LUMBERROOM_DOMAIN "$DOMAIN"
    env_set PUBLIC_URL "https://$DOMAIN"
    [ -n "$EMAIL" ] && env_set ACME_EMAIL "$EMAIL"
    info "domain set to $DOMAIN"
  fi
  # --behind-proxy takes the same PUBLIC_URL and stops there. Starting Caddy would put a second
  # thing on 80 and 443, which the operator's own proxy already holds.
  if [ "$BEHIND_PROXY" = 1 ]; then
    PROFILES=()
    info "behind-proxy: Caddy stays off; the server listens on 127.0.0.1:8787 for your proxy"
  else
    PROFILES=(--profile tls)
  fi
else
  warn "no --domain: the server will listen on 127.0.0.1:8787 with no TLS."
  warn "Reach it over an SSH tunnel, or re-run with --domain once DNS points here."
  PROFILES=()
fi
if [ "$AUTH_MODE_NOW" = oidc ]; then
  PROFILES+=(--profile logto)
  # This script writes no OIDC_* values; deploy/logto.md does that by hand. The server refuses to
  # boot in oidc mode with OIDC_ALLOWED_SUBJECTS or OIDC_REQUIRED_SCOPES empty, so an oidc branch
  # added here has to write both or the container stops at config validation.
fi

# ── key-encryption key ───────────────────────────────────────────────────────
say "3/9 key-encryption key"
# Always create the file, whatever KEK_PROVIDER ends up being: docker-compose.yml bind-mounts it
# unconditionally (see the note at the top of that file), and a missing source path there makes
# Docker create an empty directory instead of failing loudly. Under the shipped KEK_PROVIDER=file
# this is the key every private row is wrapped under, so the ordering matters: it exists before the
# first `up`, and it is never regenerated over an existing one.
#
# Generated with openssl rather than `lumberroom-server generate-kek`, which would be the tidier call and cannot
# be made here: this step runs before step 4 builds the image, so there is no `lumberroom` binary yet.
# Both produce 64 hex characters, which is one of the two encodings src/crypto/kek.rs accepts. Use
# `docker compose run --rm --no-deps -T server lumberroom-server generate-kek` when you rotate, by which point
# the image exists.
if [ ! -f secrets/lumberroom-kek ]; then
  if [ "$DRY_RUN" = 1 ]; then
    info "would generate secrets/lumberroom-kek (64 hex characters, mode 600)"
  else
    mkdir -p secrets
    secret 32 > secrets/lumberroom-kek
    chmod 600 secrets/lumberroom-kek
    info "generated secrets/lumberroom-kek"
  fi
else
  info "secrets/lumberroom-kek exists; keeping it"
fi
if [ -d .git ] && command -v git >/dev/null 2>&1 && ! git -C "$REPO_DIR" check-ignore -q secrets/lumberroom-kek 2>/dev/null; then
  warn "secrets/ is not covered by .gitignore. Add it before you commit anything: under KEK_PROVIDER=file this file is the key every private row is wrapped under, and a key in git history is a key you have to rotate, which strands every row already written under it."
fi
# Ownership only matters when KEK_PROVIDER=file, the one path that reads the file inside the
# container. It needs the uid:gid the Dockerfile pins for `lumberroom` (10001:10001) so the bind mount
# reads as owner-only. Attempted unconditionally (cheap, idempotent) and only reported as a problem
# below, in the branch where it is relevant.
CHOWNED_KEK=1
if [ "$DRY_RUN" != 1 ] && [ -f secrets/lumberroom-kek ]; then
  chown 10001:10001 secrets/lumberroom-kek 2>/dev/null || CHOWNED_KEK=0
  # The chmod fails the same way on every later run by an unprivileged operator, once the chown
  # above has landed: a file owned by 10001 takes a chmod from nobody but its owner or root. The
  # mode is already 600 in exactly that case, so treat the failure as nothing to do rather than
  # letting `set -e` end the install two steps in.
  chmod 600 secrets/lumberroom-kek 2>/dev/null || true
fi

# An explicit --kek-provider is written to .env. Without the flag, whatever .env already says stands,
# which for a .env this script just copied is the KEK_PROVIDER=file that .env.example ships. Reading
# it back rather than assuming keeps a re-run from overwriting a choice the operator made by hand.
KEK_PROVIDER_NOW="$KEK_PROVIDER_FLAG"
case "$KEK_PROVIDER_FLAG" in
  file|env|none)
    if [ "$DRY_RUN" = 1 ]; then
      info "would set KEK_PROVIDER=$KEK_PROVIDER_FLAG"
    else
      env_set KEK_PROVIDER "$KEK_PROVIDER_FLAG"
    fi
    ;;
  '')
    if [ -f .env ]; then
      KEK_PROVIDER_NOW="$(env_get KEK_PROVIDER)"
    else
      KEK_PROVIDER_NOW="$(grep -E '^KEK_PROVIDER=' .env.example | tail -1 | cut -d= -f2-)"
    fi
    [ -n "$KEK_PROVIDER_NOW" ] || KEK_PROVIDER_NOW=none
    ;;
esac

case "$KEK_PROVIDER_NOW" in
  file)
    info "KEK_PROVIDER=file: private writes are encrypted with secrets/lumberroom-kek"
    if [ "$CHOWNED_KEK" = 0 ]; then
      warn "could not chown secrets/lumberroom-kek to 10001:10001 (needs root)."
      warn "the server will fail to read it at boot until you run: sudo chown 10001:10001 $REPO_DIR/secrets/lumberroom-kek"
    fi
    warn "back that file up somewhere this box does not hold. Losing it makes every private row unrecoverable, and it must not travel with the database backups (deploy/backup.sh encrypts to a separate age key for exactly this reason)."
    ;;
  env)
    if [ "$DRY_RUN" = 1 ]; then
      info "would set LUMBERROOM_KEK from secrets/lumberroom-kek"
    else
      env_set LUMBERROOM_KEK "$(cat secrets/lumberroom-kek)"
      info "KEK_PROVIDER=env, weaker than file: the container's environment is readable by anything that can inspect it"
    fi
    warn "back up secrets/lumberroom-kek anyway. LUMBERROOM_KEK in .env is the same key and .env is not a backup."
    ;;
  *)
    info "KEK_PROVIDER=$KEK_PROVIDER_NOW: private writes are refused, not stored in plaintext"
    info "personal:finance and personal:health classify private by default, so writes there will fail until you re-run with --kek-provider file"
    ;;
esac

# ── image ─────────────────────────────────────────────────────────────────────
say "4/9 image"
# What the built binary reports on /readyz. `docker restart` and `docker compose up -d` both reuse a
# container's original image, so a rebuilt image can sit on disk while the old binary keeps serving.
# Stamped here because compose cannot run git itself, and resolved against the repository this
# script lives in rather than whatever directory the operator is standing in. Only the build path
# below reads these; a pulled image already carries its own labels from the release that built it.
export LUMBERROOM_BUILD_SHA="$(git -C "$REPO_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
export LUMBERROOM_BUILD_TAG="${LUMBERROOM_BUILD_TAG:-lumberroom-server:0.4.0}"
export LUMBERROOM_BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

if [ "$BUILD_LOCAL" = 1 ]; then
  info "--build-local: building the image from this checkout (bakes the embedding model in; a few minutes)"
  run "${COMPOSE[@]}" build server cleanup
else
  info "pulling ${LUMBERROOM_SERVER_IMAGE:-ghcr.io/lumberroom/lumberroom-server:0.4.0} (a few seconds to a minute)"
  if [ "$DRY_RUN" = 1 ]; then
    info "would run: docker compose pull server cleanup, and build from source on failure"
  elif "${COMPOSE[@]}" pull server cleanup; then
    info "pulled the published image"
  else
    warn "pull failed: no network, ghcr.io unreachable, no manifest for $(uname -m), or this checkout is ahead of the last tagged release"
    warn "building from source instead (bakes the embedding model in; a few minutes)"
    run "${COMPOSE[@]}" build server cleanup
  fi
fi

# ── oauth credentials ────────────────────────────────────────────────────────
say "5/9 oauth credentials"
if [ "$AUTH_MODE_NOW" = oauth ]; then
  if [ "$DRY_RUN" = 1 ]; then
    info "would bring up the database, prompt for the owner password, and set OWNER_PASSWORD_HASH and OAUTH_COOKIE_SECRET"
  else
    run "${COMPOSE[@]}" up -d db
    if [ -n "$(env_get OWNER_PASSWORD_HASH)" ]; then
      info "OWNER_PASSWORD_HASH already set; keeping it"
    else
      if [ "$ASSUME_YES" = 1 ] || [ ! -t 0 ]; then
        die "AUTH_MODE=oauth needs OWNER_PASSWORD_HASH and no terminal is attached to prompt for a password. Set it by hand: docker compose run --rm -T server lumberroom-server hash-password, then put the result in .env, then re-run."
      fi
      PASS="" PASS2=""
      while :; do
        read -r -s -p "  owner password for the consent screen: " PASS; echo
        read -r -s -p "  confirm: " PASS2; echo
        [ -n "$PASS" ] || { warn "cannot be empty"; continue; }
        [ "$PASS" = "$PASS2" ] && break
        warn "did not match, try again"
      done
      HASH="$(printf '%s\n' "$PASS" | "${COMPOSE[@]}" run --rm -T server lumberroom-server hash-password 2>/dev/null | tr -d '\r\n' || true)"
      PASS="" PASS2=""
      case "$HASH" in
        '$argon2'*)
          env_set OWNER_PASSWORD_HASH "$HASH"
          info "owner password hashed and stored in .env"
          ;;
        *)
          die "could not hash the owner password: 'lumberroom-server hash-password' did not return an Argon2 hash. Either the image predates that subcommand or something else failed. Check: docker compose run --rm server lumberroom-server hash-password. Set OWNER_PASSWORD_HASH by hand in .env and re-run once it works."
          ;;
      esac
    fi
    if [ -n "$(env_get OAUTH_COOKIE_SECRET)" ]; then
      info "OAUTH_COOKIE_SECRET already set; keeping it"
    else
      env_set OAUTH_COOKIE_SECRET "$(secret 32)"
      info "generated OAUTH_COOKIE_SECRET"
    fi
  fi
else
  info "AUTH_MODE=$AUTH_MODE_NOW: nothing to do here"
fi

# ── start ─────────────────────────────────────────────────────────────────────
say "6/9 start"
run "${COMPOSE[@]}" "${PROFILES[@]+${PROFILES[@]}}" up -d

# ── verify ────────────────────────────────────────────────────────────────────
say "7/9 verify"
if [ "$DRY_RUN" = 1 ]; then
  info "would poll http://127.0.0.1:8787/readyz until ready"
else
  PORT="$(env_get SERVER_PORT)"; PORT="${PORT:-8787}"
  ok=0
  for _ in $(seq 1 60); do
    if curl -fsS "http://127.0.0.1:$PORT/readyz" >/dev/null 2>&1; then ok=1; break; fi
    sleep 2
  done
  if [ "$ok" = 1 ]; then
    info "ready: $(curl -fsS "http://127.0.0.1:$PORT/readyz")"
  else
    "${COMPOSE[@]}" logs --tail 40 server || true
    die "server did not become ready. Logs above."
  fi
fi

# ── firewall ──────────────────────────────────────────────────────────────────
say "8/9 firewall"
if [ "$DO_FIREWALL" = 0 ]; then
  info "skipped (--no-firewall)"
elif [ "$BEHIND_PROXY" = 1 ]; then
  # Your proxy already holds 80 and 443, so whatever rule lets traffic reach it is already there.
  # Adding one here would claim credit for a rule this script did not write.
  info "behind-proxy: leaving the firewall alone. Your proxy already owns 80 and 443."
  info "the server and Postgres stay on 127.0.0.1; nothing new needs opening."
elif [ -n "$DOMAIN" ]; then
  if command -v ufw >/dev/null 2>&1; then
    run ufw allow 80/tcp
    run ufw allow 443/tcp
    info "ufw: 80 and 443 allowed (80 is required for the ACME challenge)"
  elif command -v firewall-cmd >/dev/null 2>&1; then
    run firewall-cmd --permanent --add-service=http
    run firewall-cmd --permanent --add-service=https
    run firewall-cmd --reload
  else
    warn "no ufw or firewalld found; open 80 and 443 yourself"
  fi
  if [ -f /etc/iptables/rules.v4 ] || (command -v iptables >/dev/null 2>&1 && iptables -S 2>/dev/null | grep -q 'REJECT'); then
    warn "this image ships iptables REJECT rules (common on Oracle Linux/Ubuntu images)."
    warn "Even with the cloud security list open, 443 stays blocked until you add a rule."
    warn "See deploy/oracle-notes.md."
  fi
else
  info "no domain: nothing to open. Postgres and the server stay on 127.0.0.1."
fi

# ── backups ───────────────────────────────────────────────────────────────────
say "9/9 backups"
if [ "$DO_BACKUPS" = 0 ]; then
  info "skipped (--no-backups)"
else
  if ! command -v age >/dev/null 2>&1 && [ -z "$(env_get BACKUP_AGE_RECIPIENT)" ]; then
    warn "no age binary and no BACKUP_AGE_RECIPIENT set. The cron will run and every backup will refuse rather than write plaintext. See the comment at the top of deploy/backup.sh."
  fi
  CRON_LINE="15 3 * * * cd $REPO_DIR && ./deploy/backup.sh >> $REPO_DIR/backups/backup.log 2>&1"
  if [ "$DRY_RUN" = 1 ]; then
    info "would install cron: $CRON_LINE"
  elif crontab -l 2>/dev/null | grep -qF 'deploy/backup.sh'; then
    info "backup cron already installed"
  else
    (crontab -l 2>/dev/null; echo "$CRON_LINE") | crontab - && info "daily backup cron installed for 03:15"
  fi
fi

# The cleanup schedule needs nothing here. The deterministic pass runs inside the server on a timer
# (CLEANUP_INTERVAL_SECS), and the model pass is a compose service under the `cleanup` profile. An
# install-time cron would be a second scheduler for the same job, and turning both on spends the
# model calls twice. docs/cleanup-schedule.md covers both halves.

# ── next steps ────────────────────────────────────────────────────────────────
say ""
say "deployed."
if [ "$BEHIND_PROXY" = 1 ]; then
  info "endpoint: https://$DOMAIN/mcp, once your proxy forwards that host to 127.0.0.1:8787"
  say ""
  say "four things the server needs from your proxy:"
  info "  Host: preserved, so the issuer and every redirect match what the client asked for"
  info "  X-Forwarded-For: the client address"
  info "  X-Forwarded-Proto: https"
  info "  no response buffering on /mcp: tool responses stream, and a buffering proxy holds them to the end"
  say ""
  info "the login limiter keys on the first entry of X-Forwarded-For, so overwrite that header rather"
  info "than appending to whatever arrived. A proxy that appends lets a caller put its own value first"
  info "and take a fresh rate-limit bucket on every request."
  info "runbook, with an nginx location block: DEPLOY.md section 3b"
elif [ -n "$DOMAIN" ]; then
  info "endpoint: https://$DOMAIN/mcp"
  info "TLS certificate issuance takes up to a minute on first request; watch: docker compose logs -f caddy"
else
  info "endpoint: http://127.0.0.1:8787/mcp (localhost only)"
fi
say ""

if [ "$AUTH_MODE_NOW" = oauth ]; then
  say "AUTH_MODE=oauth: this deployment uses the built-in authorization server."
  info "full runbook, including per-surface connection steps: deploy/oauth.md"
  info "on your Mac or wherever a client runs:"
  info "  lumberroom login --url https://$DOMAIN"
  info "that opens the consent screen in a browser; sign in with the owner password set above."
  say ""
  info "static-token clients (the CLI, hooks) still work: AUTH_TOKENS is honoured in every mode."
elif [ "$AUTH_MODE_NOW" = oidc ]; then
  say "AUTH_MODE=oidc: see deploy/logto.md to finish wiring Logto, then deploy/oauth.md is not needed."
else
  say "on your Mac, from a clone of this repo:"
  if [ -n "$DOMAIN" ]; then
    info "LUMBERROOM_TOKEN=<token> ./client/wire-mac.sh --url https://$DOMAIN"
  else
    info "ssh -N -L 8787:127.0.0.1:8787 <this-host>   # in one terminal"
    info "LUMBERROOM_TOKEN=<token> ./client/wire-mac.sh --url http://127.0.0.1:8787"
  fi
  info "<token> is the first \"token\" field in $REPO_DIR/.env (mode 600), the claude-code-mac client."
  say ""
  info "keep that token out of shell history and chat logs; it is full read/write on your memory."
  info "wire-mac.sh reads it from LUMBERROOM_TOKEN, or prompts with echo off if you leave that unset; it does not take a --token flag, since a command-line argument sits in both ps output and your shell history."
fi
