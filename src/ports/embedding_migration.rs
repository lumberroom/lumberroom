//! The store an embedding-model migration reads and writes. Only `adapters::postgres` implements it.

use async_trait::async_trait;

use crate::domain::embedding_migration::{
    Counts, FlipOutcome, FlipRequest, Intent, IntentChange, PendingRow, Published, UnitState,
};
use crate::domain::embedding_slot::VectorSlot;
use crate::domain::errors::Result;

#[async_trait]
pub trait EmbeddingMigrationRepository: Send + Sync {
    /// Creates a state row for every unit that has vectors and none: model_a and model_b from the
    /// majority id in each column, active_slot 'a' when model_a is set, else 'b'.
    async fn seed(&self) -> Result<Vec<UnitState>>;
    async fn states(&self) -> Result<Vec<UnitState>>;
    async fn state(&self, unit: &str) -> Result<Option<UnitState>>;
    /// Names `slot`'s model when it is NULL and `slot` is inactive. False when nothing changed.
    async fn name_slot(&self, unit: &str, slot: VectorSlot, model: &str) -> Result<bool>;
    /// `target` is the model the inactive slot should hold; `retire` the model being deleted.
    async fn counts(
        &self,
        unit: &str,
        state: &UnitState,
        target: Option<&str>,
        retire: Option<&str>,
    ) -> Result<Counts>;
    /// Rows pending in `slot` for `model`, id order, after `after`.
    async fn next_batch(
        &self,
        unit: &str,
        slot: VectorSlot,
        model: &str,
        after: Option<uuid::Uuid>,
        limit: i64,
    ) -> Result<Vec<PendingRow>>;
    /// Eligible rows with no vector in `slot`, id order, after `after`. The `steady` fill reads
    /// these, so a row whose vector carries another model's id stays as it is.
    async fn next_holes(
        &self,
        unit: &str,
        slot: VectorSlot,
        after: Option<uuid::Uuid>,
        limit: i64,
    ) -> Result<Vec<PendingRow>>;
    /// True only when the vector landed. False when the row is no longer eligible or pending.
    async fn store(
        &self,
        unit: &str,
        id: uuid::Uuid,
        slot: VectorSlot,
        model: &str,
        vector: Vec<f32>,
    ) -> Result<bool>;
    async fn flip(&self, req: &FlipRequest) -> Result<FlipOutcome>;
    async fn retire_batch(
        &self,
        unit: &str,
        slot: VectorSlot,
        model: &str,
        limit: i64,
    ) -> Result<u64>;
    /// Clears `slot`'s model only when no row still carries it. False when it did not clear.
    async fn clear_slot(&self, unit: &str, slot: VectorSlot, model: &str) -> Result<bool>;
    /// The control row; the defaults when it is missing.
    async fn control(&self) -> Result<(Intent, Published)>;
    /// `embedding_control.generation`, for the re-read before each fill or retire batch.
    async fn generation(&self) -> Result<i64>;
    /// One transaction: insert the row if missing, lock it `FOR UPDATE`, hand the intent to
    /// `decide`, write the change it returns with `generation + 1`, commit. `decide` returns
    /// Ok(None) for a no-op and Err(text) for a refusal; both write nothing and roll back.
    async fn change_intent(
        &self,
        // The states are re-read inside the transaction, after the row lock (review M7).
        // Higher-ranked: async_trait names elided lifetimes inside a dyn Fn argument, and the
        // adapter passes values it reads inside the transaction.
        decide: &(dyn for<'i, 's> Fn(
            &'i Intent,
            &'s [UnitState],
        ) -> std::result::Result<Option<IntentChange>, String>
              + Send
              + Sync),
    ) -> Result<ChangeOutcome>;
    /// Command mode, end of each pass.
    async fn publish(&self, published: &Published) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOutcome {
    Written(Intent),
    NoOp(Intent),
    Refused(String),
}
