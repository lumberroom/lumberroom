//! The phase rules of an embedding-model migration, pure so every row of the spec's lifecycle table
//! is a unit test (decision 0027).
//!
//! C is the view's current model, P its previous one, and "other" the unit's inactive slot. The
//! phase is derived on every pass and never stored.

use chrono::{DateTime, Utc};

use crate::domain::embedding_migration::{
    Blocked, Configured, Counts, FlipScope, PendingRow, Phase, Rollback, UnitState, UnitStatus,
};
use crate::domain::similarity::{ENGINE_FAMILIES, GUESS_FROM};

/// The model the inactive slot should hold: C when the unit is active on P, P when it is active
/// on C and P is set, None otherwise.
pub fn target(state: &UnitState, cfg: &Configured) -> Option<String> {
    let previous = cfg.previous.as_deref()?;
    let active = state.active_model();
    if active == previous {
        Some(cfg.current.clone())
    } else if active == cfg.current {
        Some(previous.to_string())
    } else {
        None
    }
}

/// `blocked` carries the reasons only the service knows (KEK, failed rows).
///
/// A third model in the other slot is tested before any other row (review M1): the `flipped` and
/// `steady` rows would otherwise claim the unit and fill over vectors whose rollback window may
/// still run. A `kek` or `failed` reason reports the unit blocked whatever phase lies beneath; the
/// service calls this once with `None` to learn that phase and keeps its fill running.
pub fn phase(
    state: &UnitState,
    cfg: &Configured,
    counts: &Counts,
    blocked: Option<Blocked>,
) -> (Phase, Option<Blocked>) {
    let current = cfg.current.as_str();
    let previous = cfg.previous.as_deref();
    let retire = cfg.retire.as_deref();
    let active = state.active_model();
    let other = state.other_model();

    if let Some(third) = other {
        if third != current && Some(third) != previous && Some(third) != retire {
            return (Phase::Blocked, Some(Blocked::OtherHoldsThird(third.to_string())));
        }
    }

    let beneath = if other.is_some() && other == retire {
        // The spec's row reads "active = C". A unit still on P with the retire model in its other
        // slot matches no row otherwise, and it cannot start its fill until that slot is cleared.
        Phase::Retiring
    } else if active == current {
        if previous.is_some() {
            Phase::Flipped
        } else {
            Phase::Steady
        }
    } else if Some(active) == previous {
        if other != Some(current) || counts.other_pending > 0 {
            Phase::Filling
        } else if !cfg.flip.allows(&state.unit) {
            Phase::Held
        } else {
            match cfg.guessed_acting.get(current).filter(|keys| !keys.is_empty()) {
                Some(keys) if blocked.is_none() => {
                    return (Phase::Blocked, Some(Blocked::Guessed(keys.clone())));
                }
                _ => Phase::Ready,
            }
        }
    } else {
        // Active on neither configured model. Boot refuses this; a unit seeded mid-run from rows
        // under a foreign id can still reach it, and the sweep must leave it alone.
        return (Phase::Blocked, None);
    };

    match blocked {
        Some(reason) => (Phase::Blocked, Some(reason)),
        None => (beneath, None),
    }
}

/// `flipped_at + rollback_days` when the inactive slot holds `cfg.retire`; the Unix epoch when it
/// does and the unit never flipped; else None.
pub fn retire_after(state: &UnitState, cfg: &Configured) -> Option<DateTime<Utc>> {
    let retire = cfg.retire.as_deref()?;
    if state.other_model() != Some(retire) {
        return None;
    }
    Some(match state.flipped_at {
        Some(at) => at + chrono::Duration::days(cfg.rollback_days),
        None => DateTime::<Utc>::UNIX_EPOCH,
    })
}

