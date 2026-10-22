# 34. The digest leads with the project and adds rows related to the request

**Date:** 20 October 2026 · **Status:** accepted; merged in #159, released in 0.6.0; amended 21 October 2026 (the preference lead ranks by the anchor), amendment implemented, `./scripts/cargo.sh nextest` 1948 passed · **Decided by:** the owner, 20 October 2026; the amendment by the owner, 21 October 2026

This record supersedes [0025](0025-the-digest-ranks-by-recency.md). 0025's near-duplicate pass and
its rule that the newer row wins stay, now over every row the digest can print. It narrows
[0028](0028-no-embedding-fallback.md) for one optional section.

## Decision

**The active project prints first and holds the dominant share.** The render order is project,
related, profile, recent, registry, and `SHARES` is `[55, 10, 15, 5, 15]` in that order. A share is
a budget: each section spends its own before leftovers move, and project, as the first section,
also takes first claim on the leftover pass. The other shares act as floors. At 6,000 characters
profile gets about 700 to 800 characters, of which its heading and reminder line take about 160,
and the registry about a dozen lines. A preference line with its suffix often runs 300 characters
or more under the 2,000-character write cap. On the render tests' fixture of five long preferences
and ten full project rows, five of the five print without a related section and three with four
related rows; `the_default_budget_prints_the_observed_count_of_five_realistic_preferences` pins
both counts.

**Profile keeps one whole entry ahead of the shares.** Before any share is cut, the layout reserves
the first profile entry, in rank order, that fits in half of the room the header and tail leave.
The lead preferences rank first, so that entry is a leading preference whenever one of them fits.
The project's share shrinks by that entry and by the profile heading, which carries the reminder
line. Without the reserve, profile's 15 percent printed no entry below about 1,400 characters on
the render tests' crowded fixture, and per-client budgets have no lower bound.
`every_budget_with_room_for_the_tail_prints_profile` pins every budget from 1,200 characters on
that fixture.

**Limits move.** `BOOTSTRAP_PROJECT_LIMIT` goes from 5 to 10, `BOOTSTRAP_PROFILE_LIMIT` from 10 to 8
and `BOOTSTRAP_RECENT_LIMIT` from 8 to 5. A new `BOOTSTRAP_RELATED_LIMIT` defaults to 4; boot
refuses a value outside 0 to 10, and 0 turns the section off. Compose now forwards
`BOOTSTRAP_RECENT_LIMIT`, `BOOTSTRAP_RECENT_DAYS`, `BOOTSTRAP_RELATED_LIMIT`,
`BOOTSTRAP_PREFERENCE_LIMIT` and `BOOTSTRAP_RULE_TAGS`, empty by default, which `config.rs` reads as its own default. A deployment
that set the old limits keeps them.

**Profile follows the active project when no query comes.** The owner ruled on 20 October 2026 how
profile picks its `user:me` and `global` rows on the paths that send no query: the session-start
digest, CLI hooks, ChatGPT and any client that never sends one. Profile ranks its pool by cosine to
the centroid of the active project's pooled rows, the newest `3 × BOOTSTRAP_PROJECT_LIMIT` rows the
caller may read there. With a query, the query's vector takes the centroid's place. With neither an
active project nor a query, or a project whose rows carry no vector, profile runs on recency. A row
without a vector sorts after every scored row, and equal scores fall back to newest first.

**Up to five preferences lead profile.** A preference is a `user:me` row carrying a
`BOOTSTRAP_RULE_TAGS` tag, default `preference,rule,convention`. The newest
`BOOTSTRAP_PREFERENCE_LIMIT` of them, default 5, lead profile, newest first, on every path, and count
against `BOOTSTRAP_PROFILE_LIMIT`. Boot refuses a value outside 0 to 10, and the service uses at most
the profile limit. Preferences past that count, and rule-tagged `global` rows, rank with the rest.
The structured rows always carry the lead; the text prints as many as profile's share holds.
Because `config.rs` reads an empty value as the default, a single comma (`BOOTSTRAP_RULE_TAGS=,`)
names no tags; profile then leads with nothing and recent admits every `user:me` and `global` row.

**The profile heading carries one reminder line:** "lumberroom stores more standing preferences
than this list, and memory_search finds the one that covers a convention." It sits in the heading,
so it prints whenever a profile entry prints and takes room the way the heading does. It is no row:
the structured rows, `left_out` and the recall log never carry it. It names what the store holds
and the tool that reaches it, and gives no order, because the MCP directory review rejects tool
text that tells a model how to behave.

**Profile reads three arms in the one statement:** the newest preferences, the rows nearest the
anchor and the newest rows, each at three times its limit. The nearest arm orders by distance
before its LIMIT, so a burst of newer rows cannot push an older near row out of the pool. The
centroid is `avg` over the project pool's stored vectors inside the statement, and no select list
carries a vector, so 0025's vectors-in-the-pools cost does not return.

