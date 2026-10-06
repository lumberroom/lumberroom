#!/usr/bin/env python3
"""Synthetic stores for lumberroom: a model writes the facts, this script shapes and loads them.

    seed.py generate --out store.jsonl [--rows 400] [--projects 5] [--model M] [--backend B]
    seed.py load store.jsonl --url http://127.0.0.1:8787 --token-file ~/.cache/token
    seed.py stats store.jsonl

`generate` asks a model for a fictional owner, their projects and their facts, and writes one JSON
object per line. `load` writes those lines into a running server through its MCP tools, the path
every real client takes, so the server embeds, dedupes, applies the tripwire and records conflicts
the way it would for real traffic. Generation costs model calls; loading costs none, so commit a
generated file and reload it as often as a test needs.

The shape of a store comes from PROFILE below, a set of design targets for a working developer's
store. Everything the model writes is fiction about people and projects that do not exist.

Two backends. `claude-cli` runs `claude -p` with no tools and no MCP servers, on whatever login the
machine has. `api` calls the Messages API with ANTHROPIC_API_KEY. `auto` picks `api` when the key is
set. Standard library only, so it runs on any box with Python 3.10.
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

# Design targets. One project dominates, as it does for anyone with a main job and side work, and a
# few percent of rows restate or replace an earlier one, which is what gives the conflict sweeper
# and supersession something to find.
PROFILE = {
    "namespace_share": {"main_project": 0.44, "other_projects": 0.31, "user:me": 0.12,
                        "global": 0.08, "personal": 0.05},
    # Restatements of an earlier row in the same namespace with one detail changed. These land in
    # the band between CONFLICT_THRESHOLD (0.90) and DEDUPE_THRESHOLD (0.97) when they work.
    "variant_rate": 0.08,
    # Rows that replace an earlier fact (a changed decision, a moved host, a corrected number).
    "supersede_rate": 0.09,
    "occurred_at_rate": 0.25,
    "private_rate_personal": 0.6,
    "length_chars": {"median": 380, "p90": 900, "max": 1600},
    # Per-slot targets that reproduce that spread; asked for as one median, a model writes every
    # row the same length.
    "length_buckets": [((80, 200), 0.20), ((250, 500), 0.55), ((600, 900), 0.18), ((1000, 1500), 0.07)],
    "tags": {"min": 2, "max": 6},
    "kinds": {
        "decision": 0.20, "trap": 0.14, "status": 0.14, "infrastructure": 0.12,
        "preference": 0.10, "measurement": 0.10, "research": 0.08, "rule": 0.07, "person": 0.05,
    },
}

STYLE = """How a stored fact reads in this store:
- One fact per row, written to stand alone in six months with no surrounding conversation.
- It names its subject and carries the numbers, identifiers, file paths, branch names, versions,
  hosts, ports and dates the fact turns on. Dates are written out: "14 September 2026".
- It carries the cause, the scope and the reversal condition when the fact turns on them
  ("Reverses if ...", "because ...", "only on ...").
- No hedges, no evaluative adjectives, no em dashes, no narration of how the fact was learned.
- Kinds of fact: a decision with its reason, a trap that cost time and what to do instead, a status
  or interface-lock note listing exactly what landed where, an infrastructure fact (where something
  lives, how it deploys, which port), a stated preference or standing rule of the owner, a
  measurement with its numbers and where it came from, a research finding with its source, a fact
  about a person (role, what they own; never contact details or anything sensitive).
- Never write a secret, a password, a token, an API key or a private key, not even a fake one. A
  credential is referred to by where it lives ("the deploy key lives in 1Password under ...").
