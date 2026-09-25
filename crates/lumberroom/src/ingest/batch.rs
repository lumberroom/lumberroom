//! `lumberroom ingest extract --batch`, the Mode C driver.
//!
//! Spec §10.11. Mode B posts one request per chunk and waits. Mode C posts every chunk as one job,
//! walks away, and collects the results on a later invocation. Both write the same
//! `out/chunk-NN.json`, so `submit` cannot tell which mode produced a chunk and needs no change.
//!
//! **The spans sit in a third party's object storage for 30 days.** OpenRouter keeps batch inputs
//! and results as JSONL artifacts in Google Cloud Storage and deletes them 30 days after the batch
//! is created. A synchronous call leaves the spans in the provider's request path and whatever it
//! retains for abuse monitoring; a batch leaves them at rest somewhere the owner cannot reach. The
//! confirm prompt says so before anything goes out, and `HELP` says so before the owner types the
//! flag.
//!
//! **Turnaround is a day.** `24h` is the only completion window OpenRouter supports, so one round
//! of tuning the prompt costs a day. Calibrate with Mode A or Mode B on a handful of chunks and
//! switch to Mode C once the prompt has stopped changing.
//!
//! Verified against OpenRouter's batch quickstart on 21 August 2026: the endpoints, the key-order
//! requirement, the `202 Accepted` creation envelope, the status list, and the inline `results`
//! array. Everything here is written to that document and to §10.11's live probes. No batch has
//! been submitted from this code.

use std::io::IsTerminal;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::{err, err_code, Result};
use crate::ingest::provider::{parse_response, resolve, Provider, Shape};
use crate::ingest::{write_json, ChunkOutput, FailedChunk, RunPaths, RunState, Usage};

pub const HELP: &str = "\
lumberroom ingest extract --run <id> --batch [flags]

Mode C. Post every chunk of a run as one batch job and collect the results
later. Roughly half the per-token price on the providers that publish a
discount, in exchange for up to a day of turnaround.

  --batch          submit the batch, or advance one this run already has:
                   poll it while it runs, split it once it is complete
  --batch-status   print the job's status and its counts. Creates nothing
  --batch-fetch    split a completed batch into out/chunk-NN.json
  --dry-run        print the estimate and the first request. Send nothing
  --retry-failed   batch only the chunks state.json marked failed. On a run
                   whose batch has finished, this submits a second batch and
                   replaces the first one's record

The batch id lands in state.json, so a lost out/ directory costs a re-fetch
instead of a re-send. Running --batch again on a submitted job polls it and
never creates a second one, so a scheduler can call this on a timer.

What the owner is agreeing to
  The spans leave this machine and sit in the provider's object storage for
  30 days. OpenRouter writes batch inputs and results to Google Cloud Storage
  and deletes them 30 days after creation. That is a different exposure from a
  synchronous call, where the spans live in the request path.

  24h is the only completion window on offer. Tune the prompt with Mode A or
  Mode B first, where a bad instruction costs seconds.

  A started batch may not be stoppable. OpenRouter's status list carries
  cancelling and cancelled and its documentation names no cancellation
  endpoint, so a submitted batch costs whatever it costs.

Every chunk gets a line. A result that will not parse, an error item and a
custom id that never came back all land in state.json as failed chunks, which
`extract --retry-failed` picks up and `submit` counts in chunks_failed.
";

/// OpenRouter is the one provider probed end to end (§10.11), so it is the one with a built-in
/// endpoint. Anyone else declares theirs under `ingest.providers.<name>.batch.endpoint` and gets
/// this same inline-requests shape, which is the part to check before declaring one: z.ai's
/// general API answers OpenAI's `/files` and `/batches` list envelopes, and that flow uploads a
/// JSONL file first rather than sending the requests in the body.
const OPENROUTER_BATCH_ENDPOINT: &str = "https://openrouter.ai/api/beta/batches";

/// The value of the batch body's `endpoint` field, which names the per-request API rather than a
/// URL. Chat completions is the only shape this driver builds.
const BATCH_TARGET_PATH: &str = "/v1/chat/completions";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAction {
    /// Submit if this run has no batch, poll if it has a running one, fetch if it has a finished
    /// one. One flag for the whole lifecycle, because the owner's question is always the same:
    /// move this run forward.
    Advance,
    /// Print status and counts. Creates nothing, writes no chunk files.
    Status,
    /// Split a finished batch into `out/`. Refuses while the batch is still running.
    Fetch,
}

#[derive(Debug, Clone)]
pub struct BatchArgs {
    pub run_id: uuid::Uuid,
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub action: BatchAction,
    /// Print the estimate and the first request item, call nothing.
    pub dry_run: bool,
    /// Only the chunks `state.json` marked failed, and only those.
    pub retry_failed: bool,
    pub yes: bool,
    pub timeout_secs: u64,
}