**A survivor of the near-duplicate pass takes its twin's preference mark.** When a newer untagged
row restates a preference, the pass drops the preference, as the newer-wins rule says. The survivor
then counts as a preference for the lead. Without that, the restatement ranked with the untagged
rows, fell past the profile limit, and the fact left the digest, since recent admits no untagged
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
0.65 has basis `shipped` in 0029: the value the router ran on before that record, tuned and never
measured.
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
warning, ranks the profile by the active project, as a call without a query does, and reports
`related_unavailable: "embed_failed"`.

**`Digest` gains `related` and `related_unavailable`.** `related_unavailable` is `None` when no
query came, when the section ran, or when `BOOTSTRAP_RELATED_LIMIT` is 0; otherwise it is
`embed_failed` or `no_measured_floor`. The brief MCP view carries it when set, and `rows: true`
carries it through serde. The rendered text says nothing about it.

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

The owner's ruling narrows 0025 for profile. 0025's evaluation saw project-similarity ranking lose
to recency for the whole digest: cosine to the project's centroid, the same weighted by age, cosine
to the last few project rows, and cosine to the slug each lost, and the rows recency found and they
missed were mostly less than a day old. 0025 never tested the centroid on profile alone. This
record applies it there by the owner's ruling, because recency let one busy chat surface fill the
section with personal facts the session had no use for. Project and recent stay on recency. 0025 also dropped
the tag-first rule after it lost to recency on proxy labels. The preference lead brings back a
narrow form of it: five rows, `user:me` only, newest first, read from the tags writers already use
for rules instead of `profile` and `identity`, which mark personal facts. The reversal condition
says how the recall log would show the ruling losing the same way.

## What lost, and why

**Ranking the whole digest by embeddings.** 0025's evidence stands.

**Embedding the project slug as the anchor.** It costs one embed on every query-less cache miss, a
path that had none, and a slug carries a word or two of meaning. 0025 saw it lose too. The centroid
needs no embed.

**Ranking inside a recency pool.** Reordering the newest 24 or 40 rows by the anchor leaves an older
near row out whenever a burst outgrows the pool, which is the incident.

**Tag-first order for every profile row.** The ruling replaces it with the lead. A store whose rules
all carry tags would fill profile with rules and leave no room for the rows near the project.

**The reminder as a row.** A row has an id, enters `rows: true`, the recall log and `left_out`,
competes for the budget, and reads as a stored fact.

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
  measured 11 to 16 ms for the local bge model. A remote embedder adds its own inference time and a
  network round trip, which nobody has measured on this path.
- **The statement grows** by one vector arm, its floor filter, the profile scores, the project
  centroid and two profile arms. The nearest arm orders every live `user:me` and `global` row the
  caller may read by distance before its LIMIT. At the defaults the profile pool reads up to 63 rows
  (15 preferences, 24 nearest, 24 newest, before the union removes overlap) where it read 24, or 40
  under a query, and recent's pool reads up to 88 where it read 49, or 65 under a query. Every
  pooled row enters the vector copy and the near-duplicate join, which compares every pair, so that
  step grows with the square of the pool, and the decrypt pass opens more private rows. The method
  is 0025's: `pgbench -M prepared` under `plan_cache_mode = force_generic_plan`, three rounds of
  20, at 4,500 and 20,000 synthetic rows, with and without a query vector. The observed numbers
  belong here when that run happens.
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
- **Only tagged `user:me` rows lead.** A writer that never tags, or tags rules with another word,
  gets no lead and loses its user rows from recent. A rule in `global` ranks on the anchor, so a
  rule about staging files can score low against a store of engine rows. The reminder line points a
  session at `memory_search` for those.
- **A surface that tags every row it writes fills the lead.** A burst of `user:me` rows tagged
  `preference` takes all five lead slots, newest first, whatever the project, which is the
  incident's shape moved into the lead. The reversal condition watches for it.
- **The text can print fewer than five.** The structured rows carry the whole lead, and the text
  prints what profile's share holds: three of five long preferences on the render tests' fixture
  when four related rows print above them.
- **Without a query, a personal row written an hour ago can lose to an older row near the
  project.** 0025 found most of recency's lead in such rows.
- **The centroid reads up to `3 × BOOTSTRAP_PROJECT_LIMIT` stored vectors** on each query-less cache
  miss with an active project.
- **The reminder line costs about 120 characters** whenever profile prints, out of the project's
  room at small budgets.
- **Untagged personal rows print only in profile's tail, in related, or as a count.** A personal
  fact a session needs reaches it through a query that matches it or through `memory_search`.