/// `instant` while every unit on C holds a complete P in its other slot, `needs_fill` once one of
/// them lacks it, `unavailable` before any unit reached C. A unit still on P needs no rollback, so
/// it counts toward neither.
pub fn rollback(statuses: &[UnitStatus], cfg: &Configured) -> Rollback {
    let Some(previous) = cfg.previous.as_deref() else {
        // P's block is gone. A unit that flipped once can only go back through a fill.
        return if statuses.iter().any(|s| s.flipped_at.is_some()) {
            Rollback::NeedsFill
        } else {
            Rollback::Unavailable
        };
    };
    let on_current: Vec<&UnitStatus> =
        statuses.iter().filter(|s| s.active == cfg.current).collect();
    if on_current.is_empty() {
        return Rollback::Unavailable;
    }
    let complete =
        |s: &&UnitStatus| s.other.as_deref() == Some(previous) && s.counts.other_pending == 0;
    if on_current.iter().all(complete) {
        Rollback::Instant
    } else {
        Rollback::NeedsFill
    }
}

/// Err carries the operator-facing message: every failing unit with its active id and both fixes,
/// and, when a flip is allowed, every guessed acting key of `cfg.current`.
pub fn boot_check(states: &[UnitState], cfg: &Configured) -> Result<(), String> {
    let command_mode = cfg.generation.is_some();
    let current = cfg.current.as_str();
    let previous = cfg.previous.as_deref();
    let mut refusals: Vec<String> = Vec::new();

    if previous == Some(current) {
        refusals.push(format!(
            "EMBED_PREVIOUS_* names the same model as EMBED_*: {current}. Remove the \
             EMBED_PREVIOUS_* block, or point it at the model the store holds now."
        ));
    }

    let stuck: Vec<String> = states
        .iter()
        .filter(|s| s.active_model() != current && Some(s.active_model()) != previous)
        .map(|s| format!("{} (active on {})", s.unit, s.active_model()))
        .collect();
    if !stuck.is_empty() {
        refusals.push(format!(
            "units active on a model no block configures: {}. Either set EMBED_* back to the \
             model each unit is active on, or name that model in EMBED_PREVIOUS_* so the server \
             moves the unit to {current}.",
            stuck.join(", ")
        ));
    }

    if let Some(retire) = cfg.retire.as_deref() {
        if retire == current || Some(retire) == previous {
            refusals.push(format!(
                "EMBED_RETIRE names {retire}, which EMBED_* or EMBED_PREVIOUS_* still configures. \
                 Retire only a model neither block names."
            ));
        }
        let serving: Vec<&str> =
            states.iter().filter(|s| s.active_model() == retire).map(|s| s.unit.as_str()).collect();
        if !serving.is_empty() {
            refusals.push(format!(
                "EMBED_RETIRE names {retire}, and units are active on it: {}. Flip them first.",
                serving.join(", ")
            ));
        }
    }

    let guessed =
        |model: &str| -> Vec<String> { cfg.guessed_acting.get(model).cloned().unwrap_or_default() };
    if let Some(previous) = previous {
        // A rollback flips onto P, so P's acting keys must be settled before any fill starts
        // (review M6).
        let keys = guessed(previous);
        if !keys.is_empty() {
            refusals.push(format!(
                "the previous model {previous} runs on guessed values for {}. Pin today's values \
                 with {}",
                keys.join(", "),
                pin_line("EMBED_PREVIOUS_THRESHOLDS", &keys)
            ));
        }
        let keys = guessed(current);
        if cfg.flip != FlipScope::None && !keys.is_empty() {
            let alternative = if command_mode {
                "or run `lumberroom-server embeddings rollback` to withdraw the flip"
            } else {
                "or set EMBED_FLIP=none to fill without flipping"
            };
            refusals.push(format!(
                "a flip onto {current} would act on guessed values for {}. Pin them with {}, {}.",
                keys.join(", "),
                pin_line("EMBED_THRESHOLDS", &keys),
                alternative
            ));
        }
    }

    if refusals.is_empty() {
        Ok(())
    } else {
        Err(refusals.join("\n"))
    }
}