- Lengths vary: some rows are one sentence, most are three to five, a few are a dense paragraph
  listing files and values."""

KIND_HINT = {
    "decision": "a decision, who made it and when, the reason, what lost",
    "trap": "a trap: what went wrong, the evidence, what to do instead",
    "status": "a status or interface-lock note: branch, files, what landed, what is stubbed",
    "infrastructure": "infrastructure: a host, port, path, deploy command or service layout",
    "preference": "a standing preference or rule the owner stated, with the date",
    "measurement": "a measurement: numbers, units, the dataset and where it was taken",
    "research": "a research finding about a third-party tool or prior art, with its source URL",
    "rule": "a working rule or convention for this codebase",
    "person": "a fact about a collaborator: role, what they own, how they like to work",
}

# ---------------------------------------------------------------------------------------------
# Model backends


class ModelError(RuntimeError):
    pass


def call_model(prompt: str, schema: dict, *, model: str, backend: str, system: str) -> dict:
    if backend == "api":
        return _call_api(prompt, schema, model=model, system=system)
    return _call_cli(prompt, schema, model=model, system=system)


def _call_cli(prompt: str, schema: dict, *, model: str, system: str) -> dict:
    claude = shutil.which("claude")
    if not claude:
        raise ModelError("claude CLI not found; install it or use --backend api")
    # No tools, no MCP servers, no settings and a scratch cwd: the call must not read this repo,
    # load a CLAUDE.md, or reach a memory server and write the fiction somewhere real.
    cmd = [claude, "-p", "--output-format", "json", "--model", model,
           "--tools", "", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
           "--setting-sources", "", "--no-session-persistence",
           "--system-prompt", system, "--json-schema", json.dumps(schema)]
    with tempfile.TemporaryDirectory(prefix="lr-seed-") as cwd:
        proc = subprocess.run(cmd, input=prompt, capture_output=True, text=True, cwd=cwd,
                              timeout=900)
    if proc.returncode != 0:
        raise ModelError(f"claude -p exited {proc.returncode}: {proc.stderr.strip()[:400]}")
    try:
        envelope = json.loads(proc.stdout)
    except json.JSONDecodeError as e:
        raise ModelError(f"claude -p printed no JSON: {proc.stdout[:300]}") from e
    if envelope.get("is_error"):
        raise ModelError(f"claude -p reported an error: {str(envelope.get('result'))[:400]}")
    if isinstance(envelope.get("structured_output"), dict):
        return envelope["structured_output"]
    return _json_from_text(envelope.get("result", ""))


def _call_api(prompt: str, schema: dict, *, model: str, system: str) -> dict:
    key = os.environ.get("ANTHROPIC_API_KEY")
    if not key:
        raise ModelError("ANTHROPIC_API_KEY is not set")
    # Forced tool use is the API's structured-output path that every current model honours.
    body = {
        "model": model,
        "max_tokens": 16000,
        "system": system,
        "messages": [{"role": "user", "content": prompt}],
        "tools": [{"name": "emit", "description": "Return the result.", "input_schema": schema}],
        "tool_choice": {"type": "tool", "name": "emit"},
    }
    req = urllib.request.Request(
        "https://api.anthropic.com/v1/messages",
        data=json.dumps(body).encode(),
        headers={"x-api-key": key, "anthropic-version": "2023-06-01",
                 "content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=600) as res:
            out = json.load(res)
    except urllib.error.HTTPError as e:
        raise ModelError(f"Messages API {e.code}: {e.read()[:400]!r}") from e
    for block in out.get("content", []):
        if block.get("type") == "tool_use":
            return block["input"]
    raise ModelError(f"Messages API returned no tool call: {str(out)[:300]}")


def _json_from_text(text: str) -> dict:
    m = re.search(r"```(?:json)?\s*(\{.*\})\s*```", text, re.S) or re.search(r"(\{.*\})", text, re.S)
    if not m:
        raise ModelError(f"no JSON object in model output: {text[:300]}")
    return json.loads(m.group(1))


def with_retries(fn, *, tries: int = 3, what: str):
    for attempt in range(1, tries + 1):
        try:
            return fn()
        except (ModelError, json.JSONDecodeError, subprocess.TimeoutExpired, KeyError) as e:
            if attempt == tries:
                raise
            print(f"  {what}: attempt {attempt} failed ({e}); retrying", file=sys.stderr)
            time.sleep(2 * attempt)


# ---------------------------------------------------------------------------------------------
# Generation

WORLD_SCHEMA = {
    "type": "object",
    "properties": {
        "owner": {"type": "object", "properties": {
            "name": {"type": "string"}, "summary": {"type": "string"},
            "machines": {"type": "array", "items": {"type": "string"}},
        }, "required": ["name", "summary", "machines"]},
        "projects": {"type": "array", "items": {"type": "object", "properties": {
            "slug": {"type": "string"}, "summary": {"type": "string"},
            "stack": {"type": "string"},
            "hosts": {"type": "array", "items": {"type": "string"}},
            "people": {"type": "array", "items": {"type": "string"}},
            "vocabulary": {"type": "array", "items": {"type": "string"}},
        }, "required": ["slug", "summary", "stack", "hosts", "people", "vocabulary"]}},
        "personal_areas": {"type": "array", "items": {"type": "string"}},
    },
    "required": ["owner", "projects", "personal_areas"],
}

ROWS_SCHEMA = {
    "type": "object",
    "properties": {"rows": {"type": "array", "items": {"type": "object", "properties": {
        "slot": {"type": "integer"},
        "content": {"type": "string"},
        "tags": {"type": "array", "items": {"type": "string"}},
        "occurred_at": {"type": ["string", "null"]},
    }, "required": ["slot", "content", "tags"]}}},
    "required": ["rows"],
}

REGISTRY_SCHEMA = {
    "type": "object",
    "properties": {"entries": {"type": "array", "items": {"type": "object", "properties": {
        "kind": {"type": "string", "enum": ["host", "service", "credential-ref", "model-route",
                                             "dataset"]},
        "key": {"type": "string"},
        "value": {},
        "namespace": {"type": "string"},
    }, "required": ["kind", "key", "value", "namespace"]}}},
    "required": ["entries"],
}

SYSTEM = ("You write synthetic seed data for a memory store used in software tests. Everything "
          "you write is fiction: invented people, companies, projects, hosts and numbers. Reply "
          "with the structured result only.")


@dataclass
class Slot:
    slot: int
    kind: str
    with_date: bool
    length: tuple = (250, 500)
    variant_of: dict | None = None    # an earlier row this one restates with one detail changed
    supersedes: dict | None = None    # an earlier row this one replaces


@dataclass
class Namespace:
    name: str
    count: int
    brief: str
    sensitivity: str | None = None
    rows: list = field(default_factory=list)


def gen_world(args) -> dict:
    prompt = f"""Invent the world for a synthetic memory store.

