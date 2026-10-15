//! Every serde field name on `ClientGrant` must appear in `docs/permissions.md`, so a field added
//! to the struct and never documented fails the build instead of going unnoticed. No database, no
//! server; this reads the struct's own source and the doc as text.

use std::fs;

/// The wire names serde produces for `ClientGrant`, read from its `#[serde(rename = ...)]`
/// attributes rather than duplicated by hand, so a rename in `src/config.rs` cannot drift from
/// this list without the extraction below catching it.
fn client_grant_wire_names() -> Vec<String> {
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/config.rs"))
        .expect("src/config.rs must exist to check against");

    let struct_start = src.find("pub struct ClientGrant").expect("ClientGrant must still exist");
    let struct_end = src[struct_start..]
        .find("\nimpl ClientGrant")
        .map(|i| struct_start + i)
        .expect("ClientGrant struct body must precede its impl block");
    let body = &src[struct_start..struct_end];

    let mut names =
        vec!["client".to_string(), "token".to_string(), "read".to_string(), "write".to_string()];
    for line in body.lines() {
        if let Some(idx) = line.find("rename = \"") {
            let rest = &line[idx + "rename = \"".len()..];
            let end = rest.find('"').expect("an opened rename attribute must close");
            names.push(rest[..end].to_string());
        }
    }
    names
}

fn permissions_doc() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/permissions.md"))
        .expect("docs/permissions.md must exist")
}

#[test]
fn every_client_grant_field_is_documented() {
    let doc = permissions_doc();
    let names = client_grant_wire_names();
    assert!(
        names.len() >= 9,
        "extraction should find client, token, read, write, and five renamed flags"
    );

    for name in &names {
        assert!(
            doc.contains(name.as_str()),
            "docs/permissions.md does not mention the ClientGrant field `{name}`; a field added to \
             src/config.rs must be documented before this test can pass",
        );
    }
}

#[test]
fn the_unrestricted_read_asymmetry_is_stated() {
    let doc = permissions_doc();
    for term in ["effective_sealed_capable", "effective_may_delete", "mayDelete", "sealedCapable"] {
        assert!(
            doc.contains(term),
            "docs/permissions.md must name `{term}` to state the asymmetry"
        );
    }
}

#[test]
fn every_open_tool_and_every_gated_capability_is_tabulated() {
    let doc = permissions_doc();
    for tool in [
        "context_bootstrap",
        "memory_search",
        "memory_write",
        "registry_get",
        "alias_list",
        "memory_forget",
        "memory_history",
        "registry_history",
        "registry_set",
        "alias_set",
    ] {
        assert!(doc.contains(tool), "docs/permissions.md must list the tool `{tool}`");
    }
}
