#!/usr/bin/env bash
# The Phase 3 exit criterion: "ChatGPT provably cannot see a fact that Claude Code can, and you
# have checked" (system PRD §7). This script is the check. Phase 3 spec §6 lays out the steps this
# follows; run it against the live deployment, with real credentials, after every grant change.
#
#   ./scripts/policy-test.sh                        its own server, its own database
#   LUMBERROOM_URL=https://memory.example.com \
#   LUMBERROOM_FULL_TOKEN=... LUMBERROOM_NARROW_TOKEN=... \
#   ./scripts/policy-test.sh --live
#
# With no argument this stands up a scratch server on --port (default 8790, never 8787) against a
# database named lumberroom_policy_test, mints both credentials with the grants described below, runs
# against that, and drops it afterwards. Steps 1 and 3 write into personal:finance and
# personal:health, and a gate that writes into the owner's private namespaces to prove he cannot
# read them has already cost him more than it told him.
#
# --live runs against LUMBERROOM_URL with the two tokens below, already configured in AUTH_TOKENS. The
# full credential needs mayDelete there, because a live run deletes every row it wrote before it
# exits. --live --keep-rows skips that teardown.
#
# Two credentials are required, already configured in AUTH_TOKENS on the server:
#
#   full    read/write "*" at sealed, sealed_capable, registryWrite. The owner's own client.
#   narrow  read/write restricted so it excludes LUMBERROOM_POLICY_TEST_NAMESPACE (default
#           "personal:finance", which classifies private by default; see the namespace defaults
#           table in Phase 3 spec §2). A grant of
#             read:  [{"namespace":"user:me","max":"open"},{"namespace":"global","max":"sealed"}]
#             write: [{"namespace":"user:me","max":"open"},{"namespace":"global","max":"open"}]
#           excludes it correctly. registryWrite is NOT required on narrow, and sealedCapable must
#           stay off: step 4 asks this credential for a sealed row and expects ciphertext back.
#           The sealed READ ceiling on LUMBERROOM_POLICY_TEST_OPEN_NAMESPACE is what step 4 needs. A
#           ceiling of open there makes /admin/sealed answer 403 naming the sealed ceiling, because
#           the server drops every namespace the caller cannot read at the sealed level before it
#           looks anything up. sealedCapable, not the ceiling, is what decides whether the bytes are
#           readable.
#
#           narrow ALSO needs LUMBERROOM_POLICY_TEST_CEILING_NAMESPACE (default "personal:health") in its
#           read grant, at a ceiling of open:
#             read:  [..., {"namespace":"personal:health","max":"open"}]
#           That is the two-axis case, and step 3 is the only step that reaches it. Every other step
#           uses a namespace the narrow grant does not name, so the namespace axis refuses first and
#           the sensitivity axis is never asked. A grant that names a namespace whose content the
#           defaults classify above open is what exercises the second axis, and a count published
#           beside a refused row is what step 3 catches.
#
# Follows the house pattern in scripts/done-when-test.sh: bash, set -euo pipefail, curl and node
# only, a nonce per run, coloured PASS/FAIL lines, non-zero exit on any failure, a summary at the
# end.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FULL_CLIENT="${LUMBERROOM_FULL_CLIENT:-claude-code-full}"
NARROW_CLIENT="${LUMBERROOM_NARROW_CLIENT:-chatgpt-narrow}"
EXCLUDED_NS="${LUMBERROOM_POLICY_TEST_NAMESPACE:-personal:finance}"
OPEN_NS="${LUMBERROOM_POLICY_TEST_OPEN_NAMESPACE:-global}"
# Reached by name from the narrow grant at a ceiling of open, and holding content the namespace
# defaults classify above open. Migration 004 ships personal:health private, which is both halves.
CEILING_NS="${LUMBERROOM_POLICY_TEST_CEILING_NAMESPACE:-personal:health}"

LIVE=0
KEEP_ROWS=0
SCRATCH_PORT=8790
SCRATCH_KEEP=0
POSITIONAL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --live) LIVE=1; shift ;;
    --keep-rows) KEEP_ROWS=1; shift ;;
    --port) SCRATCH_PORT="$2"; shift 2 ;;
    --keep) SCRATCH_KEEP=1; shift ;;
    -h|--help) sed -n '2,50p' "$0"; exit 0 ;;
    -*) echo "unknown argument: $1" >&2; exit 1 ;;
    *) POSITIONAL="$1"; shift ;;
  esac
done

command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
command -v node >/dev/null 2>&1 || { echo "node is required" >&2; exit 1; }
command -v lumberroom >/dev/null 2>&1 || {
  echo "lumberroom is not on PATH. This gate drives the shipped client." >&2
  echo "  brew install lumberroom/lumberroom/lumberroom" >&2
  exit 1
}

WORK="$(mktemp -d)"
WROTE="$WORK/wrote.txt"
: > "$WROTE"

# Every id this run creates, so a --live run can take back what it wrote. Called from the trap
# rather than the happy path, because the run that leaves rows behind is the one that died at
# step 3.
record_write() { if [ -n "${1:-}" ]; then printf '%s\n' "$1" >>"$WROTE"; fi; }

