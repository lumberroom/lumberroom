//! Any OpenAI-compatible `/embeddings` endpoint: Workers AI, OpenRouter, llama-server, TEI.
//!
//! The API carries no notion of query against document, so an asymmetric model's instructions
//! travel as text prefixes, and the API counts tokens the engine cannot see, so the input cap is in
//! characters. Both come from configuration; see `RemoteEmbedConfig`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::config::RemoteEmbedConfig;
use crate::domain::errors::{DomainError, Result};
use crate::ports::Embedder;

/// How many times a refused over-length input is cut and sent again. Token density is uneven along
/// a text: in one LongMemEval session the first 600 characters are 162 bge tokens and the next 600
/// are about 400, so a cut proportional to the whole overshoots and two cuts did not always land.
/// Each retry cuts by the server's latest count, so five converge; past that the cap is badly set
/// and failing the write says so.
const SHRINK_RETRIES: usize = 5;
/// Cut used when the server's refusal does not say how many tokens it counted.
const BLIND_SHRINK: f64 = 0.5;

/// The id stored in `embedding_model` for a remote model. Rows already carry these strings, so the
/// format never changes; `adapters::embedding::id_for` calls this too, so the two cannot drift.
pub(super) fn id(model: &str) -> String {
    format!("openai:{model}")
}

pub struct RemoteEmbedder {
    http: reqwest::Client,
    url: String,
    model: String,
    cfg: RemoteEmbedConfig,
    dim: usize,
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

enum Failure {
    /// The server counted more tokens than its window holds. Carries window over count when the
    /// server said both, so the retry cuts as far as it has to and no further.
    TooLong(Option<f64>),
    Other(DomainError),
}

impl RemoteEmbedder {
    pub fn new(model: &str, dim: usize, cfg: &RemoteEmbedConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| {
                DomainError::internal("could not build the embedding client").with_source(e)
            })?;
        Ok(Self {
            http,
            url: format!("{}/embeddings", cfg.base_url),
            model: model.to_string(),
            cfg: cfg.clone(),
            dim,
        })
    }

    async fn run(&self, prefix: &str, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let mut bodies: Vec<&str> = texts.iter().map(|t| self.capped(t, None)).collect();
        for attempt in 0..=SHRINK_RETRIES {
            let inputs: Vec<String> = bodies.iter().map(|b| format!("{prefix}{b}")).collect();
            match self.post(&inputs).await {
                Ok(vectors) => return self.fit(vectors, texts.len()),
                Err(Failure::TooLong(ratio)) if attempt < SHRINK_RETRIES => {
                    // Five percent under the reported ratio, because characters per token is not
                    // constant along a text and the head that survives may be denser.
                    let keep = ratio.map(|r| r * 0.95).unwrap_or(BLIND_SHRINK);
                    // The refusal names one count and not which input it belongs to. Charging it
                    // to the longest input and cutting only what exceeds that cut leaves an input
                    // that fit untouched; cutting each input by the ratio shrank short ones too.
                    let longest = bodies.iter().map(|b| b.chars().count()).max().unwrap_or(0);
                    let cut = (longest as f64 * keep) as usize;
                    tracing::warn!(
                        attempt,
                        keep,
                        cut,
                        "embedding input over the server's window, cutting it"
                    );
                    for body in bodies.iter_mut() {
                        *body = self.capped(body, Some(cut));
                    }
                }
                Err(Failure::TooLong(_)) => return Err(DomainError::unavailable(
                    "the embedding server refused the input as too long even after cutting it; \
                         lower EMBED_MAX_INPUT_CHARS",
                )),
                Err(Failure::Other(e)) => return Err(e),
            }
        }
        unreachable!("the loop returns on its last attempt")
    }

