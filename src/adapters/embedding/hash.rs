//! Deterministic hashing embedder. No weights, no network, no cost.
//!
//! Its job is to make tests fast and hermetic. It is never a fallback for a failed model (decision
//! 0028): retrieval quality is word overlap only, and its vector is a token sketch that stays
//! readable in the stored row, so it never suits private content.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::domain::errors::Result;
use crate::ports::Embedder;

pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0f32; self.dim];
        for token in text
            .to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
            .filter(|t| !t.is_empty())
        {
            let digest = Sha256::digest(token.as_bytes());
            let bucket = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) as usize
                % self.dim;
            // A sign from a second byte stops unrelated tokens piling up in one direction.
            let sign = if digest[4] % 2 == 0 { 1.0 } else { -1.0 };
            vec[bucket] += sign;
            if token.len() > 4 {
                let d2 = Sha256::digest(&token.as_bytes()[..4]);
                let b2 = u32::from_be_bytes([d2[0], d2[1], d2[2], d2[3]]) as usize % self.dim;
                vec[b2] += sign * 0.5;
            }
        }
        let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm == 0.0 {
            // An empty vector has no cosine distance to anything; park it on one axis instead.
            vec[0] = 1.0;
            return vec;
        }
        vec.iter().map(|v| v / norm).collect()
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    fn id(&self) -> String {
        format!("hash-v1-{}", self.dim)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_one(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[tokio::test]
    async fn produces_unit_vectors_of_the_configured_width() {
        let e = HashEmbedder::new(768);
        let v = e.embed_query("postgres runs on the oracle box").await.unwrap();
        assert_eq!(v.len(), 768);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[tokio::test]
    async fn is_deterministic() {
        let e = HashEmbedder::new(768);
        assert_eq!(
            e.embed_query("same text").await.unwrap(),
            e.embed_query("same text").await.unwrap()
        );
    }

    #[tokio::test]
    async fn scores_overlapping_text_above_unrelated_text() {
        let e = HashEmbedder::new(768);
        let doc = e
            .embed_query("the memory engine embeds with bge-base at 768 dimensions")
            .await
            .unwrap();
        let near = e.embed_query("which embedding model does the memory engine use").await.unwrap();
        let far = e.embed_query("mango chutney recipe for a wedding lunch").await.unwrap();
        assert!(dot(&doc, &near) > dot(&doc, &far));
    }

    #[tokio::test]
    async fn survives_whitespace_instead_of_emitting_a_zero_vector() {
        let e = HashEmbedder::new(768);
        let v = e.embed_query("   ").await.unwrap();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn returns_nothing_for_an_empty_batch() {
        let e = HashEmbedder::new(768);
        assert!(e.embed_documents(vec![]).await.unwrap().is_empty());
    }
}
