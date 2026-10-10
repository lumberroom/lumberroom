//! What `lumberroom-server embeddings` decides, and how the control row becomes the view the phase
//! rules read. Pure: the command and the sweep supply everything (decision 0027).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::domain::embedding_migration::{
    Configured, DiskStatus, FlipScope, Intent, IntentChange, Published, UnitState, UnitStatus, Verb,
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
pub fn decide(verb: Verb, intent: &Intent, view: &View) -> Decision {
    if !view.sweep_on {
        return Decision::Refuse(format!(
            "{} refused: the sweep is off (EMBED_MIGRATE_SECS=0), so no pass would act on it. Set \
             EMBED_MIGRATE_SECS above 0, recreate the server (docker compose up -d server) and run \
             it again.",
            verb.as_str()
        ));
    }
    match verb {
        Verb::Start => decide_start(intent, view),
        Verb::Flip => decide_flip(intent, view),
        Verb::Rollback => decide_rollback(intent, view),
        Verb::Retire => decide_retire(intent, view),
    }
}

/// Command mode: the row table in the spec. `default_active` is the default unit's active model,
/// None when it has no state row (then `blocks[0]`). It names a previous model only when every
/// unit's inactive slot is NULL or holds it. Err names both fixes when the target is no configured
/// model.
///
/// The signature carries no unit states, so this function cannot test the "every inactive slot"
/// clause itself. `start` refuses an inactive slot holding a third model before it writes a
/// target, and the phase rules block such a unit as `other_holds_third` before any other row.
pub fn configured_from_intent(
    intent: &Intent,
    blocks: &[String],
    default_active: Option<&str>,
    rollback_days: i64,
    guessed_acting: &BTreeMap<String, Vec<String>>,
) -> Result<Configured, String> {
    let view = |current: String, previous: Option<String>, flip: FlipScope| Configured {
        current,
        previous,
        retire: intent.retire.clone(),
        flip,
        rollback_days,
        guessed_acting: guessed_acting.clone(),
        generation: Some(intent.generation),
    };
    let Some(target) = &intent.target else {
        let active = default_active
            .map(str::to_string)
            .or_else(|| blocks.first().cloned())
            .ok_or_else(|| "no embedding block is configured; set EMBED_PROVIDER".to_string())?;
        return Ok(view(active, None, FlipScope::None));
    };
    if !blocks.contains(target) {
        return Err(format!(
            "embedding_control targets {target}, which no configured block names (this server \
             runs {}). Either restore {target}'s block in .env and recreate the server (docker \
             compose up -d server), or move the target back with docker compose run --rm -T \
             server lumberroom-server embeddings rollback.",
            list(blocks)
        ));
    }
    if intent.retire.is_some() {
        return Ok(view(target.clone(), None, FlipScope::All));
    }
    let previous = blocks.iter().find(|b| *b != target).cloned();
    let flip = if intent.flip { FlipScope::All } else { FlipScope::None };
    Ok(view(target.clone(), previous, flip))
}

/// The configured model no unit is active on. None unless exactly two blocks are configured and
/// every unit is active on the other one.
pub fn start_target(blocks: &[String], states: &[UnitState]) -> Option<String> {
    let [first, second] = blocks else { return None };
    if first == second {
        return None;
    }
    let active = common_active(states).ok()?;
    if active == first {
        Some(second.clone())
    } else if active == second {
        Some(first.clone())
    } else {
        None
    }
}

/// Ok(None) when the last pass ran the same ids. Ok(Some(warning)) when no pass ran in the last
/// three intervals, so nothing can be compared. Err naming both id lists when a live server runs
/// other ids.
pub fn drift(
    blocks: &[String],
    published: &Published,
    interval_secs: u64,
    now: DateTime<Utc>,
) -> Result<Option<String>, String> {
    let window = interval_secs.saturating_mul(3);
    let window_i64 = i64::try_from(window).unwrap_or(i64::MAX);
    let recent = published.seen_at.filter(|seen| (now - *seen).num_seconds() <= window_i64);
    let Some(seen) = recent else {
        let last = published.seen_at.map_or_else(|| "never".to_string(), when);
        return Ok(Some(format!(
            "warning: no embedding pass ran in the last {window} s (three intervals; last pass: \
             {last}), so this command cannot check that the server runs the models it computes. \
             A server applies the intent on its next pass."
        )));
    };
    if published.server_models.as_slice() == blocks {
        return Ok(None);
    }
    Err(format!(
        "refused: this command computes the embedding models {} from its environment, and the \
         server's pass at {} ran {}. .env changed and the server did not: recreate it (docker \
         compose up -d server), wait one pass, and run the command again. Under docker compose \
         exec the command sees the environment the container booted with, so it shows an .env \
         edit only after the recreate.",
        list(blocks),
        when(seen),
        list(&published.server_models)
    ))
}

const RECREATE: &str = "If you added the block to .env, recreate first (docker compose up -d \
                        server); exec sees the environment the container booted with.";

const STATUS_THEN_FLIP: &str =
    "Next: embeddings status until every unit is held, then embeddings flip.";

fn when(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        format!("[{}]", items.join(", "))
    }
}

fn names<'s>(states: impl IntoIterator<Item = &'s UnitState>) -> String {
    states.into_iter().map(|s| s.unit.as_str()).collect::<Vec<_>>().join(", ")
}

/// The model every unit is active on. Err lists each unit with its model when they differ, and
/// says so when there is no unit.
fn common_active(states: &[UnitState]) -> Result<&str, String> {
    let Some(first) = states.first() else {
        return Err("no unit holds vectors yet".to_string());
    };
    let model = first.active_model();
    if states.iter().all(|s| s.active_model() == model) {
        return Ok(model);
    }
    let mut by_model: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for s in states {
        by_model.entry(s.active_model()).or_default().push(&s.unit);
    }
    Err(by_model
        .iter()
        .map(|(m, units)| format!("{} on {m}", units.join(", ")))
        .collect::<Vec<_>>()
        .join("; "))
}