    /// Cuts on a character boundary. The head of a text is kept because that is what a model that
    /// truncates on its own would have kept.
    fn capped<'a>(&self, text: &'a str, limit: Option<usize>) -> &'a str {
        let Some(n) = limit.or(self.cfg.max_input_chars) else { return text };
        match text.char_indices().nth(n) {
            Some((at, _)) => &text[..at],
            None => text,
        }
    }

    async fn post(&self, inputs: &[String]) -> std::result::Result<Vec<Item>, Failure> {
        let mut req =
            self.http.post(&self.url).json(&json!({ "model": self.model, "input": inputs }));
        if let Some(key) = &self.cfg.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            Failure::Other(
                DomainError::unavailable("the embedding server is unreachable").with_source(e),
            )
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // llama-server answers "input (N tokens) is too large to process" or "... is larger
            // than the max context size"; OpenAI says "maximum context length"; TEI answers 413
            // with "`inputs` must have less than N tokens. Given: M".
            let lower = body.to_ascii_lowercase();
            if lower.contains("too large")
                || lower.contains("context size")
                || lower.contains("context length")
                || lower.contains("must have less than")
            {
                return Err(Failure::TooLong(window_ratio(&lower)));
            }
            // The body stays in the log. `Unavailable` reaches every MCP client verbatim, and a
            // provider's text can carry a masked key, an account name or a billing notice.
            let snippet: String = body.chars().take(300).collect();
            tracing::warn!(status = status.as_u16(), body = %snippet, "the embedding server refused a request");
            return Err(Failure::Other(DomainError::unavailable(format!(
                "the embedding server answered HTTP {}",
                status.as_u16()
            ))));
        }
        let parsed: Response = resp.json().await.map_err(|e| {
            Failure::Other(
                DomainError::internal("the embedding server's answer did not parse").with_source(e),
            )
        })?;
        Ok(parsed.data)
    }

    fn fit(&self, mut items: Vec<Item>, expected: usize) -> Result<Vec<Vec<f32>>> {
        if items.len() != expected {
            return Err(DomainError::internal(format!(
                "the embedding server returned {} vectors for {expected} inputs",
                items.len()
            )));
        }
        items.sort_by_key(|i| i.index);
        items
            .into_iter()
            .map(|i| {
                let mut v = i.embedding;
                // Same rule as the local adapter: zero padding leaves cosine unchanged, and
                // truncation would not.
                if v.len() > self.dim {
                    return Err(DomainError::internal(format!(
                        "the model returned {} dims, wider than the {}-dim column",
                        v.len(),
                        self.dim
                    )));
                }
                v.resize(self.dim, 0.0);
                Ok(v)
            })
            .collect()
    }
}

/// Window over count from a lowercased refusal. llama-server: "input (16255 tokens) is too large
/// to process. increase the physical batch size (current batch size: 8192)", or "input (900
/// tokens) is larger than the max context size (512 tokens)". TEI: "`inputs` must have less than
/// 512 tokens. given: 900", or the same in characters. `None` for any other wording.
fn window_ratio(message: &str) -> Option<f64> {
    let (count, window) = match number_after(message, "must have less than ") {
        Some(window) => (number_after(message, "given: ")?, window),
        None => (
            number_after(message, "input (")?,
            number_after(message, "batch size: ")
                .or_else(|| number_after(message, "context size ("))?,
        ),
    };
    (count > 0 && window < count).then(|| window as f64 / count as f64)
}

