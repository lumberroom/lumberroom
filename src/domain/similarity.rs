//! Cosine thresholds belong to the embedding model that produced the vectors they compare. Each
//! model family carries its own value per key, and an override for one model never reaches another
//! (decision 0029).

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    pub key: &'static str,
    /// Changes or merges data with no person reading it first. A unit flips onto a model only when
    /// none of that model's acting keys is guessed.
    pub acts: bool,
}

pub const DEDUPE: &str = "dedupe";
pub const CONFLICT: &str = "conflict";
pub const BOOTSTRAP_DEDUP: &str = "bootstrap_dedup";
pub const CLEANUP_NEAR_CERTAIN: &str = "cleanup_near_certain";
pub const CLEANUP_WORTH_ASKING: &str = "cleanup_worth_asking";
pub const ROUTE_MAX_TOP: &str = "route_max_top";
pub const ROUTE_MAX_SPREAD: &str = "route_max_spread";

pub const ENGINE_KEYS: &[KeySpec] = &[
    KeySpec { key: DEDUPE, acts: true },
    KeySpec { key: CONFLICT, acts: true },
    KeySpec { key: BOOTSTRAP_DEDUP, acts: false },
    KeySpec { key: CLEANUP_NEAR_CERTAIN, acts: true },
    KeySpec { key: CLEANUP_WORTH_ASKING, acts: false },
    KeySpec { key: ROUTE_MAX_TOP, acts: false },
    KeySpec { key: ROUTE_MAX_SPREAD, acts: false },
];

/// Why a table value is what it is. The status page prints it beside the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// The value this model ran on before decision 0029. Tuned or a design target; production ran
    /// on it either way.
    Shipped,
    /// Measured in the threshold study of 10 October 2026.
    Study,
    /// bge-base-en-v1.5's shipped value, copied with no measurement for this model.
    Carried,
}

#[derive(Debug, Clone, Copy)]
pub struct FamilyValue {
    pub key: &'static str,
    pub value: f64,
    pub basis: Basis,
}

/// Values for every embedder id whose lowercased form contains `family`. Quantisations of one
/// model share an entry: llama.cpp Q8_0 and ONNX q8 EmbeddingGemma 2 agreed at cosine 0.9997. A
/// fine-tune whose id contains the name inherits too; EMBED_THRESHOLDS overrides it.
#[derive(Debug, Clone, Copy)]
pub struct Family {
    pub family: &'static str,
    pub values: &'static [FamilyValue],
}

pub const BGE_BASE: &str = "bge-base-en-v1.5";
pub const EMBEDDINGGEMMA_2: &str = "embeddinggemma-2";
/// A model with no entry, or an entry without the key, borrows this family's value as a guess.
pub const GUESS_FROM: &str = BGE_BASE;

