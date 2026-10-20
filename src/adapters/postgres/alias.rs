//! Postgres implementation of `AliasRepository`. One table, one hop, one query on the read path.
//!
//! The read this file exists for is `group`. Search expands a query over every name in a group, so
//! that call sits in front of every retrieval that mentions a renamed thing and has to cost one
//! indexed scan.

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use crate::domain::errors::{DomainError, Result};
use crate::ports::alias::{Alias, AliasRepository, NewAlias};

pub struct PgAliasRepository {
    pool: PgPool,
}

impl PgAliasRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Every name in the group of `$3`, including `$3` itself.
///
/// # The lookup spans the prefix, not the one namespace
///
/// A group is a set of namespaces: `project:warden`, `project:quill` and `project:lumen` are one
/// subject under three names. The row recording that lives in whichever namespace the owner typed
/// when he wrote it, so keying the lookup on `namespace = $2` made the group findable from that
/// side and no other. Recorded under `project:lumen`, a search in `project:lumen` expanded to
/// warden and quill, and a search in `project:warden` expanded to nothing. Observed on the owner's
/// own store: 8 hits one way, 4 the other, with no error and no tell.
///
/// So the scope is the prefix. `project:` matches every project namespace and nothing outside it,
/// which is the boundary a rename actually respects: `personal:warden` is a different subject from
/// `project:warden` and stays one. A namespace with no colon falls back to matching itself, since
/// there is no prefix to widen to.
///
/// The prefix comparison is `starts_with`. Every kind the engine accepts today is plain letters,
/// and `LIKE` would read a `_` or `%` in one as a wildcard and widen the group.
///
/// The first arm resolves the name asked for to its canonical name, or keeps it when the name is
/// canonical already or unknown. The second arm collects every other name pointing at that
/// canonical. The `COALESCE` is what makes the statement total: an unknown name still produces one
/// row, so a caller substituting the result into a search never gets an empty list and never
/// searches for nothing.
///
/// One hop, no recursion, because a canonical name is never itself an alias. `put` holds that
/// invariant by repointing a group when its canonical name is renamed. A bounded recursive CTE
/// would also terminate, and it would terminate by truncating a chain longer than the bound with no
/// error, which is this system's worst failure dressed as a depth limit.
///
/// `since` and `until` are read by nobody here on purpose. Warden stopped being the current name in
/// March and the Warden facts are still in the store, so a group that dropped retired names would
/// expand a query into exactly the set that finds nothing.
const GROUP_SQL: &str = r#"
    WITH scope AS (
        SELECT CASE WHEN position(':' in $2) > 0
                    THEN split_part($2, ':', 1) || ':'
                    ELSE NULL
               END AS prefix
    ),
    rows AS (
        -- DISTINCT because the same mapping is commonly recorded in more than one namespace of the
        -- group. That was the only way to make the old namespace-keyed lookup answer from every
        -- side, so any store that used this feature before has those duplicates in it.
        SELECT DISTINCT a.alias, a.canonical
          FROM entity_alias a, scope
         WHERE a.tenant_id = $1
           AND (scope.prefix IS NULL AND a.namespace = $2
                OR scope.prefix IS NOT NULL AND starts_with(a.namespace, scope.prefix))
    ),
    root AS (
        -- LIMIT 1, because a scalar subquery returning two rows is an error rather than a wrong
        -- answer: `group` returns Err, search logs a warning and expands over nothing, and the
        -- caller sees a namespace that reaches only itself. ORDER BY makes the choice stable when
        -- one alias genuinely maps to two canonicals under one prefix, which is a conflict the
        -- owner has to resolve and not something to pick differently on each query.
        SELECT COALESCE(
                 (SELECT r.canonical FROM rows r WHERE r.alias = $3 ORDER BY r.canonical LIMIT 1),
                 $3
               ) AS name
    )
    SELECT name, 0 AS hop FROM root
    UNION
    SELECT r.alias, 1
      FROM rows r, root
     WHERE r.canonical = root.name
    ORDER BY hop, name
"#;

/// The alias the caller sends, or the one it already points through.
///
/// Recording Quill as an alias of Lumen when Warden already points at Quill demotes Quill, and
/// every name that pointed at Quill has to follow it in the same transaction. Doing the walk here,
/// at write time, is what buys the single-hop read: renames are rare and lookups are not.
const FLATTEN_SQL: &str = r#"
    SELECT canonical FROM entity_alias
     WHERE tenant_id = $1 AND namespace = $2 AND alias = $3
