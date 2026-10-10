//! The state, intent and status types of an embedding-model migration (decision 0027).

use chrono::{DateTime, Utc};

use crate::domain::embedding_slot::VectorSlot;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UnitState {
    pub unit: String,
    pub active_slot: VectorSlot,
    pub model_a: Option<String>,
    pub model_b: Option<String>,
    pub flipped_at: Option<DateTime<Utc>>,
}

impl UnitState {
    pub fn model(&self, slot: VectorSlot) -> Option<&str> {
        match slot {
            VectorSlot::A => self.model_a.as_deref(),
            VectorSlot::B => self.model_b.as_deref(),
        }
    }
    /// The CHECK constraint guarantees the active slot is named.
    pub fn active_model(&self) -> &str {
        self.model(self.active_slot).expect("embedding_state_active_named")
    }
    pub fn other_model(&self) -> Option<&str> {
        self.model(self.active_slot.other())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase { Steady, Filling, Held, Ready, Flipped, Retiring, Blocked }

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
pub enum Blocked {
    /// The inactive slot holds a model that is none of current, previous and retire.
    OtherHoldsThird(String),
    Kek,
    Failed(i64),
    /// Complete and allowed to flip, but these acting keys of the current model are guessed.
    Guessed(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rollback { Instant, NeedsFill, Unavailable }

/// `EMBED_FLIP`: which units may flip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlipScope { All, None, Units(Vec<String>) }

impl FlipScope {
    pub fn allows(&self, unit: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Units(list) => list.iter().any(|u| u == unit),
        }
    }
}

/// The view the phase rules read. In env mode it comes from `.env` at boot; in command mode from
/// `embedding_control` on every pass. `current` and `previous` are embedder ids, as
/// `adapters::embedding::id_for` computes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    pub current: String,
    pub previous: Option<String>,
    pub retire: Option<String>,
    pub flip: FlipScope,
    pub rollback_days: i64,
    /// Acting keys whose value is guessed, per configured model id; empty vectors for the rest.
    pub guessed_acting: std::collections::BTreeMap<String, Vec<String>>,
    /// The control row generation this view came from. None in env mode.
    pub generation: Option<i64>,
}

/// `EMBED_MIGRATION_CONTROL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControlMode {
    /// `lumberroom-server embeddings` writes the intent to `embedding_control`.
    #[default]
    Command,
    /// `EMBED_PREVIOUS_*`, `EMBED_FLIP` and `EMBED_RETIRE` steer. The fork's mode.
    Env,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb { Start, Flip, Rollback, Retire }

impl Verb {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Flip => "flip",
            Self::Rollback => "rollback",
            Self::Retire => "retire",
        }
    }
}

/// The intent columns of `embedding_control`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Intent {
    pub generation: i64,
    pub target: Option<String>,
    pub flip: bool,
    pub retire: Option<String>,
    pub verb: Option<Verb>,
    pub requested_at: Option<DateTime<Utc>>,
}

/// What the server last published into `embedding_control`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Published {
    pub applied_generation: i64,
    pub server_models: Vec<String>,
    /// `services::embedding_status::published`'s JSON.
    pub status: Option<serde_json::Value>,
    pub seen_at: Option<DateTime<Utc>>,
}

/// A command's change. The repository adds one to `generation` and stamps `requested_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentChange {
    pub target: Option<String>,
    pub flip: bool,
    pub retire: Option<String>,
    pub verb: Verb,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    pub eligible: i64,
    /// Pending in the inactive slot for the target model.
    pub other_pending: i64,
    /// Eligible rows with no vector in the active slot.
    pub active_holes: i64,
    /// Rows whose active-slot vector carries an id other than the active model. `steady` reports
    /// them and leaves them, so a single-model store never re-embeds a row on its own. `flipped`
    /// and `retiring` re-embed them with the active model as part of a move the operator started.
    pub foreign_model: i64,
    pub failed: i64,
    pub without_vector: i64,
    pub retire_pending: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UnitStatus {
    pub unit: String,
    pub phase: Phase,
    pub active_slot: VectorSlot,
    pub active: String,
    pub other: Option<String>,
    #[serde(flatten)]
    pub counts: Counts,
    /// At most 20, so the console can name them.
    pub failed_ids: Vec<uuid::Uuid>,
    pub blocked: Option<Blocked>,
    pub flipped_at: Option<DateTime<Utc>>,
    /// Set while `retiring` and the rollback window has not passed.
    pub retire_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DiskStatus {
    pub free_bytes: u64,
    pub floor_bytes: u64,
    pub paused: bool,
}

/// One row the sweep has to embed. `chars` sizes the request it joins.
#[derive(Debug, Clone)]
pub struct PendingRow {
    pub id: uuid::Uuid,
    pub sensitivity: crate::domain::types::Sensitivity,
    /// None for a private row; the sweep opens it through `RowOpener`.
    pub content: Option<String>,
    pub chars: i64,
}

#[derive(Debug, Clone)]
pub struct FlipRequest {
    /// The state the pass read. The flip refuses as `Stale` when the row differs.
    pub expect: UnitState,
    pub target_slot: VectorSlot,
    pub target_model: String,
    /// Command mode: the control row generation the pass read. The flip refuses as `Stale` when
    /// `embedding_control.generation` differs. None in env mode, where nothing reads that table.
    pub expect_generation: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlipOutcome {
    Flipped { flip_ms: u64 },
    /// State changed since the pass read it. Nothing written.
    Stale,
    /// Rows still pending in the target slot. Nothing written.
    Incomplete(i64),
    /// The state row or the conflict key did not come within lock_timeout. Nothing written.
    LockTimeout,
}