forget_written() {
  [ "${LIVE:-0}" -eq 1 ] || return 0
  local count; count="$(grep -c . "$WROTE" 2>/dev/null || echo 0)"
  [ "$count" -gt 0 ] || return 0
  if [ "$KEEP_ROWS" -eq 1 ]; then
    printf '\n  --keep-rows: %s rows left in %s\n' "$count" "$URL"
    sed 's/^/    /' "$WROTE"
    return 0
  fi
  local gone=0 stuck=0 code
  while read -r id; do
    [ -n "$id" ] || continue
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE \
      -H "authorization: Bearer $FULL_TOKEN" "$URL/admin/memory/$id" 2>/dev/null || echo 000)"
    if [ "$code" = "200" ]; then gone=$((gone + 1)); else
      stuck=$((stuck + 1)); printf '  \033[31mcould not delete %s (%s)\033[0m\n' "$id" "$code"
    fi
  done <"$WROTE"
  printf '\n  cleaned up: deleted %s of %s rows this run wrote\n' "$gone" "$count"
  [ "$stuck" -eq 0 ] || printf '  \033[31m%s rows are still in %s. Delete them by hand.\033[0m\n' "$stuck" "$URL"
}

if [ "$LIVE" -eq 1 ]; then
  URL="${POSITIONAL:-${LUMBERROOM_URL:-http://127.0.0.1:8787}}"
  URL="${URL%/}"
  FULL_TOKEN="${LUMBERROOM_FULL_TOKEN:-}"
  NARROW_TOKEN="${LUMBERROOM_NARROW_TOKEN:-}"
  [ -n "$FULL_TOKEN" ] || { echo "--live needs LUMBERROOM_FULL_TOKEN (full, sealed_capable, registryWrite, mayDelete)" >&2; exit 1; }
  [ -n "$NARROW_TOKEN" ] || { echo "--live needs LUMBERROOM_NARROW_TOKEN (a grant that excludes $EXCLUDED_NS)" >&2; exit 1; }
  trap 'status=$?; forget_written; rm -rf "$WORK"; exit $status' EXIT INT TERM
  printf '\033[33m  running against %s, a store this script did not create.\033[0m\n' "$URL"
  printf '\033[33m  steps 1 and 3 write into %s and %s.\033[0m\n' "$EXCLUDED_NS" "$CEILING_NS"
  printf '\033[33m  every row this run writes is deleted before it exits unless --keep-rows.\033[0m\n'
else
  [ -n "$POSITIONAL" ] && {
    echo "a URL argument needs --live. Without it this script runs against its own server." >&2
    exit 1
  }
  # Step 5 asks the narrow credential for a sealed row and expects ciphertext, so the store has to
  # hold a key. Generated per run inside scratch_start and dropped with the database.
  SCRATCH_KEK=env
  SCRATCH_DB=lumberroom_policy_test
  SCRATCH_NAME="${LUMBERROOM_POLICY_TEST_SERVER:-lumberroom-policy-test-server}"
  FULL_TOKEN="$(openssl rand -hex 32)"
  NARROW_TOKEN="$(openssl rand -hex 32)"
  # The narrow grant is the whole experiment. It never names EXCLUDED_NS, so the namespace axis
  # refuses there; it names CEILING_NS at a ceiling of open over content the defaults classify
  # private, which is the only place the sensitivity axis is reached; and it holds a sealed READ
  # ceiling on OPEN_NS without sealedCapable, which is what step 5 needs to get ciphertext back
  # rather than a 403.
  SCRATCH_TOKENS="$(cat <<JSON
[{"client":"$FULL_CLIENT","token":"$FULL_TOKEN","registryWrite":true,"sealedCapable":true,"mayDelete":true},
 {"client":"$NARROW_CLIENT","token":"$NARROW_TOKEN",
  "read":[{"namespace":"user:me","max":"open"},{"namespace":"$OPEN_NS","max":"sealed"},{"namespace":"$CEILING_NS","max":"open"}],
  "write":[{"namespace":"user:me","max":"open"},{"namespace":"$OPEN_NS","max":"open"}]}]
JSON
)"
  export SCRATCH_KEK SCRATCH_DB SCRATCH_NAME SCRATCH_TOKENS SCRATCH_PORT SCRATCH_KEEP
  # shellcheck source=lib/scratch-server.sh
  . "$REPO_DIR/scripts/lib/scratch-server.sh"
  trap 'status=$?; scratch_stop; rm -rf "$WORK"; exit $status' EXIT INT TERM
  scratch_start || exit 1
  URL="$SCRATCH_URL"
fi

NONCE="$(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' \n')"
# The client under test is the shipped binary, taken from PATH.
#
# This used to be `node bin/lumberroom.mjs`, a second client written against node built-ins alone.
# It shared no types with the server and so could not be accidentally accommodated by it, which
# earned it a place here. It also fell behind: it never learned to read an authorization server's
# metadata document, so it could not log in against a deployment whose issuer differs from its API
# base, and a gate driving a client that cannot log in proves less than it looks like it does.
MEMCTL="lumberroom"

FAILED=0
say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m  %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILED=1; }
die() { fail "$*"; printf '\npolicy-test FAILED\n'; exit 1; }

# Both credentials get their own isolated config file and seal-key file inside $WORK: never the
# operator's real ~/.config/lumberroom, and never each other's key, which is the point of the sealed test.
full() {
  LUMBERROOM_URL="$URL" LUMBERROOM_TOKEN="$FULL_TOKEN" LUMBERROOM_CONFIG="$WORK/cfg-full.json" \
    LUMBERROOM_SEAL_KEY="${SEAL_KEY:-$WORK/seal-key-full}" $MEMCTL "$@"
}
narrow() {
  LUMBERROOM_URL="$URL" LUMBERROOM_TOKEN="$NARROW_TOKEN" LUMBERROOM_CONFIG="$WORK/cfg-narrow.json" \
    LUMBERROOM_SEAL_KEY="${SEAL_KEY:-$WORK/seal-key-narrow}" $MEMCTL "$@"
}

