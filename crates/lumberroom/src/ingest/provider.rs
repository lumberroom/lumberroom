//! Provider configuration and the two request shapes.
//!
//! One code path reaches OpenAI, OpenRouter, z.ai and any local endpoint, because all four speak
//! `POST {base_url}/chat/completions`. Anthropic is the second shape: a different path, different
//! headers, and a response whose text sits in an array of blocks rather than at a fixed index.
//! Both shapes hand their text to the same `parse_response`, because the tolerances a model needs
//! (a code fence around the JSON, a bare `<no-facts/>`) have nothing to do with which vendor sent
//! it, and a second copy of that logic is a second place for the fence rule to rot.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::{err, err_code, Result};
use crate::ingest::{ChunkOutput, Usage};

/// One entry under `ingest.providers.<name>` in `~/.config/lumberroom/config.json`.
///
/// Keys live there and never in `.env`: that file is the server's, Docker Compose reads it, and
/// `AUTH_TOKENS` already proved how easily its contents end up somewhere unintended. There is no
/// `--api-key` flag, because every argument of a running process is world readable through `ps`
/// and an interactive shell writes the command line to its history file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Per-provider extras merged into every request body. The `zai` entry carries
    /// `{"thinking":{"type":"disabled"}}`, which a GLM request cannot go out without: sent with the
    /// default thinking mode, `response_format: json_object` hangs with no error and no response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// Whether to let the model reason before answering. Off unless asked for, because reasoning
    /// bills as output and this task does not need it: measured on qwen3.7-flash, a request to
    /// reply with `{"ok":true}` spent 215 output tokens, 205 of them reasoning, for an eleven
    /// character answer. Turning it off took that to five.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Whether to send `response_format: {"type":"json_object"}`. Model-level rather than
    /// provider-level, because two models behind one provider disagree: OpenRouter lists
    /// `response_format` in qwen3.7-flash's supported parameters and the model answers HTTP 400 to
    /// it, while the same provider serves models that accept it. A catalogue saying a parameter is
    /// supported is a claim, and this one is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_mode: Option<bool>,
}

/// Which of the two request shapes a provider speaks.
///
/// `custom` is deliberately `ChatCompletions`: it is the escape hatch for Ollama, LM Studio and
/// vLLM, all of which serve the OpenAI path. Pointing `--provider custom --base-url` at Anthropic
/// does not reach the messages shape, and should not, because the two differ in the auth header
/// and in where the answer sits, not only in the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    ChatCompletions,
    AnthropicMessages,
}

/// A provider resolved from config, environment and flags, ready to call.
#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub key: Option<String>,
    pub extra_body: Value,
    pub shape: Shape,
    /// Off unless the config asks for it. A model that reasons still answers, so the cost of
    /// leaving this on is a bill rather than a failure, which is why it has to be a decision.
    pub reasoning: bool,
    /// Whether the request carries `response_format`. Accepting the parameter and honouring it are
    /// two claims, and `parse_response` tolerates a fenced or bare answer either way.
    pub json_mode: bool,
}

struct Defaults {
    base_url: Option<&'static str>,
    model: Option<&'static str>,
    extra_body: Value,
    shape: Shape,
    /// The default for this provider's usual model. Config overrides it per entry, which is the
    /// point: `openrouter` serves models that disagree with each other about this.
    json_mode: bool,
}

fn defaults_for(name: &str) -> Result<Defaults> {
    Ok(match name {
        "openai" => Defaults {
            base_url: Some("https://api.openai.com/v1"),
            model: Some("gpt-4o-mini"),
            extra_body: json!({}),
            shape: Shape::ChatCompletions,
            // gpt-4o-mini honours it and returns a bare object.
            json_mode: true,
        },
        "openrouter" => Defaults {
            base_url: Some("https://openrouter.ai/api/v1"),
            model: None,
            extra_body: json!({}),
            shape: Shape::ChatCompletions,
            // Off, because OpenRouter is a door to many models and they disagree. qwen3.7-flash
            // answers HTTP 400 to `response_format` while its catalogue entry lists the parameter as
            // supported. A config entry turns it on for a model known to honour it.
            json_mode: false,
        },
        "zai" => Defaults {
            base_url: Some("https://api.z.ai/api/coding/paas/v4"),
            model: Some("glm-5.3"),
            extra_body: json!({ "thinking": { "type": "disabled" } }),
            shape: Shape::ChatCompletions,
            // GLM honours it, and only with thinking disabled: sent with the default thinking mode it
            // hangs with no error and no response.
            json_mode: true,
        },
        // The base URL carries no `/v1`: the messages path is `{base_url}/v1/messages`, and the
        // OpenAI-compatible providers put their version segment in the base URL instead.
        //
        // `claude-opus-5` runs extended thinking unless a request turns it off, which is what puts
        // a thinking block at `content[0]` and makes the accessor below load-bearing on the default
        // path rather than a guard against a mode nobody uses. Turning thinking off is the worse
        // trade here: with it disabled the model sometimes writes internal XML into the visible
        // answer, and this pipeline parses that answer as JSON. `--model` and the config file
        // override the choice for anyone who wants a cheaper tier for a whole corpus.
        "anthropic" => Defaults {
            base_url: Some("https://api.anthropic.com"),
            model: Some("claude-opus-5"),
            extra_body: json!({}),
            shape: Shape::AnthropicMessages,
            // The messages shape has no `response_format` at all.
            json_mode: false,
        },
        "custom" => Defaults {
            base_url: None,
            model: None,
            extra_body: json!({}),
            shape: Shape::ChatCompletions,
            // A local server may be anything. Off until somebody says otherwise.
            json_mode: false,
        },
        other => {
            return Err(err(format!(
                "unknown provider {other}. Use openai, anthropic, openrouter, zai or custom."
            )))
        }
    })
}