"#;

/// A `derived` alias must not overwrite a `manual` one. The owner stating that two names are the
/// same thing is a decision; something reading it out of a fact is a guess, and the guess losing is
/// correct. The same rule `registry_alias` applies to a rejected key.
const PUT_SQL: &str = r#"
    INSERT INTO entity_alias (tenant_id, namespace, alias, canonical, since, until, origin)
    VALUES ($1, $2, $3, $4, $5, $6, $7)
    ON CONFLICT (tenant_id, namespace, alias)
    DO UPDATE SET canonical = EXCLUDED.canonical,
                  since     = EXCLUDED.since,
                  until     = EXCLUDED.until,
                  origin    = EXCLUDED.origin
            WHERE EXCLUDED.origin <> 'derived' OR entity_alias.origin = 'derived'
    RETURNING namespace, alias, canonical, since, until, origin, created_at
"#;

/// Follow the demoted name's group to its new canonical.
///
/// `alias <> $4` keeps a row that already denotes the target from being rewritten into itself,
/// which the table's `entity_alias_not_self` check would refuse and abort the transaction over.
/// Under the one-hop invariant `settle` has already refused that case, so this guard earns its
/// place only against a row somebody wrote in psql.
const REPOINT_SQL: &str = r#"
    UPDATE entity_alias SET canonical = $4
     WHERE tenant_id = $1 AND namespace = $2 AND canonical = $3 AND alias <> $4
"#;

/// The row a declined write lost to, so the refusal can name what is already recorded.
const HELD_SQL: &str = r#"
    SELECT namespace, alias, canonical, since, until, origin, created_at
      FROM entity_alias
     WHERE tenant_id = $1 AND namespace = $2 AND alias = $3
"#;

const LIST_SQL: &str = r#"
    SELECT namespace, alias, canonical, since, until, origin, created_at
      FROM entity_alias
     WHERE tenant_id = $1 AND ($2::text IS NULL OR namespace = $2)
     ORDER BY namespace, canonical, alias
"#;

/// Lowercased and trimmed, in Rust, on every write and every lookup.
///
/// A person types "Warden" and the fact that mentions it says "warden". The database never folds
/// case here: `lower()` in Postgres answers to the server's collation, and a store that folded in
/// SQL on write and in Rust on read could disagree with itself after a locale change. One function
/// on both sides cannot.
fn normalize_name(raw: &str) -> Result<String> {
    let name = raw.trim().to_lowercase();
    if name.is_empty() {
        return Err(DomainError::validation("a name cannot be empty"));
    }
    Ok(name)
}

/// Where this alias should point, given what the proposed canonical name already points at.
///
/// Two refusals, both cycles. A name aliased to itself is the trivial one. The other is the pair a
/// person records by hand months apart: Warden to Quill, then Quill to Warden. A walk over that
/// pair never terminates, so it is refused at the moment it would be created rather than survived
/// at read time.
fn settle(alias: &str, canonical: &str, canonical_points_to: Option<&str>) -> Result<String> {
    if alias == canonical {
        return Err(DomainError::validation(format!("{alias:?} cannot be an alias of itself")));
    }
    let target = canonical_points_to.unwrap_or(canonical);
    if target == alias {
        return Err(DomainError::conflict(format!(
            "{alias:?} and {canonical:?} are already one group, with {alias:?} the canonical name. \
             Recording this would make each the other's canonical and no lookup would terminate. \
             Point both at whichever name is current instead."
        )));
    }
    Ok(target.to_string())
}

/// The never-empty contract, held in Rust as well as in the statement.
///
/// `GROUP_SQL`'s root arm always yields a row, so this only fires if that statement is edited into
/// something that does not. The caller substitutes this list into a search: an empty one searches
/// for nothing and reports that nothing is known about a name the store holds facts under.
fn ensure_present(name: &str, mut names: Vec<String>) -> Vec<String> {
    if !names.iter().any(|n| n == name) {
        names.insert(0, name.to_string());
    }
    names
}