- **The reserved profile entry takes room from the project** at every budget, one entry's worth.
- **A `personal:*` row shows only as a count** on the inventory line unless a query matches it.
- **A correction outside the active project, `user:me` and `global` no longer hides its stale twin
  without a query.** With a matching query it joins through related and the newer row wins.
- **More displacing related rows than `related_limit` lose a fact** for one call.
- **A survivor inherits its twin's preference mark and nothing else.** It takes its place in the
  lead by its own age.
- **A fact filed under a project other than the session's directory reaches a fresh session
  without a query only as a count on the inventory line.** `scripts/done-when-test.sh` runs both
  sessions from a directory named `lumberroom`, so the hook's digest carries `project:lumberroom`,
  where the fact's text steers the model, and a `user:me` preference reaches it through the lead.
  The script prints where session A filed the fact and warns when it sits elsewhere. The gate has
  not been run against this change.
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
floor: `route_max_top` keeps the basis 0029 gave it. The rule tags mark rows and do not classify
untagged text, so a rule written without a tag never joins the preference lead.

## Reversal condition

Two sources, and no new table. The recall log (decision 0024) records a related row with
`section = 'related'`, so a call holding one carried a query; join those rows to `memory_search`
events for the same `memory_id` in the following hours. A queried call with no row above the floor
logs no related row, so the log alone undercounts queried calls. The server log records `digest
built for a query` on each queried cache miss, with `related_candidates`, `related_printed`,
`floor` and `floor_source`.

If related rows rarely recur in later searches, or `related_candidates` is 0 for most queried
digests, register a `bootstrap_related` key per model in `src/domain/similarity.rs` and measure it
on query-to-row cosines from a copy of a real store.

The profile ruling reverses on the recall log (decision 0024) joined to the `memory` table. Take
`context_bootstrap` calls with a `project` and no `related` row; most carried no query, and a
queried call whose rows all missed the floor lands there too. For each call, read from `memory`, by
`created_at` and `namespace`, the `BOOTSTRAP_PROFILE_LIMIT` newest `user:me` and `global` rows
created before the call, and keep those its `profile` events do not name. Those are rows recency
would have printed and the centroid dropped. A call counts as a miss when a `memory_search` event
from the same tenant and `client` names one of them within 6 hours after the call. The join uses
the client and the window because the query-less callers send no session header. If more than one
call in five is a miss, over at least 100 such calls, return the rest of profile to recency and
keep the lead and the reminder line. Over the same calls, take the leading `profile` events, from
rank 1, that name a rule-tagged `user:me` row. If in more than one call in five every one of them
came from one `source_client` in the 24 hours before the call, a burst from one surface fills the
lead: lower `BOOTSTRAP_PREFERENCE_LIMIT` or narrow `BOOTSTRAP_RULE_TAGS`. All four numbers are
design targets, not measurements. `BOOTSTRAP_PREFERENCE_LIMIT=0` removes the lead without a
release. The shares are one constant, the floor cap is one constant, and the limits and tags are environment
variables.

## Amendment, 21 October 2026: the preference lead ranks by the anchor

**Decided by:** the owner, 21 October 2026, after a live digest.

**The lead ranks by the anchor the rest of profile uses.** With a query, the lead takes the
rule-tagged `user:me` rows nearest the query. Without one, it takes the rows nearest the centroid
of the active project's pooled rows. Equal scores fall back to newest first, then id. With no
anchor (no active project with vectors and no query, or a failed embed outside a project) the lead
stays newest first. A preference without a vector sorts after every scored preference. A tagged
twin the near-duplicate pass dropped keeps its place in the order and takes no lead slot, so the
structured rows carry `BOOTSTRAP_PREFERENCE_LIMIT` distinct preferences when the store holds them;
the text still prints what profile's share holds. The lead keeps its size, its place ahead of the
rest, its count inside `BOOTSTRAP_PROFILE_LIMIT`, the reminder line and the survivor's mark.

**The preference arm reads the tagged rows nearest the anchor.** It orders the live rule-tagged
`user:me` rows by distance to the anchor, then newest first, before its limit of
`3 × min(BOOTSTRAP_PREFERENCE_LIMIT, BOOTSTRAP_PROFILE_LIMIT)`, 15 at the defaults. With no anchor
every distance is NULL and the arm reads the newest, as before. Left newest first, the arm loses an
older rule to more than 15 newer tagged rows, the nearest arm loses it to 24 untagged rows nearer
the anchor, and the lead's slots fall to off-topic tagged rows by score. That is the design this
record rejected under **Ranking inside a recency pool**, moved into the lead. The arm's second sort
key keeps the HNSW index out of it: Postgres 16 orders from an index scan only when the index
matches every sort key, so the arm sorts the tag-filtered rows, and pgvector's filtering after the
index cannot shorten it. No vector enters the payload, and the grant and the ceiling stay inside
every arm.