pub const ENGINE_FAMILIES: &[Family] = &[
    Family {
        family: BGE_BASE,
        values: &[
            FamilyValue { key: DEDUPE, value: 0.97, basis: Basis::Shipped },
            FamilyValue { key: CONFLICT, value: 0.90, basis: Basis::Shipped },
            FamilyValue { key: BOOTSTRAP_DEDUP, value: 0.90, basis: Basis::Shipped },
            FamilyValue { key: CLEANUP_NEAR_CERTAIN, value: 0.97, basis: Basis::Shipped },
            FamilyValue { key: CLEANUP_WORTH_ASKING, value: 0.65, basis: Basis::Shipped },
            FamilyValue { key: ROUTE_MAX_TOP, value: 0.65, basis: Basis::Shipped },
            FamilyValue { key: ROUTE_MAX_SPREAD, value: 0.08, basis: Basis::Shipped },
        ],
    },
    Family {
        family: EMBEDDINGGEMMA_2,
        values: &[
            // Threshold study, 10 October 2026, on a production store, aggregates only. dedupe and
            // cleanup_near_certain come from labelled pairs (every human-judged must-not-merge pair
            // sits below 0.9902) and carry low confidence; the rest match the share of neighbour
            // scores at or above bge's value.
            FamilyValue { key: DEDUPE, value: 0.995, basis: Basis::Study },
            FamilyValue { key: CONFLICT, value: 0.91, basis: Basis::Study },
            FamilyValue { key: BOOTSTRAP_DEDUP, value: 0.919, basis: Basis::Study },
            FamilyValue { key: CLEANUP_NEAR_CERTAIN, value: 0.995, basis: Basis::Study },
            FamilyValue { key: CLEANUP_WORTH_ASKING, value: 0.754, basis: Basis::Study },
            // Unmeasured. Both compare a query vector with stored rows, the store keeps no query
            // text, and a fusion sweep is measuring them. bge's values until it reports.
            FamilyValue { key: ROUTE_MAX_TOP, value: 0.65, basis: Basis::Carried },
            FamilyValue { key: ROUTE_MAX_SPREAD, value: 0.08, basis: Basis::Carried },
        ],
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Override,
    Legacy,
    Shipped,
    Study,
    Carried,
    Guessed,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Resolved {
    pub value: f64,
    pub source: Source,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct SimilarityThresholds {
    pub model: String,
    /// The matched family; None for a model with no entry.
    pub family: Option<String>,
    pub values: BTreeMap<String, Resolved>,
}

impl SimilarityThresholds {
    /// A key missing here is a registration bug, so it panics with the key's name.
    pub fn get(&self, key: &str) -> f64 {
        self.values
            .get(key)
            .unwrap_or_else(|| panic!("similarity key {key} is not registered"))
            .value
    }

    /// Keys whose value is a guess, in key order.
    pub fn guessed(&self) -> Vec<&str> { unimplemented!("T-A1") }
}

/// An old single threshold variable that is set, such as `DEDUPE_THRESHOLD=0.95`.
#[derive(Debug, Clone, PartialEq)]
pub struct Legacy {
    pub key: &'static str,
    pub value: f64,
    pub variable: &'static str,
}

/// A rule across one model's keys. Err names the keys and both values.
pub type Check = fn(&SimilarityThresholds) -> Result<(), String>;

/// `conflict` above `dedupe` empties the band that yields conflict candidates, and corrections then
/// fold into the row they correct (the rule `src/config.rs:1378-1384` held for the single settings).
pub fn conflict_at_or_below_dedupe(t: &SimilarityThresholds) -> Result<(), String> { unimplemented!("T-A1") }

/// A model is asked only about pairs between the two, so the band must not be empty.
pub fn worth_asking_below_near_certain(t: &SimilarityThresholds) -> Result<(), String> { unimplemented!("T-A1") }

pub const ENGINE_CHECKS: &[Check] = &[conflict_at_or_below_dedupe, worth_asking_below_near_certain];

pub struct Registry {
    keys: Vec<KeySpec>,
    families: BTreeMap<&'static str, BTreeMap<&'static str, FamilyValue>>,
    checks: Vec<Check>,
}

impl Registry {
    /// The engine's keys, families and checks.
    pub fn engine() -> Self { unimplemented!("T-A1") }

    /// Adds keys, family values and checks. Panics, naming it, on a key registered twice or a value
    /// given twice for one family: both are registration bugs, and a boot finds them. The fork calls
    /// it once, in `src/main.rs`.
    pub fn extend(self, keys: &[KeySpec], families: &[Family], checks: &[Check]) -> Self { unimplemented!("T-A1") }

    pub fn keys(&self) -> &[KeySpec] {
        &self.keys
    }

    /// The family whose name the lowercased id contains. Two matching families are a table bug and
    /// panic with both names.
    pub fn family_of(&self, model_id: &str) -> Option<&'static str> { unimplemented!("T-A1") }

    /// Every registered key for `model_id`, first match winning: the last override for the key
    /// (`Source::Override`), the legacy value (`Source::Legacy`), the family's value (its basis),
    /// `GUESS_FROM`'s value (`Source::Guessed`). A key `GUESS_FROM` lacks is a registration bug and
    /// panics with its name.
    pub fn resolve(&self, model_id: &str, overrides: &[(String, f64)], legacy: &[Legacy])
        -> SimilarityThresholds { unimplemented!("T-A1") }

    /// Every check's error for `t`, each prefixed with `t.model`.
    pub fn check(&self, t: &SimilarityThresholds) -> Vec<String> { unimplemented!("T-A1") }

    /// Acting keys whose value in `t` is guessed, in registration order.
    pub fn guessed_acting(&self, t: &SimilarityThresholds) -> Vec<String> { unimplemented!("T-A1") }

    /// Override keys no spec registers, in input order.
    pub fn unknown_keys(&self, overrides: &[(String, f64)]) -> Vec<String> { unimplemented!("T-A1") }
}

/// Parses `key=value,key=value`. Each value must satisfy `0 < v <= 1`. Errors name the bad pair.
/// Unknown keys are checked by `Registry::unknown_keys`, because the fork registers more keys.
pub fn parse_overrides(raw: &str) -> Result<Vec<(String, f64)>, String> { unimplemented!("T-A1") }
