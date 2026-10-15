//! Local models through fastembed: bge-base-en-v1.5 by default, and the multilingual E5 pair for
//! stores that are not written in English.
//!
//! Two things the spike established and this file is shaped by:
//! `TextEmbedding::embed` takes `&mut self`, so the model sits behind a lock; and embedding is
//! CPU-bound at 11-16ms per short text, so the work goes to a blocking thread rather than
//! occupying an async worker. Doing either naively would serialise the whole server behind it.

use async_trait::async_trait;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::sync::{Arc, Mutex};

use crate::domain::errors::{DomainError, Result};
use crate::ports::Embedder;

/// bge retrieval is asymmetric: queries carry this instruction, stored documents do not.
const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// The fastembed variant a configured model name loads. The image build calls this too, so the
/// weights baked into the image are the weights the server asks for.
pub fn variant_for(model_name: &str) -> Result<EmbeddingModel> {
    Ok(match model_name {
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
        // fastembed 7 ships E5 as fp32 ONNX only, so these load several times the bytes bge's
        // quantised export does. e5-large stays out: it is 1024 wide and the column is 768.
        "intfloat/multilingual-e5-base" | "multilingual-e5-base" | "MultilingualE5Base" => {
            EmbeddingModel::MultilingualE5Base
        }
        "intfloat/multilingual-e5-small" | "multilingual-e5-small" | "MultilingualE5Small" => {
            EmbeddingModel::MultilingualE5Small
        }
        other => {
            return Err(DomainError::validation(format!(
                "unknown embed model {other:?}. This build ships bge-base-en-v1.5 (q8), \
                 bge-small-en-v1.5, all-MiniLM-L6-v2, multilingual-e5-base and \
                 multilingual-e5-small."
            )))
        }
    })
}

/// The (query, document) text each model was trained to see in front of its input. Keyed on the
/// variant rather than the name, so an alias such as `BGEBaseENV15Q` gets bge's prefix too. E5
/// without its prefixes still returns vectors, and they rank worse with nothing in the log.
fn prefixes(variant: &EmbeddingModel) -> (&'static str, &'static str) {
    match variant {
        EmbeddingModel::BGEBaseENV15Q | EmbeddingModel::BGESmallENV15Q => (BGE_QUERY_PREFIX, ""),
        EmbeddingModel::MultilingualE5Base | EmbeddingModel::MultilingualE5Small => {
            ("query: ", "passage: ")
        }
        _ => ("", ""),
    }
}

fn prompts(prefix: &str, texts: Vec<String>) -> Vec<String> {
    if prefix.is_empty() {
        return texts;
    }
    texts.into_iter().map(|t| format!("{prefix}{t}")).collect()
}

/// What `embed_documents` sends the model.
fn document_inputs(variant: &EmbeddingModel, texts: Vec<String>) -> Vec<String> {
    prompts(prefixes(variant).1, texts)
}

/// What `embed_query` sends the model.
fn query_input(variant: &EmbeddingModel, text: &str) -> String {
    format!("{}{text}", prefixes(variant).0)
}

/// The id stored in `embedding_model` for a local model. Rows already carry the `@q8` strings, so
/// their format never changes, an unknown name included; `adapters::embedding::id_for` calls this
/// too, so the two cannot drift. E5 says `@fp32` because that is the only export fastembed ships
/// for it. `resolve_retire` in `src/config.rs` reads both suffixes as an id.
pub(super) fn id(model_name: &str) -> String {
    let suffix = match variant_for(model_name) {
        Ok(EmbeddingModel::MultilingualE5Base | EmbeddingModel::MultilingualE5Small) => "fp32",
        _ => "q8",
    };
    format!("{model_name}@{suffix}")
}

pub struct LocalEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
    id: String,
    dim: usize,
    variant: EmbeddingModel,
}

