# The provider path

How `lumberroom ingest extract` reaches a model, what each setting does, and which claims here were
measured rather than assumed. Read `docs/ingestion.md` for the pipeline this sits inside, and
`docs/ingestion-mode-a.md` for the mode that needs no provider at all.

Every figure below carries the date it was taken. A provider changes its behaviour without telling
anybody, so a measurement here is a record of one day rather than a property of the world.

## Two request shapes, not five

`chat/completions` covers OpenAI, OpenRouter, z.ai and any local server, because all of them speak
that path. Anthropic is the second shape, and it differs in three ways that matter: the URL is
`/v1/messages`, the key rides an `x-api-key` header beside `anthropic-version`, and the answer sits
in a `content` array rather than at `choices[0].message.content`.

`custom` speaks `chat/completions` deliberately. It is the escape hatch for Ollama, LM Studio and
vLLM. Pointing `--provider custom --base-url https://api.anthropic.com` does not reach the messages
shape and should not, because the two differ in the auth header and in where the answer lives, not
only in the URL.

## The table

| provider | base URL | default model | JSON mode |
|---|---|---|---|
| `openai` | `https://api.openai.com/v1` | `gpt-4o-mini` | on |
| `openrouter` | `https://openrouter.ai/api/v1` | none, `--model` required | off |
| `zai` | `https://api.z.ai/api/coding/paas/v4` | `glm-5.3` | on |
| `anthropic` | `https://api.anthropic.com` | `claude-opus-5` | not applicable |
| `custom` | none, `--base-url` required | none | off |

Resolution order is the table, then `ingest.providers.<name>` in `~/.config/lumberroom/config.json`, then
`LUMBERROOM_INGEST_KEY_<PROVIDER>`, then the flags. `zai` also honours `ZAI_API_KEY`, because the owner
already had that variable set and duplicating it under a lumberroom-specific name is friction the key does
not need.

## Where keys live, and why there is no flag for them

`~/.config/lumberroom/config.json`, at mode 0600, refused when it is group or world readable. Not `.env`:
that file belongs to the server, Docker Compose reads it, several shell scripts source it, and
`AUTH_TOKENS` already proved how easily its contents end up somewhere nobody chose.

`lumberroom ingest keys set <provider>` reads the key from stdin. **There is no `--api-key` flag and
there must not be one.** Every argument of a running process is world readable through `ps`, and an
interactive shell writes the command into its history file, so a key passed that way lands in two
places the owner did not pick and stays there. The key never enters a log line, an error message or
the working directory; a failure prints the provider name and the HTTP status.

## LiteLLM, Ollama and other OpenAI-compatible servers

`custom` reaches any server that speaks `chat/completions`. The CLI posts to
`{base_url}/chat/completions` and sends the key as a bearer only when one is set, so a local server
with no auth needs no key.