**What it replaces in this record.** "newest first, on every path" under **Up to five preferences
lead profile**. "the newest preferences" under **Profile reads three arms**. "newest first" for the
lead in **How this differs from 0025**. The survivor taking its place in the lead "by its own age"
under **Costs accepted**; it now takes it by its own score. The cost **A surface that tags every
row it writes fills the lead**, which this amendment closes while an anchor exists. The done-when
cost's "a `user:me` preference reaches it through the lead", which now holds only when the
preference sits near the project's rows; otherwise a fresh preference reaches the session through
recent. The burst clause at the end of the profile ruling's reversal condition, which named
lowering `BOOTSTRAP_PREFERENCE_LIMIT` or narrowing `BOOTSTRAP_RULE_TAGS` as the remedy; the
reversal condition below replaces it.

**What forced it.** On 21 October 2026 a coding session's digest led profile with two personal
plans another client had tagged `preference`, above older standing coding rules. The lead sorted
newest first, and any writer can set the tag. A `memory_search` over the same `user:me` rows, with
a query shaped like the session's project, ranked fifteen working rules first on its fused score,
at cosines of 0.64 to 0.74, and left both plans out of its top fifteen. That observation covers the
query path. The digest that misfired sent no query and ranks by the project's centroid. Measured
on 22 October 2026 on the same store, read only: against the centroid of the project's 30 newest
rows, the two plans ranked 61st and 63rd of 63 rule-tagged `user:me` rows, at cosines of 0.711 and
0.699, while the five nearest rules scored 0.845 to 0.888. The centroid keeps both out of a
five-row lead.

**No similarity floor on the lead.** Decision 0029 says thresholds belong to the model, and no
model has a measured centroid-to-row cosine; `route_max_top` was set for a query. A floor would
also empty the lead in a project whose centroid sits far from every rule, and the lead exists to
carry rules that bind every project.

### What lost, and why

**Narrowing `BOOTSTRAP_RULE_TAGS`.** `preference` is the tag surfaces use for standing rules, so
dropping it drops most real rules, and any tag left stays open to any writer.

**Retagging in review.** Someone has to find and fix each mis-tagged row, and the next chat write
brings the problem back.

**A model classifier at read time.** It adds a model call to every cache miss, a cost and latency
the digest does not carry, and its verdict rests on a prompt nobody has evaluated.

**Lowering `BOOTSTRAP_PREFERENCE_LIMIT`,** this record's stated remedy. Fewer slots still go to the
newest tagged rows.

**Reordering the newest tagged rows alone.** Ranking the arm's 15 newest rows by the anchor leaves
out an older rule whenever a burst outgrows the arm.

**A wider newest-first arm.** Any fixed count of newest rows loses to a burst one row larger, and
every extra pooled row enters the vector copy and the near-duplicate join, which compares every
pair.

Ranking by the anchor reuses the anchor the statement already builds, costs no model call, and does
not depend on how a client tags.

### Costs accepted

- **The preference arm computes one distance per live rule-tagged `user:me` row** the caller may
  read, on each cache miss with an anchor, and sorts them. The nearest arm already orders every
  live `user:me` and `global` row by distance, a superset, though it can walk the HNSW index while
  this arm reads each tagged row's vector. Nobody has measured it; the pgbench method under
  **Costs accepted** above applies.
- **A rule that binds every project but sits far from this one can lose its lead slot** to a rule
  near the project. It still ranks with the rest of profile, and the reminder line points at
  `memory_search`.
- **A store with fewer than `BOOTSTRAP_PREFERENCE_LIMIT` tagged rows near the anchor fills the
  remaining lead slots** with the next tagged rows by score, off-topic ones included.
- **A fresh preference far from the project reaches a session-start digest through recent,** which
  prints five rows on a 5 percent share, and no longer through the lead.
- **Without an anchor the lead stays newest first,** so the CLI `bootstrap` outside a project
  still leads with the newest tagged rows.
- **Without a query, a tagged personal row the lead drops can still print in recent** when it is
  among the newest tagged rows within `BOOTSTRAP_RECENT_DAYS`. This amendment leaves recent alone.
- **A preference without a vector loses its slot** to any scored preference while an anchor exists.

### Out of scope

The dreaming pass could label rows as working rules or personal facts, and the lead could read the
label. That needs a labelling convention and a review flow of its own, and it is a separate
decision.

### Reversal condition

This replaces the burst clause of the profile ruling's reversal condition. Over
`context_bootstrap` calls that carried an anchor, take the five newest rule-tagged `user:me` rows
created before each call and keep those the call's leading `profile` events do not name. A call
counts as a miss when a `memory_search` event from the same tenant and `client` names one of them
within 6 hours. If more than one call in five is a miss, over at least 100 such calls, return the
lead to newest first in `profile_order` and the preference arm to newest first in the statement.
Both numbers are design targets, not measurements.