/// What `state.json` carries about the job, under a `batch` key beside `done` and `failed`.
///
/// §10.7 checkpoints on `out/chunk-NN.json` existing, which is per chunk and local to this
/// machine. This is one string the provider holds the whole run's state behind. The two coexist:
/// the id resumes the fetch and the chunk files stay the record of what came back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchState {
    pub id: String,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the results were split into `out/`. Set once, and it is what keeps a second poll of a
    /// finished batch from counting its tokens into `usage` twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collected_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Which chunk indices went into the batch. A custom id that never comes back is a failed
    /// chunk, and finding those needs the list of what was sent.
    #[serde(default)]
    pub chunks: Vec<usize>,
}

/// The eight values OpenRouter's documentation names, plus a bucket for a ninth nobody has seen.
///
/// An unknown status is not terminal on purpose. Treating a name this code does not recognise as
/// finished would split a batch that is still filling and write half a run's chunks as failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Validating,
    InProgress,
    Finalizing,
    Completed,
    Failed,
    Expired,
    Cancelling,
    Cancelled,
    Unknown(String),
}

impl Status {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "validating" => Self::Validating,
            "in_progress" => Self::InProgress,
            "finalizing" => Self::Finalizing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            "cancelling" => Self::Cancelling,
            "cancelled" => Self::Cancelled,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Validating => "validating",
            Self::InProgress => "in_progress",
            Self::Finalizing => "finalizing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Unknown(s) => s,
        }
    }

    /// Terminal means the provider will not change this status again. It does not mean results
    /// arrived: `failed`, `expired` and `cancelled` are terminal and carry nothing to split.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Expired | Self::Cancelled)
    }

    /// Whether there are results worth splitting into `out/`.
    pub fn has_results(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// What a human should do next, printed under every status line.
    pub fn guidance(&self) -> &'static str {
        match self {
            Self::Validating | Self::InProgress | Self::Finalizing => {
                "still running. Run this again later; a batch can take up to 24 hours"
            }
            Self::Completed => "complete. Run `ingest extract --batch` to split the results",
            Self::Failed => {
                "the provider failed the batch. Every chunk it did not return is a failed chunk"
            }
            Self::Expired => {
                "the batch missed its window. spans/ survives, so `extract --retry-failed` resends"
            }
            Self::Cancelling => "cancelling. Nothing to collect until it settles",
            Self::Cancelled => "cancelled. Every chunk it did not return is a failed chunk",
            Self::Unknown(_) => "an unrecognised status. Treating it as still running",
        }
    }
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

