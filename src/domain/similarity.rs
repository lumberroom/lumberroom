//! Cosine thresholds belong to the embedding model that produced the vectors they compare. Each
//! model family carries its own value per key, and an override for one model never reaches another
//! (decision 0029).

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    pub key: &'static str,
    /// Changes or merges data with no person reading it first. The dual-model change (decision
    /// 0027, the next PR) will flip a unit onto a model only when none of its acting keys is guessed.
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

/// Why a table value is what it is. The status page will print it beside the value once the
/// dual-model change (decision 0027) lands.
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
            // Threshold study, 10 October 2026, on a production store, aggregates only
            // (docs/results/2026-10-threshold-study.md). dedupe and cleanup_near_certain come from
            // labelled pairs (the highest human-judged must-not-merge pair sits at 0.9902) and
            // carry low confidence. conflict was chosen to reproduce the share of client
            // corrections bge flags (71.0% against 69.9%). bootstrap_dedup and
            // cleanup_worth_asking match the share of neighbour scores at or above bge's value.
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
    pub fn guessed(&self) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(_, r)| r.source == Source::Guessed)
            .map(|(k, _)| k.as_str())
            .collect()
    }
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
/// fold into the row they correct. `validate` in `src/config.rs` applies the same rule to
/// `CONFLICT_THRESHOLD` and `DEDUPE_THRESHOLD`.
pub fn conflict_at_or_below_dedupe(t: &SimilarityThresholds) -> Result<(), String> {
    let (conflict, dedupe) = (t.get(CONFLICT), t.get(DEDUPE));
    if conflict > dedupe {
        return Err(format!(
            "{CONFLICT} ({conflict}) is above {DEDUPE} ({dedupe}); the conflict band would be empty"
        ));
    }
    Ok(())
}

/// A model is asked only about pairs between the two, so the band must not be empty.
pub fn worth_asking_below_near_certain(t: &SimilarityThresholds) -> Result<(), String> {
    let (ask, certain) = (t.get(CLEANUP_WORTH_ASKING), t.get(CLEANUP_NEAR_CERTAIN));
    if ask >= certain {
        return Err(format!(
            "{CLEANUP_WORTH_ASKING} ({ask}) is not below {CLEANUP_NEAR_CERTAIN} ({certain}); the band a model is asked about would be empty"
        ));
    }
    Ok(())
}

pub const ENGINE_CHECKS: &[Check] = &[conflict_at_or_below_dedupe, worth_asking_below_near_certain];

pub struct Registry {
    keys: Vec<KeySpec>,
    families: BTreeMap<&'static str, BTreeMap<&'static str, FamilyValue>>,
    checks: Vec<Check>,
}

impl Registry {
    /// The engine's keys, families and checks.
    pub fn engine() -> Self {
        Registry { keys: Vec::new(), families: BTreeMap::new(), checks: Vec::new() }.extend(
            ENGINE_KEYS,
            ENGINE_FAMILIES,
            ENGINE_CHECKS,
        )
    }

    /// Adds keys, family values and checks. Panics, naming it, on a key registered twice or a value
    /// given twice for one family: both are registration bugs, and a boot finds them. The fork calls
    /// it once, in `src/main.rs`.
    pub fn extend(mut self, keys: &[KeySpec], families: &[Family], checks: &[Check]) -> Self {
        for spec in keys {
            if self.keys.iter().any(|k| k.key == spec.key) {
                panic!("similarity key {} is registered twice", spec.key);
            }
            self.keys.push(*spec);
        }
        for family in families {
            let entry = self.families.entry(family.family).or_default();
            for value in family.values {
                if !self.keys.iter().any(|k| k.key == value.key) {
                    panic!(
                        "family {} gives a value for {}, which no key registers",
                        family.family, value.key
                    );
                }
                if entry.insert(value.key, *value).is_some() {
                    panic!("family {} gives a value for {} twice", family.family, value.key);
                }
            }
        }
        self.checks.extend_from_slice(checks);
        self
    }

    pub fn keys(&self) -> &[KeySpec] {
        &self.keys
    }