json_field() {
  # json_field FILE PATH: dotted path, "" and a nonzero exit when absent.
  node -e '
    const fs = require("node:fs");
    let j; try { j = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); } catch { process.exit(1); }
    let v = j;
    for (const p of process.argv[2].split(".")) { if (v == null) { v = undefined; break; } v = v[p]; }
    if (v === undefined || v === null) process.exit(1);
    process.stdout.write(typeof v === "string" ? v : JSON.stringify(v));
  ' "$1" "$2" 2>/dev/null
}

stats_failures() {
  # stats_failures FILE TOOL CLIENT: sum of ToolCallStats.failures for that (tool, client) pair.
  node -e '
    const fs = require("node:fs");
    const j = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const rows = (j.by_tool || []).filter(r => r.tool === process.argv[2] && r.client === process.argv[3]);
    process.stdout.write(String(rows.reduce((n, r) => n + (r.failures || 0), 0)));
  ' "$1" "$2" "$3"
}

json_has_key() {
  # json_has_key FILE PATH KEY: exit 0 when the object at the dotted PATH has KEY as an own key.
  # Used on the bootstrap digest's inventory maps, where whole-output text search is unsafe: any
  # readable row whose CONTENT happens to mention a namespace name would false-fail a text grep.
  node -e '
    const fs = require("node:fs");
    const j = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    let v = j;
    for (const p of process.argv[2].split(".")) { if (v == null) { v = undefined; break; } v = v[p]; }
    const has = v && typeof v === "object" && Object.prototype.hasOwnProperty.call(v, process.argv[3]);
    process.exit(has ? 0 : 1);
  ' "$1" "$2" "$3" 2>/dev/null
}

inventory_count() {
  # inventory_count FILE NS: the digest inventory's count for NS, and 0 when the key is absent.
  # Absent and zero are the same claim to a reader and both are correct answers here; what matters
  # is whether the number moves when a row the caller cannot read lands.
  node -e '
    const fs = require("node:fs");
    let j; try { j = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); } catch { process.exit(1); }
    const n = (j.inventory || {})[process.argv[2]];
    process.stdout.write(String(Number.isFinite(n) ? n : 0));
  ' "$1" "$2"
}

json_list_has() {
  # json_list_has FILE PATH VALUE: exit 0 when the array at the dotted PATH contains VALUE exactly.
  # Used on the search response's namespace lists, where a whole-output grep is unsafe: a readable
  # row whose own content mentions a namespace name would false-fail one.
  node -e '
    const fs = require("node:fs");
    let j; try { j = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); } catch { process.exit(1); }
    let v = j;
    for (const p of process.argv[2].split(".")) { if (v == null) { v = undefined; break; } v = v[p]; }
    process.exit(Array.isArray(v) && v.includes(process.argv[3]) ? 0 : 1);
  ' "$1" "$2" "$3" 2>/dev/null
}

grant_ceiling() {
  # grant_ceiling WHOAMI_FILE NS: the highest read ceiling that credential holds on NS, empty when
  # no pattern matches. The glob rules are domain::namespaces::matches: "*" matches everything, a
  # trailing "*" is a prefix, anything else is exact. Highest rather than first, because two
  # matching patterns mean the caller was granted both and the union is what it holds.
  node -e '
    const fs = require("node:fs");
    const j = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const ns = process.argv[2];
    const rank = { open: 0, private: 1, sealed: 2 };
    const hit = (p) => p === "*" || (p.endsWith("*") ? ns.startsWith(p.slice(0, -1)) : p === ns);
    let best = "";
    for (const g of j.read || []) {
      if (!hit(String(g.namespace).trim().toLowerCase())) continue;
      if (best === "" || rank[g.max] > rank[best]) best = g.max;
    }
    process.stdout.write(best);
  ' "$1" "$2"
}

say "0/8 preflight: both credentials reach the server"
if full doctor >"$WORK/doctor-full.txt" 2>&1; then
  pass "the full credential authenticates ($URL)"
else
  die "the full credential could not reach $URL: $(cat "$WORK/doctor-full.txt")"
fi
if narrow doctor >"$WORK/doctor-narrow.txt" 2>&1; then
  pass "the narrow credential authenticates"
else
  die "the narrow credential could not reach $URL: $(cat "$WORK/doctor-narrow.txt")"
fi

# Two modes, two labels. `doctor` printed the CREDENTIAL's mode from whoami under a line reading
# "server auth mode", so an oauth server answering a static bearer reported "token" and looked like a
# server that had lost its own configuration. Every mode honours static tokens, so the two values
# legitimately differ and the check is that each line carries the one it names.
curl -sS -o "$WORK/readyz.json" "$URL/readyz" >/dev/null 2>&1 || true
SERVER_MODE="$(json_field "$WORK/readyz.json" auth_mode || true)"
DOCTOR_SERVER_MODE="$(sed -n 's/^server auth mode: *//p' "$WORK/doctor-full.txt" | head -1 || true)"
if [ -z "$SERVER_MODE" ]; then
  fail "/readyz did not report auth_mode, so doctor's server label cannot be checked"
elif [ "$DOCTOR_SERVER_MODE" != "$SERVER_MODE" ]; then
  fail "doctor says server auth mode \"$DOCTOR_SERVER_MODE\" and /readyz says \"$SERVER_MODE\""
elif ! grep -q '^credential auth mode:' "$WORK/doctor-full.txt"; then
  fail "doctor does not report the credential's own auth mode as a separate labelled line"
else
  pass "doctor reports the server's auth mode ($SERVER_MODE) and the credential's separately, each \
labelled with what it is"
fi

