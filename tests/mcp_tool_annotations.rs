//! Every tool in `tools/list` declares who it is and what it does to the store.
//!
//! Clients read `title`, `readOnlyHint` and `destructiveHint` to decide what to run without asking
//! the person, and connector directories refuse a server whose tools carry none. A tool
//! added without them must fail here, so the list below is the single place a new tool gets
//! classified.
//!
//! No database: the definitions come from the router, before any grant filters them.

use std::collections::BTreeSet;

use lumberroom_server::mcp::capability::TOOL_CAPABILITIES;
use lumberroom_server::mcp::{tool_definitions, SERVER_INSTRUCTIONS};

/// Tools that only read. `readOnlyHint: true`.
const READS: [&str; 7] = [
    "context_bootstrap",
    "memory_search",
    "memory_history",
    "registry_get",
    "registry_history",
    "alias_list",
    "review_queue",
];

/// Tools that delete or retire data. `readOnlyHint: false`, `destructiveHint: true`.
const DESTRUCTIVE: [&str; 2] = ["memory_forget", "review_decide"];

/// Tools that add or replace a row and keep the old one reachable. `destructiveHint: false`.
const ADDITIVE: [&str; 3] = ["memory_write", "registry_set", "alias_set"];

/// Wording that orders the model to act in a way the person has not asked for. The directory's
/// review criteria reject it, and the owner's own agent rules already carry it.
const ORDERS: [&str; 6] = [
    "silently",
    "without asking",
    "without announcing",
    "always call",
    "before any substantive work",
    "before substantive work",
];

#[test]
fn the_classification_covers_every_registered_tool_and_nothing_else() {
    let classified: BTreeSet<&str> =
        READS.iter().chain(&DESTRUCTIVE).chain(&ADDITIVE).copied().collect();
    let registered: BTreeSet<String> =
        tool_definitions().iter().map(|t| t.name.to_string()).collect();
    let registered: BTreeSet<&str> = registered.iter().map(String::as_str).collect();
    assert_eq!(
        registered, classified,
        "a tool was added or removed: classify it in READS, DESTRUCTIVE or ADDITIVE"
    );
    let gated: BTreeSet<&str> = TOOL_CAPABILITIES.iter().map(|(name, _)| *name).collect();
    assert_eq!(registered, gated, "the router and the capability table disagree");
}

#[test]
fn every_tool_has_a_title_both_where_clients_look() {
    for tool in tool_definitions() {
        let top = tool.title.as_deref().unwrap_or("");
        assert!(!top.trim().is_empty(), "{} has no title", tool.name);
        let annotated = tool.annotations.as_ref().and_then(|a| a.title.as_deref()).unwrap_or("");
        assert_eq!(top, annotated, "{} carries two different titles", tool.name);
    }
}

#[test]
fn every_tool_states_read_only_explicitly_and_stays_inside_the_store() {
    for tool in tool_definitions() {
        let a =
            tool.annotations.as_ref().unwrap_or_else(|| panic!("{} has no annotations", tool.name));
        assert!(a.read_only_hint.is_some(), "{} leaves readOnlyHint to its default", tool.name);
        assert_eq!(
            a.open_world_hint,
            Some(false),
            "{} must declare openWorldHint false",
            tool.name
        );
    }
}

#[test]
fn a_tool_that_writes_states_destructive_explicitly() {
    for tool in tool_definitions() {
        let a = tool.annotations.as_ref().expect("annotations");
        if a.read_only_hint == Some(false) {
            assert!(
                a.destructive_hint.is_some(),
                "{} writes but leaves destructiveHint to its default of true",
                tool.name
            );
        }
    }
}

#[test]
fn the_listed_reads_are_read_only() {
    for tool in tool_definitions().iter().filter(|t| READS.contains(&&*t.name)) {
        let a = tool.annotations.as_ref().expect("annotations");
        assert_eq!(a.read_only_hint, Some(true), "{}", tool.name);
    }
}

#[test]
fn the_tools_that_delete_or_retire_data_are_destructive() {
    for tool in tool_definitions().iter().filter(|t| DESTRUCTIVE.contains(&&*t.name)) {
        let a = tool.annotations.as_ref().expect("annotations");
        assert_eq!(a.read_only_hint, Some(false), "{}", tool.name);
        assert_eq!(a.destructive_hint, Some(true), "{}", tool.name);
    }
}

#[test]
fn the_additive_writers_are_not_destructive() {
    for tool in tool_definitions().iter().filter(|t| ADDITIVE.contains(&&*t.name)) {
        let a = tool.annotations.as_ref().expect("annotations");
        assert_eq!(a.read_only_hint, Some(false), "{}", tool.name);
        assert_eq!(a.destructive_hint, Some(false), "{}", tool.name);
    }
}

/// `alias_set` is an upsert on (namespace, alias), so repeating a call leaves the same row. The
/// others append a version or a row each time, and claiming otherwise would let a client retry
/// them blindly.
#[test]
fn idempotent_is_claimed_only_for_the_upsert() {
    for tool in tool_definitions() {
        let claimed = tool.annotations.as_ref().and_then(|a| a.idempotent_hint) == Some(true);
        assert_eq!(claimed, tool.name == "alias_set", "{}", tool.name);
    }
}

#[test]
fn no_description_or_instruction_orders_the_model_to_act() {
    let mut texts: Vec<(String, String)> = tool_definitions()
        .iter()
        .map(|t| (t.name.to_string(), t.description.clone().unwrap_or_default().to_string()))
        .collect();
    texts.push(("server instructions".into(), SERVER_INSTRUCTIONS.to_string()));
    for (what, text) in texts {
        let lower = text.to_lowercase();
        for order in ORDERS {
            assert!(!lower.contains(order), "{what} contains {order:?}: {text}");
        }
        assert!(!text.contains('\u{2014}'), "{what} contains an em dash");
        assert!(!text.trim().is_empty(), "{what} is empty");
    }
}