pub async fn run(args: &BatchArgs) -> Result<RunState> {
    let paths = RunPaths::new(args.run_id)?;
    paths.create()?;
    let worklist = paths.read_worklist()?;
    let mut state = paths.read_state();
    let existing = read_batch(&paths);

    let env = crate::config::ProcessEnv;
    let cfg_path = crate::config::config_path(&env);
    let file = crate::config::FileConfig::load(cfg_path);
    // The batch record is the authority on which provider this run belongs to, and `--provider`
    // is only consulted when the run has no batch yet. Resolving the flag instead would send the
    // default provider's key at the recorded endpoint the moment the owner polls without retyping
    // it, and report the resulting 401 under a provider that was never involved. A resubmission
    // stays on the same provider for the same reason: the chunks were planned and priced there.
    let provider_name =
        existing.as_ref().map(|b| b.provider.clone()).unwrap_or_else(|| args.provider.clone());
    let provider = resolve(&provider_name, &file, args.model.as_deref(), args.base_url.as_deref())?;

    let http = reqwest::Client::new();
    let total = worklist.chunks.len();

    // A run that already carries a batch id polls that batch and never creates a second one
    // (§10.11). `--batch-status` lands here too, and stops after printing.
    if let Some(mut batch) = existing {
        if args.dry_run {
            crate::out(&format!(
                "run {} already holds batch {} at {}, last seen {}",
                args.run_id, batch.id, batch.endpoint, batch.status
            ));
            return Ok(state);
        }

        let body = poll(&http, &provider, &batch, args.timeout_secs).await?;
        let status = Status::parse(body["status"].as_str().unwrap_or(""));
        print_status(&body);
        batch.status = status.as_str().to_string();

        if args.action == BatchAction::Status {
            write_run_state(&paths, &state, Some(&batch))?;
            return Ok(state);
        }

        if !status.is_terminal() {
            write_run_state(&paths, &state, Some(&batch))?;
            if args.action == BatchAction::Fetch {
                return Err(err(format!(
                    "batch {} is {}, so there is nothing to split yet",
                    batch.id,
                    status.as_str()
                )));
            }
            return Ok(state);
        }

        // `collected_at` is what stops a second `--batch` from adding the batch's token counts to
        // `usage` again. Polling a finished batch is free and the owner will do it, so the split
        // has to happen once and the re-runs have to say so.
        if batch.collected_at.is_none() {
            collect(&paths, &mut state, &batch, &status, &body);
            batch.collected_at = Some(chrono::Utc::now());
            write_run_state(&paths, &state, Some(&batch))?;
            print_usage(&state.usage);
        } else {
            write_run_state(&paths, &state, Some(&batch))?;
            crate::out(&format!("batch {} was already split into out/", batch.id));
        }

        // A finished batch is spent. `--retry-failed` on top of one means the owner wants a second
        // batch for the chunks that failed, which is the documented way back from an expired one,
        // and it costs the old id: `state.json` holds one batch record. Anything else stops here,
        // so a repeated `--batch` can never submit the corpus twice.
        if !(args.action == BatchAction::Advance && args.retry_failed) {
            return Ok(state);
        }
        crate::out(&format!(
            "batch {} is spent. Submitting a new one for the failed chunks.",
            batch.id
        ));
    } else if args.action != BatchAction::Advance {
        return Err(err(format!("run {} has no batch. Submit one with `--batch`.", args.run_id)));
    }

    // No batch on this run, so this is a submission.
    if provider.shape != Shape::ChatCompletions {
        return Err(err(format!(
            "provider {} does not speak the chat-completions batch shape. Mode C builds one body \
             shape and Anthropic's batch API is a different flow.",
            provider.name
        )));
    }
    let endpoint = batch_endpoint(&provider, &file)?;

    let all: Vec<usize> = worklist.chunks.iter().map(|c| c.index).collect();
    let todo = pending_chunks(&all, &state, &paths, args.retry_failed);
    if todo.is_empty() {
        crate::out("nothing to send: every chunk already has an out/ file");
        return Ok(state);
    }

    let mut items = Vec::with_capacity(todo.len());
    let mut total_chars = 0usize;
    for idx in &todo {
        let user = std::fs::read_to_string(paths.chunk_in(*idx))
            .map_err(|e| err(format!("could not read spans/chunk-{idx:02}.json: {e}")))?;
        total_chars += user.chars().count();
        // The same prompt Mode A and Mode B send. Two modes that extract differently are two
        // products, so this calls `prompt::provider_system` rather than restating it.
        let system = crate::ingest::prompt::provider_system(idx + 1, total);
        items.push(request_item(*idx, &system, &user, &provider));
    }

    let body = batch_body(BATCH_TARGET_PATH, &provider.model, items);

    print_estimate(&provider, &endpoint, todo.len(), total_chars, total_chars / 4);
    if args.dry_run {
        crate::out("--- first request item ---");
        crate::out(&serde_json::to_string_pretty(&body["requests"][0]).unwrap_or_default());
        return Ok(state);
    }

    confirm(&provider, &endpoint, args.yes)?;

    let created = create(&http, &provider, &endpoint, &body, args.timeout_secs).await?;
    let id = created["id"]
        .as_str()
        .ok_or_else(|| err(format!("{} accepted the batch and returned no id", provider.name)))?
        .to_string();
    let status = Status::parse(created["status"].as_str().unwrap_or("validating"));

    let batch = BatchState {
        id: id.clone(),
        provider: provider.name.clone(),
        endpoint: endpoint.clone(),
        model: provider.model.clone(),
        status: status.as_str().to_string(),
        created_at: chrono::Utc::now(),
        collected_at: None,
        chunks: todo,
    };
    state.provider = Some(provider_name.clone());
    state.model = Some(provider.model.clone());
    state.updated_at = chrono::Utc::now();
    write_run_state(&paths, &state, Some(&batch))?;

    crate::out(&format!("batch {id} submitted, status {}", status.as_str()));
    crate::out(&format!(
        "poll it with: lumberroom ingest extract --run {} --batch-status",
        args.run_id
    ));
    crate::out("no --provider needed on that line: state.json remembers where this batch went");
    Ok(state)
}

/// Which chunks go into the batch. Copies `extract.rs`: the failed list under `--retry-failed`,
/// otherwise every chunk with no `out/` file yet.
fn pending_chunks(
    all: &[usize],
    state: &RunState,
    paths: &RunPaths,
    retry_failed: bool,
) -> Vec<usize> {
    if retry_failed {
        state.failed.iter().map(|f| f.index).collect()
    } else {
        all.iter().copied().filter(|i| !paths.chunk_out(*i).exists()).collect()
    }
}

// ---------------------------------------------------------------------------
// The request body, and the key-order trap
// ---------------------------------------------------------------------------

/// `chunk-07`, `chunk-42`, `chunk-100`. The same two-digit minimum the run directory uses, so a
/// custom id in a provider's dashboard reads as the file it will become.
pub fn custom_id(index: usize) -> String {
    format!("chunk-{index:02}")
}

/// The inverse. Returns `None` for anything this driver did not send, which the fetch path treats
/// as a result it cannot route rather than a chunk to overwrite.
pub fn chunk_index(custom_id: &str) -> Option<usize> {
    custom_id.strip_prefix("chunk-").and_then(|n| n.parse::<usize>().ok())
}