fn number_after(text: &str, marker: &str) -> Option<u64> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[async_trait]
impl Embedder for RemoteEmbedder {
    fn id(&self) -> String {
        id(&self.model)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.run(&self.cfg.document_prefix, texts).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.run(&self.cfg.query_prefix, vec![text.to_string()]).await?;
        out.pop().ok_or_else(|| DomainError::internal("embedder returned no vector"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedder(max: Option<usize>) -> RemoteEmbedder {
        let cfg = RemoteEmbedConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            max_input_chars: max,
            timeout_secs: 1,
            ..Default::default()
        };
        RemoteEmbedder::new("m", 4, &cfg).unwrap()
    }

    #[test]
    fn the_cap_cuts_on_a_character_boundary() {
        let e = embedder(Some(3));
        assert_eq!(e.capped("héllo", None), "hél");
        assert_eq!(e.capped("hé", None), "hé");
        assert_eq!(e.capped("héllo", Some(1)), "h");
        assert_eq!(embedder(None).capped("héllo", None), "héllo");
    }

    #[test]
    fn vectors_come_back_in_input_order_and_padded() {
        let e = embedder(None);
        let items = vec![
            Item { embedding: vec![2.0, 2.0], index: 1 },
            Item { embedding: vec![1.0, 1.0], index: 0 },
        ];
        let out = e.fit(items, 2).unwrap();
        assert_eq!(out, vec![vec![1.0, 1.0, 0.0, 0.0], vec![2.0, 2.0, 0.0, 0.0]]);
    }

    #[test]
    fn a_refusal_that_names_both_counts_cuts_to_the_window() {
        let batch =
            "input (16255 tokens) is too large to process. increase the physical batch size \
                     (current batch size: 8192)";
        let ratio = window_ratio(batch).unwrap();
        assert!((ratio - 8192.0 / 16255.0).abs() < 1e-9);
        let ctx = "input (900 tokens) is larger than the max context size (512 tokens). skipping";
        assert!((window_ratio(ctx).unwrap() - 512.0 / 900.0).abs() < 1e-9);
        assert_eq!(window_ratio("maximum context length exceeded"), None);
    }

    #[test]
    fn a_vector_wider_than_the_column_is_refused() {
        let e = embedder(None);
        let items = vec![Item { embedding: vec![0.0; 5], index: 0 }];
        assert!(e.fit(items, 1).is_err());
    }

    #[test]
    fn a_vector_exactly_as_wide_as_the_column_is_kept_as_is() {
        let e = embedder(None);
        let items = vec![Item { embedding: vec![1.0, 2.0, 3.0, 4.0], index: 0 }];
        assert_eq!(e.fit(items, 1).unwrap(), vec![vec![1.0, 2.0, 3.0, 4.0]]);
    }

    #[test]
    fn a_refusal_whose_window_is_not_below_its_count_gives_no_ratio() {
        let odd = "input (400 tokens) is larger than the max context size (512 tokens)";
        assert_eq!(window_ratio(odd), None);
        assert_eq!(
            window_ratio("input (0 tokens) is larger than the max context size (512 tokens)"),
            None
        );
    }

    #[test]
    fn a_tei_refusal_names_both_counts() {
        let tokens = "{\"error\":\"`inputs` must have less than 512 tokens. given: 900\",\"error_type\":\"validation\"}";
        assert!((window_ratio(tokens).unwrap() - 512.0 / 900.0).abs() < 1e-9);
        let chars = "`inputs` must have less than 2000 characters. given: 5000";
        assert!((window_ratio(chars).unwrap() - 2000.0 / 5000.0).abs() < 1e-9);
    }

    // A local HTTP server that answers each POST from a script and records what it was sent. Every
    // response closes the connection, so one accept is one request.
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone)]
    enum Reply {
        /// A refusal with this status and body.
        Refuse(u16, &'static str),
        /// One vector of this width per input, in order.
        Vectors(usize),
    }

    struct Seen {
        authorization: Option<String>,
        inputs: Vec<String>,
    }

    /// Serves `script` in order and repeats its last entry once the script runs out.
    async fn mock(script: Vec<Reply>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut n = 0usize;
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let (head, body) = read_request(&mut sock).await;
                let authorization = head
                    .lines()
                    .find_map(|l| l.strip_prefix("authorization: ").map(str::to_string));
                let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let inputs: Vec<String> = parsed["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect();
                let count = inputs.len();
                log.lock().unwrap().push(Seen { authorization, inputs });
                let reply = script.get(n).or(script.last()).cloned().unwrap();
                n += 1;
                let (status, text) = match reply {
                    Reply::Refuse(status, text) => (status, text.to_string()),
                    Reply::Vectors(width) => {
                        let data: Vec<_> = (0..count)
                            .map(|i| json!({ "index": i, "embedding": vec![1.0f32; width] }))
                            .collect();
                        (200, json!({ "data": data }).to_string())
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                sock.write_all(response.as_bytes()).await.unwrap();
                let _ = sock.shutdown().await;
            }
        });
        (url, seen)
    }

