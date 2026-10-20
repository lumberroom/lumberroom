# 34. The digest leads with the project and adds rows related to the request

**Date:** 20 October 2026 · **Status:** accepted; implemented, cargo check clean, integration suite not yet run · **Decided by:** the owner, 20 October 2026

This record supersedes [0025](0025-the-digest-ranks-by-recency.md). 0025's near-duplicate pass and
its rule that the newer row wins stay, now over every row the digest can print. It narrows
[0028](0028-no-embedding-fallback.md) for one optional section.

## Decision

**The active project prints first and holds the dominant share.** The render order is project,
related, profile, recent, registry, and `SHARES` is `[55, 10, 15, 5, 15]` in that order. A share is
a budget: each section spends its own before leftovers move, and project, as the first section,
also takes first claim on the leftover pass. The other shares act as floors. At 6,000 characters
profile gets about 700 to 800 characters and the registry about a dozen lines. A rule line with its
suffix often runs 300 characters or more under the 2,000-character write cap, so profile's share
holds two long rules or about five short ones.

**Profile keeps one whole rule at any budget.** Before any share is cut, the layout reserves the
first profile entry, in rank order, that fits in half of the room the header and tail leave. The
project's share shrinks by that entry. Without the reserve, profile's 15 percent printed no rule
below about 1,400 characters on the render tests' crowded fixture, and per-client budgets have no
lower bound.

**Limits move.** `BOOTSTRAP_PROJECT_LIMIT` goes from 5 to 10, `BOOTSTRAP_PROFILE_LIMIT` from 10 to 8
and `BOOTSTRAP_RECENT_LIMIT` from 8 to 5. A new `BOOTSTRAP_RELATED_LIMIT` defaults to 4; boot
refuses a value outside 0 to 10, and 0 turns the section off. Compose now forwards
`BOOTSTRAP_RECENT_LIMIT`, `BOOTSTRAP_RECENT_DAYS`, `BOOTSTRAP_RELATED_LIMIT` and
`BOOTSTRAP_RULE_TAGS`, empty by default, which `config.rs` reads as its own default. A deployment
that set the old limits keeps them.

**Profile puts standing rules first.** A new `BOOTSTRAP_RULE_TAGS`, default
`preference,rule,convention`, names the tags that mark a rule. The profile arm orders its pool by
whether a row carries one of those tags, then by age, so the rules reach the service even after a
burst of newer personal rows. The service prints tagged rows ahead of untagged ones. Inside each
group the order is newest first, or nearest the query first when a query came. Untagged rows fill
whatever the rules leave of the profile limit. This holds on every path, with or without a query,
so the session-start digest, CLI hook clients and ChatGPT get it too. Because `config.rs` reads an
empty value as the default, a single comma (`BOOTSTRAP_RULE_TAGS=,`) is the way to name no tags
and turn the rule off.

**A survivor of the near-duplicate pass takes its twin's standing mark.** When a newer untagged row
restates a tagged rule, the pass drops the rule, as the newer-wins rule says. The survivor then
counts as standing for the profile order. Without that, the restatement sorted behind every other
rule, fell past the profile limit, and the fact left the digest, since recent admits no untagged
user row. The mark is all it inherits.

**Recent keeps to the active project and the caller's standing rules.** The filter sits in SQL:
recent reads the active project and the rows in `user:me` and `global` that carry a rule tag.
Untagged personal rows reach the digest through profile's tail or the related section. Rows in any
other namespace reach it through the related section or as a count on the inventory line. Under a
query recent also skips every row that sat in the profile pool, because the query rank already
weighed those rows and left them out.

**`context_bootstrap` takes an optional `query`, the person's request.** The server trims it, and an
empty or whitespace query counts as none. On a cache miss with a query the service embeds it once
with the unit's model. The statement's related arm returns the live rows nearest that vector across
every namespace the caller may read, with the grant and the ceiling inside the SQL, and keeps those
whose cosine reaches the floor. The service picks related last, so a row another section chose
never repeats there.

**The floor is the model's `route_max_top`, read as a pure cosine.** Decision 0029 says thresholds
belong to the model, and `route_max_top` is the key the graph router already compares with a
query's scores. Neither shipped value was measured as a query-to-row cosine. bge-base-en-v1.5's
0.65 is the value production ran the router on before 0029, tuned and never measured.
EmbeddingGemma 2's 0.76 comes from the LongMemEval fusion sweep, on fused scores, and the costs
below say what reading it as a cosine does. The section runs when the value's source is
`override`, `legacy`, `shipped`, `study` or `sweep`. A `carried` or `guessed` value is bge's number
on another model's scale, so the section stays out. A borrowed bge value on a model with a higher
score distribution, such as E5, would fill the section on every query. The server then logs one
warning per process that names the model, tells the operator to measure the model's query-to-row
scores before setting `EMBED_THRESHOLDS=route_max_top=<value>`, and says the router reads the same
key. The digest reports `related_unavailable: "no_measured_floor"`. Profile ordering still runs,
because it needs no threshold.

