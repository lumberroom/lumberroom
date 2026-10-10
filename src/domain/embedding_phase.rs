//! The phase rules of an embedding-model migration, pure so every row of the spec's lifecycle table
//! is a unit test (decision 0027). T-B2 writes the bodies.

use chrono::{DateTime, Utc};

use crate::domain::embedding_migration::{
    Blocked, Configured, Counts, FlipScope, PendingRow, Phase, Rollback, UnitState, UnitStatus,
};

/// The model the inactive slot should hold: C when the unit is active on P, P when it is active
/// on C and P is set, None otherwise.
pub fn target(state: &UnitState, cfg: &Configured) -> Option<String> {
    unimplemented!("T-B2")
}
/// `blocked` carries the reasons only the service knows (KEK, failed rows).
pub fn phase(state: &UnitState, cfg: &Configured, counts: &Counts, blocked: Option<Blocked>)
    -> (Phase, Option<Blocked>) {
    unimplemented!("T-B2")
}
/// `flipped_at + rollback_days` when the inactive slot holds `cfg.retire`; the Unix epoch when it
/// does and the unit never flipped; else None.
pub fn retire_after(state: &UnitState, cfg: &Configured) -> Option<DateTime<Utc>> {
    unimplemented!("T-B2")
}
pub fn rollback(statuses: &[UnitStatus], cfg: &Configured) -> Rollback {
    unimplemented!("T-B2")
}
/// Err carries the operator-facing message: every failing unit with its active id and both fixes,
/// and, when a flip is allowed, every guessed acting key of `cfg.current`.
pub fn boot_check(states: &[UnitState], cfg: &Configured) -> Result<(), String> {
    unimplemented!("T-B2")
}
/// How long the fill sleeps after a request that took `took`.
pub fn fill_sleep(took: std::time::Duration, duty_percent: u8) -> std::time::Duration {
    unimplemented!("T-B2")
}
/// Splits pending rows into requests of at most `max_chars` characters. A row over `max_chars`
/// goes alone.
pub fn pack(rows: Vec<PendingRow>, max_chars: usize) -> Vec<Vec<PendingRow>> {
    unimplemented!("T-B2")
}
pub fn parse_flip_scope(raw: &str) -> Result<FlipScope, String> {
    unimplemented!("T-B2")
}