/// A key file readable by group or other is a key that is already somewhere else. Matches
/// `src/crypto/kek.rs`'s check on the server's own key file: refuse rather than warn, because a
/// warning in a log nobody reads is how a bad mode survives to the next incident.
#[cfg(unix)]
fn refuse_loose_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        // No file means nothing to leak. Provider keys can arrive by environment variable alone.
        Err(_) => return Ok(()),
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(err(format!(
            "{} has mode {mode:04o} and must not be readable by group or other. Run: chmod 600 {}",
            path.display(),
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_loose_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

/// Copy every key of `overlay` onto the top level of `base`, overwriting on conflict. Used both to
/// merge a config-file `body` onto a provider's defaults and to merge a provider's `extra_body`
/// onto the request the CLI is about to send: the same shallow rule both times.
fn merge_top_level(base: &mut Value, overlay: &Value) {
    let (Some(base_obj), Some(overlay_obj)) = (base.as_object_mut(), overlay.as_object()) else {
        return;
    };
    for (k, v) in overlay_obj {
        base_obj.insert(k.clone(), v.clone());
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Defaults per name, then the config file, then `LUMBERROOM_INGEST_KEY_<PROVIDER>`, then the flags.
pub fn resolve(
    name: &str,
    file: &crate::config::FileConfig,
    model: Option<&str>,
    base_url: Option<&str>,
) -> Result<Provider> {
    refuse_loose_permissions(&file.path)?;
    let defaults = defaults_for(name)?;

    let overlay: ProviderConfig = file
        .value
        .get("ingest")
        .and_then(|v| v.get("providers"))
        .and_then(|v| v.get(name))
        .cloned()
        .map(|v| serde_json::from_value(v).unwrap_or_default())
        .unwrap_or_default();

    let mut resolved_base_url = defaults.base_url.map(str::to_string);
    if let Some(b) = &overlay.base_url {
        resolved_base_url = Some(b.clone());
    }
    if let Some(b) = base_url {
        resolved_base_url = Some(b.to_string());
    }
    let resolved_base_url = resolved_base_url
        .ok_or_else(|| err(format!("provider {name} has no base URL. Pass --base-url.")))?
        .trim_end_matches('/')
        .to_string();

    let mut resolved_model = defaults.model.map(str::to_string);
    if let Some(m) = &overlay.model {
        resolved_model = Some(m.clone());
    }
    if let Some(m) = model {
        resolved_model = Some(m.to_string());
    }
    let resolved_model = resolved_model
        .ok_or_else(|| err(format!("provider {name} has no default model. Pass --model.")))?;

    let defaults_json_mode = defaults.json_mode;
    let mut extra_body = defaults.extra_body;
    if let Some(body) = &overlay.body {
        merge_top_level(&mut extra_body, body);
    }

    let mut key = overlay.key.clone();
    // LUMBERROOM_INGEST_KEY_<PROVIDER> beats the config file's own key. ZAI_API_KEY is the one exception:
    // the owner already has that variable set from working with z.ai directly, and asking him to
    // duplicate it under a lumberroom-specific name is friction the key does not need.
    let mut env_key = env_var(&format!("LUMBERROOM_INGEST_KEY_{}", name.to_uppercase()));
    if env_key.is_none() && name == "zai" {
        env_key = env_var("ZAI_API_KEY");
    }
    if let Some(k) = env_key {
        key = Some(k);
    }

    Ok(Provider {
        // Config wins over the provider default, because the setting is really about the model and
        // one provider serves many. Reasoning stays off unless the config says otherwise: it bills
        // as output and this task does not need it.
        reasoning: overlay.reasoning.unwrap_or(false),
        json_mode: overlay.json_mode.unwrap_or(defaults_json_mode),
        name: name.to_string(),
        base_url: resolved_base_url,
        model: resolved_model,
        key,
        extra_body,
        shape: defaults.shape,
    })
}

/// One chunk, one call. Returns the parsed output or the reason it failed, never the key.
///
/// Retry and the timeout wrapper live in `extract.rs`, one call up: this makes exactly one HTTP
/// request and reports what happened to it.
pub async fn extract_chunk(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    user: &str,
    timeout_secs: u64,
) -> Result<(ChunkOutput, Usage)> {
    let (text, usage) = call_text(http, p, system, user, timeout_secs).await?;
    Ok((parse_response(&text)?, usage))
}

/// The transport half: one request, one response, the model's text and what it cost.
///
/// Retry and the timeout wrapper live in `extract.rs`, one call up: this makes exactly one HTTP
/// request and reports what happened to it. The key never enters an error message.
pub async fn call_text(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    user: &str,
    timeout_secs: u64,
) -> Result<(String, Usage)> {
    let req = match p.shape {
        Shape::ChatCompletions => chat_completions_request(http, p, system, user, timeout_secs),
        Shape::AnthropicMessages => anthropic_request(http, p, system, user, timeout_secs),
    };

    let res = req.send().await.map_err(|e| {
        if e.is_timeout() {
            err_code(format!("{} timed out after {timeout_secs}s", p.name), 3)
        } else {
            err(format!("{} could not be reached: {e}", p.name))
        }
    })?;

    let status = res.status();
    if !status.is_success() {
        // The key never enters this message. Only the provider's name and the bare status do.
        return Err(err_code(
            format!("{} answered HTTP {}", p.name, status.as_u16()),
            status.as_u16() as i32,
        ));
    }

    let value: Value = res
        .json()
        .await
        .map_err(|e| err(format!("{} answered a body that did not parse as JSON: {e}", p.name)))?;

    match p.shape {
        Shape::ChatCompletions => {
            let text = value["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(|| err(format!("{} answered with no message content", p.name)))?;
            let usage = Usage {
                requests: 1,
                prompt_tokens: value["usage"]["prompt_tokens"].as_i64().unwrap_or(0),
                completion_tokens: value["usage"]["completion_tokens"].as_i64().unwrap_or(0),
            };
            Ok((text.to_string(), usage))
        }
        Shape::AnthropicMessages => {
            let text = anthropic_text(&value, &p.name)?;
            // Anthropic counts the same two numbers under different names. `state.json` and the
            // report speak one vocabulary, so the rename happens here rather than at the reader.
            let usage = Usage {
                requests: 1,
                prompt_tokens: value["usage"]["input_tokens"].as_i64().unwrap_or(0),
                completion_tokens: value["usage"]["output_tokens"].as_i64().unwrap_or(0),
            };
            Ok((text.to_string(), usage))
        }
    }
}

fn chat_completions_request(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    user: &str,
    timeout_secs: u64,
) -> reqwest::RequestBuilder {
    let mut body = json!({
        "model": p.model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
        "temperature": 0,
    });
    if p.json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    }
    // OpenRouter's shape, and it is the one that reaches every model behind it. A provider that
    // wants a different spelling puts it in `body`, which merges last and wins.
    if !p.reasoning {
        body["reasoning"] = json!({ "enabled": false });
    }
    merge_top_level(&mut body, &p.extra_body);

    let url = format!("{}/chat/completions", p.base_url);
    let mut req = http.post(&url).timeout(std::time::Duration::from_secs(timeout_secs)).json(&body);
    if let Some(key) = &p.key {
        req = req.bearer_auth(key);
    }
    if p.name == "openrouter" {
        req = req
            .header("HTTP-Referer", "https://github.com/lumberroom/lumberroom")
            .header("X-Title", "lumberroom");
    }
    req
}

/// `max_tokens` is required by this API and has no default there, so a number has to be chosen.
/// 16k is a design target; nothing here measured it. It has to cover a chunk's worth of extracted
/// facts, and on a model with extended thinking on it caps thinking and answer together, so the
/// answer needs headroom above what the JSON alone would take. It also stays under the point where
/// a request this long-lived starts risking an HTTP timeout, and this path does not stream.
const ANTHROPIC_MAX_TOKENS: i64 = 16_000;

/// `POST {base_url}/v1/messages`. The system prompt is a top-level field rather than a message,
/// and the key rides `x-api-key` rather than `Authorization`, so neither piece of the OpenAI
/// builder transfers.
fn anthropic_request(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    user: &str,
    timeout_secs: u64,
) -> reqwest::RequestBuilder {
    let mut body = json!({
        "model": p.model,
        "max_tokens": ANTHROPIC_MAX_TOKENS,
        "system": system,
        "messages": [
            { "role": "user", "content": user },
        ],
    });
    // The config file's `body` merges here for the same reason it does on the other path: it is
    // the owner's way to reach a field this code does not know about without editing this code.
    merge_top_level(&mut body, &p.extra_body);

    let url = format!("{}/v1/messages", p.base_url);
    let mut req = http
        .post(&url)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    // The header goes out only when a key is configured, matching the other path. A keyless call
    // then fails as "anthropic answered HTTP 401", which names the problem without naming anything
    // secret.
    if let Some(key) = &p.key {
        req = req.header("x-api-key", key);
    }
    req
}

/// The first block in `content` whose `type` is `text`, and never `content[0]`.
///
/// `content` is an array of blocks, not a string. A thinking block leads it whenever the model has
/// extended thinking on, which the default model here does, so `content[0].text` reads as absent
/// and a perfectly good response fails to parse. Reading the array by index is the trap: it works
/// against every response until the day thinking is on, and then it reports the model as broken.
///
/// Two text blocks are refused rather than concatenated. A second text block is a second answer,
/// and joining them guesses which half the owner meant to keep; a chunk that fails loudly gets
/// retried, while a chunk that silently keeps the wrong half becomes a fact he never said. Zero
/// text blocks lands in the same refusal, which is also where a refusal-only response with an
/// empty `content` array ends up.
fn anthropic_text<'a>(value: &'a Value, provider: &str) -> Result<&'a str> {
    let blocks = value["content"]
        .as_array()
        .ok_or_else(|| err(format!("{provider} answered with no content array")))?;

    let texts: Vec<&Value> =
        blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("text")).collect();

    match texts.len() {
        0 => Err(err(format!("{provider} answered with no text block"))),
        1 => texts[0]["text"]
            .as_str()
            .ok_or_else(|| err(format!("{provider} answered a text block holding no text"))),
        n => Err(err(format!(
            "{provider} answered with {n} text blocks, which is {n} answers. Refusing to guess \
             which one to keep."
        ))),
    }
}

/// Tolerant of a fenced code block around the JSON and of a bare `<no-facts/>`. `glm-4.7` wrapped
/// its object in a json fence in a call whose prompt told it to return an object and nothing else.
///
/// Every field on `ChunkOutput` carries `#[serde(default)]`, which is right for a model that
/// dropped `tags` or `confidence` and wrong here unguarded: `{"error":"quota exceeded"}` would
/// deserialise straight into `facts: [], refusal: None` and get written as a done chunk holding
/// nothing durable. That is this codebase's named worst failure, so an object is only accepted
/// once it names `facts` or `refusal` for itself.
pub fn parse_response(text: &str) -> Result<ChunkOutput> {
    let candidate = strip_fence(text.trim());
    if candidate == "<no-facts/>" {
        return Ok(ChunkOutput { facts: vec![], refusal: Some("<no-facts/>".to_string()) });
    }
    let value =
        parse_json_object(text, |v| v.get("facts").is_some() || v.get("refusal").is_some())?;
    serde_json::from_value(value)
        .map_err(|_| err(format!("unparseable response: {}", crate::client::truncate(text, 200))))
}

/// The tolerances, without a shape of their own.
///
/// Two callers now: extraction wants `facts` or `refusal`, cleanup wants `clusters`. The tolerances
/// belong to the models rather than to either task, so they live here once and each caller says
/// what shape it asked for.
///
/// `shaped` is not optional and must not become so. Every field on a response struct carries
/// `#[serde(default)]`, which is right for a model that dropped one and wrong here unguarded:
/// `{"error":"quota exceeded"}` deserialises straight into an empty result and gets recorded as a
/// successful call holding nothing. Answering "nothing is known" about content that is present is
/// this system's named worst failure, so an object is only accepted once it names a field the
/// caller asked for.
pub fn parse_json_object(text: &str, shaped: impl Fn(&Value) -> bool) -> Result<Value> {
    let candidate = strip_fence(text.trim());
    let unparseable =
        || err(format!("unparseable response: {}", crate::client::truncate(text, 200)));

    let value: Value = serde_json::from_str(candidate).map_err(|_| unparseable())?;

    // Some models answer JSON mode with a one-element array around the object they were asked for.
    // Measured on 21 August 2026 against a prompt that deliberately asked for prose and an object:
    // with `response_format` set, `qwen3.7-flash` and `glm-4.7-flash` both returned an array, while
    // `deepseek-v4-flash` and `gpt-5.6-luna` returned the object. So honouring JSON mode and
    // honouring the shape asked for are two different claims, and only the first is common.
    // Refusing the array would throw away a good answer over its wrapper.
    let value = match &value {
        Value::Array(items) if items.len() == 1 && items[0].is_object() => items[0].clone(),
        _ => value,
    };

    if !(value.is_object() && shaped(&value)) {
        return Err(unparseable());
    }
    Ok(value)
}

/// One call, one response, parsed to whatever shape the caller asked for.
///
/// Everything `extract_chunk` does about transports, keys, timeouts and the two body shapes, with
/// the extraction-specific parse lifted out. `extract_chunk` is now this plus one deserialise.
pub async fn call_json(
    http: &reqwest::Client,
    p: &Provider,
    system: &str,
    user: &str,
    timeout_secs: u64,
    shaped: impl Fn(&Value) -> bool,
) -> Result<(Value, Usage)> {
    let (text, usage) = call_text(http, p, system, user, timeout_secs).await?;
    Ok((parse_json_object(&text, shaped)?, usage))
}

/// Strips one leading ```json or ``` and one trailing ``` fence, if both are present.
fn strip_fence(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .map(str::trim_start)
        .unwrap_or(s);
    s.strip_suffix("```").map(str::trim_end).unwrap_or(s)
}

/// `lumberroom ingest keys set <provider>`, reading the key from stdin and never from an argument.
///
/// There is no `--api-key` flag and must not be one. Every argument of a running process is world
/// readable through `ps`, and an interactive shell writes the command line into its history file,
/// so a key passed that way lands in two places the owner did not choose and stays there.
///
/// `read_key` is injected so the test suite can drive this without a terminal. In the CLI it reads
/// one line from stdin.
pub fn keys_set(
    name: &str,
    path: std::path::PathBuf,
    read_key: impl FnOnce() -> std::io::Result<String>,
) -> Result<()> {
    // A misspelt provider would write a key under a name nothing ever reads, and the failure would
    // surface much later as an unexplained 401. Same table `resolve` uses, same refusal.
    defaults_for(name)?;
    refuse_loose_permissions(&path)?;

    let key = read_key().map_err(|e| err(format!("could not read the key: {e}")))?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(err(format!(
            "no key arrived on stdin. Pipe one in: cat key.txt | lumberroom ingest keys set {name}"
        )));
    }

    create_owner_only(&path)?;
    let mut file = crate::config::FileConfig::load(path.clone());

    // Patch the one leaf and hand everything else back untouched. `bin/lumberroom.mjs` and `wire-mac.sh`
    // write their own keys into this file, `resolve` reads sibling providers out of this same
    // object, and a provider entry can already carry a `base_url` or a `model` worth keeping.
    let mut ingest = file.value.get("ingest").cloned().unwrap_or_else(|| json!({}));
    if !ingest.is_object() {
        ingest = json!({});
    }
    let providers = ingest
        .as_object_mut()
        .expect("ingest is an object by the line above")
        .entry("providers")
        .or_insert_with(|| json!({}));
    if !providers.is_object() {
        *providers = json!({});
    }
    let entry = providers
        .as_object_mut()
        .expect("providers is an object by the line above")
        .entry(name)
        .or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry
        .as_object_mut()
        .expect("the provider entry is an object by the line above")
        .insert("key".to_string(), json!(key));

    let mut patch = serde_json::Map::new();
    patch.insert("ingest".to_string(), ingest);
    file.save(patch).map_err(|e| err(format!("could not write {}: {e}", path.display())))?;

    // The provider and the path, and nothing else. Not the key, not a prefix of it, not its
    // length: a truncated key in a terminal scrollback is still a piece of a key on a screen.
    crate::out(&format!("stored the {name} key in {}", path.display()));
    Ok(())
}

/// Create the file empty and owner-only before a key goes into it.
///
/// `FileConfig::save` writes and then chmods, so on a first run there is a moment where a fresh
/// config sits on disk at whatever the umask allowed. Creating it at 0600 first closes that
/// window. On a file that already exists this does nothing, and the permission check has run.
#[cfg(unix)]
fn create_owner_only(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| err(format!("could not create {}: {e}", parent.display())))?;
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(err(format!("could not create {}: {e}", path.display()))),
    }
}

