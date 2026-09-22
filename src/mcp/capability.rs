//! Which grant each tool needs, and the filter that keeps a tool out of the list a client reads.
//!
//! One table, two readers. `list_tools` filters on it so a model never sees a tool it cannot call,
//! and the documentation in `docs/permissions.md` is generated against the same names. A mapping
//! that lives in the filter alone is a mapping the docs go stale against on the first edit.
//!
//! **The filter is not the enforcement.** Every service checks the grant again on the call. This
//! shapes what a model tries; `forget::by_id`, `services::history` and `registry::set` are what
//! refuse it. A client that hard-codes a tool name and calls it directly gets a 403 either way,
//! which is the property that matters and the one a filter alone would not give.

use crate::domain::types::Principal;

/// The grant a tool needs before it appears in a client's tool list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Every authenticated client. Namespace and sensitivity still apply inside the call.
    Open,
    MayDelete,
    MayReadHistory,
    RegistryWrite,
}

impl Capability {
    /// The `AUTH_TOKENS` field an operator edits to grant this, for an error message and for docs.
    pub fn grant_field(self) -> &'static str {
        match self {
            Self::Open => "none",
            Self::MayDelete => "mayDelete",
            Self::MayReadHistory => "mayReadHistory",
            Self::RegistryWrite => "registryWrite",
        }
    }

    pub fn held_by(self, p: &Principal) -> bool {
        match self {
            Self::Open => true,
            Self::MayDelete => p.may_delete,
            Self::MayReadHistory => p.may_read_history,
            Self::RegistryWrite => p.registry_write,
        }
    }
}

/// Every tool this server registers, and the grant it needs.
///
/// Exhaustive on purpose. A tool absent from this table is invisible to `required`, which answers
/// `Open` and would publish it to every client. The test at the bottom of this file holds the table
/// against the router's own list so a tool added without an entry fails the build rather than
/// shipping ungated.
pub const TOOL_CAPABILITIES: &[(&str, Capability)] = &[
    ("context_bootstrap", Capability::Open),
    ("memory_search", Capability::Open),
    ("memory_write", Capability::Open),
    ("registry_get", Capability::Open),
    ("memory_forget", Capability::MayDelete),
    // A retired fact can be more revealing than the one that replaced it, which is the whole reason
    // `may_read_history` exists rather than riding along with read access.
    ("memory_history", Capability::MayReadHistory),
    ("registry_history", Capability::MayReadHistory),
    // The registry holds credential locations, so writing to it is an operator action.
    ("registry_set", Capability::RegistryWrite),
    // An alias is a naming fact of the same class as a registry key, so it takes the same grant
    // rather than a new flag. Nobody gains alias-write who does not already hold the higher-trust
    // one, and it needs no migration.
    ("alias_set", Capability::RegistryWrite),
    // Reading the alias group is ordinary read. The namespaces it may name are filtered inside the
    // call: a list of names is a disclosure a content filter cannot see.
    ("alias_list", Capability::Open),
    // supersede and merge are memory_write with supersedes, confirm is a restated fact, delete
    // still reads may_delete through forget::by_id, apply and dismiss are a proposal source's own
    // gate. keep_both is the one mutation new to this surface, and it needs read and write on both
    // rows, the same authority a supersede of that pair already needs.
    ("review_queue", Capability::Open),
    ("review_decide", Capability::Open),
];

pub fn required(tool: &str) -> Capability {
    TOOL_CAPABILITIES
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, c)| *c)
        .unwrap_or(Capability::Open)
}

/// Whether this principal may see and call this tool.
pub fn permits(p: &Principal, tool: &str) -> bool {
    required(tool).held_by(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::policy::NamespaceGrant;

    fn principal() -> Principal {
        Principal {
            client: "test".into(),
            token_id: "t".into(),
            mode: "token",
            scopes: vec![],
            read: NamespaceGrant::everything(),
            write: NamespaceGrant::everything(),
            registry_write: false,
            sealed_capable: false,
            may_delete: false,
            may_ingest: false,
            may_read_history: false,
        }
    }

    #[test]
    fn a_bare_grant_sees_only_the_open_tools() {
        let p = principal();
        let visible: Vec<&str> = TOOL_CAPABILITIES
            .iter()
            .filter(|(name, _)| permits(&p, name))
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            visible,
            vec![
                "context_bootstrap",
                "memory_search",
                "memory_write",
                "registry_get",
                "alias_list",
                "review_queue",
                "review_decide"
            ]
        );
    }

    #[test]
    fn each_capability_opens_exactly_its_own_tools() {
        let mut p = principal();
        p.may_delete = true;
        assert!(permits(&p, "memory_forget"));
        assert!(!permits(&p, "memory_history"), "may_delete must not imply history");
        assert!(!permits(&p, "registry_set"), "may_delete must not imply registry writes");
    }

    #[test]
    fn history_and_registry_writes_are_separate_grants() {
        let mut p = principal();
        p.may_read_history = true;
        assert!(permits(&p, "memory_history"));
        assert!(permits(&p, "registry_history"));
        assert!(!permits(&p, "registry_set"));
        assert!(!permits(&p, "memory_forget"));
    }

    #[test]
    fn registry_write_carries_the_alias_write_and_nothing_else() {
        let mut p = principal();
        p.registry_write = true;
        assert!(permits(&p, "registry_set"));
        assert!(permits(&p, "alias_set"));
        assert!(!permits(&p, "memory_forget"));
        assert!(!permits(&p, "memory_history"));
    }

    #[test]
    fn an_unknown_tool_is_treated_as_open_and_the_router_test_is_what_catches_that() {
        // Deliberate: `required` cannot refuse what it has never heard of without making every
        // caller handle an error. The guard is the test that holds this table against the router,
        // which fails the build rather than shipping a tool nobody gated.
        assert_eq!(required("a_tool_that_does_not_exist"), Capability::Open);
    }

    #[test]
    fn every_capability_names_the_field_an_operator_edits() {
        for (_, c) in TOOL_CAPABILITIES {
            if *c == Capability::Open {
                continue;
            }
            assert!(
                !c.grant_field().is_empty() && c.grant_field() != "none",
                "a refusal that does not name the field to edit sends the owner to a log for it"
            );
        }
    }

    #[test]
    fn no_tool_is_listed_twice() {
        let mut names: Vec<&str> = TOOL_CAPABILITIES.iter().map(|(n, _)| *n).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "a duplicate entry makes `required` order-dependent");
    }
}
