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

/// Tools that delete, retire or overwrite data. `readOnlyHint: false`, `destructiveHint: true`.
///
/// `alias_set` sits here because its upsert replaces the canonical name, since and until of an
/// alias already recorded, and no history table keeps the earlier pairing.
const DESTRUCTIVE: [&str; 3] = ["memory_forget", "review_decide", "alias_set"];

/// Tools that add or replace a row and keep the old one reachable. `destructiveHint: false`.
const ADDITIVE: [&str; 2] = ["memory_write", "registry_set"];

/// Wording that orders the model to act in a way the person has not asked for. The directory's
/// review criteria reject it, and the owner's own agent rules already carry it.
///
/// The second half came out of the argument descriptions. Each one guarded a real trap, and the
/// fact behind it stays in the text: what the argument does, and what goes wrong with a bad value.
///
/// A denylist catches the phrasings it names and nothing else. The last group are the imperatives
/// this file's first rewrite removed, so restoring any of them fails here.
const ORDERS: [&str; 22] = [
    "silently",
    "without asking",
    "without announcing",
    "always call",
    "before any substantive work",
    "before substantive work",
    "pass it",
    "omit it unless",
    "never infer",
    "never pass",
    "only when the person",
    "set it whenever",
    "set it only when",
    "use it first",
    "pass true only",
    "copy it from",
    "omit to",
    "omit for",
    "do not",
    "leave it out",
    "only pass",
    "should reflect",
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

/// `alias_set` is an upsert on (namespace, alias), so repeating the same call leaves the same row.
/// Idempotent and destructive are separate claims: a call with a new canonical name still
/// overwrites the old one. The others append a version or a row each time, and claiming otherwise
/// would let a client retry them blindly.
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

/// Every `description` string in a schema, at any depth, with the path that reached it. Nested
/// `items` and `$defs` carry descriptions a client shows beside the argument, so stopping at the
/// top level would miss them.
fn schema_descriptions(value: &serde_json::Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let here = format!("{path}.{key}");
                match (key.as_str(), child) {
                    ("description", serde_json::Value::String(text)) => {
                        out.push((here, text.clone()))
                    }
                    _ => schema_descriptions(child, &here, out),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                schema_descriptions(child, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// A model reads an argument description in the same pass as the tool description, so an order
/// moved from one into the other is still an order. Walks every tool, gated or not.
#[test]
fn no_argument_description_orders_the_model_to_act() {
    for tool in tool_definitions() {
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let mut found = Vec::new();
        schema_descriptions(&schema, &tool.name, &mut found);
        for (what, text) in found {
            // schemars keeps the doc comment's line breaks, so "never\npass" has to match too.
            let lower = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
            for order in ORDERS {
                assert!(!lower.contains(order), "{what} contains {order:?}: {text}");
            }
            assert!(!text.contains('\u{2014}'), "{what} contains an em dash");
            assert!(!text.trim().is_empty(), "{what} is empty");
        }
    }
}

/// A walk that finds no descriptions passes every assertion in the test above, so this one pins the
/// other side: every argument of every tool carries a description a client can show. A new argument
/// without a doc comment fails here by name, where a count floor would let it through.
#[test]
fn every_argument_of_every_tool_carries_a_description() {
    let mut arguments = 0;
    for tool in tool_definitions() {
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let properties = schema.get("properties").and_then(|p| p.as_object());
        for (name, property) in properties.into_iter().flatten() {
            arguments += 1;
            let described = property.get("description").and_then(|d| d.as_str()).unwrap_or("");
            assert!(!described.trim().is_empty(), "{}.{name} has no description", tool.name);
        }
    }
    assert!(arguments > 0, "no tool exposes any argument, so the schema shape changed");
}