impl LocalEmbedder {
    pub fn new(model_name: &str, dim: usize, cache_dir: &str) -> Result<Self> {
        let variant = variant_for(model_name)?;

        let opts = TextInitOptions::new(variant.clone())
            .with_cache_dir(cache_dir.into())
            .with_show_download_progress(false);

        let model = TextEmbedding::try_new(opts).map_err(|e| {
            DomainError::internal("failed to load the embedding model")
                .with_source(SendErr(e.to_string()))
        })?;

        Ok(Self { model: Arc::new(Mutex::new(model)), id: id(model_name), dim, variant })
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
        self.run(document_inputs(&self.variant, texts)).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.run(vec![query_input(&self.variant, text)]).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_for_maps_each_multilingual_e5_name() {
        for name in ["multilingual-e5-base", "intfloat/multilingual-e5-base", "MultilingualE5Base"]
        {
            assert_eq!(variant_for(name).unwrap(), EmbeddingModel::MultilingualE5Base, "{name}");
        }
        for name in
            ["multilingual-e5-small", "intfloat/multilingual-e5-small", "MultilingualE5Small"]
        {
            assert_eq!(variant_for(name).unwrap(), EmbeddingModel::MultilingualE5Small, "{name}");
        }
    }

    #[test]
    fn variant_for_keeps_the_english_models() {
        for (name, want) in [
            ("Xenova/bge-base-en-v1.5", EmbeddingModel::BGEBaseENV15Q),
            ("bge-base-en-v1.5", EmbeddingModel::BGEBaseENV15Q),
            ("BGEBaseENV15Q", EmbeddingModel::BGEBaseENV15Q),
            ("bge-small-en-v1.5", EmbeddingModel::BGESmallENV15Q),
            ("all-MiniLM-L6-v2", EmbeddingModel::AllMiniLML6V2Q),
            ("all-MiniLM-L6-v2-fp32", EmbeddingModel::AllMiniLML6V2),
        ] {
            assert_eq!(variant_for(name).unwrap(), want, "{name}");
        }
    }

    #[test]
    fn variant_for_refuses_an_unknown_name_and_lists_the_multilingual_models() {
        // e5-large is 1024 wide and the column is 768, so it stays out on purpose.
        for name in ["multilingual-e5-large", "e5-base", "nonsense"] {
            let err = variant_for(name).unwrap_err().client_message().to_string();
            assert!(err.contains("multilingual-e5-base"), "{err}");
        }
    }

    #[test]
    fn e5_takes_query_and_passage_prefixes() {
        for v in [EmbeddingModel::MultilingualE5Base, EmbeddingModel::MultilingualE5Small] {
            assert_eq!(prefixes(&v), ("query: ", "passage: "));
        }
    }

    #[test]
    fn bge_prefixes_only_queries_and_minilm_prefixes_nothing() {
        for v in [EmbeddingModel::BGEBaseENV15Q, EmbeddingModel::BGESmallENV15Q] {
            assert_eq!(prefixes(&v), (BGE_QUERY_PREFIX, ""));
        }
        for v in [EmbeddingModel::AllMiniLML6V2Q, EmbeddingModel::AllMiniLML6V2] {
            assert_eq!(prefixes(&v), ("", ""));
        }
    }

    #[test]
    fn documents_take_the_document_prefix_and_queries_the_query_prefix() {
        // The two entry points call these, and their return types differ, so swapping them in the
        // Embedder impl stops compiling. A swap inside them fails here.
        let e5 = EmbeddingModel::MultilingualE5Base;
        assert_eq!(document_inputs(&e5, vec!["a".to_string()]), vec!["passage: a"]);
        assert_eq!(query_input(&e5, "a"), "query: a");
        let bge = EmbeddingModel::BGEBaseENV15Q;
        assert_eq!(document_inputs(&bge, vec!["a".to_string()]), vec!["a"]);
        assert_eq!(query_input(&bge, "a"), format!("{BGE_QUERY_PREFIX}a"));
    }

    #[test]
    fn prompts_prepend_the_prefix_to_every_text() {
        let texts = vec!["a".to_string(), "b".to_string()];
        assert_eq!(prompts("passage: ", texts.clone()), vec!["passage: a", "passage: b"]);
        assert_eq!(prompts("", texts.clone()), texts);
    }

    #[test]
    fn e5_ids_name_the_fp32_weights() {
        // fastembed 7 ships E5 only as fp32 ONNX, so an @q8 id would misdescribe stored vectors.
        assert_eq!(id("multilingual-e5-base"), "multilingual-e5-base@fp32");
        assert_eq!(id("intfloat/multilingual-e5-small"), "intfloat/multilingual-e5-small@fp32");
        assert_eq!(id("Xenova/bge-base-en-v1.5"), "Xenova/bge-base-en-v1.5@q8");
        assert_eq!(id("nonsense"), "nonsense@q8");
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (norm(a) * norm(b))
    }

    /// Loads the real weights, so it downloads them on first run: about 1.1 GB for base and 470 MB
    /// for small. Run with `./scripts/cargo.sh test --lib -- --ignored --nocapture
    /// e5_ranks_the_relevant_passage_first`. The printed cosines are observations from a handful of
    /// pairs and calibrate nothing.
    #[tokio::test]
    #[ignore = "downloads the E5 weights"]
    async fn e5_ranks_the_relevant_passage_first() {
        let cache = std::env::temp_dir().join("lumberroom-e5-weights");
        let passages = [
            "The project stores its data in PostgreSQL 16 with the pgvector extension.",
            "यह प्रोजेक्ट अपना डेटा pgvector के साथ PostgreSQL 16 में रखता है।",
            "My sister adopted a grey cat last spring.",
            "मेरी बहन ने पिछले वसंत में एक भूरी बिल्ली गोद ली।",
        ];
        let queries = [
            ("en", "Which database does the project use?"),
            ("hi", "प्रोजेक्ट कौन सा डेटाबेस इस्तेमाल करता है?"),
            ("ja", "このプロジェクトはどのデータベースを使っていますか？"),
        ];
        for (model, native) in [("multilingual-e5-small", 384), ("multilingual-e5-base", 768)] {
            let e = LocalEmbedder::new(model, 768, cache.to_str().unwrap()).unwrap();
            let docs = e.embed_documents(passages.iter().map(|p| p.to_string()).collect()).await;
            let docs = docs.unwrap();
            for d in &docs {
                assert_eq!(d.len(), 768, "{model} must come back at the column width");
                assert!(d[native..].iter().all(|x| *x == 0.0), "{model} pads past {native}");
            }
            println!("{model}: id {}, native dims {native}, stored dims {}", e.id(), docs[0].len());
            println!(
                "{model}: passage-passage en/hi same fact {:.4}, en/en unrelated {:.4}",
                cosine(&docs[0], &docs[1]),
                cosine(&docs[0], &docs[2])
            );
            // The same text embedded as a query and as a passage must differ, or no prefix reached
            // the model.
            let as_query = e.embed_query(passages[0]).await.unwrap();
            let same = cosine(&as_query, &docs[0]);
            println!("{model}: one text as query and as passage {same:.4}");
            assert!(same < 0.999, "{model}: query and passage prefixes did not apply");
            for (lang, q) in queries {
                let q = e.embed_query(q).await.unwrap();
                let s: Vec<f32> = docs.iter().map(|d| cosine(&q, d)).collect();
                println!(
                    "{model}: query {lang} vs en relevant {:.4}, hi relevant {:.4}, \
                     en unrelated {:.4}, hi unrelated {:.4}",
                    s[0], s[1], s[2], s[3]
                );
                assert!(s[0].min(s[1]) > s[2].max(s[3]), "{model} {lang}: {s:?}");
            }
        }
    }
}