full stats --hours 1 --json >"$WORK/stats-before.json" 2>/dev/null || echo '{"by_tool":[]}' >"$WORK/stats-before.json"

FACT="the household reserve fund reference is POLICY-$NONCE, tracked for budgeting only"
REG_KIND="host"
# The server holds registry keys to the canonical shape: two to four dot-separated segments,
# lowercase [a-z0-9-], first segment one of machines, services, credentials, routes, datasets,
# people, accounts (src/domain/canonical.rs). A bare "policy-test-<nonce>" is refused at
# /admin/registry before any grant check runs, which reads as a broken credential here.
REG_KEY="machines.policy-test-$NONCE.hostname"
REG_VALUE_MARKER="internal-$NONCE"
REG_VALUE="\"$REG_VALUE_MARKER\""

say "1/8 the full credential writes a private fact where the narrow grant cannot reach"
if full write "$FACT" --namespace "$EXCLUDED_NS" --json >"$WORK/write-private.json" 2>"$WORK/write-private.err"; then
  WRITE_NS="$(json_field "$WORK/write-private.json" namespace || true)"
  WRITE_SENS="$(json_field "$WORK/write-private.json" sensitivity || true)"
  if [ "$WRITE_NS" = "$EXCLUDED_NS" ] && [ "$WRITE_SENS" = "private" ]; then
    pass "written to $EXCLUDED_NS, classified private by the namespace default (no sensitivity was asked for)"
  else
    die "the write landed at namespace [$WRITE_NS] sensitivity [$WRITE_SENS], expected $EXCLUDED_NS / private"
  fi
else
  die "the full credential could not write to $EXCLUDED_NS: $(cat "$WORK/write-private.err")"
fi

if full registry set "$REG_KIND" "$REG_KEY" "$REG_VALUE" --namespace "$EXCLUDED_NS" >"$WORK/registry-set.txt" 2>&1; then
  pass "the full credential seeds a registry entry in $EXCLUDED_NS, so step 2's 'not found' means something"
else
  die "registry set failed (needs registryWrite on the full grant): $(cat "$WORK/registry-set.txt")"
fi
if full registry get "$REG_KIND" "$REG_KEY" --namespace "$EXCLUDED_NS" --json >"$WORK/registry-get-full.json" 2>&1 \
  && [ "$(json_field "$WORK/registry-get-full.json" found || true)" = "true" ]; then
  pass "the full credential confirms the registry entry is there before asking the narrow one"
else
  die "the seeded registry entry is not visible to the credential that just wrote it"
fi

say "2/8 the narrow credential cannot see, list, look up, or write into that namespace"

# A search within the narrow credential's own grant must succeed, filtered, not error out. An
# outright failure is a different bug from a leak and must not be read as "sees nothing."
if ! narrow search "POLICY-$NONCE" --json >"$WORK/search-narrow.json" 2>&1; then
  die "memory_search failed outright for the narrow credential, instead of succeeding with filtered \
results: $(cat "$WORK/search-narrow.json")"
fi
if ! grep -q "POLICY-$NONCE" "$WORK/search-narrow.json"; then
  pass "memory_search for the nonce returns nothing to the narrow credential"
else
  die "memory_search leaked the private fact to the narrow credential: $(cat "$WORK/search-narrow.json")"
fi

# The response's own namespace lists, checked as arrays. `also_searched` published the discovery set:
# `namespace_counts`, which applies no policy, through `filter_readable`, which applies the namespace
# axis and not the ceiling. Every nonce grep above passes while a name is handed over.
for FIELD in namespaces also_searched; do
  if json_list_has "$WORK/search-narrow.json" "$FIELD" "$EXCLUDED_NS"; then
    die "memory_search's $FIELD names $EXCLUDED_NS to a credential whose grant excludes it: \
$(cat "$WORK/search-narrow.json")"
  fi
done
pass "memory_search's namespaces and also_searched never name $EXCLUDED_NS"

if ! narrow bootstrap --json >"$WORK/bootstrap-narrow.json" 2>&1; then
  die "context_bootstrap failed outright for the narrow credential, instead of succeeding with a \
filtered digest: $(cat "$WORK/bootstrap-narrow.json")"
fi
if grep -q "POLICY-$NONCE" "$WORK/bootstrap-narrow.json"; then
  die "context_bootstrap's digest leaked the nonce to the narrow credential"
fi
# Checked as a structured key, not a whole-output text search: a readable row whose own content
# happens to mention "personal:finance" would false-fail a plain grep. What the Phase 1 bug
# actually did was put the namespace's key in the inventory map, so that is what gets checked.
if json_has_key "$WORK/bootstrap-narrow.json" inventory "$EXCLUDED_NS" \
  || json_has_key "$WORK/bootstrap-narrow.json" sealed_inventory "$EXCLUDED_NS" \
  || json_has_key "$WORK/bootstrap-narrow.json" counts.by_namespace "$EXCLUDED_NS"; then
  die "context_bootstrap's namespace inventory names $EXCLUDED_NS to a credential that cannot read it. \
This is the exact shape of the Phase 1 bug: a digest subquery that skipped the grant filter."
fi
# The rendered markdown as well as the payload, because the text is what a model reads and the two
# are built separately. The inventory line prints "<namespace> (<count>)", so that shape is what is
# searched for: a bare name grep over the whole response would false-fail on a readable row whose own
# content mentions the namespace.
NARROW_DIGEST_TEXT="$(json_field "$WORK/bootstrap-narrow.json" text || true)"
if printf '%s' "$NARROW_DIGEST_TEXT" | grep -qF "$EXCLUDED_NS ("; then
  die "the digest text hands $EXCLUDED_NS to the narrow credential with a row count beside it, while \
the payload is clean. The text is the half a model reads."
fi
pass "context_bootstrap's digest never names $EXCLUDED_NS or the nonce, in the inventory, in \
counts.by_namespace, or in the rendered text"