    /// The family whose name the lowercased id contains. Two matching families are a table bug and
    /// panic with both names.
    pub fn family_of(&self, model_id: &str) -> Option<&'static str> {
        let id = model_id.to_lowercase();
        let hits: Vec<&'static str> =
            self.families.keys().copied().filter(|name| id.contains(name)).collect();
        match hits.as_slice() {
            [] => None,
            [one] => Some(one),
            many => panic!("embedder id {model_id} matches families {}", many.join(", ")),
        }
    }

    /// Every registered key for `model_id`, first match winning: the last override for the key
    /// (`Source::Override`), the legacy value (`Source::Legacy`), the family's value (its basis),
    /// `GUESS_FROM`'s value (`Source::Guessed`). A key `GUESS_FROM` lacks is a registration bug and
    /// panics with its name.
    pub fn resolve(
        &self,
        model_id: &str,
        overrides: &[(String, f64)],
        legacy: &[Legacy],
    ) -> SimilarityThresholds {
        let family = self.family_of(model_id);
        let mut values = BTreeMap::new();
        for spec in &self.keys {
            let resolved = if let Some((_, v)) = overrides.iter().rev().find(|(k, _)| k == spec.key)
            {
                Resolved { value: *v, source: Source::Override }
            } else if let Some(l) = legacy.iter().find(|l| l.key == spec.key) {
                Resolved { value: l.value, source: Source::Legacy }
            } else if let Some(fv) = family.and_then(|f| self.families[f].get(spec.key)) {
                let source = match fv.basis {
                    Basis::Shipped => Source::Shipped,
                    Basis::Study => Source::Study,
                    Basis::Carried => Source::Carried,
                };
                Resolved { value: fv.value, source }
            } else {
                let fv =
                    self.families.get(GUESS_FROM).and_then(|f| f.get(spec.key)).unwrap_or_else(
                        || panic!("{GUESS_FROM} has no value for similarity key {}", spec.key),
                    );
                Resolved { value: fv.value, source: Source::Guessed }
            };
            values.insert(spec.key.to_string(), resolved);
        }
        SimilarityThresholds {
            model: model_id.to_string(),
            family: family.map(str::to_string),
            values,
        }
    }

    /// Each legacy value that decides its key for `model_id` and differs from the value the key
    /// would take without it, paired with that value. An `.env` copied from an older
    /// `.env.example` carries bge's values in the old variables, and they hold any other model on
    /// bge's scale with nothing in the log.
    pub fn legacy_departures<'a>(
        &self,
        model_id: &str,
        overrides: &[(String, f64)],
        legacy: &'a [Legacy],
    ) -> Vec<(&'a Legacy, f64)> {
        let without = self.resolve(model_id, overrides, &[]);
        legacy
            .iter()
            .filter(|l| !overrides.iter().any(|(k, _)| k == l.key))
            .filter_map(|l| {
                let table = without.values.get(l.key)?.value;
                (table != l.value).then_some((l, table))
            })
            .collect()
    }

    /// Every check's error for `t`, each prefixed with `t.model`.
    pub fn check(&self, t: &SimilarityThresholds) -> Vec<String> {
        self.checks.iter().filter_map(|c| c(t).err()).map(|e| format!("{}: {e}", t.model)).collect()
    }

    /// Acting keys whose value in `t` is guessed, in registration order.
    pub fn guessed_acting(&self, t: &SimilarityThresholds) -> Vec<String> {
        self.keys
            .iter()
            .filter(|k| k.acts && t.values.get(k.key).is_some_and(|r| r.source == Source::Guessed))
            .map(|k| k.key.to_string())
            .collect()
    }

    /// Override keys no spec registers, in input order.
    pub fn unknown_keys(&self, overrides: &[(String, f64)]) -> Vec<String> {
        overrides
            .iter()
            .filter(|(k, _)| !self.keys.iter().any(|s| s.key == k))
            .map(|(k, _)| k.clone())
            .collect()
    }
}

