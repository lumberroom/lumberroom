//! bge-base-en-v1.5 through fastembed.
//!
//! Two things the spike established and this file is shaped by:
//! `TextEmbedding::embed` takes `&mut self`, so the model sits behind a lock; and embedding is
//! CPU-bound at 11-16ms per short text, so the work goes to a blocking thread rather than
//! occupying an async worker. Doing either naively would serialise the whole server behind it.

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::sync::{Arc, Mutex};

use crate::domain::errors::{DomainError, Result};
use crate::ports::Embedder;

/// bge retrieval is asymmetric: queries carry this instruction, stored documents do not.
const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// The id stored in `embedding_model` for a local model. Rows already carry these strings, so the
/// format never changes; `adapters::embedding::id_for` calls this too, so the two cannot drift.
pub(super) fn id(model_name: &str) -> String {
    format!("{model_name}@q8")
}

pub struct LocalEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
    id: String,
    dim: usize,
    is_bge: bool,
}

impl LocalEmbedder {
    pub fn new(model_name: &str, dim: usize, cache_dir: &str) -> Result<Self> {
        let variant = match model_name {
            "Xenova/bge-base-en-v1.5" | "bge-base-en-v1.5" | "BGEBaseENV15Q" => {
                EmbeddingModel::BGEBaseENV15Q
            }
            "bge-small-en-v1.5" | "BGESmallENV15Q" => EmbeddingModel::BGESmallENV15Q,
            // Carried for one reason: it is the model agentmemory's published LongMemEval numbers
            // were produced with, and a retrieval comparison run on a different embedder measures
            // the embedder. The quantised arm is the closer match, because their local provider
            // loads @xenova/transformers, which quantises by default.
            "sentence-transformers/all-MiniLM-L6-v2" | "all-MiniLM-L6-v2" | "AllMiniLML6V2Q" => {
                EmbeddingModel::AllMiniLML6V2Q
            }
            "all-MiniLM-L6-v2-fp32" | "AllMiniLML6V2" => EmbeddingModel::AllMiniLML6V2,
            other => {
                return Err(DomainError::validation(format!(
                    "unknown embed model {other:?}. This build ships bge-base-en-v1.5 (q8), \
                     bge-small-en-v1.5 and all-MiniLM-L6-v2."
                )))
            }
        };

        let opts = InitOptions::new(variant)
            .with_cache_dir(cache_dir.into())
            .with_show_download_progress(false);

        let model = TextEmbedding::try_new(opts).map_err(|e| {
            DomainError::internal("failed to load the embedding model")
                .with_source(SendErr(e.to_string()))
        })?;

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            id: id(model_name),
            dim,
            is_bge: model_name.to_ascii_lowercase().contains("bge"),
        })
    }

    async fn run(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = Arc::clone(&self.model);
        let expected = self.dim;

        let vectors = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
            let mut guard = model
                .lock()
                .map_err(|_| DomainError::internal("embedding model lock was poisoned"))?;
            guard.embed(texts, None).map_err(|e| {
                DomainError::internal("embedding failed").with_source(SendErr(e.to_string()))
            })
        })
        .await
        .map_err(|e| {
            DomainError::internal("embedding task panicked").with_source(SendErr(e.to_string()))
        })??;

        // A model narrower than the column is padded with zeros rather than refused. Cosine is
        // invariant under zero padding: the dot product and both norms are unchanged, so a
        // 384-dim model stored in a vector(768) column ranks identically to one stored natively.
        // This is what lets the LongMemEval comparison run on all-MiniLM-L6-v2 against a schema
        // built for bge-base, with no migration and no second database.
        let mut vectors = vectors;
        for v in &mut vectors {
            match v.len().cmp(&expected) {
                std::cmp::Ordering::Equal => {}
                std::cmp::Ordering::Less => v.resize(expected, 0.0),
                std::cmp::Ordering::Greater => {
                    return Err(DomainError::internal(format!(
                        "model returned {} dims, wider than the {expected}-dim column. Truncating \
                         would change the distances, so this is refused.",
                        v.len()
                    )))
                }
            }
        }
        Ok(vectors)
    }

    /// Loads weights and proves the model produces a real vector. Called at boot so the first
    /// tool call a model ever makes is a fast one.
    pub async fn warm(&self) -> Result<()> {
        self.run(vec!["warm".to_string()]).await.map(|_| ())
    }
}

#[async_trait]
impl Embedder for LocalEmbedder {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.run(texts).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prompt =
            if self.is_bge { format!("{BGE_QUERY_PREFIX}{text}") } else { text.to_string() };
        let mut out = self.run(vec![prompt]).await?;
        out.pop().ok_or_else(|| DomainError::internal("embedder returned no vector"))
    }
}

/// fastembed's error type is not Send + Sync, which `DomainError::with_source` requires. Carrying
/// the message keeps the cause in the log without leaking a non-Send type across an await.
#[derive(Debug)]
struct SendErr(String);

impl std::fmt::Display for SendErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SendErr {}