# registry_get against an explicitly out-of-grant namespace may legitimately answer found:false or
# refuse the call outright, so this leg stays lenient on how it fails. What it checks for is the
# seeded VALUE, not the bare nonce: RegistryGetResult echoes the requested key back even on
# found:false, and the key itself carries the nonce, which would false-fail a bare-nonce grep.
narrow registry get "$REG_KIND" "$REG_KEY" --namespace "$EXCLUDED_NS" --json >"$WORK/registry-get-narrow.json" 2>&1 || true
if [ "$(json_field "$WORK/registry-get-narrow.json" found || true)" = "false" ] \
  || ! grep -qF "$REG_VALUE_MARKER" "$WORK/registry-get-narrow.json"; then
  pass "registry_get for the equivalent key reports not found to the narrow credential"
else
  die "registry_get leaked the entry to the narrow credential: $(cat "$WORK/registry-get-narrow.json")"
fi

if narrow write "a note the narrow credential should not be able to leave here" --namespace "$EXCLUDED_NS" \
  >"$WORK/write-narrow.txt" 2>&1; then
  die "the narrow credential was able to write into $EXCLUDED_NS, which its grant excludes"
else
  pass "a write into $EXCLUDED_NS from the narrow credential is denied: $(head -c 160 "$WORK/write-narrow.txt")"
fi

say "3/8 the two-axis case: a namespace the narrow grant NAMES and the ceiling refuses"

# The count beside the namespace name, which is the half no gate here checked. `namespace_counts`
# applies no ceiling, and a digest inventory built from it published "personal:finance: 1" to a
# client granted "*" at open: the row refused, the namespace and the number handed over. The nonce
# was absent throughout, so every step above passed while the disclosure shipped.
#
# Steps 1 and 2 cannot reach this, because the narrow grant does not name $EXCLUDED_NS at all and
# the namespace axis refuses before the ceiling is consulted. This step needs a namespace the grant
# does name, at a ceiling below what the content is classified at.
WHOAMI_STATUS="$(curl -sS -o "$WORK/whoami-narrow.json" -w '%{http_code}' \
  -H "authorization: Bearer $NARROW_TOKEN" "$URL/admin/whoami" || echo 000)"
CEILING_GRANT=""
if [ "$WHOAMI_STATUS" = 200 ]; then
  CEILING_GRANT="$(grant_ceiling "$WORK/whoami-narrow.json" "$CEILING_NS" || true)"
fi