fn alias_from_row(r: &sqlx::postgres::PgRow) -> Alias {
    Alias {
        namespace: r.get("namespace"),
        alias: r.get("alias"),
        canonical: r.get("canonical"),
        since: r.get("since"),
        until: r.get("until"),
        origin: r.get("origin"),
        created_at: r.get("created_at"),
    }
}

#[async_trait]
impl AliasRepository for PgAliasRepository {
    /// Record one name as another name for the same thing.
    ///
    /// Three statements in one transaction. A crash between the upsert and the repoint would leave
    /// a canonical name that is also an alias, which is the one thing `group` assumes away.
    async fn put(&self, tenant: &str, a: NewAlias) -> Result<Alias> {
        let alias = normalize_name(&a.alias)?;
        let asked_canonical = normalize_name(&a.canonical)?;
        if let (Some(since), Some(until)) = (a.since, a.until) {
            // The table checks this too. Catching it here names the two fields in the message
            // instead of handing the caller a constraint violation.
            if since > until {
                return Err(DomainError::validation(
                    "a name cannot stop being current before it started: since is after until",
                ));
            }
        }

        let mut tx = self.pool.begin().await?;

        let hop = sqlx::query_scalar::<_, String>(FLATTEN_SQL)
            .bind(tenant)
            .bind(&a.namespace)
            .bind(&asked_canonical)
            .fetch_optional(&mut *tx)
            .await?;
        let canonical = settle(&alias, &asked_canonical, hop.as_deref())?;

        let row = sqlx::query(PUT_SQL)
            .bind(tenant)
            .bind(&a.namespace)
            .bind(&alias)
            .bind(&canonical)
            .bind(a.since)
            .bind(a.until)
            .bind(&a.origin)
            .fetch_optional(&mut *tx)
            .await?;

        let Some(row) = row else {
            // The `DO UPDATE ... WHERE` declined: a derived alias met a manual one.
            let held = sqlx::query(HELD_SQL)
                .bind(tenant)
                .bind(&a.namespace)
                .bind(&alias)
                .fetch_optional(&mut *tx)
                .await?;
            let held = held.as_ref().map(alias_from_row).ok_or_else(|| {
                DomainError::internal("the alias write was declined and the row it lost to is gone")
            })?;
            if held.canonical == canonical {
                // The two agree. An ingest that keeps proposing a name the owner already recorded
                // is not a conflict, and erroring on it would fill the queue with noise.
                tx.commit().await?;
                return Ok(held);
            }
            return Err(DomainError::conflict(format!(
                "{alias:?} is already recorded by hand as a name for {:?}. \
                 A derived alias does not overwrite one the owner stated.",
                held.canonical
            )));
        };

        // Whatever pointed at the name just demoted follows it. This is the write half of the
        // one-hop invariant; `group` is the read that depends on it.
        sqlx::query(REPOINT_SQL)
            .bind(tenant)
            .bind(&a.namespace)
            .bind(&alias)
            .bind(&canonical)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(alias_from_row(&row))
    }

    async fn group(&self, tenant: &str, namespace: &str, name: &str) -> Result<Vec<String>> {
        let name = normalize_name(name)?;
        let rows = sqlx::query(GROUP_SQL)
            .bind(tenant)
            .bind(namespace)
            .bind(&name)
            .fetch_all(&self.pool)
            .await?;
        let names = rows.iter().map(|r| r.get::<String, _>("name")).collect();
        Ok(ensure_present(&name, names))
    }

