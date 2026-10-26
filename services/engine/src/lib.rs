//! Core of ops-support: workspace configuration, code and Docker parsing, graph building,
//! the Memgraph store, and the log, datastore and LSP tooling used by `ops` and `ops-mcp`.
//!
//! Modules are added by the implementation plans in `docs/superpowers/plans/`.

pub mod config;
pub mod redact;