if [ "$WHOAMI_STATUS" != 200 ]; then
  fail "/admin/whoami answered $WHOAMI_STATUS for the narrow credential, so its ceiling on \
$CEILING_NS cannot be read and this step cannot run"
elif [ "$CEILING_GRANT" != "open" ]; then
  fail "the narrow credential holds [${CEILING_GRANT:-no grant}] on $CEILING_NS and this step needs \
\"open\". Add {\"namespace\":\"$CEILING_NS\",\"max\":\"open\"} to its read grant in AUTH_TOKENS, or \
point LUMBERROOM_POLICY_TEST_CEILING_NAMESPACE at a namespace it already names at open. Until then nothing \
in this script reaches the sensitivity axis and the digest's counts go unchecked, which is how the \
inventory leak shipped."
else
  if ! narrow bootstrap --json >"$WORK/bootstrap-ceiling-before.json" 2>&1; then
    die "context_bootstrap failed for the narrow credential: $(cat "$WORK/bootstrap-ceiling-before.json")"
  fi
  BEFORE_CEILING_COUNT="$(inventory_count "$WORK/bootstrap-ceiling-before.json" "$CEILING_NS")"

  if ! full write "the appointment note CEILING-$NONCE, filed for planning" --namespace "$CEILING_NS" \
    --json >"$WORK/write-ceiling.json" 2>"$WORK/write-ceiling.err"; then
    die "the full credential could not write to $CEILING_NS: $(cat "$WORK/write-ceiling.err")"
  fi
  CEILING_SENS="$(json_field "$WORK/write-ceiling.json" sensitivity || true)"
  if [ "$CEILING_SENS" = "open" ]; then
    fail "the write into $CEILING_NS landed at open, so the narrow credential may read it and this \
step proves nothing. Point LUMBERROOM_POLICY_TEST_CEILING_NAMESPACE at a namespace the defaults classify \
above open; personal:health ships private in migration 004."
  else
    if ! narrow bootstrap --json >"$WORK/bootstrap-ceiling-after.json" 2>&1; then
      die "context_bootstrap failed for the narrow credential: $(cat "$WORK/bootstrap-ceiling-after.json")"
    fi
    AFTER_CEILING_COUNT="$(inventory_count "$WORK/bootstrap-ceiling-after.json" "$CEILING_NS")"
    if [ "$AFTER_CEILING_COUNT" != "$BEFORE_CEILING_COUNT" ]; then
      die "the digest inventory counted a $CEILING_SENS row in $CEILING_NS for a credential whose \
ceiling there is open ($BEFORE_CEILING_COUNT -> $AFTER_CEILING_COUNT). The content was refused and \
the name and the number were published, which is the leak: every nonce grep in this script passes \
while it happens."
    fi
    # The rendered half of the same claim. The inventory line prints "<namespace> (<count>)" and
    # only for a count above zero, so with nothing readable there the name must not appear beside a
    # bracket at all.
    if [ "$AFTER_CEILING_COUNT" = 0 ] \
      && printf '%s' "$(json_field "$WORK/bootstrap-ceiling-after.json" text || true)" \
        | grep -qF "$CEILING_NS ("; then
      die "the rendered digest text prints a row count for $CEILING_NS while the payload counts none \
for this credential. The text is the half a model reads."
    fi
    pass "a $CEILING_SENS row in $CEILING_NS, which the narrow grant names at open, changes nothing \
in that credential's inventory ($BEFORE_CEILING_COUNT -> $AFTER_CEILING_COUNT)"

    # The same axis, in memory_search. With nothing readable in $CEILING_NS, the name must not reach
    # `also_searched` either: surviving `filter_readable` means the namespace axis admits the name,
    # which is not the same as this credential being able to read anything there.
    if ! narrow search "CEILING-$NONCE" --json >"$WORK/search-ceiling.json" 2>&1; then
      die "memory_search failed outright for the narrow credential: $(cat "$WORK/search-ceiling.json")"
    fi
    if grep -qF "CEILING-$NONCE" "$WORK/search-ceiling.json"; then
      die "memory_search handed the narrow credential a $CEILING_SENS row from $CEILING_NS: \
$(cat "$WORK/search-ceiling.json")"
    fi
    if [ "$AFTER_CEILING_COUNT" = 0 ] \
      && json_list_has "$WORK/search-ceiling.json" also_searched "$CEILING_NS"; then
      die "memory_search's also_searched names $CEILING_NS while this credential can read nothing \
there. The name outlives the row the ceiling refused, which is the inventory leak in a second field."
    fi
    pass "memory_search's also_searched does not name $CEILING_NS to a credential whose ceiling \
there refuses every row in it"
  fi

  # The positive control, and the proof these reads are fresh rather than cached: a write clears the
  # digest cache server-side, and an open row the narrow credential MAY read has to raise its count
  # by exactly one. A digest that answered zero for everything would pass the leg above and be
  # useless to a model.
  # Baselined on the digest read BEFORE the write above, which is the one file this branch always
  # produced, and no less current for it: the only write in between landed in $CEILING_NS.
  BEFORE_OPEN_COUNT="$(inventory_count "$WORK/bootstrap-ceiling-before.json" "$OPEN_NS")"
  if ! full write "an open note OPENCOUNT-$NONCE that any credential here may read" \
    --namespace "$OPEN_NS" --json >"$WORK/write-open-count.json" 2>"$WORK/write-open-count.err"; then
    die "the full credential could not write to $OPEN_NS: $(cat "$WORK/write-open-count.err")"
  fi
  OPEN_SENS="$(json_field "$WORK/write-open-count.json" sensitivity || true)"
  if [ "$OPEN_SENS" != "open" ]; then
    fail "the write into $OPEN_NS landed at $OPEN_SENS rather than open, so it is not a positive \
control. Point LUMBERROOM_POLICY_TEST_OPEN_NAMESPACE at a namespace the defaults leave open."
  else
    if ! narrow bootstrap --json >"$WORK/bootstrap-open-after.json" 2>&1; then
      die "context_bootstrap failed for the narrow credential: $(cat "$WORK/bootstrap-open-after.json")"
    fi
    AFTER_OPEN_COUNT="$(inventory_count "$WORK/bootstrap-open-after.json" "$OPEN_NS")"
    if [ "$AFTER_OPEN_COUNT" -ne "$((BEFORE_OPEN_COUNT + 1))" ]; then
      die "an open row the narrow credential may read did not reach its inventory count for $OPEN_NS \
($BEFORE_OPEN_COUNT -> $AFTER_OPEN_COUNT, expected $((BEFORE_OPEN_COUNT + 1))). Either the count is \
not filtered on the ceiling and reports something else, or these digests are stale: a write clears \
the digest cache, so a number that does not move is a real failure rather than a caching artefact."
    fi
    pass "an open row the narrow credential may read still raises its count for $OPEN_NS \
($BEFORE_OPEN_COUNT -> $AFTER_OPEN_COUNT), so the filtered inventory is filtered rather than empty"
  fi
fi

say "4/8 the full credential still sees its own fact"
if full search "POLICY-$NONCE" --json >"$WORK/search-full.json" 2>&1 && grep -q "POLICY-$NONCE" "$WORK/search-full.json"; then
  pass "memory_search for the nonce finds it under the full credential"
else
  die "the full credential lost its own fact: $(cat "$WORK/search-full.json")"
fi

say "5/8 sealed content: ciphertext on the wire, plaintext only for a client holding the key"
SEAL_KEY_NAME="policy-test-seal-$NONCE"
SEAL_VALUE="sealed-value-$NONCE"
FULL_SEAL_KEY="$WORK/seal-key-full"
WRONG_SEAL_KEY="$WORK/seal-key-wrong"

if SEAL_KEY="$FULL_SEAL_KEY" full seal "$SEAL_KEY_NAME" --namespace "$OPEN_NS" --value "$SEAL_VALUE" \
  >"$WORK/seal.txt" 2>&1; then
  pass "sealed $SEAL_KEY_NAME in $OPEN_NS (client-side AES-256-GCM; the server never saw the plaintext)"
else
  die "seal failed: $(cat "$WORK/seal.txt")"
fi

