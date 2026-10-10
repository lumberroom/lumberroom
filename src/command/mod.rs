//! Operator subcommands of `lumberroom-server` that drive the store directly, with the server's own
//! configuration and `DATABASE_URL`. Shell access to the host is their authority, so none of them
//! has an HTTP route (decision 0027).

pub mod embeddings;
