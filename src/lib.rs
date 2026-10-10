//! lumberroom: a durable memory control plane.
//!
//! Exposed as a library so the integration suite can exercise the services directly against a
//! real database, the same shape the previous build used.

pub mod adapters;
pub mod authserver;
pub mod build_info;
pub mod command;
pub mod config;
pub mod console;
pub mod crypto;
pub mod domain;
pub mod http;
pub mod mcp;
pub mod ports;
pub mod services;