# The wire check: fetch the raw row with a credential that never held sealed_capable, and confirm
# what comes back is ciphertext, never the plaintext value, regardless of who is asking. Computed
# with the same HMAC the CLI uses (bin/lumberroom.mjs's sealedKeyHmac), from the key that did the sealing.
KEY_HMAC="$(node -e '
  const fs = require("node:fs");
  const crypto = require("node:crypto");
  const key = Buffer.from(fs.readFileSync(process.argv[1], "utf8").trim(), "base64");
  const NUL = String.fromCharCode(0);
  process.stdout.write(
    crypto.createHmac("sha256", key).update(process.argv[2] + NUL + process.argv[3]).digest("hex"),
  );
' "$FULL_SEAL_KEY" "$OPEN_NS" "$SEAL_KEY_NAME" || true)"
NS_ENCODED="$(node -e 'process.stdout.write(encodeURIComponent(process.argv[1]))' "$OPEN_NS" || true)"
SEALED_WIRE_STATUS="$(curl -sS -o "$WORK/sealed-wire.json" -w '%{http_code}' \
  -H "authorization: Bearer $NARROW_TOKEN" \
  "$URL/admin/sealed?namespace=$NS_ENCODED&key_hmac=$KEY_HMAC" || echo 000)"
if [ "$SEALED_WIRE_STATUS" = 200 ] && [ -n "$(json_field "$WORK/sealed-wire.json" ciphertext || true)" ] \
  && ! grep -qF "$SEAL_VALUE" "$WORK/sealed-wire.json"; then
  pass "the sealed row is served as ciphertext over the wire, with no plaintext anywhere in the response"
else
  die "the sealed endpoint did not return ciphertext-only (status $SEALED_WIRE_STATUS): $(cat "$WORK/sealed-wire.json")"
fi

UNSEALED="$(SEAL_KEY="$FULL_SEAL_KEY" full unseal "$SEAL_KEY_NAME" --namespace "$OPEN_NS" 2>"$WORK/unseal-capable.err" || true)"
if [ "$UNSEALED" = "$SEAL_VALUE" ]; then
  pass "a client holding the matching key decrypts the sealed value correctly"
else
  die "the capable client could not recover the plaintext: $(cat "$WORK/unseal-capable.err")"
fi

if SEAL_KEY="$WRONG_SEAL_KEY" full unseal "$SEAL_KEY_NAME" --namespace "$OPEN_NS" \
  >"$WORK/unseal-incapable.txt" 2>&1; then
  die "a client with the WRONG key still recovered the plaintext, which should be impossible"
else
  pass "a client without the matching key cannot read the plaintext (a different key means a different \
key_hmac, so the lookup itself fails closed): $(head -c 120 "$WORK/unseal-incapable.txt")"
fi

say "6/8 the credential tripwire: a live-looking token is refused at open, without echoing it back"
# Assembled at runtime, and the tail is invented rather than copied from anywhere. A public repo
# carrying a literal token-shaped string collects secret-scanning alerts and push-protection
# blocks forever after, and the tripwire matches on shape: prefix plus thirty or more tail
# characters (src/domain/tripwire.rs). Shape is all this needs to be.
SECRET="$(printf 'gh%s_' p)aB3dEf7hJk2mNp5qRs8tUv1wXy4zAc6eGi9L"
if full write "the deploy key is $SECRET" --namespace "$OPEN_NS" >"$WORK/tripwire.txt" 2>&1; then
  die "a github-token-shaped write was accepted at 'open', which the tripwire exists to prevent"
else
  if grep -qi 'github_token' "$WORK/tripwire.txt" && grep -qi 'sealed' "$WORK/tripwire.txt"; then
    pass "the write is refused, names the rule (github_token), and suggests sealed"
  else
    die "the refusal did not name a rule and suggest sealed: $(cat "$WORK/tripwire.txt")"
  fi
  if grep -qF "$SECRET" "$WORK/tripwire.txt"; then
    die "the refusal echoed the credential back in its own error message: $(cat "$WORK/tripwire.txt")"
  else
    pass "the refusal does not repeat the secret it refused"
  fi
fi

say "7/8 both denials are observable in tool_calls, not silent"
if ! full stats --hours 1 --json >"$WORK/stats-after.json" 2>&1; then
  die "'lumberroom stats' failed after the denials it is supposed to record: $(cat "$WORK/stats-after.json")"
fi
BEFORE_NARROW_FAILURES="$(stats_failures "$WORK/stats-before.json" memory_write "$NARROW_CLIENT" || echo 0)"
AFTER_NARROW_FAILURES="$(stats_failures "$WORK/stats-after.json" memory_write "$NARROW_CLIENT" || echo 0)"
BEFORE_FULL_FAILURES="$(stats_failures "$WORK/stats-before.json" memory_write "$FULL_CLIENT" || echo 0)"
AFTER_FULL_FAILURES="$(stats_failures "$WORK/stats-after.json" memory_write "$FULL_CLIENT" || echo 0)"

if [ "$AFTER_NARROW_FAILURES" -gt "$BEFORE_NARROW_FAILURES" ]; then
  pass "the narrow credential's denied write shows up in 'lumberroom stats' as a memory_write failure \
(client $NARROW_CLIENT: $BEFORE_NARROW_FAILURES -> $AFTER_NARROW_FAILURES)"
else
  die "the narrow credential's denied write left no trace in tool_calls. \
Check that LUMBERROOM_NARROW_CLIENT ($NARROW_CLIENT) matches the \"client\" field on its AUTH_TOKENS grant."
fi
if [ "$AFTER_FULL_FAILURES" -gt "$BEFORE_FULL_FAILURES" ]; then
  pass "the tripwire refusal shows up in 'lumberroom stats' as a memory_write failure \
(client $FULL_CLIENT: $BEFORE_FULL_FAILURES -> $AFTER_FULL_FAILURES)"
else
  die "the tripwire refusal left no trace in tool_calls. \
Check that LUMBERROOM_FULL_CLIENT ($FULL_CLIENT) matches the \"client\" field on its AUTH_TOKENS grant."
fi

