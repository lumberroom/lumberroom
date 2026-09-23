# 19. A caller corrects a proposal

**Date:** 23 September 2026 · **Status:** accepted, not built · **Decided by:** the owner

## Decision

`ProposalSource::decide` takes a `ProposalDecision`: the verdict, an optional corrected text, an
optional reason, the version of the proposal the caller read, and the surface the call arrived on.
A repair is `apply` with `content`, not a new verdict. A proposal decision over MCP requires a
`reason` and a `version`; over HTTP both stay optional. The data fence `render` draws around row
content and proposal text takes a nonce drawn fresh per call, so a row cannot pre-forge its closing
line. Once the person has asked an agent to work the queue, that agent may decide proposal items on
its own, one `review_decide` call per item, without reading each one back first.

## Context

`ProposalSource::decide` (`src/services/review_queue.rs:221`) took `(ctx, id, verdict)` alone.
`Decision` already carried `content` and `reason`, but `decide_proposal` read only `d.verdict` and
`d.key` before calling the source: a caller's corrected text and its reason for overriding a check
stopped at the engine, and no source could tell one proposal from an earlier version of itself.

## What lost, and why

A `repair` verdict lost to `apply` plus `content`. CLI 0.5.0's `wire::Verdict`
(`crates/lumberroom/src/wire.rs:331`) is a closed enum with no fallback variant; a new word fails
every installed 0.5.0 CLI's read of any page holding a repairable item, where `apply` with content
reads every page and simply ignores a field it does not know. Four more trait parameters lost to
one struct, because the next field a source needs is then a line in the struct rather than a
signature change across every implementation. Reading `via` off `Ctx.invocation` lost, because that
value comes from a header the caller sets, not from the transport the call actually arrived on.
Requiring a reason on every surface, or on every override, lost because CLI 0.5.0 sends a reason on
delete alone (`crates/lumberroom/src/review.rs:638`); either would refuse the CLI's own `apply` on
every proposal a check already held. `repaired` as the answer
field's name lost to `content_written`, because a downstream proposal source may already publish a
field called `repaired` with another meaning.

## Costs accepted

A source can still be handed `content` for an item it never marked `repairable`, and has to refuse
it itself; the trait cannot stop that call from arriving. A source that ignores `content` and
writes its own text anyway is visible only after the fact, through `content_written: false` and a
warning the engine logs by origin. A caller over HTTP, or the CLI, or any script holding a bearer
token, can still decide a proposal with no reason and no version, so the thirty-day read that
settles whether `reason_required` earns its keep covers MCP calls alone. An instruction injected
into the owner's own turn, through text the owner's own client rendered, is bounded by the record
and the source's undo, not prevented at the call.

## Not for

Conflict and stale items. They stay with the person; nothing here gives an agent a path to decide
them unasked, and `delete` in particular runs through no source check at all.

## Reversal

Reverse `reason_required` over MCP if thirty days of reasons recorded that way read as empty of
information, one fixed sentence repeated on every item rather than a reason peculiar to it. Reverse
the rule letting an agent decide proposal items unasked once the person has asked it to work the
queue, on the first applied proposal nobody asked for or the first override whose recorded reason
does not hold up. Replace `apply` plus `content` with a dedicated verdict once CLI 0.5.0 is out of
use and `wire::Verdict` carries a fallback variant.
