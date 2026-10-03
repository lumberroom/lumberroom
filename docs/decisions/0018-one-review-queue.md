# 0018. One review queue, one decide path

22 September 2026. Accepted, implemented.

What was run, and where: on 22 September 2026, `./scripts/cargo.sh check --all-targets` ran clean,
then the library suite, the `review_queue` acceptance suite and the full suite. `test -j 1` reported
1045 passed, 0 failed; the library suite reported 796 passed; `review_queue` reported 27 passed;
`review_queue_mcp` reported 8 passed.

The measurements the design rests on come from the maintainer's own store on 22 September 2026: 39
conflict pairs at the 0.90 threshold over about 1,390 live rows, the conflicts read taking 4.2
seconds through `lumberroom review`. A dev-container probe on the same machine timed the self-join
at 2.48 seconds over 1,400 rows in one namespace, 13.33 seconds at 3,000 and 34.96 seconds at 5,000.

## The decision

One queue shape with a `source` field, one decide route, a ledger that remembers "both are fine",
and a proposal seam the engine publishes and leaves empty.

`GET /admin/review/queue` answers `conflict`, `stale` and `proposal` in one envelope; every item
carries a key that addresses it (`conflict:<id>:<id>`, `stale:<id>`, `proposal:<origin>:<id>`), the
rows in full, and the verdicts that item takes for that caller. `POST /admin/review/decide` takes a
key and a verdict and calls the service that already owns the act: `review::supersede`,
`write::run` plus `review::supersede` for a merge, `review::confirm`, `forget::by_id`, or the
proposal source's own `decide`. The verdict list on the item is what the CLI draws its key line
from, so a client never offers a key the server will refuse on a grant or on a shape.

`memory_pair_dismissed` records a `keep_both`: the two ids in uuid order, the client, and the token
fingerprint. The conflicts statement anti-joins it, so a pair somebody has read and kept leaves the
queue and the queue shrinks as it is worked.

`CONFLICT_SCAN_MAX` bounds the conflict source. The self-join is O(n squared) in the rows of one
namespace with no index able to help it, and the queue now answers requests rather than running by
hand, so a namespace count runs first and the source refuses past the ceiling instead of hanging.

`last_confirmed_at` enters the stale predicate. The column existed and no reader read it, so
confirming a row changed nothing anybody could see.

## What lost

A `review` subcommand per source, which spreads one decision across four commands and four
response shapes. A stored queue table for conflicts, which drifts out of step with the store it
describes. A `skip` verdict remembered on the server: a skip is not a judgement about the rows, and
a client that wants to walk past an item can keep its own cursor. A `mayReview` capability flag,
which would gate the read of rows the caller may already read. `expire` on the loop, because
decision 0017 owns that verb and its own surface. Folding the `cleanup` proposal queue in as the
engine's first source, which would double-list every pair the cleanup pass already holds at 0.97 and
above while the conflict source reads from 0.90 with no anti-join between them.

## Costs accepted

Two round trips per pair, since each half is re-fetched by id for its own ceiling check. An offset
cursor rather than a keyset one, bounded at 2,000. A verdict list computed per item per caller. One
namespace count before every conflicts read. A conflict source that refuses a large namespace
rather than answering slowly. A confirmed stale row returning after one window rather than never.
A trait with no implementation in this repository, exercised by a test double.

## Not for

Proposals this engine produces. The `cleanup` pass keeps its own queue and its own routes until the
double-listing above is resolved. Anything that writes text on the caller's behalf: a merge takes
the text from whoever is driving, and nothing in the engine generates it.

## Reversal

Spec section 9 carries all four conditions. The one that decides this record: if a month of use
leaves the conflict backlog no smaller than the 39 pairs measured on 22 September 2026, the queue is
not reducing the work it was built to reduce, and the ledger plus the loop come out in favour of the
four read-only commands they replaced.