/// The override line that pins each key at the value it runs on today: `GUESS_FROM`'s. A key only
/// the fork registers has no engine value to print, so its value is left for the operator.
fn pin_line(variable: &str, keys: &[String]) -> String {
    let today = |key: &str| -> String {
        ENGINE_FAMILIES
            .iter()
            .find(|f| f.family == GUESS_FROM)
            .and_then(|f| f.values.iter().find(|v| v.key == key))
            .map(|v| v.value.to_string())
            .unwrap_or_else(|| "<value>".to_string())
    };
    let pairs: Vec<String> = keys.iter().map(|k| format!("{k}={}", today(k))).collect();
    format!("{variable}={}", pairs.join(","))
}

/// How long the fill sleeps after a request that took `took`.
pub fn fill_sleep(took: std::time::Duration, duty_percent: u8) -> std::time::Duration {
    // Config holds the duty to 1..=100; a zero here would divide by zero, so it reads as 1.
    let duty = u32::from(duty_percent.clamp(1, 100));
    took * (100 - duty) / duty
}

/// Splits pending rows into requests of at most `max_chars` characters. A row over `max_chars`
/// goes alone.
pub fn pack(rows: Vec<PendingRow>, max_chars: usize) -> Vec<Vec<PendingRow>> {
    let mut batches: Vec<Vec<PendingRow>> = Vec::new();
    let mut open: Vec<PendingRow> = Vec::new();
    let mut held = 0usize;
    for row in rows {
        let chars = usize::try_from(row.chars).unwrap_or(0);
        if !open.is_empty() && held + chars > max_chars {
            batches.push(std::mem::take(&mut open));
            held = 0;
        }
        held += chars;
        open.push(row);
    }
    if !open.is_empty() {
        batches.push(open);
    }
    batches
}