**A failed query embed leaves the related section out and the digest serves.** The service logs a
warning, orders the profile without scores, and reports `related_unavailable: "embed_failed"`.

**`Digest` gains `related` and `related_unavailable`.** `related_unavailable` is `None` when no
query came, when the section ran, or when `BOOTSTRAP_RELATED_LIMIT` is 0; otherwise it is
`embed_failed` or `no_measured_floor`. The brief MCP view carries it when set, and `rows: true`
carries it through serde. The rendered text says nothing about it.

**Under a query profile widens its pool** to the `profile_limit * 5` rows the arm ranks first, and
the service orders each group by cosine to the query, rows with no score last in recency order.

**The near-duplicate pass runs over every pool the digest prints from:** profile, project, recent
and related. Newer still wins across sections. Project, profile and recent each note the dropped
rows they would have printed with no near-duplicate pass: dropped rows among the first `limit`
rows of their pool that no earlier section chose. A related row that dropped one of those moves to
the front of the related pool before the cut, so a newer correction filed in another project
replaces the project row instead of vanishing past `related_limit`. A related row whose twin would
never have printed, because it sat only in the related pool or past its section's limit, keeps its
nearness order.

**Sections pick in the order project, profile, recent, related.**

**The cache key gains the query,** as `q:` plus the trimmed text or `-`, last. Each insert first
sweeps entries older than `cache_ms`, since a new query on every prompt would otherwise grow the map
without bound. The embed runs only on a miss.

**The API stays compatible.** `bootstrap::run(ctx, project)` keeps its signature and calls
`run_with_query(ctx, project, None)`. MCP `BootstrapArgs` gains `query: Option<String>`. No HTTP
route serves the digest, and the CLI `bootstrap` command does not send a query.

**One more render rule.** The small shares let an ordinary rule count as longer than its section's
whole budget at small totals, and the shortening fallback then cut inside the rule's first word.
A shortened body now keeps at least its first word when the body has spaces, or the entry stays out
and the footer counts it. That covers a body that opens with a URL longer than the room and has
words after it: it stays out rather than print a cut URL, and `left_out` still names it. CJK text
and a body with no spaces still cut at a grapheme.

## The context that forced it

In a coding session the digest's profile held personal-finance rows that another surface wrote,
the recent section held a note from an unrelated project, and the project section lost most of its
rows to the 6,000-character budget. The session then skipped one of the owner's standing working
rules, which the profile did not carry.

Three causes. Recency alone ranked every section. Profile printed first with the largest share, 40
percent, so project at 25 percent lost rows while profile, recent and registry each printed their
own share. Recent read every namespace the caller could read, so a burst of writes anywhere filled
it.

## How this differs from 0025

0025's evaluation asked whether embeddings should rank the whole digest, and recency won. That
evidence stands for project and recent, which stay on recency. The related section is narrower: a
handful of rows ranked by the caller's own request, above a floor, beside recency-led sections. The
request is a signal 0025's rankers never had: they ranked by the project's centroid, its recent rows
or its slug.

The owner's ruling narrows 0025 for profile. 0025 dropped the tag-first rule because it lost to
plain recency on proxy labels: which rows `memory_search` returned in the hours after a digest, and
which rows a session superseded. The ruling names a different goal for profile, the standing rules
that bind a session in every project, and the incident shows recency failing that goal. The rule
returns with two changes. It reads the tags writers already use for rules, `preference`, `rule`
and `convention`, instead of `profile` and `identity`, which mark personal facts. And it governs
recent's user rows too, so the facts it holds back from profile cannot print one section lower.

## What lost, and why

**Ranking the whole digest by embeddings.** 0025's evidence stands.

**Ordering profile by cosine to the project's rows.** It rewards personal facts that share the
project's topic and holds back the rules that apply everywhere. "Never stage with `git add -A`"
scores low against a store of engine rows. 0025's evaluation also found the centroid and the
recent-rows variants losing to recency.

**Down-ranking rows written by another surface.** The owner's rules come from several writers,
including the dreaming pass, so the writer does not separate a rule from a fact.

**Refusing small budgets at boot.** The budget where profile's share empties depends on how long
the rules are, so no single minimum holds. The reserved entry follows the rows.

**Strict project priority,** project against the whole budget before any other section. Ten long
project rows would empty profile, and a profile with no standing rules is how a session skips the
rule that binds it.

**A second statement for the related rows.** Ordering the profile needs the query scores before
`pick`, and excluding chosen rows needs `pick`'s result, so a separate statement either runs twice
or scores nothing. The repository port promises the digest in one round trip.

**A new `bootstrap_related` similarity key now.** It would ship one more unmeasured value per model.
`route_max_top` at least carries the model's own number and a source that says how it was set.

**A wide arm kept only for the near-duplicate pass.** A row outside `Selection::new` drops nothing,
and returning such rows' ids to the service would bypass the decrypt guard that keeps an unreadable
private row from pushing out its readable twin.