    async fn list(&self, tenant: &str, namespace: Option<&str>) -> Result<Vec<Alias>> {
        let rows = sqlx::query(LIST_SQL).bind(tenant).bind(namespace).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(alias_from_row).collect())
    }

    /// Drop one name from its group.
    ///
    /// The facts that mention the name are untouched and stay searchable under it. Forgetting an
    /// alias says the two names never denoted the same thing; it does not say the older name was
    /// never written down.
    async fn forget(&self, tenant: &str, namespace: &str, alias: &str) -> Result<bool> {
        let alias = normalize_name(alias)?;
        let done = sqlx::query(
            "DELETE FROM entity_alias WHERE tenant_id = $1 AND namespace = $2 AND alias = $3",
        )
        .bind(tenant)
        .bind(namespace)
        .bind(&alias)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(done > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kind prefix is compared literally. `LIKE` would read `_` or `%` in it as a wildcard.
    #[test]
    fn the_group_scope_matches_its_prefix_literally() {
        assert!(GROUP_SQL.contains("starts_with(a.namespace, scope.prefix)"));
        assert!(!GROUP_SQL.contains("LIKE"));
    }

    #[test]
    fn a_name_cannot_be_its_own_alias() {
        assert!(settle("warden", "warden", None).is_err());
    }

    #[test]
    fn lowercasing_makes_the_self_reference_visible() {
        // "Warden" and "warden" are one name, so this pair has to be refused for the same reason
        // the identical pair is. Both sides go through normalize_name before settle sees them.
        let alias = normalize_name(" Warden ").unwrap();
        let canonical = normalize_name("WARDEN").unwrap();
        assert_eq!(alias, "warden");
        assert_eq!(canonical, "warden");
        assert!(settle(&alias, &canonical, None).is_err());
    }

    #[test]
    fn an_empty_name_is_refused_rather_than_stored() {
        assert!(normalize_name("   ").is_err());
    }

    #[test]
    fn recording_the_reverse_of_an_existing_alias_is_refused() {
        // warden -> quill is on the table. Recording quill -> warden would make each the other's
        // canonical, and a walk over the pair would never terminate.
        let err = settle("quill", "warden", Some("quill")).unwrap_err();
        assert!(err.client_message().contains("already one group"));
    }

    #[test]
    fn a_rename_of_a_canonical_name_flattens_to_the_new_one() {
        // quill -> lumen, where nothing points away from lumen yet.
        assert_eq!(settle("quill", "lumen", None).unwrap(), "lumen");
        // warden -> quill, recorded after quill was demoted, lands on lumen directly rather than
        // creating the second hop that would force a recursive read.
        assert_eq!(settle("warden", "quill", Some("lumen")).unwrap(), "lumen");
    }

    #[test]
    fn a_name_with_nothing_recorded_still_comes_back() {
        assert_eq!(ensure_present("lumen", vec![]), vec!["lumen".to_string()]);
    }

    #[test]
    fn a_name_already_in_the_group_is_not_repeated() {
        let group = vec!["lumen".to_string(), "warden".to_string()];
        assert_eq!(ensure_present("lumen", group.clone()), group);
    }

    #[test]
    fn the_group_read_is_total_and_puts_the_canonical_name_first() {
        // The COALESCE is the whole never-empty contract: drop it and an unknown name returns no
        // rows, the caller expands a search into nothing, and the store reports it knows nothing
        // about a name it holds facts under.
        assert!(GROUP_SQL.contains("COALESCE("));
        assert!(GROUP_SQL.contains("0 AS hop"));
        assert!(GROUP_SQL.contains("ORDER BY hop, name"));
    }

    #[test]
    fn the_group_read_never_recurses() {
        // The cycle guard is the one-hop invariant, held at write time. A RECURSIVE clause here
        // would mean the invariant had been abandoned and the read had inherited the cycle.
        assert!(!GROUP_SQL.to_uppercase().contains("RECURSIVE"));
    }

    #[test]
    fn a_derived_alias_cannot_overwrite_a_manual_one() {
        assert!(PUT_SQL.contains("EXCLUDED.origin <> 'derived' OR entity_alias.origin = 'derived'"));
    }

    const MIGRATION: &str = include_str!("../../../migrations/20260821000014_alias.sql");

    #[test]
    fn the_table_refuses_the_trivial_cycle_and_the_inverted_period() {
        assert!(MIGRATION.contains("CHECK (alias <> canonical)"));
        assert!(MIGRATION.contains("CHECK (since IS NULL OR until IS NULL OR since <= until)"));
    }

    #[test]
    fn the_group_lookup_has_an_index_behind_it() {
        assert!(MIGRATION.contains("entity_alias (tenant_id, namespace, canonical)"));
        assert!(MIGRATION.contains("PRIMARY KEY (tenant_id, namespace, alias)"));
    }

    #[test]
    fn the_origin_vocabulary_matches_the_port() {
        // `registry_alias` carries a third value, 'rejected-write', that belongs to key validation
        // and has no meaning for a rename. Copying its CHECK across would admit it here.
        assert!(MIGRATION.contains("CHECK (origin IN ('manual', 'derived'))"));
    }
}