/// Parses `key=value,key=value`. Each value must satisfy `0 <= v <= 1`. `DEDUPE_THRESHOLD` and
/// `CONFLICT_THRESHOLD` accept 0, and moving one of them into an override must not be refused.
/// Errors name the bad pair. Unknown keys are checked by `Registry::unknown_keys`, because the fork registers more
/// keys.
pub fn parse_overrides(raw: &str) -> Result<Vec<(String, f64)>, String> {
    let mut out = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (key, value) = item
            .split_once('=')
            .ok_or_else(|| format!("threshold override {item:?} is not key=value"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("threshold override {item:?} has no key"));
        }
        let v: f64 = value
            .trim()
            .parse()
            .map_err(|_| format!("threshold override {item:?} has a value that is not a number"))?;
        // Written as a positive test so NaN fails it.
        if !(0.0..=1.0).contains(&v) {
            return Err(format!("threshold override {item:?} is outside 0 <= value <= 1"));
        }
        out.push((key.to_string(), v));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BGE_Q8: &str = "Xenova/bge-base-en-v1.5@q8";
    const BGE_OPENAI: &str = "openai:BAAI/bge-base-en-v1.5";
    const GEMMA: &str = "openai:google/embeddinggemma-2";

    fn ov(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn legacy(key: &'static str, value: f64) -> Legacy {
        Legacy { key, value, variable: "LEGACY_VAR" }
    }

    #[test]
    fn parse_overrides_reads_pairs_and_rejects_bad_items() {
        assert_eq!(
            parse_overrides("dedupe=0.95, conflict=0.8 ,,route_max_top=1").unwrap(),
            ov(&[("dedupe", 0.95), ("conflict", 0.8), ("route_max_top", 1.0)])
        );
        assert_eq!(parse_overrides("").unwrap(), vec![]);
        assert_eq!(parse_overrides(" , ").unwrap(), vec![]);
        // DEDUPE_THRESHOLD takes 0, so the override that replaces it must too.
        assert_eq!(parse_overrides("dedupe=0").unwrap(), ov(&[("dedupe", 0.0)]));
        for bad in [
            "dedupe",
            "=0.5",
            "dedupe=abc",
            "dedupe=-0.1",
            "dedupe=1.01",
            "dedupe=NaN",
            "dedupe=inf",
        ] {
            let raw = format!("conflict=0.5,{bad}");
            let err = parse_overrides(&raw).unwrap_err();
            assert!(err.contains(bad), "error for {bad:?} should name the item: {err}");
        }
    }

    #[test]
    fn an_override_beats_legacy_and_the_table() {
        let r = Registry::engine();
        let t = r.resolve(BGE_Q8, &ov(&[(DEDUPE, 0.5), (DEDUPE, 0.6)]), &[legacy(DEDUPE, 0.99)]);
        assert_eq!(t.values[DEDUPE], Resolved { value: 0.6, source: Source::Override });
        assert_eq!(t.get(CONFLICT), 0.90);
    }

    #[test]
    fn legacy_beats_the_table() {
        let r = Registry::engine();
        let t = r.resolve(GEMMA, &[], &[legacy(CONFLICT, 0.8)]);
        assert_eq!(t.values[CONFLICT], Resolved { value: 0.8, source: Source::Legacy });
        assert_eq!(t.values[DEDUPE].source, Source::Study);
    }

    #[test]
    fn a_legacy_value_off_the_table_is_named_with_the_table_value() {
        let r = Registry::engine();
        let l = [legacy(DEDUPE, 0.97), legacy(CONFLICT, 0.90)];
        assert!(r.legacy_departures(BGE_Q8, &[], &l).is_empty());
        assert_eq!(r.legacy_departures(GEMMA, &[], &l), vec![(&l[0], 0.995), (&l[1], 0.91)]);
        // An override decides its key, so the legacy value changes nothing there.
        assert_eq!(r.legacy_departures(GEMMA, &ov(&[(DEDUPE, 0.99)]), &l), vec![(&l[1], 0.91)]);
    }

    #[test]
    fn the_bge_ids_resolve_to_shipped_values() {
        let r = Registry::engine();
        for id in [BGE_Q8, BGE_OPENAI] {
            let t = r.resolve(id, &[], &[]);
            assert_eq!(t.model, id);
            assert_eq!(t.family.as_deref(), Some(BGE_BASE));
            assert_eq!(t.values.len(), ENGINE_KEYS.len());
            assert_eq!(t.get(DEDUPE), 0.97);
            assert_eq!(t.get(CONFLICT), 0.90);
            assert_eq!(t.get(ROUTE_MAX_SPREAD), 0.08);
            assert!(t.values.values().all(|v| v.source == Source::Shipped));
            assert!(t.guessed().is_empty());
        }
    }

    #[test]
    fn the_gemma_id_reads_the_study_values() {
        let t = Registry::engine().resolve(GEMMA, &[], &[]);
        assert_eq!(t.family.as_deref(), Some(EMBEDDINGGEMMA_2));
        for (key, value) in [
            (DEDUPE, 0.995),
            (CONFLICT, 0.91),
            (BOOTSTRAP_DEDUP, 0.919),
            (CLEANUP_NEAR_CERTAIN, 0.995),
            (CLEANUP_WORTH_ASKING, 0.754),
        ] {
            assert_eq!(t.values[key], Resolved { value, source: Source::Study }, "{key}");
        }
        for (key, value) in [(ROUTE_MAX_TOP, 0.65), (ROUTE_MAX_SPREAD, 0.08)] {
            assert_eq!(t.values[key], Resolved { value, source: Source::Carried }, "{key}");
        }
    }

    #[test]
    fn embeddinggemma_300m_matches_no_family() {
        let r = Registry::engine();
        assert_eq!(r.family_of("openai:google/embeddinggemma-300m"), None);
        assert_eq!(r.family_of("openai:Google/EmbeddingGemma-2"), Some(EMBEDDINGGEMMA_2));
        let t = r.resolve("openai:google/embeddinggemma-300m", &[], &[]);
        assert_eq!(t.family, None);
    }

    #[test]
    fn an_unlisted_model_guesses_every_key_from_bge() {
        let r = Registry::engine();
        let t = r.resolve("some/other-model", &[], &[]);
        assert_eq!(t.values.len(), ENGINE_KEYS.len());
        for spec in ENGINE_KEYS {
            let v = &t.values[spec.key];
            assert_eq!(v.source, Source::Guessed, "{}", spec.key);
            assert_eq!(v.value, r.resolve(BGE_Q8, &[], &[]).get(spec.key), "{}", spec.key);
        }
        let mut all: Vec<&str> = ENGINE_KEYS.iter().map(|k| k.key).collect();
        all.sort();
        assert_eq!(t.guessed(), all);
    }

    #[test]
    fn guessed_acting_names_only_acting_keys() {
        let r = Registry::engine();
        let t = r.resolve("some/other-model", &[], &[]);
        assert_eq!(r.guessed_acting(&t), vec![DEDUPE, CONFLICT, CLEANUP_NEAR_CERTAIN]);
        let pinned = r.resolve("some/other-model", &ov(&[(DEDUPE, 0.97)]), &[]);
        assert_eq!(r.guessed_acting(&pinned), vec![CONFLICT, CLEANUP_NEAR_CERTAIN]);
        assert!(r.guessed_acting(&r.resolve(GEMMA, &[], &[])).is_empty());
    }

    #[test]
    fn extend_adds_keys_to_an_existing_family() {
        const EXTRA: &[FamilyValue] =
            &[FamilyValue { key: "extra", value: 0.5, basis: Basis::Study }];
        let r = Registry::engine().extend(
            &[KeySpec { key: "extra", acts: true }],
            &[Family { family: BGE_BASE, values: EXTRA }],
            &[],
        );
        assert_eq!(r.keys().len(), ENGINE_KEYS.len() + 1);
        let bge = r.resolve(BGE_Q8, &[], &[]);
        assert_eq!(bge.values["extra"], Resolved { value: 0.5, source: Source::Study });
        assert_eq!(bge.get(DEDUPE), 0.97);
        let gemma = r.resolve(GEMMA, &[], &[]);
        assert_eq!(gemma.values["extra"], Resolved { value: 0.5, source: Source::Guessed });
        assert_eq!(r.guessed_acting(&gemma), vec!["extra"]);
    }

    #[test]
    #[should_panic(expected = "dedupe")]
    fn extend_panics_on_a_key_registered_twice() {
        let _ = Registry::engine().extend(&[KeySpec { key: DEDUPE, acts: true }], &[], &[]);
    }

    #[test]
    #[should_panic(expected = "dedupe")]
    fn extend_panics_on_a_value_given_twice_for_one_family() {
        const DUP: &[FamilyValue] = &[FamilyValue { key: DEDUPE, value: 0.5, basis: Basis::Study }];
        let _ = Registry::engine().extend(&[], &[Family { family: BGE_BASE, values: DUP }], &[]);
    }

    #[test]
    #[should_panic(expected = "nobody_registered_this")]
    fn extend_panics_on_a_value_for_an_unregistered_key() {
        const BAD: &[FamilyValue] =
            &[FamilyValue { key: "nobody_registered_this", value: 0.5, basis: Basis::Study }];
        let _ = Registry::engine().extend(&[], &[Family { family: BGE_BASE, values: BAD }], &[]);
    }

    #[test]
    #[should_panic(expected = "bge-base")]
    fn two_families_matching_one_id_panic() {
        let r = Registry::engine().extend(&[], &[Family { family: "bge-base", values: &[] }], &[]);
        let _ = r.family_of(BGE_Q8);
    }

    #[test]
    fn every_registered_key_has_a_bge_value() {
        let r = Registry::engine();
        let t = r.resolve(BGE_BASE, &[], &[]);
        for spec in r.keys() {
            assert_eq!(t.values[spec.key].source, Source::Shipped, "{}", spec.key);
        }
    }

    #[test]
    #[should_panic(expected = "lonely")]
    fn a_key_bge_lacks_panics_at_resolve() {
        let r = Registry::engine().extend(&[KeySpec { key: "lonely", acts: false }], &[], &[]);
        let _ = r.resolve("some/other-model", &[], &[]);
    }

    #[test]
    fn the_checks_refuse_conflict_above_dedupe() {
        let r = Registry::engine();
        let bad = r.resolve(BGE_Q8, &ov(&[(CONFLICT, 0.98)]), &[]);
        let errs = r.check(&bad);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].starts_with(BGE_Q8), "{}", errs[0]);
        assert!(errs[0].contains("conflict") && errs[0].contains("dedupe"), "{}", errs[0]);
        assert!(errs[0].contains("0.98") && errs[0].contains("0.97"), "{}", errs[0]);
        let equal = r.resolve(BGE_Q8, &ov(&[(CONFLICT, 0.97)]), &[]);
        assert!(r.check(&equal).is_empty());
        let band = r.resolve(BGE_Q8, &ov(&[(CLEANUP_WORTH_ASKING, 0.97)]), &[]);
        let errs = r.check(&band);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("cleanup_worth_asking") && errs[0].contains("cleanup_near_certain")
        );
    }

    #[test]
    fn every_engine_family_passes_the_engine_checks() {
        let r = Registry::engine();
        for f in ENGINE_FAMILIES {
            let t = r.resolve(f.family, &[], &[]);
            assert_eq!(r.check(&t), Vec::<String>::new(), "{}", f.family);
            for check in ENGINE_CHECKS {
                assert!(check(&t).is_ok());
            }
        }
    }

    #[test]
    fn unknown_keys_are_named() {
        let r = Registry::engine();
        let o = ov(&[("dedupe", 0.9), ("typo", 0.5), ("conflict", 0.8), ("also_typo", 0.5)]);
        assert_eq!(r.unknown_keys(&o), vec!["typo", "also_typo"]);
        assert!(r.unknown_keys(&[]).is_empty());
    }
}