#[cfg(not(unix))]
fn create_owner_only(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| err(format!("could not create {}: {e}", parent.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn missing_file() -> FileConfig {
        FileConfig {
            path: PathBuf::from("/nonexistent-lumberroom-provider-test/config.json"),
            value: json!({}),
        }
    }

    fn write_config(contents: &Value, mode: u32) -> FileConfig {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("lumberroom-provider-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, serde_json::to_vec(contents).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        FileConfig { path, value: contents.clone() }
    }

    // --- parse_response ---

    #[test]
    fn parses_a_bare_object() {
        let out = parse_response(
            r#"{"facts":[{"content":"c","namespace":"user:me","source_span_id":"s1"}]}"#,
        )
        .unwrap();
        assert_eq!(out.facts.len(), 1);
        assert_eq!(out.facts[0].content, "c");
    }

    #[test]
    fn parses_a_json_fenced_object() {
        let text = "```json\n{\"facts\":[]}\n```";
        let out = parse_response(text).unwrap();
        assert!(out.facts.is_empty());
    }

    #[test]
    fn parses_a_bare_fenced_object() {
        let text = "```\n{\"facts\":[]}\n```";
        let out = parse_response(text).unwrap();
        assert!(out.facts.is_empty());
    }

    #[test]
    fn a_bare_no_facts_token_is_a_success() {
        let out = parse_response("<no-facts/>").unwrap();
        assert!(out.facts.is_empty());
        assert_eq!(out.refusal.as_deref(), Some("<no-facts/>"));

        let fenced = parse_response("```\n<no-facts/>\n```").unwrap();
        assert_eq!(fenced.refusal.as_deref(), Some("<no-facts/>"));
    }

    #[test]
    fn garbage_is_an_error_naming_what_came_back() {
        let e = parse_response("well, here you go: not json").unwrap_err();
        assert!(e.message.contains("not json"), "{}", e.message);
    }

    #[test]
    fn an_object_with_neither_facts_nor_refusal_is_an_error_not_a_silent_empty_chunk() {
        let e = parse_response(r#"{"error":"quota exceeded"}"#).unwrap_err();
        assert!(e.message.contains("quota exceeded"), "{}", e.message);

        // The prompt asks for "facts"; a typo'd key must not silently deserialise into an empty,
        // successful chunk on the strength of every ChunkOutput field being #[serde(default)].
        let e = parse_response(r#"{"fact":[{"content":"c"}]}"#).unwrap_err();
        assert!(e.message.contains("fact"), "{}", e.message);
    }

    // --- resolve ---

    #[test]
    fn defaults_cover_openai_and_zai() {
        let f = missing_file();
        let p = resolve("openai", &f, None, None).unwrap();
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.model, "gpt-4o-mini");
        assert!(p.key.is_none());

        let p = resolve("zai", &f, None, None).unwrap();
        assert_eq!(p.base_url, "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(p.model, "glm-5.3");
        assert_eq!(p.extra_body, json!({ "thinking": { "type": "disabled" } }));
    }

    #[test]
    fn openrouter_and_custom_demand_what_has_no_default() {
        let f = missing_file();
        assert!(resolve("openrouter", &f, None, None).is_err());
        assert!(resolve("openrouter", &f, Some("m"), None).is_ok());

        assert!(resolve("custom", &f, Some("m"), None).is_err());
        assert!(resolve("custom", &f, Some("m"), Some("http://localhost:1234/v1")).is_ok());
    }

    #[test]
    fn unknown_provider_names_are_refused() {
        let f = missing_file();
        let e = resolve("bedrock", &f, None, None).unwrap_err();
        assert!(e.message.contains("bedrock"));
    }

    #[test]
    fn a_config_overlay_wins_over_defaults_and_merges_the_body() {
        let cfg = json!({
            "ingest": { "providers": { "zai": {
                "key": "cfg-key", "base_url": "http://mock/v4", "model": "glm-x",
                "body": { "extra": "1" }
            } } }
        });
        let f = write_config(&cfg, 0o600);
        let p = resolve("zai", &f, None, None).unwrap();
        assert_eq!(p.base_url, "http://mock/v4");
        assert_eq!(p.model, "glm-x");
        assert_eq!(p.key.as_deref(), Some("cfg-key"));
        // The default thinking-disabled flag survives an overlay that adds a key rather than
        // replacing it.
        assert_eq!(p.extra_body["thinking"]["type"], json!("disabled"));
        assert_eq!(p.extra_body["extra"], json!("1"));
    }

    #[test]
    fn flags_win_over_a_config_overlay() {
        let cfg = json!({ "ingest": { "providers": { "custom": {
            "base_url": "http://cfg/v1", "model": "cfg-model"
        } } } });
        let f = write_config(&cfg, 0o600);
        let p = resolve("custom", &f, Some("flag-model"), Some("http://flag/v1")).unwrap();
        assert_eq!(p.base_url, "http://flag/v1");
        assert_eq!(p.model, "flag-model");
    }

    #[test]
    fn an_env_key_overrides_the_config_file() {
        let cfg = json!({ "ingest": { "providers": { "custom": { "key": "cfg-key" } } } });
        let f = write_config(&cfg, 0o600);
        std::env::set_var("LUMBERROOM_INGEST_KEY_CUSTOM", "env-key");
        let p = resolve("custom", &f, Some("m"), Some("http://x/v1")).unwrap();
        std::env::remove_var("LUMBERROOM_INGEST_KEY_CUSTOM");
        assert_eq!(p.key.as_deref(), Some("env-key"));
    }

    #[test]
    fn zai_falls_back_to_the_bare_zai_api_key_env_var() {
        let f = missing_file();
        std::env::set_var("ZAI_API_KEY", "already-set-key");
        let p = resolve("zai", &f, None, None).unwrap();
        std::env::remove_var("ZAI_API_KEY");
        assert_eq!(p.key.as_deref(), Some("already-set-key"));
    }

    #[test]
    fn a_group_readable_config_file_is_refused() {
        let f = write_config(&json!({}), 0o644);
        let e = resolve("openai", &f, None, None).unwrap_err();
        assert!(e.message.contains("chmod 600"), "{}", e.message);
    }

    // --- the Anthropic content accessor ---

    fn anthropic_body(content: Value) -> Value {
        json!({ "content": content, "usage": { "input_tokens": 10, "output_tokens": 4 } })
    }

    #[test]
    fn the_first_text_block_wins_past_a_leading_thinking_block() {
        // Exactly the shape that breaks `content[0].text`: extended thinking puts its own block
        // first, and the answer is the second element of the array.
        let body = anthropic_body(json!([
            { "type": "thinking", "thinking": "weighing the spans" },
            { "type": "text", "text": "{\"facts\":[]}" },
        ]));
        assert_eq!(anthropic_text(&body, "anthropic").unwrap(), "{\"facts\":[]}");

        let plain = anthropic_body(json!([{ "type": "text", "text": "hello" }]));
        assert_eq!(anthropic_text(&plain, "anthropic").unwrap(), "hello");
    }

    #[test]
    fn two_text_blocks_are_two_answers_and_neither_is_kept() {
        let body = anthropic_body(json!([
            { "type": "thinking", "thinking": "..." },
            { "type": "text", "text": "{\"facts\":[]}" },
            { "type": "text", "text": "{\"facts\":[{\"content\":\"c\"}]}" },
        ]));
        let e = anthropic_text(&body, "anthropic").unwrap_err();
        assert!(e.message.contains("2 text blocks"), "{}", e.message);
        // Nothing from either block leaks into the message, so a failed chunk cannot half-report
        // a fact the owner never approved.
        assert!(!e.message.contains("facts"), "{}", e.message);
    }

    #[test]
    fn a_response_with_no_text_block_is_a_failed_chunk() {
        let thinking_only = anthropic_body(json!([{ "type": "thinking", "thinking": "..." }]));
        let e = anthropic_text(&thinking_only, "anthropic").unwrap_err();
        assert!(e.message.contains("no text block"), "{}", e.message);

        // A stop-before-any-output response carries an empty array, and lands in the same refusal.
        let empty = anthropic_body(json!([]));
        assert!(anthropic_text(&empty, "anthropic").is_err());

        // So does a body with no `content` at all, which is what an error envelope looks like.
        assert!(anthropic_text(&json!({ "error": "overloaded" }), "anthropic").is_err());

        // And a text block whose `text` is not a string.
        let malformed = anthropic_body(json!([{ "type": "text", "text": { "a": 1 } }]));
        assert!(anthropic_text(&malformed, "anthropic").is_err());
    }

    #[test]
    fn anthropic_has_defaults_and_the_messages_shape() {
        let f = missing_file();
        let p = resolve("anthropic", &f, None, None).unwrap();
        assert_eq!(p.shape, Shape::AnthropicMessages);
        // No `/v1`: the path is `{base_url}/v1/messages`.
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert!(!p.model.is_empty());

        // custom stays on the OpenAI path even when pointed at Anthropic's host.
        let c = resolve("custom", &f, Some("m"), Some("https://api.anthropic.com")).unwrap();
        assert_eq!(c.shape, Shape::ChatCompletions);
    }

    // --- keys set ---

    fn temp_config_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("lumberroom-keys-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    fn read_back(path: &PathBuf) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn keys_set_writes_the_key_and_keeps_every_other_setting() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "url": "https://lumberroom.example.com",
                "oauth": { "access_token": "keep-me" },
                "ingest": {
                    "watermark_note": "keep-me-too",
                    "providers": {
                        "zai": { "key": "zai-key", "model": "glm-5.3" },
                        "anthropic": { "model": "claude-opus-5" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        keys_set("anthropic", path.clone(), || Ok("  sk-ant-secret\n".to_string())).unwrap();

        let back = read_back(&path);
        assert_eq!(back["ingest"]["providers"]["anthropic"]["key"], json!("sk-ant-secret"));
        // The provider's own other fields survive.
        assert_eq!(back["ingest"]["providers"]["anthropic"]["model"], json!("claude-opus-5"));
        // So does the sibling provider, the rest of `ingest`, and the other client's top-level keys.
        assert_eq!(back["ingest"]["providers"]["zai"]["key"], json!("zai-key"));
        assert_eq!(back["ingest"]["watermark_note"], json!("keep-me-too"));
        assert_eq!(back["oauth"]["access_token"], json!("keep-me"));
        assert_eq!(back["url"], json!("https://lumberroom.example.com"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn keys_set_creates_a_missing_config_owner_only() {
        let path = temp_config_path();
        assert!(!path.exists());

        keys_set("openai", path.clone(), || Ok("sk-openai\n".to_string())).unwrap();

        assert_eq!(read_back(&path)["ingest"]["providers"]["openai"]["key"], json!("sk-openai"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    #[cfg(unix)]
    fn keys_set_refuses_a_group_readable_config_and_writes_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_config_path();
        std::fs::write(&path, br#"{"ingest":{"providers":{}}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let e =
            keys_set("anthropic", path.clone(), || Ok("sk-ant-secret".to_string())).unwrap_err();
        assert!(e.message.contains("chmod 600"), "{}", e.message);
        // The refusal happens before the key is read, so nothing reached the disk.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("sk-ant-secret"), "{raw}");

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn keys_set_refuses_an_empty_key_and_an_unknown_provider() {
        let path = temp_config_path();

        let e = keys_set("anthropic", path.clone(), || Ok("   \n".to_string())).unwrap_err();
        assert!(e.message.contains("no key"), "{}", e.message);

        let e = keys_set("bedrock", path.clone(), || Ok("k".to_string())).unwrap_err();
        assert!(e.message.contains("bedrock"), "{}", e.message);

        // Neither refusal left a config file behind.
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}

#[cfg(test)]
mod json_mode_shapes {
    use super::parse_response;

    /// Measured against four models on 21 August 2026. Asked for prose and an object with
    /// `response_format` set, two returned the object and two wrapped it in a one-element array.
    /// The wrapper is not a malformed answer, and refusing it would lose a whole chunk.
    #[test]
    fn a_single_element_array_around_the_object_is_unwrapped() {
        let out = parse_response(r#"[{"facts": [], "refusal": "<no-facts/>"}]"#).expect("unwraps");
        assert_eq!(out.refusal.as_deref(), Some("<no-facts/>"));
        assert!(out.facts.is_empty());
    }

    #[test]
    fn a_fenced_array_is_unwrapped_too() {
        let out = parse_response("```json\n[{\"facts\": [], \"refusal\": \"<no-facts/>\"}]\n```")
            .expect("fence then array");
        assert_eq!(out.refusal.as_deref(), Some("<no-facts/>"));
    }

    #[test]
    fn an_array_of_two_objects_is_refused_rather_than_guessed() {
        // Two answers, and picking one would be a guess about which half to keep. That is the same
        // rule the Anthropic accessor follows for two text blocks.
        assert!(parse_response(r#"[{"facts": []}, {"facts": []}]"#).is_err());
    }

    #[test]
    fn an_array_of_something_else_is_still_unparseable() {
        assert!(parse_response(r#"["facts"]"#).is_err());
        assert!(parse_response("[]").is_err());
    }

    #[test]
    fn a_bare_object_still_parses_exactly_as_before() {
        let out =
            parse_response(r#"{"facts": [], "refusal": "<no-facts/>"}"#).expect("bare object");
        assert_eq!(out.refusal.as_deref(), Some("<no-facts/>"));
    }
}
