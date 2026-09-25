# lumberroom

The client for a [lumberroom](https://lumberroom.cloud) memory server: one store of durable facts
that every AI tool you use reads and writes, with per-client policy deciding what each may see.

This crate is the command line half. It runs on the machine holding your transcripts and your
credentials, and it talks to a server you host.

```bash
cargo install lumberroom
lumberroom doctor
```

Homebrew works too, and carries a prebuilt binary rather than compiling one:

```bash
brew install lumberroom/lumberroom/lumberroom
```

The [releases page](https://github.com/lumberroom/lumberroom/releases) also carries plain
binaries, `.tar.gz` archives, and a `.deb` and `.rpm` per architecture, each covered by one
`SHA256SUMS`.

`doctor` reports the endpoint, whether your credential is accepted, and which tools it opens. It is
the command to run first and the command to run when something is wrong.

## What it does

```bash
lumberroom search "how do we deploy"
lumberroom write "the cleanup pass runs every six hours" --namespace project:acme
lumberroom registry get host services.acme.database
lumberroom seal aws-deploy-key --namespace credentials:aws
lumberroom review --stale --conflicts
lumberroom stats --hours 168 --by-client
lumberroom ingest run
lumberroom cleanup daemon
```

Two of those are worth calling out.

`seal` encrypts on this machine and sends bytes the server holds no key for. The key sits at
`~/.config/lumberroom/seal-key`, 0600, generated on first use, and it never travels. Lose it and
those items are gone, including from every backup.

`ingest` walks Claude Code and Codex transcripts, cuts them into spans, asks a model for candidate
facts, and queues proposals rather than writing them. Nothing reaches the store until you approve it.

## Configuration

`~/.config/lumberroom/config.json` holds the endpoint and the credential, and `lumberroom login`
writes it through an OAuth 2.1 flow with PKCE. A static bearer token in `LUMBERROOM_TOKEN` works
too, and takes precedence.

## The server

This crate talks to a server; it is not one. Running your own takes a Linux box with Docker:

```bash
git clone https://github.com/lumberroom/lumberroom.git && cd lumberroom
sudo ./deploy/install.sh
```

Full documentation, including how to grant a client access to one project and nothing else, lives in
the [repository](https://github.com/lumberroom/lumberroom).

## Licence

Apache-2.0.
