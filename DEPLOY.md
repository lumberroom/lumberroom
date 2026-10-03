# Deploy runbook

Target: a Linux VM you control, arm64 or amd64, with Docker. The PRD assumes an Oracle Cloud
Always Free Ampere A1; nothing here is Oracle-specific except
[deploy/oracle-notes.md](deploy/oracle-notes.md).

Resource floor: 2 vCPU, 2 GB RAM, 20 GB disk. The local embedder holds about 350 MB resident.
On a box under 1.8 GB the installer warns and suggests `EMBED_PROVIDER=openai`.

The token-mode path is Phase 1 and has been exercised. The OAuth path has run against a live
server: [`scripts/oauth-flow-test.sh`](scripts/oauth-flow-test.sh) passed 43 assertions across 13
steps, in both acceptance gates recorded in [VERIFY.md](VERIFY.md). The key-encryption key and the
age-encrypted backups are implemented and have not been run against a live server. Where a step is
proved by a script, this document names the script and says whether it has run.

---

## Which path do you want

**Token mode, on loopback.** One command, no DNS, no TLS, no consent screen. The server binds
`127.0.0.1:8787` and you reach it over an SSH tunnel or from the same box. This is enough for
Claude Code, Hermes, OpenWebUI and the `lumberroom` CLI, all of which authenticate with a static bearer
header. It is the Phase 1 path and it is the one that has been exercised.

**OAuth mode, behind a domain.** The built-in authorization server ([decision
0002](docs/decisions/0002-built-in-oauth-server.md)). Needed for the Claude.ai family, Cowork,
mobile and probably ChatGPT, none of which will take a static header. Static tokens keep working
alongside it, so switching modes does not break the CLI or the hooks. A flow has run against a live
server: [`scripts/oauth-flow-test.sh`](scripts/oauth-flow-test.sh) passed 43 assertions across 13
steps, in both acceptance gates recorded in [VERIFY.md](VERIFY.md).

**Behind a proxy you already run.** The host has its own nginx on 80 and 443 terminating TLS, and
lumberroom sits behind it on `127.0.0.1:8787`. Install with `--behind-proxy` and Caddy never starts.
Either auth mode works. Section 3b is the runbook.

Sections 1 to 4 are common to all three. Section 5 is the OAuth runbook. Section 8 is the KEK,
which you need before any `private` write can land.

---

## 1. Before you start

- A DNS A record pointing at the VM, for example `memory.example.com`. Caddy needs it to issue a
  certificate, and OAuth mode requires it: the server refuses to boot in oauth mode without an
  `https` or loopback `PUBLIC_URL`. You can deploy without a domain and add TLS later by re-running
  the installer with `--domain`.
- Inbound 80 and 443 open in your cloud firewall. Port 80 is required for the ACME challenge.
- SSH key-only, on a non-default port if you like. Nothing here depends on it.
- Outbound HTTPS once, either to pull the published image from `ghcr.io` (the default, a few
  seconds to a minute) or, if that pull fails or you pass `--build-local`, to build it: the build
  downloads the embedding weights and a prebuilt ONNX Runtime and bakes both in, a few minutes.
  After that the server needs no outbound access at all. On a box with no egress, pull or build the
  image elsewhere and `docker save`/`docker load` it across.

## 2. Get the repo onto the VM

Clone the repo onto the VM over https:

```bash
git clone https://github.com/lumberroom/lumberroom.git
```

For a private fork, give the box read access with a deploy key instead, or clone over an
agent-forwarded SSH session:

```bash
ssh -A <vm>
git clone git@github.com:<you>/lumberroom.git
```

No GitHub access from the box works too. One file, scp it over:

```bash
git bundle create /tmp/lumberroom.bundle --all
scp /tmp/lumberroom.bundle <vm>:~/
ssh <vm> 'git clone lumberroom.bundle lumberroom'
```

Do not rsync the working directory. It carries the local `.env`, whose secrets are development
throwaways, and copying it skips the fresh secret generation the installer does.

## 3. Install

