use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::embedding_slot::VectorSlot;
use crate::domain::errors::{DomainError, Result};
use crate::domain::similarity::SimilarityThresholds;
use crate::ports::Embedder;

/// The slot, embedders and thresholds one request uses for one unit.
#[derive(Clone)]
pub struct UnitEmbedding {
    pub unit: String,
    /// The column readers use for this unit. Always `A` in E2a.
    pub slot: VectorSlot,
    /// The model of `slot`. Query vectors and the write's first vector come from it.
    pub embedder: Arc<dyn Embedder>,
    /// The other configured model, for a write's second vector in `slot.other()`. None in E2a.
    pub second: Option<Arc<dyn Embedder>>,
    /// Resolved for `embedder`'s id. Never the other model's values.
    pub thresholds: Arc<SimilarityThresholds>,
}

pub struct EmbedderSet {
    current: Arc<dyn Embedder>,
    /// Resolved once at boot for every configured embedder id.
    thresholds: HashMap<String, Arc<SimilarityThresholds>>,
}

impl EmbedderSet {
    pub fn single(current: Arc<dyn Embedder>,
                  thresholds: HashMap<String, Arc<SimilarityThresholds>>) -> Self {
        Self { current, thresholds }
    }

    pub fn current(&self) -> Arc<dyn Embedder> {
        Arc::clone(&self.current)
    }

    /// Every configured model's values, for the status page.
    pub fn all_thresholds(&self) -> &HashMap<String, Arc<SimilarityThresholds>> {
        &self.thresholds
    }

    /// An internal error naming `model` when boot resolved nothing for it.
    pub fn thresholds_for(&self, model: &str) -> Result<Arc<SimilarityThresholds>> {
        self.thresholds.get(model).cloned().ok_or_else(|| {
            DomainError::internal(format!("no thresholds resolved for embedder {model}"))
        })
    }

    pub fn for_unit(&self, unit: &str) -> Result<UnitEmbedding> {
        Ok(UnitEmbedding {
            unit: unit.to_string(),
            slot: VectorSlot::A,
            embedder: Arc::clone(&self.current),
            second: None,
            thresholds: self.thresholds_for(&self.current.id())?,
        })
    }
}