/// `EMBED_THRESHOLDS` for the first block, `EMBED_PREVIOUS_THRESHOLDS` for the second.
fn threshold_variable(blocks: &[String], model: &str) -> &'static str {
    match blocks.iter().position(|b| b == model) {
        Some(1) => "EMBED_PREVIOUS_THRESHOLDS",
        _ => "EMBED_THRESHOLDS",
    }
}

/// The override line that pins today's values. A guessed key runs on `GUESS_FROM`'s table value,
/// so the line changes no behaviour; it records the choice.
fn pin_line(variable: &str, keys: &[String]) -> String {
    use crate::domain::similarity::{ENGINE_FAMILIES, GUESS_FROM};
    let value = |key: &str| {
        ENGINE_FAMILIES
            .iter()
            .find(|f| f.family == GUESS_FROM)
            .and_then(|f| f.values.iter().find(|v| v.key == key))
            .map_or_else(|| "<value>".to_string(), |v| v.value.to_string())
    };
    let pairs: Vec<String> = keys.iter().map(|k| format!("{k}={}", value(k))).collect();
    format!("{variable}={}", pairs.join(","))
}

fn guessed<'v>(view: &'v View, model: &str) -> &'v [String] {
    view.guessed_acting.get(model).map_or(&[], Vec::as_slice)
}

fn status_of<'v>(view: &'v View, unit: &str) -> Option<&'v UnitStatus> {
    view.statuses.iter().find(|s| s.unit == unit)
}

