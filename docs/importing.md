# Importing a memory out of somebody else's product

This document covers one import: a conversation export or a memory dump from another product,
arriving as text a model has to sort into facts. `lumberroom archive import` is a different command
for a different job, and the difference matters enough to state before anything else here.

## Archive import is not this document

`lumberroom archive export <path>` writes your whole store, every table, to one file: `.lumber` is
an alias for `.jsonl.gz.age`, and a passphrase is the only thing standing between that file and
whoever holds it. Open one without the CLI at all:

```
age -d store.lumber | gunzip | jq .
```

`lumberroom archive import <path>` reads that file back and writes straight to the store. It does not
queue, and it does not wait for you to approve each row. That is the opposite of every promise the
rest of this document makes, and it is deliberate: an archive is a backup or a migration between two
installs you already trust, not a fact arriving from somewhere else that has to earn its place. Use
`--dry-run` to see the row counts before anything lands.

**The credential tripwire covers open rows alone.** It runs during `services::write` at
`Sensitivity::Open` and nowhere else, so a `private` or `sealed` row in an archive passes through
unscanned in both directions. That is correct for a `private` row, encrypted before it ever reaches
the tripwire's view, and it is why an archive import refuses to start at all when `SENSITIVITY_TRIPWIRE`
is off: the one check that exists for the rows it can see has to actually be on.

**Restore is exact for memories, near-exact for aliases, and silent on registry history.** A
restored memory or sealed item keeps its original `created_at`. A restored alias replays through the
same call the console uses to create one, which has no field for the original `created_at` and
stamps a fresh one on insert. Registry history has no write path at all, so an import never replays
it: the values an archived registry key passed through stay out of reach after a restore, even though
the archive file lists them.

Everything below this point is the other import, the one that goes through the review queue.

## The queue import

Two halves arrive by different routes, and the difference decides what an import costs.

**Conversations** come in an export archive. ChatGPT puts one behind Settings, Data controls, Export
data. claude.ai puts one behind Settings, Privacy, Export data, and mails a manifest naming four
parts: `light_metadata`, `projects`, `memories` and `conversations`.

**A memory store** sometimes comes in that archive and sometimes does not. claude.ai ships the
`memories` part. OpenAI does not document saved memories as part of the ChatGPT export, so check the
archive you receive before relying on it. When the archive has no memory file, the route to that
store is to ask the assistant holding it. That is what the dump prompt below is for, and you are
the transport.

Conversations go through extraction and spend provider tokens. A memory dump arrives already sorted
into facts, so it needs no extraction pass. Whatever the route, nothing writes to the store: every
line lands in the queue behind the same review gate as `ingest`, and you approve it.

## What leaves your machine, and when

Read this before running an import over a memory export, because the answer is not "nothing until
you say so".

`--submit` controls **writing to the queue** and nothing else. Two other things send the dump's
content off the machine, and both happen without it:

The **credential tripwire** posts every kept line to your own lumberroom server, which is where the
rules about what counts as a secret live. It answers with rule names; the matched text never comes
back. This is your server.

The **judgement pass** posts every kept line to a model provider, in batches of 25, and it runs by
default. That is somebody else's computer. `--keep-all` is the flag that stops it:

```
lumberroom import memory-dump --file <path>              # parses, scans, asks a model, reports
lumberroom import memory-dump --file <path> --keep-all   # parses, scans, reports. No provider
lumberroom import memory-dump --file <path> --drop-junk  # asks a model, drops what it flags unasked
lumberroom import memory-dump --file <path> --submit     # all of the above, then fills the queue
```

`--delete-source` removes the file after a successful `--submit`, and refuses on its own: a dry run
posts nothing, so deleting would throw away the only copy of a dump that never reached the queue. It
also keeps the file when the run landed nothing, because a dump whose lines were all refused or
blocked is one you still have to fix and retry.

The judgement pass will not run when the tripwire could not. Handing a line to a provider before
anything has checked it for a key is the one outcome this import must never produce, so a missing
credential skips the pass and says so rather than pressing on.

## Judging what is worth keeping

Structural rules run always and decide nothing about meaning: a line with no letters, a line under
ten characters, a line echoing the prompt back. They stay narrow because every rule that catches a
worthless aside also catches a terse true fact, and a real dump is full of short true ones sitting
beside short worthless ones.

Meaning is the judgement pass. It asks a model which lines are **junk**, never which to keep: under
the other shape a lazy or truncated answer deletes the tail of your memory and an empty answer wipes
the import, while under this one those failures keep everything. You see what it flagged and decide,
unless you passed `--drop-junk`. A provider that cannot be reached is not a verdict; every line
stays and the run says the pass did not happen.

## Downloading a claude.ai export is a browser job

`lumberroom import claude` does not fetch anything, and that is not an omission.

An export URL is not a file link. Measured on 25 August 2026: a GET from outside a browser is
answered with the claude.ai single page application, 101,695 bytes of HTML carrying no file URL and
no reference to any storage host. The archive is fetched afterwards by that application's own
JavaScript, using the session cookie your browser holds. What comes back also depends on the client:
curl was answered HTTP 403 with a Cloudflare interstitial while reqwest was answered HTTP 200 with
the shell, one minute apart against the same link.

A command line client could only succeed by carrying your session cookie, which would mean pasting a
credential into a terminal for the one workflow whose point is being easy, and it would break each
time that cookie rotates. So the browser downloads and the CLI checks:

```
lumberroom import claude --manifest <manifest.json> [--dir <where you saved them>]
```