`docker-compose.yml` pins `server` and `cleanup` to `ghcr.io/lumberroom/lumberroom-server:0.4.0`
and `ghcr.io/lumberroom/lumberroom:0.4.0`, the images `.github/workflows/publish-docker.yml`
pushes on every `v*` tag. `install.sh` pulls those by default, a few seconds to a minute, and falls
back to building from this checkout when the pull fails: no network, `ghcr.io` unreachable, no
manifest for this architecture, or a working tree ahead of the last tagged release. Either path,
the script says out loud which one it took.

Moved: these used to publish as `ghcr.io/the-cybersapien/lumberroom-server` and
`ghcr.io/the-cybersapien/lumberroom`. Both stay public, frozen at 0.4.0, and take no new versions;
the images above replace them.

Force the build path yourself with `--build-local`, for a different `EMBED_PROVIDER` or
`EMBED_MODEL` than the published image carries, or to run something ahead of the last release on
purpose:

```bash
sudo ./deploy/install.sh --build-local
```

To move to a different version, set `LUMBERROOM_SERVER_IMAGE` and `LUMBERROOM_CLIENT_IMAGE` before
running the installer, or edit them in `.env`:

```bash
LUMBERROOM_SERVER_IMAGE=ghcr.io/lumberroom/lumberroom-server:0.4.0 \
LUMBERROOM_CLIENT_IMAGE=ghcr.io/lumberroom/lumberroom:0.4.0 \
  sudo -E ./deploy/install.sh
```

Both variables are read by `docker-compose.yml` and default to `0.4.0`. Neither tracks `latest`: a
memory store should not upgrade itself on a routine restart, so the version in `.env` or the shell
is the version that runs until you change it by hand.

```bash
cd lumberroom

# token mode, loopback
sudo ./deploy/install.sh

# token mode with TLS
sudo ./deploy/install.sh --domain memory.example.com --email you@example.com

# oauth mode, which requires a domain
sudo ./deploy/install.sh --domain memory.example.com --email you@example.com --auth-mode oauth
```

Add `--dry-run` first if you want to see every action without changing anything. Other flags:
`--kek-provider none|file|env` (section 8), `--build-local` (above), `--no-firewall`,
`--no-backups`, `--yes` (never prompt; fails rather than blocks on anything that needs a TTY).

Nine steps:

1. preflight: Docker, the compose plugin, architecture, available memory
2. configuration: `.env` with a fresh Postgres password and client token, `chmod 600`
3. key-encryption key: generates `secrets/lumberroom-kek`, sets ownership and mode
4. image: pulls the pinned `ghcr.io` image, or builds from source on a failed pull or `--build-local`
5. oauth credentials: prompts for the owner password, hashes it, generates the cookie secret
6. start: Postgres, the server, and Caddy when a domain was given
7. verify: polls `/readyz` until the server answers, and dumps logs if it does not
8. firewall: opens 80 and 443 with ufw or firewalld
9. backups: a daily cron at 03:15 with 14 day retention

Re-running it is safe. Existing secrets in `.env` are kept, so the token your Mac already holds
stays valid, and it will not overwrite `AUTH_MODE` or `KEK_PROVIDER` unless you pass the flag
again.

`AUTH_TOKENS` in its JSON form must be **single-quoted** in `.env`. `scripts/cargo.sh` sources
`.env` through `sh`, which strips the double quotes from an unquoted value and leaves invalid
JSON behind. Docker Compose parses `.env` itself and is fine either way, which is why this stayed
invisible until something else sourced the file.

Step 5 runs only in oauth mode, and it needs `lumberroom-server hash-password` inside the image. That
subcommand exists. If the install is running non-interactively, it stops with instructions rather
than guessing a password.

## 3b. Behind an existing reverse proxy

The host already terminates TLS on 80 and 443. Caddy stays off, the server keeps its
`127.0.0.1:8787` bind, and your proxy reaches it there. No `--email`, since nothing asks ACME for a
certificate.

```bash
sudo ./deploy/install.sh --domain memory.example.com --behind-proxy
```