One owner: a software developer who runs {args.projects} projects of their own and uses several AI
coding agents that share one memory server. Give them a name, a two-sentence summary, and two or
three machine names they work on.

{args.projects} projects with distinct domains (for example a payments service, a mobile app, a
data pipeline, a game server, a developer tool). For each: a lowercase kebab-case slug, a
three-sentence summary, the stack, two to four host or service names, two or three collaborators
by invented name and role, and ten domain words or identifiers its facts would use. Every name is
invented: no real company, product or brand in a slug, host or project name.

Two personal areas as lowercase slugs, such as finance or health.

Facts in this store are dated between {args.start} and {args.end}.
{f"Theme to lean towards: {args.theme}" if args.theme else ""}"""
    return with_retries(lambda: call_model(prompt, WORLD_SCHEMA, model=args.model,
                                           backend=args.backend, system=SYSTEM), what="world")


def plan_namespaces(world: dict, rows: int, rng: random.Random) -> list[Namespace]:
    share = PROFILE["namespace_share"]
    projects = world["projects"]
    main, others = projects[0], projects[1:]
    owner = world["owner"]
    out = [Namespace(f"project:{main['slug']}", max(1, round(rows * share["main_project"])),
                     _project_brief(main))]
    per_other = max(1, round(rows * share["other_projects"] / max(1, len(others))))
    out += [Namespace(f"project:{p['slug']}", per_other, _project_brief(p)) for p in others]
    out.append(Namespace("user:me", max(1, round(rows * share["user:me"])),
                         f"Facts about the owner {owner['name']}: {owner['summary']} Their "
                         f"preferences, standing rules for agents, working habits, machines "
                         f"({', '.join(owner['machines'])})."))
    out.append(Namespace("global", max(1, round(rows * share["global"])),
                         "Facts true across every project: shared infrastructure, credential "
                         "locations (never values), conventions, toolchains, the hosts "
                         + ", ".join(h for p in projects for h in p["hosts"][:1]) + "."))
    areas = world.get("personal_areas") or ["finance"]
    per_area = max(1, round(rows * share["personal"] / len(areas)))
    for a in areas:
        sens = "private" if rng.random() < PROFILE["private_rate_personal"] else None
        out.append(Namespace(f"personal:{a}", per_area,
                             f"The owner's personal {a} notes: plain facts, plans and rules. No "
                             f"account numbers, no identifiers that could be real.", sens))
    return out


def _project_brief(p: dict) -> str:
    return (f"Project {p['slug']}: {p['summary']} Stack: {p['stack']}. Hosts: "
            f"{', '.join(p['hosts'])}. People: {', '.join(p['people'])}. Vocabulary: "
            f"{', '.join(p['vocabulary'])}.")


def pick_kind(rng: random.Random) -> str:
    kinds = PROFILE["kinds"]
    return rng.choices(list(kinds), weights=list(kinds.values()))[0]


def gen_namespace(ns: Namespace, world: dict, args, seed: int, log) -> list[dict]:
    rng = random.Random(seed)
    done: list[dict] = []
    batch = args.batch
    n = 0
    while n < ns.count:
        size = min(batch, ns.count - n)
        slots = []
        for i in range(size):
            buckets = PROFILE["length_buckets"]
            s = Slot(slot=i + 1, kind=pick_kind(rng),
                     with_date=rng.random() < PROFILE["occurred_at_rate"],
                     length=rng.choices([b for b, _ in buckets], weights=[w for _, w in buckets])[0])
            # Relations point only at rows already written, so a load in file order always finds
            # the target id. The first batch of a namespace has nothing to point at.
            if done:
                r = rng.random()
                if r < PROFILE["variant_rate"]:
                    s.variant_of = rng.choice(done)
                elif r < PROFILE["variant_rate"] + PROFILE["supersede_rate"]:
                    live = [d for d in done if not d.get("_retired")]
                    if live:
                        s.supersedes = rng.choice(live)
                        s.supersedes["_retired"] = True
            slots.append(s)
        prompt = _rows_prompt(ns, world, slots, done, args)
        got = with_retries(lambda: call_model(prompt, ROWS_SCHEMA, model=args.model,
                                              backend=args.backend, system=SYSTEM),
                           what=f"{ns.name} rows {n + 1}-{n + size}")
        by_slot = {r["slot"]: r for r in got.get("rows", []) if isinstance(r, dict)}
        for s in slots:
            r = by_slot.get(s.slot)
            if not r or not str(r.get("content", "")).strip():
                continue
            key = f"{ns.name.replace(':', '-')}-{len(done) + 1:04d}"
            row = {
                "type": "memory", "key": key, "namespace": ns.name, "kind": s.kind,
                "content": r["content"].strip(),
                "tags": _clean_tags(r.get("tags", [])),
                "occurred_at": _clean_date(r.get("occurred_at"), args.end) if s.with_date else None,
                "sensitivity": ns.sensitivity,
                "variant_of": s.variant_of["key"] if s.variant_of else None,
                "supersedes": s.supersedes["key"] if s.supersedes else None,
            }
            done.append(row)
        n += size
        log(f"{ns.name}: {len(done)}/{ns.count}")
    return [{k: v for k, v in d.items() if not k.startswith("_")} for d in done]


def _rows_prompt(ns: Namespace, world: dict, slots: list[Slot], done: list[dict], args) -> str:
    recent = done[-30:]
    lines = []
    for s in slots:
        line = f"slot {s.slot}: {KIND_HINT[s.kind]}, {s.length[0]} to {s.length[1]} characters."
        if s.with_date:
            line += " Set occurred_at to the date the fact became true (YYYY-MM-DD)."
        else:
            line += " occurred_at null."
        if s.variant_of:
            line += (f" RESTATE this earlier row about the same subject, as a different agent would "
                     f"record it on another day: same subject and most details, reworded, with one "
                     f"number, date or name changed. Earlier row: \"{s.variant_of['content']}\"")
        if s.supersedes:
            line += (f" REPLACE this earlier row: the fact changed (a decision reversed, a host "
                     f"moved, a number corrected). Write the new fact so it stands alone and says "
                     f"what it replaces and when. Earlier row: \"{s.supersedes['content']}\"")
        lines.append(line)
    seen = "\n".join(f"- {d['content'][:220]}" for d in recent) or "(none yet)"
    return f"""Write {len(slots)} memory rows for the namespace `{ns.name}`.

