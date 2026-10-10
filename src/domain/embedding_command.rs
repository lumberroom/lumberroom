//! What `lumberroom-server embeddings` decides, and how the control row becomes the view the phase
//! rules read. Pure: the command and the sweep supply everything (decision 0027).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::domain::embedding_migration::{
    Configured, DiskStatus, Intent, IntentChange, Published, UnitState, UnitStatus, Verb,
};

/// Everything a verb's decision rests on, read before the control row lock except `intent`.
pub struct View<'a> {
    pub sweep_on: bool,
    /// Embedder ids this process computes from its configuration: `EMBED_*` first, then
    /// `EMBED_PREVIOUS_*` when set.
    pub blocks: &'a [String],
    pub states: &'a [UnitState],
    /// From the server's last published status; empty when none.
    pub statuses: &'a [UnitStatus],
    pub published: &'a Published,
    /// Acting keys whose value is guessed, per block id.
    pub guessed_acting: &'a BTreeMap<String, Vec<String>>,
    pub rollback_days: i64,
    pub interval_secs: u64,
    pub now: DateTime<Utc>,
    /// Set when a disk floor is configured.
    pub disk: Option<DiskStatus>,
    /// The start target's probe: None when none ran (a local target, or another verb).
    pub probe: Option<Result<(), String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Write this change; print the message.
    Write { change: IntentChange, message: String },
    /// Nothing to write; print the message, exit 0.
    NoOp(String),
    /// Nothing written; print the reason, exit 1.
    Refuse(String),
}

/// The verb table in the spec, "The switch command". Messages name units, counts and dates, and
/// end with the next step.
pub fn decide(verb: Verb, intent: &Intent, view: &View) -> Decision { unimplemented!("T-B9") }

/// Command mode: the row table in the spec. `default_active` is the default unit's active model,
/// None when it has no state row (then `blocks[0]`). It names a previous model only when every
/// unit's inactive slot is NULL or holds it. Err names both fixes when the target is no configured
/// model.
pub fn configured_from_intent(
    intent: &Intent,
    blocks: &[String],
    default_active: Option<&str>,
    rollback_days: i64,
    guessed_acting: &BTreeMap<String, Vec<String>>,
) -> Result<Configured, String> { unimplemented!("T-B9") }

/// The configured model no unit is active on. None unless exactly two blocks are configured and
/// every unit is active on the other one.
pub fn start_target(blocks: &[String], states: &[UnitState]) -> Option<String> { unimplemented!("T-B9") }

/// Ok(None) when the last pass ran the same ids. Ok(Some(warning)) when no pass ran in the last
/// three intervals, so nothing can be compared. Err naming both id lists when a live server runs
/// other ids.
pub fn drift(blocks: &[String], published: &Published, interval_secs: u64, now: DateTime<Utc>)
    -> Result<Option<String>, String> { unimplemented!("T-B9") }