```nginx
location / {
    proxy_pass http://127.0.0.1:8787;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto https;
    proxy_buffering off;          # tool responses stream
    proxy_read_timeout 300s;
    client_max_body_size 1m;
}
```

`/readyz`, `/console` and `/oauth/*` ride the same upstream, so one block covers all of them. The
login limiter reads the first entry of `X-Forwarded-For` and `$proxy_add_x_forwarded_for` appends
to what arrived, so trust an incoming header only from an edge you control: `set_real_ip_from` the
Cloudflare ranges, or set the header to `$remote_addr`.

The Caddy notes in this file do not apply here, neither the access-log redaction nor `header_up`.
Redact your own proxy log: drop the query string, or at least `q`, `ns`, `next` and `state`.

## 4. Verify on the box

```bash
curl -s localhost:8787/readyz | jq
curl -s -H "Authorization: Bearer <token>" localhost:8787/admin/whoami | jq
docker compose logs -f caddy      # watch the certificate get issued, once
```

The `caddy_data` volume also holds `/data/access.log`, with query redaction on for `q`, `ns`,
`next` and `state`. That volume sits outside `deploy/backup.sh`, which dumps Postgres, so restoring
a backup does not bring the access log back.

`/readyz` returns `ok: true` only when Postgres answers, the schema dimension matches the
configured one, and the embedder has produced a real vector. It needs no credential and describes
the deployment: Postgres reachability, the configured vector dimension, and embedder health. Keep
it behind the proxy or restrict it if that shape of information should not be public. `/admin/whoami`,
called with any credential, reports what that credential resolves to: client, read and write lists
with their ceilings, the registry-write flag, and the auth mode that produced them. It reads from
the code path that enforces the grant, so it is the answer rather than a reconstruction of one.

`lumberroom doctor` does both checks and lists the tool surface, and it wants Node, which the installer
does not put on the box. Run it from the Mac after section 6 and stay with curl here.

**The tool list a client sees depends on its grant.** Four capabilities decide it: `mayDelete`,
`mayReadHistory`, `registryWrite`, and everything else. A credential holding none of them sees five
tools; the owner's own sees ten. `/admin/whoami` reports every one of those flags, so it answers
"why can this client not call `memory_history`" without a log.
[`docs/permissions.md`](docs/permissions.md) is the reference for writing a grant, including the
asymmetry that catches people: an absent `read` means unrestricted and implies `sealedCapable`, and
it never implies `mayDelete`, `mayIngest` or `mayReadHistory`.

**`scripts/deploy-check.sh` runs every check in this section against a deployed URL**, plus the
three failures that stay invisible otherwise: an MCP endpoint that answers health checks and refuses
every tool call, a store whose key was not recognised at boot, and a schema behind the running
binary.

## 5. The OAuth production runbook

[deploy/oauth.md](deploy/oauth.md) is the detail: per-surface connection steps, the exact commands
for each secret, and the failure modes that are silent. This section is the map.

**Two secrets, both required before the server will start in oauth mode.** The installer generates
them at step 5. By hand:

```bash
read -rs OWNER_PW; echo
printf '%s' "$OWNER_PW" | docker compose run --rm -T server lumberroom-server hash-password
unset OWNER_PW
openssl rand -hex 32
```

`hash-password` reads stdin, prints one argon2id PHC string on stdout, and refuses a password under
12 characters. Put it in `.env` as `OWNER_PASSWORD_HASH`, **single-quoted**: a PHC string is full of
`$` and anything that sources `.env` with `sh` will mangle an unquoted one. The `openssl` line is
`OAUTH_COOKIE_SECRET`, which signs the consent-screen session cookie and needs at least 32
characters. The server validates both at boot with a message naming the setting, so a missing one is
a startup failure rather than a surprise at the first login.

