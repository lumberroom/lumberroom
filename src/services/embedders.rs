use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::domain::embedding_migration::{Configured, FlipScope, UnitState};
use crate::domain::embedding_slot::VectorSlot;
use crate::domain::errors::{DomainError, Result};
use crate::domain::similarity::SimilarityThresholds;
use crate::ports::Embedder;

/// The slot, embedders and thresholds one request uses for one unit.
#[derive(Clone)]
pub struct UnitEmbedding {
    pub unit: String,
    /// The column readers use for this unit.
    pub slot: VectorSlot,
    /// The model of `slot`. Query vectors and the write's first vector come from it.
    pub embedder: Arc<dyn Embedder>,
    /// The other configured model, for a write's second vector in `slot.other()`. None outside a
    /// migration, and None when the inactive slot names a model that is neither configured one.
    pub second: Option<Arc<dyn Embedder>>,
    /// Resolved for `embedder`'s id. Never the other model's values.
    pub thresholds: Arc<SimilarityThresholds>,
}

pub struct EmbedderSet {
    /// One per configured block, built and warmed at boot. Ids differ (boot refuses P equal to C).
    built: Vec<Arc<dyn Embedder>>,
    /// Which built model is current and which previous. Fixed in env mode; the sweep replaces it on
    /// every pass in command mode.
    configured: RwLock<Arc<Configured>>,
    /// Loaded at boot, replaced every sweep pass, and updated after each commit that changes a
    /// unit's state. A stale entry is safe: the slot it names still holds its model.
    states: RwLock<HashMap<String, UnitState>>,
    /// Resolved once at boot for every configured embedder id.
    thresholds: HashMap<String, Arc<SimilarityThresholds>>,
}

impl EmbedderSet {
    /// One model and no migration: the single-block boot and most tests.
    pub fn single(
        current: Arc<dyn Embedder>,
        thresholds: HashMap<String, Arc<SimilarityThresholds>>,
    ) -> Self {
        let configured = Configured {
            current: current.id(),
            previous: None,
            retire: None,
            flip: FlipScope::All,
            rollback_days: 7,
            guessed_acting: Default::default(),
            generation: None,
        };
        Self::new(vec![current], configured, thresholds)
    }

    pub fn new(
        built: Vec<Arc<dyn Embedder>>,
        configured: Configured,
        thresholds: HashMap<String, Arc<SimilarityThresholds>>,
    ) -> Self {
        Self {
            built,
            configured: RwLock::new(Arc::new(configured)),
            states: RwLock::new(HashMap::new()),
            thresholds,
        }
    }

    pub fn configured(&self) -> Arc<Configured> {
        Arc::clone(&self.configured.read().expect("configured lock poisoned"))
    }

    pub fn set_configured(&self, configured: Arc<Configured>) {
        *self.configured.write().expect("configured lock poisoned") = configured;
    }

    /// The built embedder whose id is `id`.
    pub fn by_id(&self, id: &str) -> Option<Arc<dyn Embedder>> {
        self.built.iter().find(|e| e.id() == id).cloned()
    }

    /// The embedder `configured().current` names. Boot builds every configured block, so a miss is
    /// a wiring bug and panics with the id.
    pub fn current(&self) -> Arc<dyn Embedder> {
        let id = self.configured().current.clone();
        self.by_id(&id).unwrap_or_else(|| panic!("no built embedder for the current model {id}"))
    }

    pub fn previous(&self) -> Option<Arc<dyn Embedder>> {
        self.configured().previous.as_deref().and_then(|id| self.by_id(id))
    }

    pub fn set_states(&self, states: Vec<UnitState>) {
        let map = states.into_iter().map(|s| (s.unit.clone(), s)).collect();
        *self.states.write().expect("states lock poisoned") = map;
    }

    pub fn put_state(&self, state: UnitState) {
        self.states.write().expect("states lock poisoned").insert(state.unit.clone(), state);
    }

    /// Every model id some unit is active on, plus the id a unit with no state row answers with.
    /// The fork's readiness reads it.
    pub fn active_ids(&self) -> Vec<String> {
        let configured = self.configured();
        let fallback = configured.previous.clone().unwrap_or_else(|| configured.current.clone());
        let mut ids: Vec<String> = self
            .states
            .read()
            .expect("states lock poisoned")
            .values()
            .map(|s| s.active_model().to_string())
            .collect();
        ids.push(fallback);
        ids.sort();
        ids.dedup();
        ids
    }

    /// Every configured model's values, for the sweep's published status and the console's
    /// model-change section.
    pub fn all_thresholds(&self) -> &HashMap<String, Arc<SimilarityThresholds>> {
        &self.thresholds
    }

    /// An internal error naming `model` when boot resolved nothing for it.
    pub fn thresholds_for(&self, model: &str) -> Result<Arc<SimilarityThresholds>> {
        self.thresholds.get(model).cloned().ok_or_else(|| {
            DomainError::internal(format!("no thresholds resolved for embedder {model}"))
        })
    }

    /// Synchronous on purpose: it answers from the in-memory state map, so no request path reads
    /// the state table. It reads `configured()` once, so one request never mixes two views.
    pub fn for_unit(&self, unit: &str) -> Result<UnitEmbedding> {
        let configured = self.configured();
        let state = self.states.read().expect("states lock poisoned").get(unit).cloned();
        let built = |id: &str| {
            self.by_id(id).ok_or_else(|| {
                DomainError::internal(format!(
                    "unit {unit} is active on {id}, which this process has no embedder for"
                ))
            })
        };

        let (slot, embedder, second) = match (&configured.previous, state) {
            // No migration: the unit's state names the slot and the model; no state row means
            // slot A and the current model.
            (None, None) => (VectorSlot::A, built(&configured.current)?, None),
            (None, Some(s)) => (s.active_slot, built(s.active_model())?, None),
            // A migration with no state row yet: the unit still serves the previous model from
            // slot A, and writes also embed with the current one.
            (Some(previous), None) => {
                (VectorSlot::A, built(previous)?, Some(built(&configured.current)?))
            }
            (Some(previous), Some(s)) => {
                let active = s.active_model().to_string();
                let other_id = if active == configured.current {
                    previous.clone()
                } else {
                    configured.current.clone()
                };
                // The inactive slot holds a third model: a second vector there would overwrite it
                // (review M1). The sweep reports the unit blocked instead.
                let other_ok = match s.other_model() {
                    None => true,
                    Some(m) => m == other_id,
                };
                let second =
                    if other_ok && other_id != active { self.by_id(&other_id) } else { None };
                (s.active_slot, built(&active)?, second)
            }
        };
        let thresholds = self.thresholds_for(&embedder.id())?;
        Ok(UnitEmbedding { unit: unit.to_string(), slot, embedder, second, thresholds })
    }
}