    /// Headers lowercased, and the body read to its content-length.
    async fn read_request(sock: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let split = loop {
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed before sending headers");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .map_or(0, |v| v.trim().parse().unwrap());
        let mut body = buf[split..].to_vec();
        while body.len() < length {
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed mid-body");
            body.extend_from_slice(&chunk[..n]);
        }
        (head, body)
    }

    fn remote(url: &str, key: Option<&str>) -> RemoteEmbedder {
        let cfg = RemoteEmbedConfig {
            base_url: url.into(),
            api_key: key.map(str::to_string),
            timeout_secs: 5,
            ..Default::default()
        };
        RemoteEmbedder::new("m", 4, &cfg).unwrap()
    }

    const TEI_REFUSAL: &str =
        "{\"error\":\"`inputs` must have less than 500 tokens. Given: 1000\",\"error_type\":\"Validation\"}";

    #[tokio::test]
    async fn a_too_long_refusal_is_cut_by_the_reported_ratio_and_sent_again() {
        let (url, seen) = mock(vec![Reply::Refuse(413, TEI_REFUSAL), Reply::Vectors(4)]).await;
        let out = remote(&url, None).embed_documents(vec!["x".repeat(100)]).await.unwrap();
        assert_eq!(out, vec![vec![1.0; 4]]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].inputs, vec!["x".repeat(100)]);
        // 100 characters times 500/1000 times the 0.95 margin.
        assert_eq!(seen[1].inputs, vec!["x".repeat(47)]);
    }

    #[tokio::test]
    async fn a_refusal_that_never_relents_stops_after_six_posts() {
        let (url, seen) = mock(vec![Reply::Refuse(413, TEI_REFUSAL)]).await;
        let e = remote(&url, None).embed_documents(vec!["x".repeat(4000)]).await.unwrap_err();
        assert!(e.client_message().contains("even after cutting"), "{}", e.client_message());
        assert_eq!(seen.lock().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn only_the_inputs_longer_than_the_cut_are_cut() {
        let (url, seen) = mock(vec![Reply::Refuse(413, TEI_REFUSAL), Reply::Vectors(4)]).await;
        let texts = vec!["a".repeat(10), "b".repeat(100)];
        remote(&url, None).embed_documents(texts).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[1].inputs, vec!["a".repeat(10), "b".repeat(47)]);
    }

    #[tokio::test]
    async fn the_key_travels_as_a_bearer_header() {
        let (url, seen) = mock(vec![Reply::Vectors(4)]).await;
        remote(&url, Some("k-123")).embed_query("q").await.unwrap();
        assert_eq!(seen.lock().unwrap()[0].authorization.as_deref(), Some("bearer k-123"));
    }

    #[tokio::test]
    async fn no_key_sends_no_authorization_header() {
        let (url, seen) = mock(vec![Reply::Vectors(4)]).await;
        remote(&url, None).embed_query("q").await.unwrap();
        assert_eq!(seen.lock().unwrap()[0].authorization, None);
    }

    #[tokio::test]
    async fn a_provider_error_reaches_the_client_as_its_status_alone() {
        let (url, _) = mock(vec![Reply::Refuse(401, "invalid api key sk-****wxyz")]).await;
        let e = remote(&url, Some("k")).embed_query("q").await.unwrap_err();
        let shown = e.client_message();
        assert!(shown.contains("401"), "{shown}");
        assert!(!shown.contains("sk-"), "provider text leaked: {shown}");
        assert!(!shown.contains("invalid api key"), "provider text leaked: {shown}");
    }

    #[tokio::test]
    async fn a_column_wide_answer_from_the_server_is_accepted_and_a_wider_one_refused() {
        let (url, _) = mock(vec![Reply::Vectors(4)]).await;
        assert_eq!(remote(&url, None).embed_query("q").await.unwrap(), vec![1.0; 4]);
        let (url, _) = mock(vec![Reply::Vectors(5)]).await;
        assert!(remote(&url, None).embed_query("q").await.is_err());
        let (url, _) = mock(vec![Reply::Vectors(2)]).await;
        assert_eq!(remote(&url, None).embed_query("q").await.unwrap(), vec![1.0, 1.0, 0.0, 0.0]);
    }
}