**What the consent screen asks.** The owner logs in with the password behind
`OWNER_PASSWORD_HASH`, rate limited per `OAUTH_LOGIN_ATTEMPTS_PER_MINUTE`. The screen names the
requesting client, its redirect URI, and whether it was issued by hand or arrived through dynamic
registration. Registration alone holds nothing: a self-registered client sits in `oauth_client` with
a null `consented_at` and empty grants, and every token request against it fails until the owner
approves. Approving writes a grant profile into that client's row and stamps `consented_at`, which
is what makes it live. No restart, and the next request sees the new grant ([decision
0003](docs/decisions/0003-grants-in-the-database.md)).

**The three profiles**, defined once in `src/domain/oauth.rs`, applied identically to read and
write:

| Profile | Namespaces | Ceiling | Delete | Registry write | Sealed |
|---|---|---|---|---|---|
| `full` | `*` | sealed | yes | yes | yes |
| `standard` | `user:me`, `global`, `project:*` | open | no | no | no |
| `narrow` | `user:me`, `global` | open | no | no | no |

`full` is the owner's own client and the only profile that can delete a memory or touch the
registry. `standard` adds project notes and stops at `open`, whatever the client asks for. `narrow`
drops project notes too. `OAUTH_DEFAULT_PROFILE` preselects one on the screen; the owner still
chooses per client.

**Connecting a client.** Add a custom connector pointing at `https://<domain>/mcp`. It discovers
`/.well-known/oauth-protected-resource`, follows that to the authorization server document, registers
itself, and sends the owner to the consent screen in a browser. Claude.ai web and Cowork are
indistinguishable from this server, so decide before you connect either one whether they need
different grants; retrofitting means re-adding the connector on every device.

**Revoking a client.** Open `/console/clients` and click Revoke on that client's card: one click, no
confirmation, in effect on the client's next call. `lumberroom clients`, run from the Mac against
`/oauth/clients`, lists every OAuth row with how it registered, whether the owner consented, and
whether it is revoked, and is the way to find a `client_id` without a browser. Without browser access
to `/console`, the same effect by hand:

```bash
docker compose exec -T db psql -U lumberroom -d lumberroom -c \
  "UPDATE oauth_client SET revoked_at = now() WHERE client_id = '<client_id>' AND revoked_at IS NULL;"
```

Every authenticated request looks the client row up and refuses a revoked one, so this takes
effect on the next call rather than at token expiry, and there is no token table to sweep. Confirm
it with `/admin/whoami` using the revoked client's access token.

**Rolling back.** Set `AUTH_MODE=token` and restart. Static-token clients were never affected.
Browser clients lose their connection and reconnect when you switch back.

## 6. Wire your Mac

From a clone of this repo on the Mac:

```bash
# token mode
./client/wire-mac.sh --url https://memory.example.com --token <token from the installer>

# oauth mode: no token here, Claude Code runs its own flow on first use
./client/wire-mac.sh --url https://memory.example.com --oauth-mode
```

Idempotent, takes `--dry-run`, and backs up every file it edits to `<file>.lumberroom.bak`.
It registers the MCP server with Claude Code at user scope, installs `lumberroom` to
`~/.local/bin`, installs the SessionStart hook to `~/.claude/hooks/lumberroom-bootstrap.sh`, appends
the hook to `~/.claude/settings.json` without disturbing hooks already there, and writes the
memory rules into `~/.claude/CLAUDE.md` between managed markers.

Start a new Claude Code session and run `/mcp`. The `memory` server should be connected.

## 7. Prove the loop

```bash
LUMBERROOM_URL=https://memory.example.com LUMBERROOM_TOKEN=<token> \
  ./scripts/done-when-test.sh
```

Session A states a fact and expects the model to write it unprompted. Session B is a fresh
session that never mentions the fact and has to recover it through the SessionStart hook. Neither
session touches your `~/.claude`: the MCP server and the hook are supplied per invocation.

The three later scripts take the same shape and none has been run against a live deployment:

```bash
LUMBERROOM_URL=... LUMBERROOM_OWNER_PASSWORD=...                    ./scripts/oauth-flow-test.sh
LUMBERROOM_URL=... LUMBERROOM_FULL_TOKEN=... LUMBERROOM_NARROW_TOKEN=...  ./scripts/policy-test.sh
LUMBERROOM_URL=... LUMBERROOM_TOKEN=...                             ./scripts/correction-test.sh
```