It reads the manifest, looks in the directory, and reports each part as `have`, `missing`, or
`not a zip`. That last one is the case `ls` cannot show you: a browser that was signed out saves the
HTML shell under the filename it was given, and it sits there looking like a successful download
until something reads the first four bytes. Anything outstanding gets its link printed for you to
open, and the retry line names the same command again rather than a shorter one that does not exist.

Every link works once, which is why nothing here makes a request. Opening one in the browser spends
it. Running this command again does not.

## The dump prompt

`lumberroom import prompt` prints the text below. It is one string in
`crates/lumberroom/src/import/dump_prompt.rs`, and a test in that file fails when this document and
the binary disagree, because a prompt that drifts between the two produces dumps the parser half
understands.

Paste it into ChatGPT, claude.ai, or any assistant that has been keeping notes on you. Save the code
block it answers with, then hand that file to `lumberroom import`.

```
Export everything you have stored about me: your saved memories, the instructions you
keep about me, and any profile or long-term context you carry between conversations.

Rules:
- Use my words verbatim wherever you have them, above all for instructions, preferences
  and corrections I made to you.
- Export what you have stored. Do not summarise a conversation, and do not infer a fact
  to fill a gap. Fewer true lines beat more plausible ones.
- Leave out any password, API key, token, private key, card number, or connection string
  that carries credentials. Skip the whole line rather than masking part of it.
- Export only what you hold yourself. If you have a memory tool, a connector or a search
  that can fetch facts about me from somewhere else, do not call it and do not include
  anything it would return. Those facts already live where they came from.

Cover these sections, in this order. Drop a section you hold nothing for.

1. INSTRUCTIONS. Rules I told you to follow: tone, format, what to always do, what to
   never do, and corrections I made to how you work.
2. IDENTITY. Name, where I live, family, languages, education, what I am interested in.
3. CAREER. Current and past roles, companies, and the skills I work in.
4. SETUP. My machines, operating systems, tools, hosts, paths, services and accounts.
5. PROJECTS. One entry per project I built or committed to. Open the line with the
   project name, then what it does, where it stands, and what shaped it.
6. DECISIONS. A choice I made and the reason: what I picked, what it beat, and any date
   I said I would look at it again.
7. PREFERENCES. Tastes and working style that hold across topics.

Write one entry per line, oldest first inside each section, under the section name on
its own line. Each line looks like this:

  [YYYY-MM-DD] (stated) - The fact, as one sentence that stands on its own.

Use [unknown] when you hold no date for it. Mark a line (inferred) in place of (stated)
when it comes from reading a past conversation rather than from something you saved.

Put the whole export inside one code block, and make the last line inside that block
either COMPLETE or MORE REMAINS. If you are running short of room, stop after a whole
entry and write MORE REMAINS; a dump that stops in the middle of a line is worse than a
short one. If more remains, I will ask you to continue.
```

## Why it asks for what it asks for

**Anthropic shipped this pattern first.** Their cross-provider import on the claude.ai memory
settings page hands the owner a prompt, and takes back a code block of `[YYYY-MM-DD] - entry` lines
under section headers. The format here stays compatible: a dump written to their prompt parses here,
so an owner who already ran theirs does not run ours twice. Their instruction to preserve the
person's words verbatim rests on the same reasoning that puts corrections first in
`crates/lumberroom/src/ingest/prompt.rs`.

Three things this asks for that theirs does not, each because something downstream has a field for
it:

**`(stated)` against `(inferred)`.** A proposal already carries a `confidence` whose two values are
`stated` and `inferred`, and claude.ai's own exported memory files mark every bullet `- [stated]` in
that same vocabulary. Measured against a real export on 25 August 2026: 107 bullets across 18 memory
files, every one `stated` and none `inferred`. A line arriving with no marker reads as `stated`,
which is what both formats mean by leaving it off.

**A SETUP section.** Hosts, paths, machines and services belong in `global`. A taxonomy of identity,
career and preferences has nowhere to put them.

**A DECISIONS section.** What was chosen, what it beat, and when to look at it again. An owner
cannot reconstruct that later, and no memory feature on either surface stores it well.

## Take the dump with your memory connector switched off

Run on ChatGPT on 25 August 2026 with the lumberroom connector attached to the chat, this prompt
came back with **14 of its 54 entries lifted out of lumberroom itself**, the custom-instruction block
among them. The assistant cannot tell "what I remember about you" apart from "what I just read out
of your memory server", so it exported ours as its own. Importing that dump would have written the
store back into itself as fresh proposals.

The prompt now tells the assistant not to call such a tool. Switching the connector off for that one
chat is the check that does not depend on the model following an instruction.

## A dump can stop in the middle of a sentence

The same run ended mid-word, with no `COMPLETE` and no `MORE REMAINS`: the output limit ate the
marker along with the last entry, and three of the seven sections never appeared. The marker is
least reliable in the one case it exists for.

So the prompt asks for the marker inside the code block and asks the assistant to stop after a whole
entry rather than mid-line, and the parser treats a final line that fails the grammar as truncation
whatever the marker says. When a dump is short, ask the assistant to continue and import the second
half as its own file.

The refusal line earns its place. `services::write` runs the credential tripwire over every
proposal and would catch a key that arrived this way, and a key that never enters the dump never
reaches the clipboard, the downloads directory, or the assistant's next context window.

## What a dump is worth

Read it before you approve it. An assistant asked to recall everything it knows about a person will
produce fluent lines whether or not it holds them, which is the failure the verbatim rule and the
`(inferred)` marker exist to make visible rather than to prevent. The queue is where that gets
decided, and the reviewer is you.
