//! The integration suite, as one test binary.
//!
//! Every file directly under `tests/` used to build as a binary of its own, and each one linked the
//! whole server crate again. One binary links it once. Each old file is a module here under its old
//! name, so `--test integration <filter>` became `--test it integration::<filter>`.
//!
//! The modules now share a process, and with it the environment, the digest cache in
//! `services::bootstrap` and every other static. `common::SERIAL` is what keeps one module's setup
//! from reading another's `AUTH_TOKENS`; see the comment on it before adding a module that sets an
//! environment variable or loads the config.

// Declared first and with `#[macro_use]`: `step!` reaches the modules below by textual scope.
#[path = "../common/mod.rs"]
#[macro_use]
mod common;

mod archive_merge;
mod archive_restore;
mod bootstrap_ranking;
mod cleanup;
mod cleanup_unreject;
mod client_label;
mod conflict_pairs;
mod console;
mod console_cleanup;
mod dump_console;
mod embedding_command;
mod embedding_migration;
mod ingest;
mod integration;
mod mcp_brief;
mod mcp_capability;
mod mcp_source;
mod mcp_tool_annotations;
mod migration_lock;
mod oauth_refresh_owner;
mod permissions_doc;
mod recall_event;
mod recall_settings;
mod review_queue;
mod review_queue_mcp;
mod search_touch_hot;
mod setup_fails_closed;
mod shared_process;
mod thresholds_per_model;
mod trigger_restore;
