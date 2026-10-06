# Telling a connector client how to use lumberroom

A client that connects lumberroom as a remote MCP server, such as claude.ai on the web, desktop or
mobile, Cowork, or ChatGPT, learns two things from the server: the tools, and a short description of
each. Since the change behind issue #92, neither tells the model how to behave. Anthropic's connector
directory rejects tool descriptions and server instructions that give the model orders, so the
server says what each tool does and when it applies, and stops there.

The orders still matter. A model that reads "memory_write records one durable fact" writes less on
its own initiative than one told to write after every decision, and it may ask before each write.
Claude Code, `CLAUDE.md` and `AGENTS.md` clients already carry the orders through
[`client/CLAUDE.md.snippet`](../client/CLAUDE.md.snippet),
[`client/AGENTS.md.snippet`](../client/AGENTS.md.snippet) and the
[lumberroom-claude-code](https://github.com/lumberroom/lumberroom-claude-code) plugin. This page gives
a connector client the same rules, by one of two routes. Either one is enough.

No client on this page has been driven end to end against a live server with these rules in place.
The menu paths come from each vendor's documentation, read on 4 October 2026.

---

## Route 1: paste the instructions

[`client/connector-instructions.md.snippet`](../client/connector-instructions.md.snippet) holds the
rules in under 1,000 characters, short enough for every field below.

| Client | Where to paste it |
| --- | --- |
| claude.ai, Claude Desktop, Claude mobile | The project instructions of the project you work in, or your profile preferences under Settings to cover every chat |
| ChatGPT | Custom instructions, which cap a field at 1,500 characters |

Profile preferences apply to every chat on the account, which suits a store you want consulted
everywhere. Project instructions keep the rules inside one project.

## Route 2: install the skill

[`client/skills/lumberroom-memory/`](../client/skills/lumberroom-memory/SKILL.md) carries the same
rules as a skill, with the phrasing rules for a fact and the limits on the review and delete tools in
full. The model loads a skill when its description matches the conversation, so the rules cost
context only when lumberroom is in play.

On claude.ai, custom skills need code execution switched on. Zip the folder:

```
cd client/skills && zip -r lumberroom-memory.zip lumberroom-memory
```

Then open Customize, then Skills, choose +, then Create skill, then Upload a skill, and upload the
zip. A skill uploaded there belongs to your account alone. See Anthropic's
[Use skills in Claude](https://support.claude.com/en/articles/12512180-use-skills-in-claude).

In Cowork, the lumberroom-memory plugin carries this skill beside its MCP server from version 0.3.2,
so installing the plugin from Anthropic's directory covers both once that version is listed.

---

## How to tell it worked

Start a new chat and ask something an earlier conversation settled. A client following the rules
calls `context_bootstrap` or `memory_search` before it answers. Then state a decision ("we deploy on
Fridays now") and watch for a `memory_write` call. In a second new chat, ask what was decided about
deploys; the answer should come from `memory_search`. A client that asked before writing, or
announced the write, is reading the server's descriptions without the rules.