**Failing the request on a failed embed.** The related section and the profile's query order are
optional; the rest of the digest needs no vector.

**Dropping `user:me` and `global` from recent.** The owner's ruling keeps their working rules.

## Costs accepted

- **One embedding call on a path that had none,** only on a cache miss with a query. Decision 0001
  measured 11 to 16 ms for the local bge model. Production runs EmbeddingGemma 2 on CPU through a
  remote embedder, which the 9 October 2026 benchmark put at about 0.19 to 0.30 s for 100 tokens
  and 0.8 to 1.8 s for 500. A 1,000-character request is about 250 tokens, so roughly 0.5 to 0.8 s
  plus the network round trip. That figure is interpolated, not measured on this path.
- **The statement grows** by one vector arm, its floor filter and the profile scores, and the
  profile arm sorts every live `user:me` and `global` row by tag before its LIMIT, where it used to
  read the newest few. Its latency is not measured yet. The method is 0025's: `pgbench -M prepared`
  under `plan_cache_mode = force_generic_plan`, three rounds of 20, at 4,500 and 20,000 synthetic
  rows, with and without a query vector. The observed numbers belong here when that run happens.
- **The floor is approximate, and on EmbeddingGemma 2 it is probably strict.** 0029 calibrated
  `route_max_top` for that model on the linear fused score, cosine plus a lexical term capped at
  0.35 plus a usage boost, times a penalty below 1 for non-primary namespaces. Read as a pure cosine
  it sits stricter by up to 0.35, and it binds hardest on the second to fourth nearest rows. The
  section the ruling sizes at three to five rows may hold none or one on most prompts. Nobody has
  measured the query-to-row cosine distribution for that model. The floor applies under every
  `SEARCH_FUSION` value, although the router stands down under `rrf` and `linear_minmax`. An
  operator who tunes the router moves the digest floor too.
- **Models whose `route_max_top` is carried or guessed get no related section** until an operator
  sets one.
- **Rules without a rule tag sort behind tagged rows.** A writer that never tags, or tags rules
  with another word, gets profile on recency and loses its user rows from recent. The setting takes
  the store's own words.
- **Untagged personal rows print only in profile's tail, in related, or as a count.** A personal
  fact a session needs reaches it through a query that matches it or through `memory_search`.
- **The reserved profile entry takes room from the project** at every budget, one rule's worth.
- **A `personal:*` row shows only as a count** on the inventory line unless a query matches it.
- **A correction outside the active project, `user:me` and `global` no longer hides its stale twin
  without a query.** With a matching query it joins through related and the newer row wins.
- **More displacing related rows than `related_limit` lose a fact** for one call.
- **A survivor inherits its twin's standing mark and not its query score.** Under a query a dropped
  tagged twin that scored higher than its untagged survivor still loses that rank, and the
  survivor sorts by its own score inside the standing group.
- **A fact filed under a project other than the session's directory reaches a fresh session
  without a query only as a count on the inventory line.** `scripts/done-when-test.sh` plants its
  fact in session A, wherever the model files it, and asks for it in session B from a temporary
  directory through the CLI hook, which sends no query. Recent used to carry that fact; it no
  longer does, even with a rule tag, unless the fact sits in `user:me` or `global`. The gate has
  not been run against this change and may need session B to run in a directory named for the
  fact's project, or the hook to send a query.
- **Under a query recent is mostly project overflow,** because the tagged user rows within 14 days
  mostly sit in the profile pool.
- **A failed embed shows only in `related_unavailable` and a warning line.**
- **The cache holds the query text** for `cache_ms`, 30 seconds by default.
- **The text changes for every caller.** Project now prints above profile, and each existing section
  has a different share and limit.

## Decision 0028

0028 says a failed embed fails the request. This record makes one exception: the digest's related
section and its profile query order, both optional, drop out and the digest serves. No second model
embeds anything, so 0028's core rule holds.

## What this is not for

It does not change `memory_search` ranking. It does not dedup the store. It does not calibrate the
floor: `route_max_top` keeps the basis 0029 gave it. The rule tags mark rows; they do not classify
untagged text as a rule.

## Reversal condition

Two sources, and no new table. The recall log (decision 0024) records a related row with
`section = 'related'`, so a call holding one carried a query; join those rows to `memory_search`
events for the same `memory_id` in the following hours. A queried call with no row above the floor
logs no related row, so the log alone undercounts queried calls. The server log records `digest
built for a query` on each queried cache miss, with `related_candidates`, `related_printed`,
`floor` and `floor_source`.

If related rows rarely recur in later searches, or `related_candidates` is 0 for most queried
digests, register a `bootstrap_related` key per model in `src/domain/similarity.rs` and measure it
on query-to-row cosines from a copy of a real store. If the profile rows later searches reach for
are untagged rows the rule tags held back, return profile to recency, or widen the tag list. The
shares are one constant, the floor cap is one constant, and the limits and tags are environment
variables.