say "8/8 the operator routes answer the narrow credential without describing the rest of the store"

# /statsz authenticates and used to authorize nothing. Every by_tool row carries the client that made
# the calls, by_client lists every client that has called anything, and staleness counts every row in
# the tenant, so a token granted one namespace at open learned which other surfaces the owner runs,
# how often each one calls and fails, and how large the store it cannot read is.
STATS_STATUS="$(curl -sS -o "$WORK/statsz-narrow.json" -w '%{http_code}' \
  -H "authorization: Bearer $NARROW_TOKEN" "$URL/statsz?hours=24" || echo 000)"
if [ "$STATS_STATUS" != 200 ]; then
  fail "/statsz answered $STATS_STATUS for the narrow credential, so its scope cannot be checked"
else
  STATS_SCOPE="$(json_field "$WORK/statsz-narrow.json" scope || true)"
  if grep -qF "\"$FULL_CLIENT\"" "$WORK/statsz-narrow.json"; then
    die "/statsz named client $FULL_CLIENT to the narrow credential. One client learning that another \
exists, and how much it calls, is the shape of somebody else's deployment: \
$(cat "$WORK/statsz-narrow.json")"
  fi
  if [ "$STATS_SCOPE" != "self" ]; then
    fail "/statsz reported scope [${STATS_SCOPE:-none}] for the narrow credential, expected \"self\". \
A report that does not say what it is bounded to is a report that will be read as the whole store."
  else
    pass "/statsz answers the narrow credential with scope self and never names $FULL_CLIENT"
  fi
fi

STATS_CLIENT_STATUS="$(curl -sS -o "$WORK/statsz-narrow-by-client.json" -w '%{http_code}' \
  -H "authorization: Bearer $NARROW_TOKEN" "$URL/statsz?hours=24&by=client" || echo 000)"
if [ "$STATS_CLIENT_STATUS" != 200 ]; then
  fail "/statsz?by=client answered $STATS_CLIENT_STATUS for the narrow credential"
elif json_field "$WORK/statsz-narrow-by-client.json" staleness.live_rows >/dev/null 2>&1; then
  die "/statsz?by=client handed the narrow credential live_rows for the whole tenant: \
$(cat "$WORK/statsz-narrow-by-client.json")"
elif grep -qF "\"$FULL_CLIENT\"" "$WORK/statsz-narrow-by-client.json"; then
  die "/statsz?by=client named client $FULL_CLIENT to the narrow credential: \
$(cat "$WORK/statsz-narrow-by-client.json")"
else
  pass "/statsz?by=client gives the narrow credential its own rows and no tenant-wide row counts"
fi

# The full credential still gets the report the CLI exists to print, so the narrowing above is the
# grant rather than an endpoint that stopped working. Step 7 depends on this: it reads the narrow
# client's failures through the full token.
if full stats --hours 1 --json >"$WORK/statsz-full.json" 2>&1 \
  && [ "$(json_field "$WORK/statsz-full.json" scope || true)" = "tenant" ]; then
  pass "the full credential still gets the tenant-wide report"
else
  die "the full credential lost its own stats report: $(cat "$WORK/statsz-full.json")"
fi

# /admin/export pages rows straight out of `list_for_export`, which takes no grant, and narrows them
# afterwards. The private fact from step 1 is the discriminator.
EXPORT_STATUS="$(curl -sS -o "$WORK/export-narrow.json" -w '%{http_code}' \
  -H "authorization: Bearer $NARROW_TOKEN" "$URL/admin/export?limit=200" || echo 000)"
if [ "$EXPORT_STATUS" != 200 ]; then
  fail "/admin/export answered $EXPORT_STATUS for the narrow credential, so its rows cannot be checked"
elif grep -qF "POLICY-$NONCE" "$WORK/export-narrow.json"; then
  die "/admin/export handed the narrow credential the private fact from $EXCLUDED_NS: \
$(cat "$WORK/export-narrow.json")"
elif grep -qF "$EXCLUDED_NS" "$WORK/export-narrow.json"; then
  die "/admin/export named $EXCLUDED_NS to the narrow credential: $(cat "$WORK/export-narrow.json")"
else
  pass "/admin/export gives the narrow credential no row and no namespace name from $EXCLUDED_NS"
fi

say "what each step proved"
cat <<SUMMARY
  1  the full credential can write private content the narrow grant does not reach
  2  memory_search, context_bootstrap (digest text, the inventory, and counts.by_namespace), and
     registry_get all agree: the narrow credential sees nothing there, and a write into it is
     refused rather than silently dropped
  3  the second axis on its own: in a namespace the narrow grant NAMES at open, a row above open
     changes no count in that credential's digest, and an open row still does. Names and counts are
     the disclosure a nonce grep cannot see
  4  the full credential's own view of its own fact is unaffected
  5  a sealed row is served as ciphertext regardless of grant; only a client holding the matching
     local key recovers the plaintext
  6  a credential-shaped write at 'open' is refused, names the rule, suggests sealed, and never
     echoes the secret back
  7  both denials are observable afterward, not silent
  8  /statsz and /admin/export authorize rather than only authenticate: the narrow credential gets
     its own rows, no other client's name, no tenant-wide row count, and nothing out of the excluded
     namespace, while the full credential keeps the report the CLI prints
SUMMARY

if [ "$FAILED" = 1 ]; then
  echo ""
  echo "  policy-test FAILED"
  exit 1
fi
echo ""
echo "  policy-test PASSED"
exit 0