/// One `{"custom_id": ..., "body": ...}` item. The body is the §10.3 request the synchronous path
/// would have sent for this chunk, minus `model`: the batch names the model once at the top level
/// and OpenRouter's own example omits it per request.
///
/// Field for field with `provider.rs::chat_completions_request`, and the two drifting apart is the
/// trap. This sent `response_format` unconditionally while `json_mode` defaulted off for
/// OpenRouter, the one provider with a built-in batch endpoint, because qwen3.7-flash answers HTTP
/// 400 to that field. A batch built that way fails every request a day after it goes out. Leaving
/// `reasoning` unsent is the other half: reasoning bills as output on every chunk of the corpus,
/// measured at 205 wasted tokens out of 215 on one call, which is more than the batch discount
/// buys back.
pub fn request_item(index: usize, system: &str, user: &str, p: &Provider) -> Value {
    let mut body = json!({
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
        "temperature": 0,
    });
    if p.json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    }
    if !p.reasoning {
        body["reasoning"] = json!({ "enabled": false });
    }
    // Last, so a config-file `body` still wins over both of the decisions above.
    merge_top_level(&mut body, &p.extra_body);
    json!({ "custom_id": custom_id(index), "body": body })
}

/// The batch body, and the field order is load-bearing.
///
/// OpenRouter parses this body as a stream, so `endpoint` and `model` have to be serialised before
/// `requests` or the parser reaches the end of the body without having found the array and answers
/// 400 `Batch body ended before a requests array was found`. §10.11 caught that 400 live. A
/// reordering here reads as cosmetic in review and turns every batch into a 400, so
/// `key_order_survives_serialisation` pins it. That test is also the tripwire on serde_json's
/// `preserve_order` feature: drop it from `Cargo.toml` and this map starts sorting its keys.
pub fn batch_body(endpoint_path: &str, model: &str, requests: Vec<Value>) -> Value {
    json!({
        "endpoint": endpoint_path,
        "model": model,
        "requests": requests,
    })
}

/// Copy every key of `overlay` onto the top level of `base`. `provider.rs` keeps its own copy
/// private; this one exists so a config-file `body` reaches a batched request the same way it
/// reaches a synchronous one. The `zai` entry's `thinking: disabled` is the case that matters.
fn merge_top_level(base: &mut Value, overlay: &Value) {
    let (Some(base_obj), Some(overlay_obj)) = (base.as_object_mut(), overlay.as_object()) else {
        return;
    };
    for (k, v) in overlay_obj {
        base_obj.insert(k.clone(), v.clone());
    }
}

