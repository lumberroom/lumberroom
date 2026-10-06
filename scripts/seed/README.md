# Synthetic stores

`seed.py` builds a store full of fiction that is shaped like a real one, for tests, demos and
measurements that need more than a handful of rows.

```sh
# Ask a model for 400 rows across five invented projects (about 25 model calls).
scripts/seed/seed.py generate --out store.jsonl --rows 400

# Write them into a running server through its MCP tools.
scripts/seed/seed.py load store.jsonl --url http://127.0.0.1:8787 --token-file ~/.lumberroom-token

# Count what a file holds.
scripts/seed/seed.py stats store.jsonl
```

Generation needs either the `claude` CLI logged in, or `ANTHROPIC_API_KEY` for the Messages API.
The CLI runs with no tools, no MCP servers and no settings, so a generation run cannot reach a
memory server. Loading needs only the server and a token whose grant writes `*` and holds
`registryWrite`. The token's client name becomes every row's `source`.

`load` keeps a `<file>.ids.json` beside the input, mapping each row's key to the id the server
returned. Run it again after a failure and it skips what already landed. A row that supersedes
another is written with the server id of its target, so load a file in order and into one store.

## The shape

`PROFILE` in `seed.py` holds the numbers: the share of rows per namespace (one project holds 44%),
the length spread, how many rows carry `occurred_at`, how many restate an earlier row with one
detail changed (8%, aimed at the band between `CONFLICT_THRESHOLD` and `DEDUPE_THRESHOLD`) and how
many supersede one (9%). They are design targets, and no prompt carries anything but the invented
world and the rows the model already wrote.

A restatement is a request to the model, so how many pairs land at or above 0.90 depends on the
embedder. `scripts/measure-conflicts.sh` counts the stored pairs after a load.

## Reusing a world

The first line of a generated file holds the invented owner and projects. To write more rows
about the same world, extract it and pass `--world`:

```sh
head -1 store.jsonl | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["world"]))' > world.json
scripts/seed/seed.py generate --out more.jsonl --rows 200 --world world.json --seed 2
```