/// `EMBED_FLIP`: `all`, `none`, or a comma list of unit ids. Duplicates collapse.
pub fn parse_flip_scope(raw: &str) -> Result<FlipScope, String> {
    match raw.trim() {
        "all" => return Ok(FlipScope::All),
        "none" => return Ok(FlipScope::None),
        "" => return Err("expected all, none, or a comma list of unit ids, got nothing".into()),
        _ => {}
    }
    let mut units: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let unit = part.trim();
        if unit.is_empty() {
            return Err(format!("{raw:?} holds an empty unit id"));
        }
        if !units.iter().any(|u| u == unit) {
            units.push(unit.to_string());
        }
    }
    Ok(FlipScope::Units(units))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use chrono::TimeZone;

    use super::*;
    use crate::domain::embedding_slot::VectorSlot;
    use crate::domain::types::Sensitivity;

    const C: &str = "openai:google/embeddinggemma-2";
    const P: &str = "Xenova/bge-base-en-v1.5@q8";
    const R: &str = "openai:old-model";
    const X: &str = "openai:third-model";

    fn state(active: VectorSlot, a: Option<&str>, b: Option<&str>) -> UnitState {
        UnitState {
            unit: "me".into(),
            active_slot: active,
            model_a: a.map(str::to_string),
            model_b: b.map(str::to_string),
            flipped_at: None,
        }
    }

    fn cfg(previous: Option<&str>) -> Configured {
        Configured {
            current: C.into(),
            previous: previous.map(str::to_string),
            retire: None,
            flip: FlipScope::All,
            rollback_days: 7,
            guessed_acting: BTreeMap::new(),
            generation: None,
        }
    }

    fn pending(other_pending: i64) -> Counts {
        Counts { eligible: 10, other_pending, ..Counts::default() }
    }

    fn guessed(model: &str, keys: &[&str]) -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([(model.to_string(), keys.iter().map(|k| k.to_string()).collect())])
    }

    // One test per row of the spec's lifecycle table.

    #[test]
    fn a_unit_on_the_current_model_with_no_previous_is_steady() {
        let s = state(VectorSlot::A, Some(C), None);
        assert_eq!(phase(&s, &cfg(None), &pending(0), None), (Phase::Steady, None));
    }

    #[test]
    fn a_unit_on_the_previous_model_with_rows_pending_is_filling() {
        let named = state(VectorSlot::A, Some(P), Some(C));
        assert_eq!(phase(&named, &cfg(Some(P)), &pending(5), None), (Phase::Filling, None));
        let unnamed = state(VectorSlot::A, Some(P), None);
        assert_eq!(phase(&unnamed, &cfg(Some(P)), &pending(5), None), (Phase::Filling, None));
    }

    #[test]
    fn a_complete_unit_outside_the_flip_list_is_held() {
        let s = state(VectorSlot::A, Some(P), Some(C));
        let c = Configured { flip: FlipScope::Units(vec!["other".into()]), ..cfg(Some(P)) };
        assert_eq!(phase(&s, &c, &pending(0), None), (Phase::Held, None));
    }

    #[test]
    fn a_complete_allowed_unit_with_no_guessed_key_is_ready() {
        let s = state(VectorSlot::A, Some(P), Some(C));
        assert_eq!(phase(&s, &cfg(Some(P)), &pending(0), None), (Phase::Ready, None));
    }

    #[test]
    fn a_unit_on_the_current_model_with_a_previous_set_is_flipped() {
        let named = state(VectorSlot::B, Some(P), Some(C));
        assert_eq!(phase(&named, &cfg(Some(P)), &pending(3), None), (Phase::Flipped, None));
        let unnamed = state(VectorSlot::B, None, Some(C));
        assert_eq!(phase(&unnamed, &cfg(Some(P)), &pending(3), None), (Phase::Flipped, None));
    }

    #[test]
    fn a_unit_whose_other_slot_holds_the_retire_model_is_retiring() {
        let s = state(VectorSlot::B, Some(R), Some(C));
        let c = Configured { retire: Some(R.into()), ..cfg(None) };
        assert_eq!(phase(&s, &c, &pending(0), None), (Phase::Retiring, None));
    }

    #[test]
    fn a_third_model_in_the_other_slot_blocks() {
        let s = state(VectorSlot::A, Some(C), Some(X));
        assert_eq!(
            phase(&s, &cfg(None), &pending(0), None),
            (Phase::Blocked, Some(Blocked::OtherHoldsThird(X.into())))
        );
    }

    #[test]
    fn an_unverified_kek_blocks_a_filling_unit() {
        let s = state(VectorSlot::A, Some(P), Some(C));
        assert_eq!(
            phase(&s, &cfg(Some(P)), &pending(2), Some(Blocked::Kek)),
            (Phase::Blocked, Some(Blocked::Kek))
        );
    }

    #[test]
    fn failed_rows_block_the_unit() {
        let s = state(VectorSlot::A, Some(P), Some(C));
        assert_eq!(
            phase(&s, &cfg(Some(P)), &pending(2), Some(Blocked::Failed(2))),
            (Phase::Blocked, Some(Blocked::Failed(2)))
        );
    }

    #[test]
    fn ready_with_guessed_acting_keys_reports_blocked_guessed() {
        let s = state(VectorSlot::A, Some(P), Some(C));
        let c = Configured { guessed_acting: guessed(C, &["dedupe", "conflict"]), ..cfg(Some(P)) };
        assert_eq!(
            phase(&s, &c, &pending(0), None),
            (Phase::Blocked, Some(Blocked::Guessed(vec!["dedupe".into(), "conflict".into()])))
        );
    }

    #[test]
    fn guessed_keys_do_not_block_a_unit_still_filling() {
        let s = state(VectorSlot::A, Some(P), Some(C));
        let c = Configured { guessed_acting: guessed(C, &["dedupe"]), ..cfg(Some(P)) };
        assert_eq!(phase(&s, &c, &pending(4), None), (Phase::Filling, None));
    }

    // Review M1: the flipped row also matches a unit whose other slot holds a third model.
    #[test]
    fn other_holds_third_wins_over_every_other_row() {
        let s = state(VectorSlot::B, Some(X), Some(C));
        assert_eq!(
            phase(&s, &cfg(Some(P)), &pending(0), Some(Blocked::Failed(1))),
            (Phase::Blocked, Some(Blocked::OtherHoldsThird(X.into())))
        );
    }

    #[test]
    fn a_flipped_unit_left_without_its_previous_block_blocks_on_the_old_model() {
        let s = state(VectorSlot::B, Some(P), Some(C));
        assert_eq!(
            phase(&s, &cfg(None), &pending(0), None),
            (Phase::Blocked, Some(Blocked::OtherHoldsThird(P.into())))
        );
    }

    #[test]
    fn a_unit_on_no_configured_model_blocks_with_no_reason() {
        let s = state(VectorSlot::A, Some(X), None);
        assert_eq!(phase(&s, &cfg(Some(P)), &pending(0), None), (Phase::Blocked, None));
    }

    #[test]
    fn the_target_follows_the_active_model() {
        assert_eq!(target(&state(VectorSlot::A, Some(P), None), &cfg(Some(P))), Some(C.into()));
        assert_eq!(target(&state(VectorSlot::B, Some(P), Some(C)), &cfg(Some(P))), Some(P.into()));
        assert_eq!(target(&state(VectorSlot::A, Some(C), None), &cfg(None)), None);
        assert_eq!(target(&state(VectorSlot::A, Some(X), None), &cfg(Some(P))), None);
    }

    #[test]
    fn retire_waits_for_the_rollback_window() {
        let flipped = Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap();
        let s = UnitState { flipped_at: Some(flipped), ..state(VectorSlot::B, Some(R), Some(C)) };
        let c = Configured { retire: Some(R.into()), ..cfg(None) };
        assert_eq!(retire_after(&s, &c), Some(flipped + chrono::Duration::days(7)));
    }

    #[test]
    fn a_never_flipped_unit_retires_without_waiting() {
        let s = state(VectorSlot::A, Some(C), Some(R));
        let c = Configured { retire: Some(R.into()), ..cfg(None) };
        assert_eq!(retire_after(&s, &c), Some(DateTime::<Utc>::UNIX_EPOCH));
    }

    #[test]
    fn retire_after_is_none_unless_the_other_slot_holds_the_retire_model() {
        let s = state(VectorSlot::A, Some(C), Some(X));
        let c = Configured { retire: Some(R.into()), ..cfg(None) };
        assert_eq!(retire_after(&s, &c), None);
        assert_eq!(retire_after(&state(VectorSlot::A, Some(C), Some(R)), &cfg(None)), None);
    }

    fn status(
        active_slot: VectorSlot,
        active: &str,
        other: Option<&str>,
        pending: i64,
    ) -> UnitStatus {
        UnitStatus {
            unit: "me".into(),
            phase: Phase::Flipped,
            active_slot,
            active: active.into(),
            other: other.map(str::to_string),
            counts: Counts { other_pending: pending, ..Counts::default() },
            failed_ids: vec![],
            blocked: None,
            flipped_at: Some(Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap()),
            retire_after: None,
        }
    }

    #[test]
    fn rollback_is_instant_only_when_every_other_slot_is_full() {
        let full = status(VectorSlot::B, C, Some(P), 0);
        let behind = UnitStatus { unit: "t2".into(), ..status(VectorSlot::B, C, Some(P), 4) };
        assert_eq!(rollback(std::slice::from_ref(&full), &cfg(Some(P))), Rollback::Instant);
        assert_eq!(rollback(&[full.clone(), behind], &cfg(Some(P))), Rollback::NeedsFill);
        let unnamed = UnitStatus { unit: "t3".into(), ..status(VectorSlot::B, C, None, 0) };
        assert_eq!(rollback(&[full, unnamed], &cfg(Some(P))), Rollback::NeedsFill);
    }

    #[test]
    fn rollback_is_unavailable_before_any_flip() {
        let filling = UnitStatus { flipped_at: None, ..status(VectorSlot::A, P, Some(C), 9) };
        assert_eq!(rollback(&[filling], &cfg(Some(P))), Rollback::Unavailable);
        assert_eq!(rollback(&[], &cfg(None)), Rollback::Unavailable);
    }

    #[test]
    fn removing_the_previous_block_after_a_flip_needs_a_fill() {
        let s = status(VectorSlot::B, C, Some(P), 0);
        assert_eq!(rollback(&[s], &cfg(None)), Rollback::NeedsFill);
    }

    #[test]
    fn boot_passes_a_fresh_store() {
        assert_eq!(boot_check(&[], &cfg(None)), Ok(()));
        assert_eq!(boot_check(&[], &cfg(Some(P))), Ok(()));
    }

    #[test]
    fn boot_refuses_a_unit_active_on_an_unconfigured_model() {
        let s = state(VectorSlot::A, Some(X), None);
        let err = boot_check(&[s], &cfg(Some(P))).unwrap_err();
        assert!(err.contains("me") && err.contains(X), "{err}");
        assert!(err.contains("EMBED_PREVIOUS_"), "names the second fix: {err}");
        assert!(err.contains("set EMBED_"), "names the first fix: {err}");
    }

    #[test]
    fn boot_names_every_stuck_unit() {
        let one = UnitState { unit: "t1".into(), ..state(VectorSlot::A, Some(X), None) };
        let two = UnitState { unit: "t2".into(), ..state(VectorSlot::B, Some(C), Some(R)) };
        let fine = UnitState { unit: "t3".into(), ..state(VectorSlot::A, Some(C), None) };
        let err = boot_check(&[one, fine, two], &cfg(None)).unwrap_err();
        assert!(err.contains("t1") && err.contains(X), "{err}");
        assert!(err.contains("t2") && err.contains(R), "{err}");
        assert!(!err.contains("t3"), "{err}");
    }

    #[test]
    fn boot_refuses_previous_equal_to_current() {
        let err = boot_check(&[], &cfg(Some(C))).unwrap_err();
        assert!(err.contains(C), "{err}");
    }

    #[test]
    fn boot_refuses_retiring_the_previous_model() {
        let c = Configured { retire: Some(P.into()), ..cfg(Some(P)) };
        let err = boot_check(&[], &c).unwrap_err();
        assert!(err.contains("EMBED_RETIRE") && err.contains(P), "{err}");
        let c = Configured { retire: Some(C.into()), ..cfg(Some(P)) };
        assert!(boot_check(&[], &c).is_err());
    }

    #[test]
    fn boot_refuses_retiring_a_model_a_unit_is_active_on() {
        let s = state(VectorSlot::A, Some(C), Some(R));
        let ok = Configured { retire: Some(R.into()), ..cfg(None) };
        assert_eq!(boot_check(std::slice::from_ref(&s), &ok), Ok(()));
        let on_r = UnitState { unit: "t2".into(), ..state(VectorSlot::B, Some(C), Some(R)) };
        let err = boot_check(&[s, on_r], &ok).unwrap_err();
        assert!(err.contains("t2"), "{err}");
    }

    #[test]
    fn boot_refuses_a_flip_onto_guessed_acting_keys() {
        let c = Configured { guessed_acting: guessed(C, &["dedupe", "conflict"]), ..cfg(Some(P)) };
        let err = boot_check(&[], &c).unwrap_err();
        assert!(err.contains("EMBED_THRESHOLDS=dedupe=0.97,conflict=0.9"), "{err}");
        assert!(err.contains("EMBED_FLIP=none"), "{err}");
    }

    #[test]
    fn boot_allows_a_fill_onto_guessed_keys_under_flip_none() {
        let c = Configured {
            guessed_acting: guessed(C, &["dedupe"]),
            flip: FlipScope::None,
            ..cfg(Some(P))
        };
        assert_eq!(boot_check(&[state(VectorSlot::A, Some(P), Some(C))], &c), Ok(()));
    }

    #[test]
    fn boot_refuses_a_previous_model_with_guessed_acting_keys() {
        let c = Configured {
            guessed_acting: guessed(P, &["cleanup_near_certain"]),
            flip: FlipScope::None,
            ..cfg(Some(P))
        };
        let err = boot_check(&[], &c).unwrap_err();
        assert!(err.contains("EMBED_PREVIOUS_THRESHOLDS=cleanup_near_certain=0.97"), "{err}");
    }

    #[test]
    fn boot_passes_a_unit_active_on_an_unlisted_model() {
        // A model with no family entry runs on guessed values; main.rs has already warned.
        let c =
            Configured { current: X.into(), guessed_acting: guessed(X, &["dedupe"]), ..cfg(None) };
        assert_eq!(boot_check(&[state(VectorSlot::A, Some(X), None)], &c), Ok(()));
    }

    #[test]
    fn fill_sleep_at_half_duty_equals_the_request_time() {
        assert_eq!(fill_sleep(Duration::from_millis(1800), 50), Duration::from_millis(1800));
    }

    #[test]
    fn fill_sleep_scales_with_the_duty() {
        assert_eq!(fill_sleep(Duration::from_millis(1000), 100), Duration::ZERO);
        assert_eq!(fill_sleep(Duration::from_millis(1000), 25), Duration::from_millis(3000));
        assert_eq!(fill_sleep(Duration::from_millis(1000), 0), Duration::from_millis(99_000));
    }

    fn row(chars: i64) -> PendingRow {
        PendingRow {
            id: uuid::Uuid::new_v4(),
            sensitivity: Sensitivity::Open,
            content: Some("x".repeat(chars as usize)),
            chars,
        }
    }

    fn sizes(batches: &[Vec<PendingRow>]) -> Vec<Vec<i64>> {
        batches.iter().map(|b| b.iter().map(|r| r.chars).collect()).collect()
    }

    #[test]
    fn pack_caps_characters() {
        let batches = pack(vec![row(1500), row(1500), row(1500), row(400), row(4000)], 4000);
        assert_eq!(sizes(&batches), vec![vec![1500, 1500], vec![1500, 400], vec![4000]]);
    }

    #[test]
    fn pack_sends_a_long_row_alone() {
        let batches = pack(vec![row(100), row(9000), row(100)], 4000);
        assert_eq!(sizes(&batches), vec![vec![100], vec![9000], vec![100]]);
    }

    #[test]
    fn pack_keeps_id_order_and_returns_nothing_for_nothing() {
        let rows = vec![row(10), row(10), row(10)];
        let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
        let packed: Vec<_> = pack(rows, 4000).into_iter().flatten().map(|r| r.id).collect();
        assert_eq!(packed, ids);
        assert!(pack(vec![], 4000).is_empty());
    }

    #[test]
    fn flip_scope_parses_all_none_and_lists() {
        assert_eq!(parse_flip_scope("all"), Ok(FlipScope::All));
        assert_eq!(parse_flip_scope(" none "), Ok(FlipScope::None));
        assert_eq!(
            parse_flip_scope("me, acme ,me"),
            Ok(FlipScope::Units(vec!["me".into(), "acme".into()]))
        );
    }

    #[test]
    fn flip_scope_refuses_an_empty_entry() {
        assert!(parse_flip_scope("").is_err());
        assert!(parse_flip_scope("me,,acme").is_err());
    }
}