fn decide_start(intent: &Intent, view: &View) -> Decision {
    let blocks = view.blocks;
    if blocks.len() != 2 {
        return Decision::Refuse(format!(
            "start refused: one embedding block is configured ({}), and a switch needs two. Add \
             the new model as EMBED_* and move the current lines to EMBED_PREVIOUS_*. {RECREATE}",
            list(blocks)
        ));
    }
    if blocks[0] == blocks[1] {
        return Decision::Refuse(format!(
            "start refused: EMBED_* and EMBED_PREVIOUS_* both compute {}. Point EMBED_* at the new \
             model. {RECREATE}",
            blocks[0]
        ));
    }
    let active = match common_active(view.states) {
        Ok(a) => a,
        Err(_) if view.states.is_empty() => {
            return Decision::Refuse(format!(
                "start refused: no unit holds vectors yet, so nothing needs moving. Writes already \
                 embed with {}; remove the EMBED_PREVIOUS_* block and recreate the server (docker \
                 compose up -d server).",
                blocks[0]
            ))
        }
        Err(spread) => {
            return Decision::Refuse(format!(
                "start refused: units are active on more than one model ({spread}). Finish or \
                 roll back the move in progress first; embeddings status shows it."
            ))
        }
    };
    let Some(target) = start_target(blocks, view.states) else {
        return Decision::Refuse(format!(
            "start refused: units {} are active on {active}, which neither block configures \
             (this command computes {}). Name {active} as EMBED_PREVIOUS_* and the new model as \
             EMBED_*. {RECREATE}",
            names(view.states),
            list(blocks)
        ));
    };
    if intent.target.as_deref() == Some(target.as_str()) && intent.retire.is_none() {
        return Decision::NoOp(format!(
            "start: the target is already {target} (generation {}); nothing written. \
             {STATUS_THEN_FLIP}",
            intent.generation
        ));
    }
    let leaving = guessed(view, active);
    if !leaving.is_empty() {
        return Decision::Refuse(format!(
            "start refused: units {} would leave {active}, whose acting keys {} resolve as \
             guessed, and a rollback would land on them. Pin today's values with this line in \
             .env, recreate the server (docker compose up -d server) and run start again:\n  {}",
            names(view.states),
            leaving.join(", "),
            pin_line(threshold_variable(blocks, active), leaving)
        ));
    }
    let third: Vec<&UnitState> =
        view.states.iter().filter(|s| s.other_model().is_some_and(|m| m != target)).collect();
    if let Some(first) = third.first() {
        let held = first.other_model().unwrap_or_default();
        let detail = third
            .iter()
            .map(|s| format!("{} holds {}", s.unit, s.other_model().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("; ");
        if intent.retire.as_deref() == Some(held) {
            return Decision::Refuse(format!(
                "start refused: a retire of {held} is deleting its vectors ({detail}). start runs \
                 once that retire finishes; embeddings status shows the unit steady then."
            ));
        }
        return Decision::Refuse(format!(
            "start refused: an inactive slot holds a model other than {target} ({detail}). Run \
             embeddings retire to clear it, then start."
        ));
    }
    if let Some(d) = view.disk.filter(|d| d.free_bytes <= d.floor_bytes) {
        return Decision::Refuse(format!(
            "start refused: {} bytes free on the database disk and the floor is {} bytes \
             (EMBED_DISK_FLOOR_MB). No command overrides the floor. Free space, or lower the \
             floor and recreate the server, then run start again.",
            d.free_bytes, d.floor_bytes
        ));
    }
    if let Some(Err(e)) = &view.probe {
        return Decision::Refuse(format!(
            "start refused: the probe embedding to {target} failed: {e}. Its block's \
             EMBED_BASE_URL must answer from inside the server container, where 127.0.0.1 is the \
             container itself. Fix it, recreate the server (docker compose up -d server) and run \
             start again."
        ));
    }
    let change =
        IntentChange { target: Some(target.clone()), flip: false, retire: None, verb: Verb::Start };
    let message = if intent.retire.as_deref() == Some(target.as_str()) {
        format!(
            "start written: the cancel of {target} stops and the fill resumes from the vectors \
             still there, for units {}. {STATUS_THEN_FLIP}",
            names(view.states)
        )
    } else {
        let rows: i64 = view.statuses.iter().map(|s| s.counts.eligible).sum();
        let rows = if view.statuses.is_empty() {
            String::new()
        } else {
            format!(" ({rows} eligible rows)")
        };
        format!(
            "start written: units {} move from {active} to {target}. The server writes both \
             vectors from its next pass and fills {target} into each inactive slot{rows}. \
             {STATUS_THEN_FLIP}",
            names(view.states)
        )
    };
    Decision::Write { change, message }
}

fn decide_flip(intent: &Intent, view: &View) -> Decision {
    let Some(target) = intent.target.as_deref() else {
        return Decision::Refuse(
            "flip refused: no target is set, so there is no model to flip onto. Run embeddings \
             start first."
                .to_string(),
        );
    };
    if !view.blocks.iter().any(|b| b == target) {
        return Decision::Refuse(format!(
            "flip refused: the target {target} is in no configured block (this command computes \
             {}). Restore its block in .env and recreate the server (docker compose up -d \
             server), or run embeddings rollback.",
            list(view.blocks)
        ));
    }
    let waiting: Vec<&UnitState> =
        view.states.iter().filter(|s| s.active_model() != target).collect();
    if waiting.is_empty() {
        return Decision::NoOp(format!(
            "flip: every unit ({}) is active on {target} already; nothing written. Next: \
             embeddings retire once the rollback window closes.",
            names(view.states)
        ));
    }
    if intent.flip {
        return Decision::NoOp(format!(
            "flip: already requested at generation {}; units {} flip onto {target} once their \
             fill completes. Next: embeddings status.",
            intent.generation,
            names(waiting.iter().copied())
        ));
    }
    let keys = guessed(view, target);
    if !keys.is_empty() {
        return Decision::Refuse(format!(
            "flip refused: {target}'s acting keys {} resolve as guessed, and a flip would let \
             unattended merges run on them. Pin the values with this line in .env, recreate the \
             server (docker compose up -d server) and run flip again:\n  {}",
            keys.join(", "),
            pin_line(threshold_variable(view.blocks, target), keys)
        ));
    }
    let change = IntentChange {
        target: Some(target.to_string()),
        flip: true,
        retire: intent.retire.clone(),
        verb: Verb::Flip,
    };
    let filling: Vec<(&str, i64)> = waiting
        .iter()
        .filter_map(|s| status_of(view, &s.unit))
        .filter(|st| st.counts.other_pending > 0)
        .map(|st| (st.unit.as_str(), st.counts.other_pending))
        .collect();
    let message = if filling.is_empty() {
        format!(
            "flip written: units {} flip onto {target} on the next pass. Next: embeddings status; \
             if retrieval is worse, embeddings rollback.",
            names(waiting.iter().copied())
        )
    } else {
        let detail = filling
            .iter()
            .map(|(u, n)| format!("{u} has {n} rows pending"))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "flip written: the fill into {target} is not complete ({detail}), so the flip waits \
             and each unit flips once its slot is full. Next: embeddings status until every unit \
             is flipped."
        )
    };
    Decision::Write { change, message }
}

fn decide_rollback(intent: &Intent, view: &View) -> Decision {
    let Some(target) = intent.target.as_deref() else {
        return Decision::NoOp(match &intent.retire {
            Some(r) => format!(
                "rollback: nothing is started; a cancel of {r} is deleting its partial vectors \
                 (generation {}). Nothing written. Next: embeddings status.",
                intent.generation
            ),
            None => "rollback: nothing was started; nothing written.".to_string(),
        });
    };
    let on_target: Vec<&UnitState> =
        view.states.iter().filter(|s| s.active_model() == target).collect();

    if on_target.is_empty() {
        // A target set by an earlier rollback that no pass has applied yet. Read as "before any
        // flip", this would cancel the old model and delete the vectors the rollback restores.
        if intent.verb == Some(Verb::Rollback) && intent.flip {
            return Decision::NoOp(format!(
                "rollback: a rollback onto {target} is already requested at generation {}; units \
                 {} flip back on the next pass. Nothing written. Next: embeddings status.",
                intent.generation,
                names(view.states)
            ));
        }
        let filled: i64 =
            view.statuses.iter().map(|s| (s.counts.eligible - s.counts.other_pending).max(0)).sum();
        let filled = if view.statuses.is_empty() {
            String::new()
        } else {
            format!(" ({filled} rows hold one)")
        };
        return Decision::Write {
            change: IntentChange {
                target: None,
                flip: false,
                retire: Some(target.to_string()),
                verb: Verb::Rollback,
            },
            message: format!(
                "rollback written: no unit flipped onto {target}, so this cancels the start. The \
                 server deletes {target}'s partial vectors{filled} and units {} stay where they \
                 are. Next: embeddings status until every unit is steady.",
                names(view.states)
            ),
        };
    }

    let mut held: Vec<&str> = on_target.iter().filter_map(|s| s.other_model()).collect();
    held.sort_unstable();
    held.dedup();
    let old = match held.as_slice() {
        [one] => one.to_string(),
        [] => match view.blocks.iter().find(|b| *b != target) {
            Some(b) => b.clone(),
            None => {
                return Decision::Refuse(format!(
                    "rollback refused: units {} are active on {target}, retire has cleared their \
                     other slot, and no EMBED_PREVIOUS_* block names a model to go back to. \
                     Restore the old model's block in .env as EMBED_PREVIOUS_*, recreate the \
                     server (docker compose up -d server) and run rollback again.",
                    names(on_target.iter().copied())
                ))
            }
        },
        many => {
            return Decision::Refuse(format!(
                "rollback refused: units on {target} hold different old models ({}). Retire the \
                 one you do not want back first.",
                many.join(", ")
            ))
        }
    };
    if !view.blocks.contains(&old) {
        return Decision::Refuse(format!(
            "rollback refused: units {} are active on {target} and their old model {old} is gone \
             from .env (this command computes {}). Restore {old}'s block as EMBED_PREVIOUS_*, \
             recreate the server (docker compose up -d server) and run rollback again.",
            names(on_target.iter().copied()),
            list(view.blocks)
        ));
    }

    let retire_touched = intent.retire.as_deref() == Some(old.as_str())
        || on_target.iter().any(|s| s.other_model().is_none());
    let refill: i64 = on_target
        .iter()
        .filter_map(|s| status_of(view, &s.unit).map(|st| (s, st)))
        .map(|(s, st)| {
            if s.other_model().is_none() {
                st.counts.eligible
            } else if retire_touched {
                (st.counts.eligible - st.counts.retire_pending).max(0)
            } else {
                st.counts.other_pending
            }
        })
        .sum();
    let units = names(on_target.iter().copied());
    let message = if retire_touched {
        format!(
            "rollback written: retire has deleted {old}'s vectors from {refill} rows of units \
             {units}; the server refills them before the units flip back onto {old}. Next: \
             embeddings status until every unit is flipped. To move forward again, run \
             embeddings start and then embeddings flip."
        )
    } else if refill > 0 {
        format!(
            "rollback written: units {units} flip back onto {old} once the server fills {refill} \
             rows that missed it. Next: embeddings status. To move forward again, run embeddings \
             start and then embeddings flip."
        )
    } else {
        format!(
            "rollback written: units {units} flip back onto {old} on the next pass; its slot is \
             complete, so the rollback is instant. Next: embeddings status. To move forward \
             again, run embeddings start and then embeddings flip."
        )
    };
    Decision::Write {
        change: IntentChange { target: Some(old), flip: true, retire: None, verb: Verb::Rollback },
        message,
    }
}

fn decide_retire(intent: &Intent, view: &View) -> Decision {
    if view.states.is_empty() {
        return Decision::NoOp("retire: no unit holds vectors; nothing written.".to_string());
    }
    let goal = match intent.target.as_deref() {
        Some(t) => t.to_string(),
        None => match common_active(view.states) {
            Ok(a) => a.to_string(),
            Err(spread) => {
                return Decision::Refuse(format!(
                    "retire refused: units are active on more than one model ({spread}) and no \
                     target is set. Run embeddings start and flip to bring them onto one model."
                ))
            }
        },
    };
    let behind: Vec<&UnitState> = view.states.iter().filter(|s| s.active_model() != goal).collect();
    if !behind.is_empty() {
        let detail = behind
            .iter()
            .map(|s| format!("{} on {}", s.unit, s.active_model()))
            .collect::<Vec<_>>()
            .join("; ");
        return Decision::Refuse(format!(
            "retire refused: units not yet flipped onto {goal}: {detail}. Run embeddings flip and \
             wait until embeddings status shows every unit flipped."
        ));
    }
    let mut held: Vec<&str> = view.states.iter().filter_map(|s| s.other_model()).collect();
    held.sort_unstable();
    held.dedup();
    let retire = match held.as_slice() {
        [] => {
            return Decision::NoOp(format!(
                "retire: no unit holds a second model beside {goal}; nothing written."
            ))
        }
        [one] => one.to_string(),
        many => {
            return Decision::Refuse(format!(
                "retire refused: inactive slots hold more than one model ({}), and one \
                 retire deletes one model from every unit. Bring the units onto one pair first; \
                 embeddings status shows each unit's other model.",
                many.join(", ")
            ))
        }
    };
    if intent.retire.as_deref() == Some(retire.as_str()) {
        return Decision::NoOp(format!(
            "retire: {retire} is already being retired (generation {}); nothing written. Next: \
             embeddings status until every unit is steady.",
            intent.generation
        ));
    }
    if retire == goal {
        return Decision::Refuse(format!(
            "retire refused: both slots name {goal}, so retiring the inactive one would delete \
             the model units are active on."
        ));
    }

    let unpublished: Vec<&UnitState> =
        view.states.iter().filter(|s| status_of(view, &s.unit).is_none()).collect();
    if !unpublished.is_empty() {
        return Decision::Refuse(format!(
            "retire refused: the server has published no status for units {}, so their holes \
             and failed rows are unknown. Start the server, wait one pass (EMBED_MIGRATE_SECS) \
             and run retire again.",
            names(unpublished)
        ));
    }
    let unsafe_units: Vec<String> = view
        .states
        .iter()
        .filter_map(|s| status_of(view, &s.unit))
        .filter(|st| st.counts.active_holes > 0 || st.counts.failed > 0)
        .map(|st| {
            format!(
                "unit {} has {} active holes and {} failed rows",
                st.unit, st.counts.active_holes, st.counts.failed
            )
        })
        .collect();
    if !unsafe_units.is_empty() {
        return Decision::Refuse(format!(
            "retire refused: {}. Retire deletes the only other vector those rows could fall back \
             on. embeddings status names the failed rows; run retire once both counts are 0.",
            unsafe_units.join("; ")
        ));
    }
    let days = chrono::Duration::days(view.rollback_days);
    let open: Vec<String> = view
        .states
        .iter()
        .filter_map(|s| s.flipped_at.map(|f| (s, f, f + days)))
        .filter(|(_, _, closes)| *closes > view.now)
        .map(|(s, f, closes)| {
            format!(
                "unit {} flipped at {} and its rollback window closes at {}",
                s.unit,
                when(f),
                when(closes)
            )
        })
        .collect();
    if !open.is_empty() {
        return Decision::Refuse(format!(
            "retire refused: {} (EMBED_ROLLBACK_DAYS={}). Until then rollback stays instant.",
            open.join("; "),
            view.rollback_days
        ));
    }
    Decision::Write {
        change: IntentChange {
            target: intent.target.clone(),
            flip: intent.flip,
            retire: Some(retire.clone()),
            verb: Verb::Retire,
        },
        message: format!(
            "retire written: the server deletes {retire}'s vectors from the inactive slot of \
             units {} in batches, then clears the slot. From then on a rollback needs a full \
             fill. Next: embeddings status until every unit is steady, then remove {retire}'s \
             block from .env and recreate the server (docker compose up -d server).",
            names(view.states)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::embedding_migration::{Counts, FlipScope, Phase};
    use crate::domain::embedding_slot::VectorSlot;

    const BGE: &str = "Xenova/bge-base-en-v1.5@q8";
    const GEMMA: &str = "openai:google/embeddinggemma-2";
    const THIRD: &str = "openai:some/other-model";

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn now() -> DateTime<Utc> {
        at("2026-10-20T12:00:00Z")
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Active in slot A on `model`, with `other` in slot B.
    fn on(unit: &str, model: &str, other: Option<&str>) -> UnitState {
        UnitState {
            unit: unit.into(),
            active_slot: VectorSlot::A,
            model_a: Some(model.into()),
            model_b: other.map(str::to_string),
            flipped_at: None,
        }
    }

    /// Flipped onto `model` in slot B at `when`, with `other` left in slot A.
    fn flipped(unit: &str, model: &str, other: Option<&str>, when: &str) -> UnitState {
        UnitState {
            unit: unit.into(),
            active_slot: VectorSlot::B,
            model_a: other.map(str::to_string),
            model_b: Some(model.into()),
            flipped_at: Some(at(when)),
        }
    }

    fn status(state: &UnitState, phase: Phase, counts: Counts) -> UnitStatus {
        UnitStatus {
            unit: state.unit.clone(),
            phase,
            active_slot: state.active_slot,
            active: state.active_model().to_string(),
            other: state.other_model().map(str::to_string),
            counts,
            failed_ids: Vec::new(),
            blocked: None,
            flipped_at: state.flipped_at,
            retire_after: None,
        }
    }

    fn eligible(n: i64) -> Counts {
        Counts { eligible: n, ..Counts::default() }
    }

    fn intent(
        target: Option<&str>,
        flip: bool,
        retire: Option<&str>,
        verb: Option<Verb>,
    ) -> Intent {
        Intent {
            generation: 4,
            target: target.map(str::to_string),
            flip,
            retire: retire.map(str::to_string),
            verb,
            requested_at: None,
        }
    }

    struct Fixture {
        sweep_on: bool,
        blocks: Vec<String>,
        states: Vec<UnitState>,
        statuses: Vec<UnitStatus>,
        published: Published,
        guessed: BTreeMap<String, Vec<String>>,
        rollback_days: i64,
        disk: Option<DiskStatus>,
        probe: Option<Result<(), String>>,
    }

    impl Fixture {
        /// `EMBED_*` names Gemma and `EMBED_PREVIOUS_*` names bge, as the switch guide sets them.
        fn new(states: Vec<UnitState>) -> Self {
            let statuses = states.iter().map(|s| status(s, Phase::Steady, eligible(300))).collect();
            Fixture {
                sweep_on: true,
                blocks: ids(&[GEMMA, BGE]),
                states,
                statuses,
                published: Published::default(),
                guessed: BTreeMap::new(),
                rollback_days: 7,
                disk: None,
                probe: None,
            }
        }

        fn view(&self) -> View<'_> {
            View {
                sweep_on: self.sweep_on,
                blocks: &self.blocks,
                states: &self.states,
                statuses: &self.statuses,
                published: &self.published,
                guessed_acting: &self.guessed,
                rollback_days: self.rollback_days,
                interval_secs: 30,
                now: now(),
                disk: self.disk,
                probe: self.probe.clone(),
            }
        }

        fn decide(&self, verb: Verb, intent: &Intent) -> Decision {
            decide(verb, intent, &self.view())
        }
    }

    fn written(d: Decision) -> (IntentChange, String) {
        match d {
            Decision::Write { change, message } => (change, message),
            other => panic!("expected a write, got {other:?}"),
        }
    }

    fn refused(d: Decision) -> String {
        match d {
            Decision::Refuse(text) => text,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn noop(d: Decision) -> String {
        match d {
            Decision::NoOp(text) => text,
            other => panic!("expected a no-op, got {other:?}"),
        }
    }

    fn change(target: Option<&str>, flip: bool, retire: Option<&str>, verb: Verb) -> IntentChange {
        IntentChange {
            target: target.map(str::to_string),
            flip,
            retire: retire.map(str::to_string),
            verb,
        }
    }

    fn assert_names(text: &str, parts: &[&str]) {
        for part in parts {
            assert!(text.contains(part), "{part:?} missing from: {text}");
        }
    }

    #[test]
    fn start_writes_the_other_block() {
        let f = Fixture::new(vec![on("me", BGE, None), on("t2", BGE, None)]);
        let (c, message) = written(f.decide(Verb::Start, &Intent::default()));
        assert_eq!(c, change(Some(GEMMA), false, None, Verb::Start));
        assert_names(&message, &["me", "t2", BGE, GEMMA, "600", "embeddings flip"]);
    }

    #[test]
    fn start_is_a_noop_when_already_moving_there() {
        let f = Fixture::new(vec![on("me", BGE, Some(GEMMA))]);
        let i = intent(Some(GEMMA), false, None, Some(Verb::Start));
        assert_names(&noop(f.decide(Verb::Start, &i)), &[GEMMA, "generation 4"]);
    }

    #[test]
    fn start_refuses_one_block() {
        let mut f = Fixture::new(vec![on("me", BGE, None)]);
        f.blocks = ids(&[BGE]);
        let text = refused(f.decide(Verb::Start, &Intent::default()));
        assert_names(
            &text,
            &[
                "EMBED_PREVIOUS_",
                "If you added the block to .env, recreate first (docker compose up -d server); \
                 exec sees the environment the container booted with.",
            ],
        );
    }

    #[test]
    fn start_refuses_two_blocks_naming_one_model() {
        let mut f = Fixture::new(vec![on("me", BGE, None)]);
        f.blocks = ids(&[BGE, BGE]);
        assert_names(&refused(f.decide(Verb::Start, &Intent::default())), &[BGE]);
    }

    #[test]
    fn start_refuses_a_store_with_no_unit() {
        let f = Fixture::new(vec![]);
        assert_names(&refused(f.decide(Verb::Start, &Intent::default())), &[GEMMA]);
    }

    #[test]
    fn start_refuses_units_on_two_models() {
        let f = Fixture::new(vec![
            on("me", BGE, None),
            flipped("t2", GEMMA, Some(BGE), "2026-10-01T00:00:00Z"),
        ]);
        let text = refused(f.decide(Verb::Start, &Intent::default()));
        assert_names(&text, &["me", "t2", BGE, GEMMA]);
    }

    #[test]
    fn start_refuses_units_on_a_model_no_block_names() {
        let f = Fixture::new(vec![on("me", THIRD, None)]);
        let text = refused(f.decide(Verb::Start, &Intent::default()));
        assert_names(&text, &["me", THIRD, "EMBED_PREVIOUS_"]);
    }

    #[test]
    fn start_refuses_an_inactive_slot_holding_a_third_model() {
        let f = Fixture::new(vec![on("me", BGE, Some(THIRD))]);
        let text = refused(f.decide(Verb::Start, &Intent::default()));
        assert_names(&text, &["me", THIRD, "embeddings retire"]);
    }

    #[test]
    fn start_refuses_during_a_retire_of_another_model() {
        let f = Fixture::new(vec![on("me", BGE, Some(THIRD))]);
        let i = intent(None, false, Some(THIRD), Some(Verb::Retire));
        assert_names(&refused(f.decide(Verb::Start, &i)), &[THIRD, "retire", "finishes"]);
    }

    #[test]
    fn start_resumes_a_cancel_of_the_same_model() {
        let f = Fixture::new(vec![on("me", BGE, Some(GEMMA))]);
        let i = intent(None, false, Some(GEMMA), Some(Verb::Rollback));
        let (c, message) = written(f.decide(Verb::Start, &i));
        assert_eq!(c, change(Some(GEMMA), false, None, Verb::Start));
        assert_names(&message, &["resumes", GEMMA]);
    }

    #[test]
    fn start_after_a_rollback_targets_the_new_model_again() {
        // G10 step 10: the rollback left bge as the target with flip on, and the unit is back on bge.
        let f = Fixture::new(vec![flipped("me", BGE, Some(GEMMA), "2026-10-19T09:00:00Z")]);
        let i = intent(Some(BGE), true, None, Some(Verb::Rollback));
        let (c, _) = written(f.decide(Verb::Start, &i));
        assert_eq!(c, change(Some(GEMMA), false, None, Verb::Start));
    }

    #[test]
    fn start_refuses_with_the_sweep_off() {
        let mut f = Fixture::new(vec![on("me", BGE, None)]);
        f.sweep_on = false;
        for verb in [Verb::Start, Verb::Flip, Verb::Rollback, Verb::Retire] {
            let text = refused(f.decide(verb, &Intent::default()));
            assert_names(&text, &[verb.as_str(), "EMBED_MIGRATE_SECS=0"]);
        }
    }

    #[test]
    fn start_refuses_to_leave_a_model_with_guessed_acting_keys() {
        let mut f = Fixture::new(vec![on("me", THIRD, None)]);
        f.blocks = ids(&[GEMMA, THIRD]);
        f.guessed.insert(THIRD.into(), ids(&["dedupe", "conflict", "cleanup_near_certain"]));
        let text = refused(f.decide(Verb::Start, &Intent::default()));
        assert_names(
            &text,
            &[
                THIRD,
                "EMBED_PREVIOUS_THRESHOLDS=dedupe=0.97,conflict=0.9,cleanup_near_certain=0.97",
            ],
        );
    }

    #[test]
    fn start_refuses_below_the_disk_floor() {
        let mut f = Fixture::new(vec![on("me", BGE, None)]);
        f.disk = Some(DiskStatus { free_bytes: 1_000, floor_bytes: 16_106_127_360, paused: false });
        let text = refused(f.decide(Verb::Start, &Intent::default()));
        assert_names(&text, &["1000", "16106127360", "EMBED_DISK_FLOOR_MB"]);
    }

    #[test]
    fn start_refuses_a_failed_probe() {
        let mut f = Fixture::new(vec![on("me", BGE, None)]);
        f.probe = Some(Err("connection refused".into()));
        let text = refused(f.decide(Verb::Start, &Intent::default()));
        assert_names(&text, &[GEMMA, "connection refused", "EMBED_BASE_URL"]);
    }

    #[test]
    fn flip_mid_fill_is_written_and_says_it_waits() {
        let mut f = Fixture::new(vec![on("me", BGE, Some(GEMMA))]);
        f.statuses = vec![status(
            &f.states[0],
            Phase::Filling,
            Counts { eligible: 12_000, other_pending: 1_840, ..Counts::default() },
        )];
        let i = intent(Some(GEMMA), false, None, Some(Verb::Start));
        let (c, message) = written(f.decide(Verb::Flip, &i));
        assert_eq!(c, change(Some(GEMMA), true, None, Verb::Flip));
        assert_names(&message, &["me", "1840", "waits"]);
    }

    #[test]
    fn flip_refuses_without_a_target() {
        let f = Fixture::new(vec![on("me", BGE, None)]);
        assert_names(&refused(f.decide(Verb::Flip, &Intent::default())), &["embeddings start"]);
    }

    #[test]
    fn flip_refuses_guessed_acting_keys() {
        let mut f = Fixture::new(vec![on("me", BGE, Some(THIRD))]);
        f.blocks = ids(&[THIRD, BGE]);
        f.guessed.insert(THIRD.into(), ids(&["dedupe"]));
        let i = intent(Some(THIRD), false, None, Some(Verb::Start));
        assert_names(&refused(f.decide(Verb::Flip, &i)), &[THIRD, "EMBED_THRESHOLDS=dedupe=0.97"]);
    }

    #[test]
    fn flip_is_a_noop_once_every_unit_is_on_the_target() {
        let f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-12T09:31:00Z")]);
        let i = intent(Some(GEMMA), false, None, Some(Verb::Start));
        assert_names(&noop(f.decide(Verb::Flip, &i)), &["me", GEMMA]);
    }

    #[test]
    fn flip_is_a_noop_when_already_requested() {
        let f = Fixture::new(vec![on("me", BGE, Some(GEMMA))]);
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        assert_names(&noop(f.decide(Verb::Flip, &i)), &["generation 4"]);
    }

    #[test]
    fn rollback_before_a_flip_cancels() {
        let mut f = Fixture::new(vec![on("me", BGE, Some(GEMMA))]);
        f.statuses = vec![status(
            &f.states[0],
            Phase::Filling,
            Counts { eligible: 300, other_pending: 100, ..Counts::default() },
        )];
        let i = intent(Some(GEMMA), false, None, Some(Verb::Start));
        let (c, message) = written(f.decide(Verb::Rollback, &i));
        assert_eq!(c, change(None, false, Some(GEMMA), Verb::Rollback));
        assert_names(&message, &["cancel", GEMMA, "200", "me"]);
    }

    #[test]
    fn rollback_after_a_flip_targets_the_old_model() {
        let f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-12T09:31:00Z")]);
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        let (c, message) = written(f.decide(Verb::Rollback, &i));
        assert_eq!(c, change(Some(BGE), true, None, Verb::Rollback));
        assert_names(&message, &["me", BGE, "instant", "embeddings start"]);
    }

    #[test]
    fn rollback_after_retire_says_how_many_rows_it_refills() {
        let mut f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-01T09:31:00Z")]);
        f.statuses = vec![status(
            &f.states[0],
            Phase::Retiring,
            Counts { eligible: 300, retire_pending: 120, ..Counts::default() },
        )];
        let i = intent(Some(GEMMA), true, Some(BGE), Some(Verb::Retire));
        let (c, message) = written(f.decide(Verb::Rollback, &i));
        assert_eq!(c, change(Some(BGE), true, None, Verb::Rollback));
        assert_names(&message, &["180", "refill"]);
    }

    #[test]
    fn rollback_after_a_finished_retire_targets_the_previous_block() {
        let f = Fixture::new(vec![flipped("me", GEMMA, None, "2026-10-01T09:31:00Z")]);
        let i = intent(Some(GEMMA), true, Some(BGE), Some(Verb::Retire));
        let (c, message) = written(f.decide(Verb::Rollback, &i));
        assert_eq!(c, change(Some(BGE), true, None, Verb::Rollback));
        assert_names(&message, &["300", "refill"]);
    }

    #[test]
    fn rollback_refuses_when_the_old_block_is_gone() {
        let mut f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-12T09:31:00Z")]);
        f.blocks = ids(&[GEMMA]);
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        assert_names(&refused(f.decide(Verb::Rollback, &i)), &[BGE, "EMBED_PREVIOUS_", "Restore"]);
    }

    #[test]
    fn rollback_with_nothing_started_is_a_noop() {
        let f = Fixture::new(vec![on("me", BGE, None)]);
        noop(f.decide(Verb::Rollback, &Intent::default()));
        let cancelling = intent(None, false, Some(GEMMA), Some(Verb::Rollback));
        assert_names(&noop(f.decide(Verb::Rollback, &cancelling)), &[GEMMA]);
    }

    #[test]
    fn a_second_rollback_before_the_server_applies_the_first_is_a_noop() {
        // The first rollback made bge the target; the unit has not flipped back yet. Reading that as
        // "before any flip" would cancel bge and delete the old model's vectors.
        let f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-12T09:31:00Z")]);
        let i = intent(Some(BGE), true, None, Some(Verb::Rollback));
        assert_names(&noop(f.decide(Verb::Rollback, &i)), &[BGE, "generation 4"]);
    }

    #[test]
    fn retire_refuses_before_every_unit_flipped() {
        let f = Fixture::new(vec![
            flipped("me", GEMMA, Some(BGE), "2026-10-01T09:31:00Z"),
            on("t2", BGE, Some(GEMMA)),
        ]);
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        let text = refused(f.decide(Verb::Retire, &i));
        assert_names(&text, &["t2", "embeddings flip"]);
        assert!(!text.contains("me "), "{text}");
    }

    #[test]
    fn retire_refuses_an_active_hole_or_a_failed_row() {
        let s = flipped("me", GEMMA, Some(BGE), "2026-10-01T09:31:00Z");
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        for (holes, failed, part) in [(3, 0, "3 active holes"), (0, 2, "2 failed rows")] {
            let mut f = Fixture::new(vec![s.clone()]);
            f.statuses = vec![status(
                &s,
                Phase::Flipped,
                Counts { eligible: 300, active_holes: holes, failed, ..Counts::default() },
            )];
            assert_names(&refused(f.decide(Verb::Retire, &i)), &["me", part]);
        }
    }

    #[test]
    fn retire_refuses_without_a_published_status() {
        let mut f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-01T09:31:00Z")]);
        f.statuses.clear();
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        assert_names(&refused(f.decide(Verb::Retire, &i)), &["me", "status"]);
    }

    #[test]
    fn retire_is_refused_inside_the_window() {
        let f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-15T09:31:00Z")]);
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        assert_names(
            &refused(f.decide(Verb::Retire, &i)),
            &["retire refused: unit me flipped at 2026-10-15 09:31 UTC and its rollback window \
                 closes at 2026-10-22 09:31 UTC (EMBED_ROLLBACK_DAYS=7). Until then rollback stays \
                 instant."],
        );
    }

    #[test]
    fn retire_names_the_inactive_model() {
        let f = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-12T09:31:00Z")]);
        let i = intent(Some(GEMMA), true, None, Some(Verb::Flip));
        let (c, message) = written(f.decide(Verb::Retire, &i));
        assert_eq!(c, change(Some(GEMMA), true, Some(BGE), Verb::Retire));
        assert_names(&message, &["me", BGE, ".env"]);
    }

    #[test]
    fn retire_clears_a_third_model_with_no_target() {
        let f = Fixture::new(vec![on("me", BGE, Some(THIRD))]);
        let (c, _) = written(f.decide(Verb::Retire, &Intent::default()));
        assert_eq!(c, change(None, false, Some(THIRD), Verb::Retire));
    }

    #[test]
    fn retire_is_a_noop_with_nothing_in_the_inactive_slot() {
        let f = Fixture::new(vec![on("me", BGE, None)]);
        noop(f.decide(Verb::Retire, &Intent::default()));
        let again = Fixture::new(vec![flipped("me", GEMMA, Some(BGE), "2026-10-01T09:31:00Z")]);
        let i = intent(Some(GEMMA), true, Some(BGE), Some(Verb::Retire));
        assert_names(&noop(again.decide(Verb::Retire, &i)), &[BGE, "generation 4"]);
    }

    fn published(models: &[&str], seen: Option<&str>) -> Published {
        Published {
            applied_generation: 4,
            server_models: ids(models),
            status: None,
            seen_at: seen.map(at),
        }
    }

    #[test]
    fn drift_passes_on_the_same_ids() {
        let p = published(&[GEMMA, BGE], Some("2026-10-20T11:59:30Z"));
        assert_eq!(drift(&ids(&[GEMMA, BGE]), &p, 30, now()), Ok(None));
    }

    #[test]
    fn drift_refuses_a_live_server_on_other_ids() {
        let p = published(&[BGE], Some("2026-10-20T11:59:30Z"));
        let text = drift(&ids(&[GEMMA, BGE]), &p, 30, now()).unwrap_err();
        assert_names(&text, &[GEMMA, BGE, "docker compose up -d server", "recreate"]);
    }

    #[test]
    fn drift_warns_with_no_recent_pass() {
        let stale = published(&[BGE], Some("2026-10-20T11:58:00Z"));
        let w = drift(&ids(&[GEMMA, BGE]), &stale, 30, now()).unwrap().expect("a warning");
        assert_names(&w, &["2026-10-20 11:58 UTC", "90"]);
        let never = published(&[], None);
        let w = drift(&ids(&[GEMMA, BGE]), &never, 30, now()).unwrap().expect("a warning");
        assert_names(&w, &["never"]);
    }

    #[test]
    fn start_target_names_the_block_no_unit_is_on() {
        let blocks = ids(&[GEMMA, BGE]);
        let both = [on("me", BGE, None), on("t2", BGE, Some(GEMMA))];
        assert_eq!(start_target(&blocks, &both), Some(GEMMA.to_string()));
        let back = [flipped("me", GEMMA, Some(BGE), "2026-10-12T09:31:00Z")];
        assert_eq!(start_target(&blocks, &back), Some(BGE.to_string()));
        assert_eq!(start_target(&ids(&[GEMMA]), &both), None);
        let mixed = [on("me", BGE, None), back[0].clone()];
        assert_eq!(start_target(&blocks, &mixed), None);
        assert_eq!(start_target(&blocks, &[on("me", THIRD, None)]), None);
        assert_eq!(start_target(&blocks, &[]), None);
        assert_eq!(start_target(&ids(&[BGE, BGE]), &both), None);
    }

    #[test]
    fn configured_from_intent_maps_each_row_of_the_table() {
        let blocks = ids(&[GEMMA, BGE]);
        let mut guessed = BTreeMap::new();
        guessed.insert(GEMMA.to_string(), Vec::new());
        let view = |i: &Intent, active: Option<&str>| {
            configured_from_intent(i, &blocks, active, 7, &guessed).unwrap()
        };

        // No target, no retire: steady on the default unit's model.
        let c = view(&intent(None, false, None, None), Some(BGE));
        assert_eq!((c.current.as_str(), c.previous, c.retire), (BGE, None, None));
        assert_eq!(c.flip, FlipScope::None);
        assert_eq!((c.rollback_days, c.generation), (7, Some(4)));
        assert_eq!(c.guessed_acting, guessed);
        // With no state row the default unit counts as on EMBED_*'s model.
        assert_eq!(view(&intent(None, false, None, None), None).current, GEMMA);

        // Target, no retire: the other block is previous; flip follows the flag.
        let c = view(&intent(Some(GEMMA), false, None, Some(Verb::Start)), Some(BGE));
        assert_eq!((c.current.as_str(), c.previous.as_deref(), c.retire), (GEMMA, Some(BGE), None));
        assert_eq!(c.flip, FlipScope::None);
        let c = view(&intent(Some(GEMMA), true, None, Some(Verb::Flip)), Some(BGE));
        assert_eq!(c.flip, FlipScope::All);
        // A rollback target is the old model, and the new one becomes previous.
        let c = view(&intent(Some(BGE), true, None, Some(Verb::Rollback)), Some(GEMMA));
        assert_eq!((c.current.as_str(), c.previous.as_deref()), (BGE, Some(GEMMA)));

        // Target and retire: no previous, flip all.
        let c = view(&intent(Some(GEMMA), false, Some(BGE), Some(Verb::Retire)), Some(GEMMA));
        assert_eq!((c.current.as_str(), c.previous, c.retire.as_deref()), (GEMMA, None, Some(BGE)));
        assert_eq!(c.flip, FlipScope::All);

        // No target, retire: a cancel on the default unit's model.
        let c = view(&intent(None, false, Some(GEMMA), Some(Verb::Rollback)), Some(BGE));
        assert_eq!((c.current.as_str(), c.previous, c.retire.as_deref()), (BGE, None, Some(GEMMA)));
        assert_eq!(c.flip, FlipScope::None);
    }

    #[test]
    fn configured_from_intent_refuses_an_unconfigured_target() {
        let err = configured_from_intent(
            &intent(Some(THIRD), false, None, Some(Verb::Start)),
            &ids(&[GEMMA, BGE]),
            Some(BGE),
            7,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert_names(
            &err,
            &[
                THIRD,
                "restore",
                "docker compose run --rm -T server lumberroom-server embeddings rollback",
            ],
        );
    }
}
