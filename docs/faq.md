# Questions people actually ask

Short answers with a pointer to the long one. If a question here has no link, the answer is the
whole answer.

## Running it

**How do I deploy this?**
One command on any Linux box with Docker. `sudo ./deploy/install.sh` gives you token mode on
loopback, which is what a Mac with Claude Code needs. Add `--domain` and `--auth-mode oauth` when a
browser has to reach the server. [DEPLOY.md](../DEPLOY.md) is the runbook;
[deploy/oauth.md](../deploy/oauth.md) covers the production path per surface.

**Do I need a domain name?**
Only for the surfaces that speak OAuth: Claude.ai, ChatGPT, anything in a browser or on a phone.
Without `--domain` the server binds `127.0.0.1:8787`, opens no public port, and you reach it over an
SSH tunnel.

**How do I connect a client?**
`LUMBERROOM_TOKEN=<token> ./client/wire-mac.sh --url <url>` registers the MCP server with Claude Code,
installs the session hook and appends the write rule. For the OAuth surfaces, the client registers
itself and waits at the consent screen. [connect-claude-code.md](connect-claude-code.md) walks the
Mac path end to end.

**How do I tell whether it is working?**
`lumberroom doctor` checks connectivity, auth, readiness and the tool list. `/healthz` needs no
credentials, `/readyz` reports the embedder and checks the schema dimension against the configured
one, and `/admin/whoami` answers what the credential you present resolves to. To prove the whole
loop, `./scripts/done-when-test.sh` states a fact in one session and recovers it in a fresh one.

**Which port, which container, where are the logs?**
`docker compose ps` and `docker compose logs -f server`. The server is `lumberroom-server-1` on
`127.0.0.1:8787` and Postgres is `lumberroom-db-1` on `127.0.0.1:5432`.

## Access and credentials

**How do I manage what each client can see?**
Sign in to `/console/clients`. Every client is a card showing what it reads, what it writes and what
else it may do, with a **Change access** editor behind it.
[managing.md](managing.md#clients) has the walkthrough.

**How do I give a client access to one project only?**
Open Change access, pick a shape, choose "Only these namespaces", tick the project. The shape decides
how deep the client sees; the scope decides where. Ticking `project:sivella` under Read and write
grants it that namespace at `sealed` for reads and `open` for writes, and nothing else.

**Does changing a grant break the client's connection?**
No. The grant lives on the client row and every request reads it there, so a change lands on the
client's next call. Nothing reconnects and no token is reissued.

**How do I revoke a client?**
The Revoke button on its card, one click. It kills the client and every token it holds, effective on
the next request. `lumberroom clients` lists client ids if you need one, and
[deploy/oauth.md](../deploy/oauth.md) carries the psql statement for a deployment not running the
console.

**What is the difference between a static token and an OAuth client?**
Where the grant lives. A static bearer token's grant sits in `AUTH_TOKENS` in `.env`, read once at
boot, so changing it means editing the file and restarting. An OAuth client's grant is a row in
`oauth_client` that the console and the consent screen both write, and it changes without a restart.
The two never copy into each other ([decision 0003](decisions/0003-grants-in-the-database.md)).

**Both at once, or one or the other?**
Both. `AUTH_MODE` selects what is accepted on top of static tokens rather than instead of them, so
switching OAuth on does not break the CLI or the hooks.

**How do I rotate a static token?**
Edit its entry in `AUTH_TOKENS`, restart the server, rewire the client with the new value. There is
no live-reload path for that variable.

**Why can a client not see a fact I know is there?**
Call `/admin/whoami` with that client's credential. It prints the read and write lists and the
capability flags from the code path that enforces them. Then check three things in order: whether the
namespace matches the client's globs, whether the row classifies above the client's ceiling for that
namespace, and whether the row is retired and the client lacks `mayReadHistory`.

**Which tools does a capability open?**
[permissions.md](permissions.md#which-tools-each-capability-opens) has the table. `mayDelete` opens
`memory_forget` and no shape grants it, so a fresh install deletes nothing until you tick it
yourself.

## Data and keys

**What happens if I lose the key-encryption key?**
Every `private` row becomes unreadable, in the database and in every backup. `secrets/lumberroom-kek`
is the file. Back it up somewhere the server does not hold. Nothing rewraps and there is no escrow
([decision 0004](decisions/0004-kek-provider.md)).

**How do I know the key still matches the store?**
`docker compose exec -T server lumberroom-server verify-kek`, or read the health line on any console
screen. The server refuses to boot on a key that does not match, rather than writing rows nothing can
read later.

**How do I back up?**
`deploy/backup.sh` writes one age-encrypted `pg_dump` a day and keeps fourteen. Back up
`secrets/lumberroom-kek` separately and somewhere else, since a dump without the key leaves every
private row as ciphertext.

**What does `private` actually protect?**
The verbatim text of a row from whoever holds the database. It leaks the gist, because the embedding
stays plaintext and published inversion work recovers much of a short text from one. It protects
nothing from the live server, which decrypts to answer a search. `sealed` is the level that keeps the
server out: the client encrypts before sending and the server holds no key.
[research/encryption-and-sensitivity.md](research/encryption-and-sensitivity.md) carries the
citations.

**Why was my write refused?**
Four common reasons. The content tripped the credential tripwire, which refuses anything shaped like
a secret. The namespace classifies `private` and no key is configured, so the server refuses rather
than storing it in the clear. `occurred_at` fell inside the near-now fence or lay in the future. Or the write asked for a
sensitivity the credential's ceiling does not reach. The refusal names which.

**How do I delete something for good?**
`lumberroom forget <id>`, with `--dry-run` first. It needs `mayDelete`, which no shape grants. The
row goes and its wrapped key goes with it.

**How do I correct a fact rather than write a second one?**
Open the fact in the console and use Replace this fact, or call `memory_write` again with
`supersedes` pointing at the row you are replacing. A write that lands near an existing fact returns
`possible_conflicts`, which is the server telling the model what it declined to merge.

## Keeping it healthy

**How do I keep the store from filling with near-duplicates?**
The cleanup passes propose duplicates, paraphrases, contradictions and stale rows into a queue you
decide, and never retire a row on their own. [cleanup-schedule.md](cleanup-schedule.md) covers the
two cadences; [managing.md](managing.md#the-cleanup-queue) covers deciding the queue.

**Do the models actually call these tools, or do I have to ask?**
`lumberroom stats` answers exactly that. Every tool call writes a row, refused ones included, and
`unprompted` separates the model deciding from you or the hook forcing it. `--by-client` splits it
per credential.

**How good is retrieval?**
[benchmarks.md](benchmarks.md) is the one page carrying every figure with the run that produced it
and what the number does not say. `./scripts/eval-longmemeval.sh` is the standing gate.

**How do I run the tests?**
`./scripts/cargo.sh test -j 1`. The `-j 1` is not optional: linking two test binaries at once gets
the linker OOM-killed in the container, and it reads as a compile error. The suite skips rather than
fails when no database is reachable, so read the count and not just the exit code.
[CONTRIBUTING.md](../CONTRIBUTING.md) has the rest.

**What is known broken or missing?**
The "What is not built yet" section of the [README](../README.md), and [VERIFY.md](../VERIFY.md) for
which gates cover what.
