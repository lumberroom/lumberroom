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
            .map_err(|e| DomainError::internal("could not build the embedding client").with_source(e))?;
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
        let mut inputs: Vec<String> =
            texts.iter().map(|t| format!("{prefix}{}", self.capped(t, None))).collect();
        for attempt in 0..=SHRINK_RETRIES {
            match self.post(&inputs).await {
                Ok(vectors) => return self.fit(vectors, texts.len()),
                Err(Failure::TooLong(ratio)) if attempt < SHRINK_RETRIES => {
                    // Five percent under the reported ratio, because characters per token is not
                    // constant along a text and the head that survives may be denser.
                    let keep = ratio.map(|r| r * 0.95).unwrap_or(BLIND_SHRINK);
                    tracing::warn!(attempt, keep, "embedding input over the server's window, cutting it");
                    inputs = texts
                        .iter()
                        .zip(&inputs)
                        .map(|(t, sent)| {
                            let body = sent.chars().count() - prefix.chars().count();
                            let limit = (body as f64 * keep) as usize;
                            format!("{prefix}{}", self.capped(t, Some(limit)))
                        })
                        .collect();
                }
                Err(Failure::TooLong(_)) => {
                    return Err(DomainError::unavailable(
                        "the embedding server refused the input as too long even after cutting it; \
                         lower EMBED_MAX_INPUT_CHARS",
                    ))
                }
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
        let mut req = self.http.post(&self.url).json(&json!({ "model": self.model, "input": inputs }));
        if let Some(key) = &self.cfg.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            Failure::Other(DomainError::unavailable("the embedding server is unreachable").with_source(e))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // llama-server answers "input (N tokens) is too large to process" or "... is larger
            // than the max context size"; OpenAI says "maximum context length".
            let lower = body.to_ascii_lowercase();
            if lower.contains("too large") || lower.contains("context size") || lower.contains("context length") {
                return Err(Failure::TooLong(window_ratio(&lower)));
            }
            let snippet: String = body.chars().take(300).collect();
            return Err(Failure::Other(DomainError::unavailable(format!(
                "the embedding server answered {status}: {snippet}"
            ))));
        }
        let parsed: Response = resp.json().await.map_err(|e| {
            Failure::Other(DomainError::internal("the embedding server's answer did not parse").with_source(e))
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

/// Window over count from a llama-server refusal: "input (16255 tokens) is too large to process.
/// increase the physical batch size (current batch size: 8192)", or "input (900 tokens) is larger
/// than the max context size (512 tokens)". `None` for any other wording.
fn window_ratio(message: &str) -> Option<f64> {
    let count = number_after(message, "input (")?;
    let window = number_after(message, "batch size: ")
        .or_else(|| number_after(message, "context size ("))?;
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
        format!("openai:{}", self.model)
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
        let batch = "input (16255 tokens) is too large to process. increase the physical batch size \
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
}