/// Built in for OpenRouter, declared per provider otherwise, and an error rather than a fallback.
///
/// §10.11: `--batch` against a provider with no batch entry exits 1 naming the provider. Falling
/// back to the synchronous path would make an interactive run wait a day because a default moved,
/// or spend a day's turnaround the owner did not ask for.
fn batch_endpoint(provider: &Provider, file: &crate::config::FileConfig) -> Result<String> {
    let configured = file
        .value
        .get("ingest")
        .and_then(|v| v.get("providers"))
        .and_then(|v| v.get(&provider.name))
        .and_then(|v| v.get("batch"))
        .and_then(|v| v.get("endpoint"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if let Some(url) = configured {
        return Ok(url);
    }
    if provider.name == "openrouter" {
        return Ok(OPENROUTER_BATCH_ENDPOINT.to_string());
    }
    Err(err(format!(
        "provider {} declares no batch endpoint, so --batch has nowhere to post. Add \
         ingest.providers.{}.batch.endpoint to the config file, or drop --batch to run the \
         synchronous path.",
        provider.name, provider.name
    )))
}

// ---------------------------------------------------------------------------
// The network, kept to two calls
// ---------------------------------------------------------------------------

async fn create(
    http: &reqwest::Client,
    p: &Provider,
    endpoint: &str,
    body: &Value,
    timeout_secs: u64,
) -> Result<Value> {
    let mut req =
        http.post(endpoint).timeout(std::time::Duration::from_secs(timeout_secs)).json(body);
    if let Some(key) = &p.key {
        req = req.bearer_auth(key);
    }
    if p.name == "openrouter" {
        req = req
            .header("HTTP-Referer", "https://github.com/lumberroom/lumberroom")
            .header("X-Title", "lumberroom");
    }
    send(req, p, timeout_secs).await
}

/// `GET {endpoint}/{id}`. The results arrive inline on this same response; there is no download
/// step and no file to fetch.
async fn poll(
    http: &reqwest::Client,
    p: &Provider,
    batch: &BatchState,
    timeout_secs: u64,
) -> Result<Value> {
    let url = format!("{}/{}", batch.endpoint.trim_end_matches('/'), batch.id);
    let mut req = http.get(&url).timeout(std::time::Duration::from_secs(timeout_secs));
    if let Some(key) = &p.key {
        req = req.bearer_auth(key);
    }
    send(req, p, timeout_secs).await
}

/// One request, and the key never enters an error message. Only the provider's name and the bare
/// status, matching `provider.rs`.
async fn send(req: reqwest::RequestBuilder, p: &Provider, timeout_secs: u64) -> Result<Value> {
    let res = req.send().await.map_err(|e| {
        if e.is_timeout() {
            err_code(format!("{} timed out after {timeout_secs}s", p.name), 3)
        } else {
            err(format!("{} could not be reached: {e}", p.name))
        }
    })?;
    let status = res.status();
    if !status.is_success() {
        return Err(err_code(
            format!("{} answered HTTP {}", p.name, status.as_u16()),
            status.as_u16() as i32,
        ));
    }
    res.json()
        .await
        .map_err(|e| err(format!("{} answered a body that did not parse as JSON: {e}", p.name)))
}

// ---------------------------------------------------------------------------
// Splitting the results
// ---------------------------------------------------------------------------

/// One routed result: the chunk it belongs to and either its output or why it failed.
#[derive(Debug)]
pub struct Routed {
    pub ok: Vec<(usize, ChunkOutput)>,
    pub failed: Vec<FailedChunk>,
    /// Results whose `custom_id` matches nothing this run sent. Reported and never written.
    pub unroutable: Vec<String>,
}

/// Split a completed batch's `results` array by `custom_id`.
///
/// Four ways a chunk fails and every one lands in `state.json`: an `error` item, a `status_code`
/// that is not 200, a body the §10.3 parser refuses, and a custom id that never came back at all.
/// A batch reports `{"total":100,"completed":98,"failed":2}` and the owner has to be able to name
/// those two, so nothing here drops a chunk on the floor.
pub fn route_results(results: &[Value], expected: &[usize]) -> Routed {
    let mut routed = Routed { ok: vec![], failed: vec![], unroutable: vec![] };
    let mut seen: Vec<usize> = Vec::with_capacity(expected.len());

    for item in results {
        let raw_id = item["custom_id"].as_str().unwrap_or("");
        let Some(idx) = chunk_index(raw_id) else {
            routed.unroutable.push(crate::client::truncate(raw_id, 60));
            continue;
        };
        if !expected.contains(&idx) {
            routed.unroutable.push(crate::client::truncate(raw_id, 60));
            continue;
        }
        seen.push(idx);

        if let Some(error) = item.get("error").filter(|e| !e.is_null()) {
            routed.failed.push(FailedChunk {
                index: idx,
                reason: "batch_error".to_string(),
                detail: Some(crate::client::truncate(&error.to_string(), 200)),
            });
            continue;
        }

        let Some(response) = item.get("response").filter(|r| !r.is_null()) else {
            routed.failed.push(FailedChunk {
                index: idx,
                reason: "batch_error".to_string(),
                detail: Some("the result item carried neither a response nor an error".to_string()),
            });
            continue;
        };

        let code = response["status_code"].as_i64().unwrap_or(0);
        if code != 200 {
            routed.failed.push(FailedChunk {
                index: idx,
                reason: format!("http_{code}"),
                detail: Some(crate::client::truncate(&response["body"].to_string(), 200)),
            });
            continue;
        }

        let text = response["body"]["choices"][0]["message"]["content"].as_str();
        let parsed = match text {
            Some(t) => parse_response(t),
            None => Err(err("the result carried no message content")),
        };
        match parsed {
            Ok(output) => routed.ok.push((idx, output)),
            Err(e) => routed.failed.push(FailedChunk {
                index: idx,
                reason: "unparseable".to_string(),
                detail: Some(crate::client::truncate(&e.message, 200)),
            }),
        }
    }

    // A chunk the batch never mentioned. This is the silent-gap case: without it a half-failed
    // batch reads as a short run rather than a run with two chunks missing.
    for idx in expected {
        if !seen.contains(idx) {
            routed.failed.push(FailedChunk {
                index: *idx,
                reason: "missing_from_batch".to_string(),
                detail: Some("the batch returned no result for this chunk".to_string()),
            });
        }
    }

    routed.ok.sort_by_key(|(i, _)| *i);
    routed.failed.sort_by_key(|f| f.index);
    routed
}

/// Write the results, update `state.json`, and print a line per chunk.
///
/// Returns nothing to fail on. A chunk whose file will not write becomes a failed chunk the same
/// way a chunk whose body will not parse does, because bailing on the first bad write throws away
/// the fetch of every chunk after it and the results expire.
fn collect(
    paths: &RunPaths,
    state: &mut RunState,
    batch: &BatchState,
    status: &Status,
    body: &Value,
) {
    let empty: Vec<Value> = vec![];
    let results = body["results"].as_array().unwrap_or(&empty);
    if !status.has_results() {
        crate::out(&format!(
            "batch {} ended {}. Every chunk it did not return is a failed chunk.",
            batch.id,
            status.as_str()
        ));
    }

    let routed = route_results(results, &batch.chunks);

    for (idx, output) in &routed.ok {
        match write_json(&paths.chunk_out(*idx), output) {
            Ok(()) => {
                state.done.retain(|d| d != idx);
                state.done.push(*idx);
                state.failed.retain(|f| f.index != *idx);
                state.usage.requests += 1;
                crate::out(&format!("chunk {idx:02}  {}", fact_summary(output)));
            }
            Err(e) => {
                state.failed.retain(|f| f.index != *idx);
                state.failed.push(FailedChunk {
                    index: *idx,
                    reason: "write_failed".to_string(),
                    detail: Some(crate::client::truncate(&e.message, 200)),
                });
                crate::out(&format!("chunk {idx:02}  failed: write_failed"));
            }
        }
    }
    for failure in &routed.failed {
        state.failed.retain(|f| f.index != failure.index);
        state.failed.push(failure.clone());
        crate::out(&format!("chunk {:02}  failed: {}", failure.index, failure.reason));
    }
    for id in &routed.unroutable {
        crate::out(&format!("ignored a result with an unknown custom id: {id}"));
    }

    // The batch's own token counts, which are what the provider bills. `usage.requests` counts the
    // chunks that came back, so the report totals the same unit in both modes.
    let usage = &body["usage"];
    state.usage.prompt_tokens += usage["prompt_tokens"].as_i64().unwrap_or(0);
    state.usage.completion_tokens += usage["completion_tokens"].as_i64().unwrap_or(0);
    state.updated_at = chrono::Utc::now();
    state.failed.sort_by_key(|f| f.index);
    state.done.sort_unstable();
}

fn fact_summary(output: &ChunkOutput) -> String {
    match output.facts.len() {
        0 => "<no-facts/>".to_string(),
        1 => "1 fact".to_string(),
        n => format!("{n} facts"),
    }
}

// ---------------------------------------------------------------------------
// state.json, and the key Mode B carries for this driver
// ---------------------------------------------------------------------------

/// `RunState.batch` holds the record as an untyped `Value`, so `mod.rs` carries the key without
/// carrying this driver's types. Mode B rewrites `state.json` after every chunk and preserves the
/// key on the way through, which is what stops a `--retry-failed` from erasing the id while the
/// results sit on a provider for another thirty days.
fn read_batch(paths: &RunPaths) -> Option<BatchState> {
    serde_json::from_value(paths.read_state().batch?).ok()
}

fn write_run_state(paths: &RunPaths, state: &RunState, batch: Option<&BatchState>) -> Result<()> {
    let mut state = state.clone();
    if let Some(batch) = batch {
        state.batch = Some(
            serde_json::to_value(batch)
                .map_err(|e| err(format!("could not serialise the batch record: {e}")))?,
        );
    }
    paths.write_state(&state)
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

fn print_status(body: &Value) {
    let status = Status::parse(body["status"].as_str().unwrap_or(""));
    let id = body["id"].as_str().unwrap_or("?");
    let counts = &body["request_counts"];
    crate::out(&format!(
        "batch {id}  {}  total {} · completed {} · failed {}",
        status.as_str(),
        counts["total"].as_i64().unwrap_or(0),
        counts["completed"].as_i64().unwrap_or(0),
        counts["failed"].as_i64().unwrap_or(0),
    ));
    crate::out(&format!("  {}", status.guidance()));
}

fn print_estimate(p: &Provider, endpoint: &str, chunks: usize, chars: usize, tokens: usize) {
    crate::out(&format!("chunks {chunks} · {chars} chars · ~{tokens} tokens (4 chars/token)"));
    crate::out(&format!("provider {} batching at {endpoint}", p.name));
    crate::out(&format!("model {}", p.model));
}

fn print_usage(usage: &Usage) {
    crate::out(&format!(
        "usage: {} requests, {} prompt tokens, {} completion tokens",
        usage.requests, usage.prompt_tokens, usage.completion_tokens
    ));
}

/// The consent block, and the 30-day line is the reason it differs from Mode B's.
fn confirm(p: &Provider, endpoint: &str, yes: bool) -> Result<()> {
    // Printed whatever `--yes` says. A cron log with no record of where a corpus went is not a log.
    crate::out(&format!(
        "the spans go to {} and sit in its storage until that batch is deleted",
        host_of(endpoint)
    ));
    if p.name == "openrouter" {
        crate::out(
            "OpenRouter writes batch inputs and results to Google Cloud Storage and deletes",
        );
        crate::out(
            "them 30 days after the batch is created. A synchronous call leaves no copy at rest.",
        );
        crate::out(
            "24h is the only completion window, and a submitted batch may not be cancellable.",
        );
    } else {
        // The 30 days is OpenRouter's published figure and nobody else's. Quoting it at a provider
        // whose retention nobody has read is worse than saying the number is unknown.
        crate::out(&format!(
            "{}'s retention for batch inputs is not recorded here. OpenRouter keeps them 30 days;",
            p.name
        ));
        crate::out(
            "read this provider's terms before sending a corpus. Turnaround may be a full day.",
        );
    }
    if p.key.is_none() {
        crate::out(&format!("no key is configured for {}, so this will be refused", p.name));
    }

    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(err("--batch needs --yes to submit with no terminal to confirm on"));
    }
    crate::prompt("submit the batch? [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| err(format!("could not read the answer: {e}")))?;
    let answer = answer.trim();
    if !answer.eq_ignore_ascii_case("y") && !answer.eq_ignore_ascii_case("yes") {
        return Err(err("not confirmed, sending nothing"));
    }
    Ok(())
}

/// The host out of a URL, without pulling in a URL parser for one line of consent text.
fn host_of(url: &str) -> String {
    url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolved provider without touching the filesystem. `resolve` is tested in `provider.rs`;
    /// what these tests ask is what this driver does with the answer.
    fn provider_like(name: &str, json_mode: bool, reasoning: bool, extra_body: Value) -> Provider {
        Provider {
            name: name.to_string(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            model: "test-model".to_string(),
            key: None,
            extra_body,
            shape: Shape::ChatCompletions,
            reasoning,
            json_mode,
        }
    }

    fn item_body(index: usize) -> Value {
        request_item(
            index,
            "system prompt",
            "user chunk",
            &provider_like("openai", true, false, json!({})),
        )
    }

    #[test]
    fn the_request_array_holds_one_item_per_chunk() {
        let items = vec![item_body(0), item_body(1)];
        let body = batch_body(BATCH_TARGET_PATH, "openai/gpt-4o", items);

        assert_eq!(body["endpoint"], "/v1/chat/completions");
        assert_eq!(body["model"], "openai/gpt-4o");
        let requests = body["requests"].as_array().expect("requests is an array");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["custom_id"], "chunk-00");
        assert_eq!(requests[1]["custom_id"], "chunk-01");
        // The per-request body is the synchronous request minus `model`, which the batch names once.
        assert_eq!(requests[0]["body"]["temperature"], 0);
        assert_eq!(requests[0]["body"]["response_format"]["type"], "json_object");
        assert_eq!(requests[0]["body"]["messages"][0]["role"], "system");
        assert_eq!(requests[0]["body"]["messages"][1]["content"], "user chunk");
        assert!(requests[0]["body"].get("model").is_none());
    }

    /// The 400 in §10.11. OpenRouter parses the body as a stream and gives up at the end of it if
    /// `requests` came first, so this pins the byte order rather than the key set.
    #[test]
    fn key_order_survives_serialisation() {
        let body = batch_body(BATCH_TARGET_PATH, "m", vec![item_body(0)]);
        let text = serde_json::to_string(&body).expect("the body serialises");

        let endpoint_at = text.find("\"endpoint\"").expect("endpoint is present");
        let model_at = text.find("\"model\"").expect("model is present");
        let requests_at = text.find("\"requests\"").expect("requests is present");
        assert!(endpoint_at < model_at, "endpoint must serialise before model:\n{text}");
        assert!(model_at < requests_at, "model must serialise before requests:\n{text}");
        assert!(text.starts_with("{\"endpoint\":"), "the body must open on endpoint:\n{text}");
    }

    #[test]
    fn the_config_body_reaches_a_batched_request() {
        let zai = provider_like("zai", true, false, json!({ "thinking": { "type": "disabled" } }));
        let item = request_item(0, "s", "u", &zai);
        assert_eq!(item["body"]["thinking"]["type"], "disabled");
        assert_eq!(item["body"]["temperature"], 0);
        assert_eq!(item["body"]["response_format"]["type"], "json_object");
    }

    /// The one that matters on the one provider Mode C can reach without a config entry.
    /// OpenRouter's default is `json_mode: false`, because qwen3.7-flash answers HTTP 400 to
    /// `response_format`, and a batch that sends it anyway fails every request a day later.
    #[test]
    fn a_batched_request_carries_response_format_on_exactly_the_providers_the_sync_path_does() {
        let openrouter = provider_like("openrouter", false, false, json!({}));
        let item = request_item(3, "s", "u", &openrouter);
        assert!(
            item["body"].get("response_format").is_none(),
            "json_mode is off, so response_format must not go out: {}",
            item["body"]
        );

        let openai = provider_like("openai", true, false, json!({}));
        let item = request_item(3, "s", "u", &openai);
        assert_eq!(item["body"]["response_format"]["type"], "json_object");
    }

    /// Reasoning bills as output on every chunk of the corpus, which is more than the batch
    /// discount buys back. Off unless the config asks for it, matching the synchronous path.
    #[test]
    fn reasoning_is_disabled_unless_the_provider_asks_for_it() {
        let quiet = provider_like("openrouter", false, false, json!({}));
        assert_eq!(request_item(0, "s", "u", &quiet)["body"]["reasoning"]["enabled"], json!(false));

        let thinking = provider_like("openrouter", false, true, json!({}));
        let item = request_item(0, "s", "u", &thinking);
        assert!(
            item["body"].get("reasoning").is_none(),
            "a provider that asked to reason must not be told not to: {}",
            item["body"]
        );
    }

    /// `body` in the config file is the owner's way past both decisions above without editing this
    /// file, so it merges last on this path exactly as it does on the synchronous one.
    #[test]
    fn the_config_body_overrides_the_fields_this_driver_sets() {
        let p = provider_like(
            "custom",
            false,
            false,
            json!({ "reasoning": { "enabled": true }, "response_format": { "type": "text" } }),
        );
        let item = request_item(0, "s", "u", &p);
        assert_eq!(item["body"]["reasoning"]["enabled"], json!(true));
        assert_eq!(item["body"]["response_format"]["type"], "text");
    }

    #[test]
    fn custom_ids_round_trip_past_nine() {
        for index in [0usize, 7, 9, 10, 42, 99, 100, 1234] {
            let id = custom_id(index);
            assert_eq!(chunk_index(&id), Some(index), "{id} did not round-trip");
        }
        assert_eq!(custom_id(7), "chunk-07");
        assert_eq!(custom_id(10), "chunk-10");
        assert_eq!(custom_id(100), "chunk-100");
        assert_eq!(chunk_index("chunk-10"), Some(10));
        assert_eq!(chunk_index("req-0001"), None);
        assert_eq!(chunk_index("chunk-"), None);
        assert_eq!(chunk_index("chunk-xx"), None);
    }

    fn ok_result(index: usize, content: &str) -> Value {
        json!({
            "id": "batch_req_1",
            "custom_id": custom_id(index),
            "response": {
                "status_code": 200,
                "request_id": "request_1",
                "body": { "choices": [ { "message": { "content": content } } ] }
            },
            "error": null
        })
    }

    #[test]
    fn a_malformed_result_is_a_failed_chunk_and_never_a_gap() {
        let results = vec![
            ok_result(0, "{\"facts\":[],\"refusal\":\"<no-facts/>\"}"),
            ok_result(1, "{\"error\":\"quota exceeded\"}"),
        ];
        let routed = route_results(&results, &[0, 1]);

        assert_eq!(routed.ok.len(), 1);
        assert_eq!(routed.ok[0].0, 0);
        assert_eq!(routed.failed.len(), 1);
        assert_eq!(routed.failed[0].index, 1);
        assert_eq!(routed.failed[0].reason, "unparseable");
        assert!(routed.unroutable.is_empty());
    }

    #[test]
    fn an_error_item_a_bad_status_and_an_absent_chunk_all_land_in_failed() {
        let results = vec![
            json!({ "custom_id": "chunk-00", "response": null, "error": { "message": "no such model" } }),
            json!({
                "custom_id": "chunk-01",
                "response": { "status_code": 429, "request_id": "r", "body": { "error": "slow down" } },
                "error": null
            }),
            ok_result(2, "{\"facts\":[]}"),
            json!({ "custom_id": "req-0001", "response": null, "error": null }),
        ];
        // Chunk 3 went out and never came back.
        let routed = route_results(&results, &[0, 1, 2, 3]);

        assert_eq!(routed.ok.len(), 1);
        assert_eq!(routed.ok[0].0, 2);

        let reasons: Vec<(usize, &str)> =
            routed.failed.iter().map(|f| (f.index, f.reason.as_str())).collect();
        assert_eq!(reasons, vec![(0, "batch_error"), (1, "http_429"), (3, "missing_from_batch")]);
        assert_eq!(routed.unroutable, vec!["req-0001".to_string()]);
    }

    #[test]
    fn every_chunk_of_a_batch_with_no_results_is_a_failed_chunk() {
        let routed = route_results(&[], &[0, 1, 2]);
        assert!(routed.ok.is_empty());
        assert_eq!(routed.failed.len(), 3);
        assert!(routed.failed.iter().all(|f| f.reason == "missing_from_batch"));
    }

    #[test]
    fn the_status_mapping_names_terminal_and_running_apart() {
        for name in ["completed", "failed", "expired", "cancelled"] {
            let s = Status::parse(name);
            assert_eq!(s.as_str(), name);
            assert!(s.is_terminal(), "{name} is terminal");
        }
        for name in ["validating", "in_progress", "finalizing", "cancelling"] {
            let s = Status::parse(name);
            assert_eq!(s.as_str(), name);
            assert!(!s.is_terminal(), "{name} is not terminal");
        }
        // Only `completed` carries results. The other three terminal states end a batch and hand
        // back nothing, which is what turns their chunks into failed chunks.
        assert!(Status::parse("completed").has_results());
        for name in ["failed", "expired", "cancelled", "in_progress"] {
            assert!(!Status::parse(name).has_results(), "{name} carries no results");
        }
        // A status this code has never seen keeps the batch running rather than splitting it early.
        let unknown = Status::parse("quarantined");
        assert_eq!(unknown, Status::Unknown("quarantined".to_string()));
        assert!(!unknown.is_terminal());
        assert!(!unknown.has_results());
    }

    #[test]
    fn host_of_names_the_destination_the_consent_block_prints() {
        assert_eq!(host_of("https://openrouter.ai/api/beta/batches"), "openrouter.ai");
        assert_eq!(host_of("http://127.0.0.1:11434/v1/batches"), "127.0.0.1:11434");
        assert_eq!(host_of("openrouter.ai"), "openrouter.ai");
    }

    #[test]
    fn the_help_states_the_thirty_day_retention() {
        assert!(HELP.contains("30 days"));
        assert!(HELP.contains("24h"));
    }
}
