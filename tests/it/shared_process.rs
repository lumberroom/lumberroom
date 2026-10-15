//! The rule `common::SERIAL` depends on, checked against the source of every module here.
//!
//! The modules of `tests/it` share one process under `cargo test`. A module with a mutex of its own,
//! or one that sets the environment without the shared lock, races every other module's setup, and
//! nextest, which CI runs, gives each test its own process and so never shows the race. Only a plain
//! `cargo test` run does, and only sometimes. This reads the files instead.

use std::fs;
use std::path::Path;

/// Calls that write the process environment or read state another module writes.
const SHARED_STATE: &[&str] = &["set_var(", "remove_var(", "config::load(", "clear_cache("];

fn modules() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it");
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).expect("reading tests/it") {
        let path = entry.expect("a tests/it entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // This file names the patterns it looks for.
        if !name.ends_with(".rs") || name == "shared_process.rs" {
            continue;
        }
        out.push((name, fs::read_to_string(&path).expect("reading a module")));
    }
    assert!(out.len() > 20, "found {} modules in {}", out.len(), dir.display());
    out
}

#[test]
fn no_module_declares_a_serial_mutex_of_its_own() {
    let own: Vec<String> = modules()
        .into_iter()
        .filter(|(_, src)| src.contains("static SERIAL"))
        .map(|(n, _)| n)
        .collect();
    assert!(own.is_empty(), "these declare their own SERIAL; use crate::common::SERIAL: {own:?}");
}

#[test]
fn every_module_that_touches_shared_state_takes_the_shared_lock() {
    let unguarded: Vec<String> = modules()
        .into_iter()
        .filter(|(_, src)| SHARED_STATE.iter().any(|call| src.contains(call)))
        .filter(|(_, src)| !src.contains("SERIAL.lock()"))
        .map(|(n, _)| n)
        .collect();
    assert!(
        unguarded.is_empty(),
        "these set the environment, load the config or clear the digest cache without taking \
         common::SERIAL: {unguarded:?}"
    );
}