{ns.brief}

Owner: {world['owner']['name']}. Dates fall between {args.start} and {args.end}.

{STYLE}

Each slot names its length in characters; keep to it.
Tags: {PROFILE['tags']['min']} to {PROFILE['tags']['max']} short lowercase kebab-case labels.

Rows already in this namespace (do not repeat them unless a slot says RESTATE):
{seen}

One row per slot, returned with its slot number:
""" + "\n".join(lines)


def _clean_tags(tags) -> list[str]:
    out = []
    for t in tags if isinstance(tags, list) else []:
        t = re.sub(r"[^a-z0-9-]+", "-", str(t).lower()).strip("-")
        if t and t not in out:
            out.append(t)
    return out[: PROFILE["tags"]["max"]]


def _clean_date(d, end: str) -> str | None:
    # ISO dates compare as strings. A date past the window is the model inventing a future.
    ok = isinstance(d, str) and re.fullmatch(r"\d{4}-\d{2}-\d{2}", d) and d <= end
    return d if ok else None


KIND_DOMAIN = {"host": "machines", "service": "services", "credential-ref": "credentials",
               "model-route": "routes", "dataset": "datasets"}


def gen_registry(world: dict, args) -> list[dict]:
    slugs = [p["slug"] for p in world["projects"]]
    prompt = f"""Write registry entries for this synthetic memory store. The registry holds exact