`policy-test.sh` needs two credentials already in `AUTH_TOKENS`: one granting `*` at `sealed`, and
one restricted so it excludes the test namespace. Run it after every grant change, which is the
point of it being a script.

## 8. The key-encryption key

Nothing writes an encrypted row until this is settled. `KEK_PROVIDER` picks where the key comes
from, and the default refuses rather than degrades.

| `KEK_PROVIDER` | Key comes from | Defends | Does not defend |
|---|---|---|---|
| `none` (default) | nowhere | nothing, because a `private` write is refused | |
| `file` | `KEK_PATH`, owner-only | a stolen database dump, a leaked backup | a stolen disk or disk image, root on the box, a live compromise |
| `env` | `KEK_ENV_VAR` | the same | the same, plus anything that can read the container's environment |

Said plainly for `file`, because this is the one people get wrong: a key in a file on the same disk
as the database defends a dump and defends a backup, and does not defend the disk. Whoever takes the
volume takes both halves. The provider refuses a key file readable by group or other, which is
hygiene rather than a boundary. `env` is weaker again, because `docker inspect`, `/proc` and a core
dump all read a container's environment; it ships for platforms that offer environment variables and
no writable secret path.

Nothing software-only defends a live compromise. The server decrypts to answer a search, so an
attacker who is the server reads what the server reads. That holds for a hosted KMS too, and
[decision 0004](docs/decisions/0004-kek-provider.md) says so: root on the box calls the same API
with the same instance identity.

**Generating and installing one.** `./deploy/install.sh --kek-provider file` writes
`secrets/lumberroom-kek`, chowns it to `10001:10001` (the uid the Dockerfile pins for the `lumberroom` user) and
chmods it 600. The compose file bind-mounts that path. By hand:

```bash
docker compose run --rm -T server lumberroom-server generate-kek > secrets/lumberroom-kek
sudo chown 10001:10001 secrets/lumberroom-kek && chmod 600 secrets/lumberroom-kek
```

**The boot fingerprint check, and the rule it enforces.** `kek_state` (migration
`20260819000008_encryption.sql`) holds one row per tenant: the `kek_id`, a fingerprint, the provider
that supplied the key, and when it was last verified. The fingerprint is HMAC-SHA256 of a frozen
label under the KEK, truncated to 128 bits, so it names a key without helping anyone recover it. At
boot the server reads the live key, computes the fingerprint, and compares. First run records it. A
match sets the verified flag. A mismatch means a swapped, rotated or wrong key, and the server logs
it and keeps refusing private writes rather than encrypting under a key this store was not sealed
with.

`REQUIRE_VERIFIED_KEK=true`, the default, is what makes that a rule the code holds: no encrypted row
is written until a restart has proved the key can be recovered. That is the step in the migration
order that can strand data, and stranded data here has no repair. `lumberroom-server verify-kek` runs the same
comparison on demand, and on a store with nothing recorded it writes the fingerprint, exactly as a
boot would.

**Escrow is open, and it blocks turning this on.** Losing the key makes every `private` row
permanently unreadable. Per-row keys are wrapped in the row itself, so a perfect restore of a perfect
dump yields ciphertext and nothing else, in every backup ever taken. The research calls escrow a
requirement rather than an edge case, and decision 0004 leaves it open on purpose because it is the
owner's call: whether a wrapped offline copy of the KEK exists, where it lives, and who can reach
it. The default by omission is no escrow, which is a single point of failure nobody has agreed to.
Answer it before switching a namespace default to `private` in production, not after.

Rotation stays manual. `KEK_ID` is written on every encrypted row, so a rotation is distinguishable
from data loss, and nothing rewraps a row today.

## 9. Backups

