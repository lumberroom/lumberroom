# The recall event log

`RECALL_EVENT_LOG=true` makes the server keep one row per memory per call in `recall_event`: which
rows one `memory_search` or one `context_bootstrap` returned, in what order, and in which digest
section. It is off by default. [Decision 0024](decisions/0024-a-per-call-recall-log.md) records why
it exists and what lost.

## What a row holds

| column | what it is |
|---|---|
| `call_id` | one uuid per tool call, shared by every row that call returned |
| `tool` | `memory_search` or `context_bootstrap` |
| `client` | the client that called, named as `tool_calls.client` names it |
| `session_id` | the `X-Session-Id` the client sent, when it sent one |
| `project` | the active project namespace, such as `project:alpha`, when the call named a project |
| `memory_id` | the row returned |
| `namespace` | that row's namespace |
| `section` | `profile`, `project` or `recent` for a digest; empty for a search |
| `rank` | position from 1, within the search result or within the digest section |
| `emitted_at` | when the server wrote the event |

A digest served from the cache logs its rows as a new call, because a caller received them. A row
that sits in two digest sections, such as a profile fact written this week that also lands in
recent, gets one event per section. The digest rows are the structured payload's rows. The
rendered markdown can leave an entry out for budget, and the log does not record that.

A private row is logged by id like any other. `recall_emission` skips private rows because it
stores a keyed digest of the content. This log stores no content, so there is nothing to probe. It
does add what the row's own `access_count` and `last_accessed_at` do not hold: which client read a
private row, when, and alongside which other rows.

## What it is not for

It is not analytics. Nothing in the server reads it, no screen shows it, and no ranking uses it. It
exists so that an offline evaluation of search and digest ranking can join exact per-call results
against what happened next, where `recall_emission` can only give the first and last time a fact
went out.

It stores no content, no content hash, no query text and no credential. It does say which facts a
client used and when, so a deployment that turns it on should say so in its privacy notice.

## Retention and deletion

The cleanup pass deletes events older than `RECALL_EVENT_RETENTION_DAYS` on each run, in batches
of 5,000. That pass runs on `CLEANUP_INTERVAL_SECS`, so boot refuses `RECALL_EVENT_LOG=true` with
`CLEANUP_INTERVAL_SECS=0`: the window would never be enforced.

The window is a floor on deletion, so plan for these:

- An event can live up to `RECALL_EVENT_RETENTION_DAYS` plus one `CLEANUP_INTERVAL_SECS`. The pass
  skips its first tick after boot, so each restart starts that wait over.
- The purge runs whether or not the log is on, so turning the log off still lets old rows age out.
  With the log off and `CLEANUP_INTERVAL_SECS=0`, nothing deletes them.
- The purge reaches the live database only. Each dump `deploy/backup.sh` writes keeps the events it
  holds for the backup's own retention, `BACKUP_RETAIN_DAYS`, 14 days by default.

Forgetting a memory deletes its events through the foreign key's `ON DELETE CASCADE`, the same way
`recall_emission` rows go.

An archive does not carry the log, as it does not carry `recall_emission` or `tool_calls`. An
import assigns new ids, so the events would point at rows the destination does not have.

## Settings

| variable | default | what it does |
|---|---|---|
| `RECALL_EVENT_LOG` | `false` | writes one `recall_event` row per returned memory per search and digest |
| `RECALL_EVENT_RETENTION_DAYS` | `30` | days an event is kept; at least 1 |

## What it costs

The events ride in the same statement as the `recall_emission` insert, on the same spawned task, so
a call gains no round trip and waits on nothing. The numbers measured on a seeded local store are
in decision 0024.