A [LiteLLM](https://docs.litellm.ai/) proxy, with its virtual key or master key:

```
cat litellm-key.txt | lumberroom ingest keys set custom
lumberroom ingest extract --run <id> --provider custom \
  --base-url http://localhost:4000 --model <model_name from the LiteLLM config>
```

`LUMBERROOM_INGEST_KEY_CUSTOM` overrides the stored key, which suits CI. Save the settings so
`--provider custom` is the only flag:

```json
{"ingest": {"providers": {"custom": {"base_url": "http://localhost:4000", "model": "my-extractor", "json_mode": true}}}}
```

`json_mode` is off by default for `custom`. Turn it on only when the model behind the server accepts
`response_format: json_object`; some answer HTTP 400 to it (see `json_mode` below).

LiteLLM fronts Anthropic, Bedrock, Vertex, Ollama and many more and translates them to the OpenAI
shape, so Claude or any other model it routes to works through `custom` without the `anthropic`
provider.

Ollama, vLLM and LM Studio work the same way. Ollama's base URL is `http://localhost:11434/v1`.

There is one `custom` entry. For a second OpenAI-compatible endpoint, point another entry elsewhere:
`ingest.providers.openai.base_url` replaces the OpenAI default. Set `model` and `json_mode` in the
same entry to match that server, since the `openai` defaults (`gpt-4o-mini`, JSON mode on) still apply.

## `reasoning`, off by default

```json
{"ingest": {"providers": {"openrouter": {"reasoning": true}}}}
```

Off unless a config entry asks for it, because reasoning bills as output and this task does not need
it.

**Measured 21 August 2026, qwen3.7-flash through OpenRouter.** A request to reply with `{"ok":true}`
and nothing else came back with 215 completion tokens, 205 of them reasoning tokens, for an eleven
character answer. The same request with `reasoning: {"enabled": false}` came back with 5. That is
roughly forty times the output bill for no gain on a task that is classification rather than
deduction.

The flag is sent in OpenRouter's spelling, which reaches every model behind it. A provider wanting a
different spelling puts it in `body`, which merges last and wins. That is how `zai` carries
`{"thinking": {"type": "disabled"}}`.

## `json_mode`, a model-level setting

```json
{"ingest": {"providers": {"openrouter": {"model": "deepseek/deepseek-v4-flash", "json_mode": true}}}}
```

It is model-level rather than provider-level because one provider serves models that disagree with
each other, and OpenRouter is a door to hundreds.

**Accepting the parameter and honouring it are two claims, and the second is rarer.** Measured
21 August 2026 with a prompt that asked for prose and an object at once, so that a model honouring
`json_object` could not obey the prose instruction:

| model | `json_mode` off | `json_mode` on |
|---|---|---|
| `deepseek/deepseek-v4-flash` | prose then JSON | the object, prose suppressed |
| `openai/gpt-5.6-luna` | prose then JSON | the object, prose suppressed |
| `qwen/qwen3.7-flash` | prose then JSON | a one-element **array**, carrying a `thought` key |
| `z-ai/glm-4.7-flash` | prose then JSON | a one-element **array** |

Two of four honour JSON. Two of those four honour the shape asked for. `qwen3.7-flash` put a
`thought` key inside its payload with reasoning already disabled, which is chain-of-thought leaking
into structured output.

**On this pipeline's real task, `qwen3.7-flash` scored better with `json_mode` off.** Its best run on
the duplicate-audit probe carried no `response_format` at all, and the prompt instruction alone was
enough. Turning JSON mode on made its shape worse rather than better.

`glm-4.7-flash` is the clearest case for turning it on: without it the model wraps its answer in a
```json fence, and with it the answer comes back bare.

## What `parse_response` tolerates, and why each tolerance exists

Every one of these was added because a real model did the thing, not as defensive habit.

- **A fenced code block.** `glm-4.7` wrapped its object in a json fence in a call whose prompt told
  it to return an object and nothing else (20 August 2026).
- **A bare `<no-facts/>`** arriving as text rather than inside the object. That refusal is a correct
  and common answer: most chunks are ordinary work with nothing durable in them.
- **A one-element array around the object.** `qwen3.7-flash` and `glm-4.7-flash` both return one
  under JSON mode (21 August 2026). Refusing it would throw away a good answer over its wrapper.
- **Two objects in an array are refused**, not merged. A second answer is not a half to guess
  between, which is the same rule the Anthropic accessor follows for two text blocks.

Anything that parses to none of these is a failed chunk, named in the report with the first 200
characters of what came back, and recorded in `state.json` rather than dropped.

## The Anthropic accessor, and the trap in it

Take **the first block in `content` whose `type` is `text`**, never `content[0]`. `content` is an
array of blocks and a thinking block leads it whenever extended thinking is on, which
`claude-opus-5` does by default. Read `content[0].text` and a perfectly good response comes back as
absent.

Concatenate nothing. A second text block is a second answer, and that is a failed chunk rather than
a guess about which half to keep.

`max_tokens` is required by that API and has no default there. It ships at 16,000, which covers
thinking plus a chunk's worth of facts and stays under the point where a non-streaming request risks
a timeout. That number is a design target and nothing measured it.

## Known instability

`qwen/qwen3.7-flash` answered HTTP 400 to `response_format` three times in a row on 21 August, then
answered 200 to four different request shapes an hour later, and one large request with JSON mode set
hung past 300 seconds rather than returning an error. The route is occasionally flaky. That is an
argument for the retry and the per-request timeout that `extract.rs` already carries, and not for a
config flag.

`z.ai` on 20 August: `response_format: json_object` sent with the default thinking mode **hangs**,
with no error and no response, and the same request with thinking disabled returns in 2.1 seconds.
For GLM that is mandatory rather than an optimisation, which is why the `zai` defaults carry it.

## What has and has not been run

**Run against a live provider:** the `chat/completions` path, through z.ai with `glm-5.3` for the
whole first ingestion, and through OpenRouter with `qwen3.7-flash` for the model probes.

**Never called:** the Anthropic path. Its request shape, headers and response accessor are
implemented against the API contract and tested against fixtures, and not one request has gone to
Anthropic from this code. The same is true of Mode C: `batch.rs` is type-checked and fixture-tested,
and no batch has been submitted.

## Adding a provider

Add an arm to `defaults_for` naming its base URL, default model, shape, JSON mode default and any
`extra_body` it cannot go out without. If it speaks `chat/completions` that is the whole change. If
it speaks something else, it needs a `Shape` and a response accessor, and the accessor is where the
traps live: read the Anthropic one first.

Then measure it before trusting it. `docs/research/` is where a probe's output belongs, and the two
questions worth asking are whether it honours the shape you asked for and what it does when the
prompt and `response_format` disagree.