`deploy/backup.sh` writes `backups/lumberroom-YYYY-MM-DD.sql.gz.age`, mode 600, and prunes past 14 days.
It refuses to write a plaintext dump: no `age` binary and no configured recipient is a hard stop
rather than a fallback. `BACKUP_ALLOW_PLAINTEXT=true` opts back in for a box that holds no real data.

```bash
age-keygen -o backup-key.txt          # on the Mac, not on the server
# put the age1... public line in BACKUP_AGE_RECIPIENT in .env on the box
./deploy/backup.sh
./deploy/backup.sh --restore backups/lumberroom-2026-08-19.sql.gz.age
```

Restore asks for confirmation and then wants `docker compose restart server`.

**The recipient key is not the KEK, and that separation is the design.** The KEK decrypts
`private` rows inside the running server and has to live on the box. A backup key exists to open an
archive somewhere else, later, after the box is gone. Using one key for both means the key travels
with the dumps it protects, and one compromise opens every historical archive at once. Keep
`backup-key.txt` off the server: the Mac, plus one offline copy.

Encrypted dumps and the restore path have not been exercised against a real Postgres.

## 9b. The cleanup schedule

Two halves, both inside the product, no cron on the box.

The deterministic pass runs on a timer inside the server, `CLEANUP_INTERVAL_SECS`, default 3600.
Zero turns it off and is the only way to. It writes proposals into a queue and retires nothing.

The model pass is its own container, off unless you ask for it:

```bash
docker compose --profile cleanup up -d
docker compose --profile cleanup logs -f cleanup
```

It needs `LUMBERROOM_CLEANUP_TOKEN` set to a client carrying `mayIngest`, which `deploy/install.sh` mints
on a fresh `.env`. On an `.env` that predates this, add the client and the variable by hand.
`CLEANUP_DAILY_AT`, `CLEANUP_PROVIDER`, `CLEANUP_MODEL` and `CLEANUP_MIN_SIMILARITY` are the rest of
the settings, and the provider key comes from `ZAI_API_KEY` or `OPENROUTER_API_KEY` in `.env`.

**The daily pass sends open-row text to a provider. The in-server pass sends nothing anywhere.**
That split is a boundary rather than packaging: the server holds the KEK, so the provider call runs
in a process that does not.
[`docs/cleanup-schedule.md`](docs/cleanup-schedule.md) has the rest, and
[decision 0011](docs/decisions/0011-cleanup-proposes.md) has the reasoning.

Nothing here retires a memory on its own. The queue is at `/console/cleanup` and through
`lumberroom cleanup list`.

## 10. Upgrading

**Migrations are forward-only.** `sqlx` embeds them at compile time, so once a newer binary has
migrated the store, an older image cannot boot against it. This bit during verification: the
running image knew migrations 1 to 3, a newer binary applied 4 to 8, and the old image would not
start. The safe order is back up, then get the new image, then recreate:

```bash
./deploy/backup.sh
git pull
LUMBERROOM_SERVER_IMAGE=ghcr.io/lumberroom/lumberroom-server:0.4.0 \
LUMBERROOM_CLIENT_IMAGE=ghcr.io/lumberroom/lumberroom:0.4.0 \
  docker compose pull server cleanup
docker compose up -d
```

Pin the new version in `.env` too, or the next `up -d` with no override falls back to `0.4.0`.
Building from source instead is `docker compose build server cleanup && docker compose up -d`, or
re-run the installer with `--build-local`.

`docker compose up -d` recreates the container against the new image, which is what runs the
migrations at boot. `docker compose restart` is not an upgrade: it reuses the image id already
running, so a pulled but unbuilt change never takes effect and no new migration runs. Stateless
transport means connected clients survive either one.

---

## Using an external issuer instead

[decision 0002](docs/decisions/0002-built-in-oauth-server.md) made the built-in authorization server
the position, and `AUTH_MODE=oidc` is the documented exit from it. Take it when a vulnerability
class in this code outruns what the project can keep up with, or when a client needs a protocol
feature an external provider already ships.

