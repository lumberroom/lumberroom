//! The harness's own promise: once a database answers, a setup step that fails fails the test.
//!
//! Every integration binary used to end each setup step in `.ok()?` or print "skipping" and return
//! `None`, and the test then reported ok with nothing asserted. A test database holding a migration
//! this checkout does not have failed `migrate` that way on 24 September and 12 October 2026, and a
//! mutation that should have broken a test passed both times.
//!
//! Only a database that never answered may skip, and `LUMBERROOM_TEST_REQUIRE_DB` takes even that
//! away. `scripts/cargo.sh` and CI set it, since both always attach a database.

use std::any::Any;

use lumberroom_server::adapters::postgres;
use sqlx::AssertSqlSafe;

use crate::common;

/// A database of its own, so planting a foreign migration never touches `lumberroom_rust_test`.
const PROBE_DB: &str = "lumberroom_rust_test_fails_closed";

/// A version no checkout of this repository will ever ship.
const FOREIGN: i64 = 20991231900001;

fn panic_text(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(s) => *s,
        Err(payload) => payload.downcast::<&str>().map(|s| s.to_string()).unwrap_or_default(),
    }
}

#[test]
fn a_database_that_never_answered_skips_unless_one_is_required() {
    assert!(common::skip_or_fail::<()>(false, "connecting", "refused").is_none());

    let failed =
        std::panic::catch_unwind(|| common::skip_or_fail::<()>(true, "connecting", "refused"))
            .expect_err("a required database must not skip");
    let text = panic_text(failed);
    assert!(text.contains("connecting failed"), "{text}");
    assert!(text.contains("LUMBERROOM_TEST_REQUIRE_DB"), "{text}");
}

#[test]
fn the_switch_reads_unset_empty_and_zero_as_off_and_anything_else_as_on() {
    assert!(!common::requires_db(None));
    assert!(!common::requires_db(Some("")));
    assert!(!common::requires_db(Some("0")));
    assert!(common::requires_db(Some("1")));
}

/// `scripts/cargo.sh` and CI set the switch by this literal name, and `scripts/cargo-sh-test.sh`
/// pins the shell side. A rename here alone would leave both setting a variable nobody reads.
#[test]
fn the_switch_keeps_the_name_the_scripts_set() {
    assert_eq!(common::REQUIRE_DB_VAR, "LUMBERROOM_TEST_REQUIRE_DB");
}

#[test]
fn only_already_exists_and_a_catalog_collision_count_as_losing_the_create_race() {
    assert!(common::lost_creation_race(Some("42P04")), "duplicate_database");
    assert!(common::lost_creation_race(Some("23505")), "unique_violation on pg_database");
    assert!(!common::lost_creation_race(Some("42501")), "insufficient_privilege");
    assert!(!common::lost_creation_race(Some("53300")), "too_many_connections");
    assert!(!common::lost_creation_race(None), "an error with no SQLSTATE");
}

#[test]
fn a_step_that_fails_after_the_database_answered_panics_rather_than_returning_none() {
    fn setup() -> Option<()> {
        step!("truncating the test database", Err::<(), _>("relation does not exist"));
        Some(())
    }
    let failed = std::panic::catch_unwind(setup).expect_err("a failed step must not skip");
    let text = panic_text(failed);
    assert!(text.contains("truncating the test database failed"), "{text}");
}

#[tokio::test]
async fn a_migration_this_checkout_lacks_fails_the_test_and_names_the_fix() {
    let Some((admin, base)) = common::connect_admin().await else {
        common::skipping();
        return;
    };
    // DDL takes no bind parameter. Audited: PROBE_DB is a compile-time constant.
    let drop_probe = format!("DROP DATABASE IF EXISTS {PROBE_DB} WITH (FORCE)");
    sqlx::raw_sql(AssertSqlSafe(drop_probe.clone())).execute(&admin).await.unwrap();
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {PROBE_DB}")))
        .execute(&admin)
        .await
        .unwrap();
    let url = format!("{base}/{PROBE_DB}");
    let pool = postgres::connect(&url).await.unwrap();
    postgres::migrate(&pool).await.expect("migrations apply to an empty database");

    // What a run from another branch, or a fork with its own migrations, leaves behind.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES ($1, 'from another checkout', true, '\\x00'::bytea, 0)",
    )
    .bind(FOREIGN)
    .execute(&pool)
    .await
    .unwrap();

    pool.close().await;

    // A thread and runtime of their own, so the panic comes back as a value this test can read.
    // tokio::spawn would need the migrate future to be Send, and sqlx's migrator future is not.
    // The pool opens inside that runtime because a connection stays bound to the one it began on.
    let outcome = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let pool = postgres::connect(&url).await.unwrap();
            common::migrate(&pool, PROBE_DB).await;
        });
    })
    .join();

    sqlx::raw_sql(AssertSqlSafe(drop_probe)).execute(&admin).await.unwrap();

    let text = panic_text(outcome.expect_err("a foreign migration must fail setup"));
    assert!(text.contains(&format!("VersionMissing({FOREIGN})")), "{text}");
    assert!(text.contains(&format!("DROP DATABASE {PROBE_DB} WITH (FORCE)")), "{text}");
}