operational values under canonical keys `<domain>.<entity>.<attribute>`: three lowercase segments
of [a-z0-9-], the attribute singular. The domain follows the kind:

- host: `machines.<machine>.<attribute>`, attributes such as address, os, cpu, ram, region
- service: `services.<service>.<attribute>`, attributes such as port, endpoint, url, version
- credential-ref: `credentials.<system>.location`, the value naming where it lives (a
  password-manager item or a file path), never the credential
- model-route: `routes.<task>.model`, value such as an object with provider and model
- dataset: `datasets.<name>.<attribute>`, attributes such as path, size, owner

Values are a string or a number, or an object when the fact has parts.

Owner: {world['owner']['name']}, machines {', '.join(world['owner']['machines'])}.
Projects: {json.dumps(world['projects'])}

Write three entries per project in namespace `project:<slug>` (slugs: {', '.join(slugs)}) and six
in `global`, using every kind at least once. Addresses use reserved ranges only: 10.x, 192.168.x,
100.64.x, or names under example.com, example.net or .internal."""
    got = with_retries(lambda: call_model(prompt, REGISTRY_SCHEMA, model=args.model,
                                          backend=args.backend, system=SYSTEM), what="registry")
    out = []
    for e in got.get("entries", []):
        key = re.sub(r"[^a-z0-9._-]+", "-", str(e.get("key", "")).lower()).strip(".-")
        parts = key.split(".")
        # The server's domain list is closed (src/domain/canonical.rs DOMAINS); a model that
        # writes `hosts.x.address` gets the domain its kind implies.
        if parts and parts[0] not in KIND_DOMAIN.values() and e.get("kind") in KIND_DOMAIN:
            parts = [KIND_DOMAIN[e["kind"]]] + parts[1:]
            if len(parts) == 2:
                parts.append("address" if e["kind"] == "host" else "location")
            key = ".".join(parts[:4])
        if key and e.get("namespace"):
            out.append({"type": "registry", "kind": e["kind"], "key": key, "value": e["value"],
                        "namespace": e["namespace"]})
    return out


def cmd_generate(args) -> int:
    if args.backend == "auto":
        args.backend = "api" if os.environ.get("ANTHROPIC_API_KEY") else "claude-cli"
    rng = random.Random(args.seed)
    lock = threading.Lock()

    def log(msg):
        with lock:
            print(f"  {msg}", file=sys.stderr)

    print(f"backend {args.backend}, model {args.model}, {args.rows} rows", file=sys.stderr)
    world = json.loads(Path(args.world).read_text()) if args.world else gen_world(args)
    namespaces = plan_namespaces(world, args.rows, rng)
    for ns in namespaces:
        log(f"plan {ns.name}: {ns.count} rows{' (private)' if ns.sensitivity else ''}")

    results: dict[str, list[dict]] = {}
    with cf.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futs = {pool.submit(gen_namespace, ns, world, args, args.seed * 7919 + i, log): ns
                for i, ns in enumerate(namespaces)}
        registry_fut = pool.submit(gen_registry, world, args)
        for f in cf.as_completed(futs):
            results[futs[f].name] = f.result()
        registry = registry_fut.result()

    out = Path(args.out)
    with out.open("w") as fh:
        fh.write(json.dumps({"type": "world", "generator": "scripts/seed/seed.py",
                             "model": args.model, "seed": args.seed, "world": world}) + "\n")
        for ns in namespaces:
            for row in results.get(ns.name, []):
                fh.write(json.dumps(row) + "\n")
        for e in registry:
            fh.write(json.dumps(e) + "\n")
    print(f"wrote {out}", file=sys.stderr)
    return cmd_stats(argparse.Namespace(file=str(out)))


# ---------------------------------------------------------------------------------------------
# Loading


class Mcp:
    """The two calls a loader needs: initialize, then tools/call, over streamable HTTP."""

    def __init__(self, url: str, token: str, client: str):
        self.url = url.rstrip("/") + ("" if url.rstrip("/").endswith("/mcp") else "/mcp")
        self.token = token
        self.client = client
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                                "clientInfo": {"name": client, "version": "1"}})

    def rpc(self, method: str, params: dict) -> dict:
        self.n += 1
        body = json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        req = urllib.request.Request(self.url, data=body.encode(), headers={
            "authorization": f"Bearer {self.token}",
            "content-type": "application/json",
            "accept": "application/json, text/event-stream",
        })
        try:
            with urllib.request.urlopen(req, timeout=120) as res:
                ctype = res.headers.get("content-type", "")
                text = res.read().decode()
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"{method}: HTTP {e.code} {e.read()[:300]!r}") from e
        msg = _sse_message(text) if "event-stream" in ctype else json.loads(text)
        if msg.get("error"):
            raise RuntimeError(f"{method}: {msg['error'].get('message', msg['error'])}")
        return msg.get("result") or {}

    def tool(self, name: str, arguments: dict) -> tuple[bool, dict, str]:
        r = self.rpc("tools/call", {"name": name, "arguments": arguments})
        text = "\n".join(b.get("text", "") for b in r.get("content", []) if isinstance(b, dict))
        return (not r.get("isError", False)), (r.get("structuredContent") or {}), text


def _sse_message(text: str) -> dict:
    data = [line[5:].strip() for line in text.splitlines() if line.startswith("data:")]
    for d in reversed(data):
        if d:
            return json.loads(d)
    raise RuntimeError("empty event stream")


def cmd_load(args) -> int:
    token = args.token or (Path(args.token_file).expanduser().read_text().strip()
                           if args.token_file else os.environ.get("LUMBERROOM_TOKEN"))
    if not token:
        print("load: pass --token, --token-file or set LUMBERROOM_TOKEN", file=sys.stderr)
        return 2
    ids_path = Path(args.file + ".ids.json")
    ids: dict[str, str] = json.loads(ids_path.read_text()) if ids_path.exists() else {}
    mcp = Mcp(args.url, token, args.client)
    counts = {"written": 0, "deduplicated": 0, "conflicts_flagged": 0, "refused": 0,
              "skipped": 0, "registry": 0, "registry_refused": 0}
    lines = [json.loads(l) for l in Path(args.file).read_text().splitlines() if l.strip()]
    start = time.time()
    for i, row in enumerate(lines):
        if row["type"] == "memory":
            if ids.get(row["key"]):
                counts["skipped"] += 1
                continue
            a = {"content": row["content"], "namespace": row["namespace"], "tags": row["tags"]}
            # The server refuses a date after today, and a file generated for a later window can
            # carry one; the row is still worth loading without it.
            if row.get("occurred_at") and row["occurred_at"] <= time.strftime("%Y-%m-%d", time.gmtime()):
                a["occurred_at"] = row["occurred_at"]
            if row.get("sensitivity"):
                a["sensitivity"] = row["sensitivity"]
            if row.get("supersedes"):
                target = ids.get(row["supersedes"])
                if target:
                    a["supersedes"] = target
            ok, out, text = mcp.tool("memory_write", a)
            if not ok or not out.get("id"):
                counts["refused"] += 1
                print(f"  refused {row['key']}: {text[:200]}", file=sys.stderr)
                ids[row["key"]] = ""
                continue
            ids[row["key"]] = out["id"]
            counts["written"] += 1
            counts["deduplicated"] += bool(out.get("deduplicated"))
            counts["conflicts_flagged"] += bool(out.get("possible_conflicts"))
        elif row["type"] == "registry":
            rkey = f"registry {row['namespace']} {row['key']}"
            if ids.get(rkey):
                counts["skipped"] += 1
                continue
            ok, _, text = mcp.tool("registry_set", {k: row[k] for k in
                                                    ("kind", "key", "value", "namespace")})
            ids[rkey] = "set" if ok else ""
            counts["registry" if ok else "registry_refused"] += 1
            if not ok:
                print(f"  registry refused {row['key']}: {text[:200]}", file=sys.stderr)
        if i % 25 == 0:
            ids_path.write_text(json.dumps(ids))
            print(f"  {i + 1}/{len(lines)} lines, {time.time() - start:.0f}s", file=sys.stderr)
    ids_path.write_text(json.dumps(ids))
    print(json.dumps(counts))
    return 0 if counts["refused"] == 0 else 1


def cmd_stats(args) -> int:
    rows = [json.loads(l) for l in Path(args.file).read_text().splitlines() if l.strip()]
    mem = [r for r in rows if r["type"] == "memory"]
    reg = [r for r in rows if r["type"] == "registry"]
    by_ns: dict[str, int] = {}
    for r in mem:
        by_ns[r["namespace"]] = by_ns.get(r["namespace"], 0) + 1
    lengths = sorted(len(r["content"]) for r in mem) or [0]
    print(json.dumps({
        "memories": len(mem), "registry": len(reg), "namespaces": by_ns,
        "variants": sum(1 for r in mem if r.get("variant_of")),
        "supersedes": sum(1 for r in mem if r.get("supersedes")),
        "dated": sum(1 for r in mem if r.get("occurred_at")),
        "private": sum(1 for r in mem if r.get("sensitivity") == "private"),
        "length_median": lengths[len(lengths) // 2],
        "length_p90": lengths[int(len(lengths) * 0.9)],
        "length_max": lengths[-1],
    }, indent=2))
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("generate", help="ask a model for a synthetic store")
    g.add_argument("--out", required=True)
    g.add_argument("--rows", type=int, default=400)
    g.add_argument("--projects", type=int, default=5)
    g.add_argument("--model", default="claude-sonnet-5-5")
    g.add_argument("--backend", choices=["auto", "claude-cli", "api"], default="auto")
    g.add_argument("--batch", type=int, default=20, help="rows per model call")
    g.add_argument("--jobs", type=int, default=4, help="namespaces generated at once")
    g.add_argument("--seed", type=int, default=1)
    g.add_argument("--start", default="2026-06-01")
    g.add_argument("--end", default="2026-10-01")
    g.add_argument("--theme", default="")
    g.add_argument("--world", help="reuse the world from a JSON file instead of generating one")
    g.set_defaults(fn=cmd_generate)
    l = sub.add_parser("load", help="write a generated file into a running server")
    l.add_argument("file")
    l.add_argument("--url", required=True, help="server base URL or its /mcp endpoint")
    l.add_argument("--token")
    l.add_argument("--token-file")
    l.add_argument("--client", default="lumberroom-seed")
    l.set_defaults(fn=cmd_load)
    s = sub.add_parser("stats", help="summarise a generated file")
    s.add_argument("file")
    s.set_defaults(fn=cmd_stats)
    args = p.parse_args()
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