See [deploy/logto.md](deploy/logto.md), which has never been executed. Short version: bring up the
`logto` profile, register the MCP server as an API resource, set `AUTH_MODE=oidc` with the issuer
and audience, restart. The server validates JWTs and serves RFC 9728 metadata; it never issues
tokens in that mode. Reversing costs an issuer to configure and the grants to re-key onto its client
ids. The schema, the tools and the database are unaffected.

## Changing the embedding model

The `vector(768)` column is fixed in SQL, and the server refuses to start when `EMBED_DIM`
disagrees with it. Changing providers at the same width needs no migration, but vectors from two
models do not compare, so re-embed:

```bash
docker compose exec -T db psql -U lumberroom -d lumberroom \
  -c "SELECT id, content FROM memory WHERE embedding_model <> '<new model id>'"
```

There is no re-embed command. With a store this size, the practical move is to dump the content,
truncate, and rewrite through `lumberroom write`. That does not reach `private` rows, whose `content`
column is NULL. If you are changing the width as well, write a new migration that alters the column
type and rebuilds the HNSW index, then set `EMBED_DIM` to match.

---

## Troubleshooting

**`/readyz` returns 503 with `embedder_degraded: true`.** The model failed to load and
`EMBED_ALLOW_FALLBACK` sent it to the hash embedder. Anything written meanwhile retrieves badly.
Check `docker compose logs server`, fix the cause, restart, and rewrite those rows.

**The server will not start in oauth mode.** It names the setting. The three it refuses without are
`OWNER_PASSWORD_HASH`, `OAUTH_COOKIE_SECRET` and an `https` or loopback `PUBLIC_URL`. A hash mangled
by an unquoted `.env` line reads as present and fails at login instead, with the reason only in the
log, so check that the value in `.env` is single-quoted and still starts with `$argon2id$`.

**A private write is refused.** Two messages, two causes. "no encryption key is configured" means
`KEK_PROVIDER=none`; set a provider and restart. "the encryption key was not verified at boot" means
the fingerprint check found a key this store was not sealed with, and the log line carries both
`recorded_kek_id` and `live_kek_id`:
`docker compose logs server | grep kek_id`. `lumberroom-server verify-kek` runs the same comparison on demand.
Restore the original key, or accept that the sealed rows are lost and clear `kek_state`.

**Caddy will not issue a certificate.** Port 80 must reach the box from the internet. Check the
cloud firewall first, then the host firewall, then `deploy/oracle-notes.md` for the iptables
rules Oracle images ship with.

**Claude Code shows the server as failed.** Run `lumberroom doctor` from the Mac, which `wire-mac.sh`
installed to `~/.local/bin`. A 401 means the token in `~/.claude.json` does not match `AUTH_TOKENS`
on the box. Re-run `wire-mac.sh` with the right token.

**A browser client fails before it ever offers to authenticate.** An unauthenticated request must
come back `401` with a `WWW-Authenticate` header carrying the resource-metadata pointer, not a `200`
with an error body. Claude Code's fallback probing masks this whole class of bug, so a green result
from Claude Code proves nothing about Claude.ai or ChatGPT. Check the header by hand:
`curl -si https://<domain>/mcp | grep -i www-authenticate`.

**The model never calls the tools on its own.** Check `lumberroom stats --hours 168`. If
`unprompted` stays at zero, the digest is arriving but the write rule is not: confirm the
`lumberroom` block is in `~/.claude/CLAUDE.md`.

**The SessionStart hook produces nothing.** It exits 0 silently by design so it can never block a
session. Run it by hand:
`LUMBERROOM_BIN=~/.local/bin/lumberroom ~/.claude/hooks/lumberroom-bootstrap.sh`.

**A reindex or restore fails with "could not resize shared memory segment".** Docker's default
`/dev/shm` is 64MB and a parallel HNSW build needs closer to 512MB. The compose file sets
`shm_size: 1gb` for this reason. If you run Postgres outside compose, set it there too.

**Postgres is unreachable after a reboot.** `docker compose up -d`. Compose restarts containers
with `unless-stopped`, so this only happens if they were stopped by hand.
