//! nanocode — Rust agent runtime (port of Lyt060814/nanocode).
//!
//! Module tree mirrors the TS layout; layering discipline per RUST_DESIGN.md:
//! core (types/errors/api/agent) ← tools ← cli. `pub(crate)` keeps internals
//! internal until a module graduates to public API.

pub mod core;
pub mod context;
pub mod permissions;
pub mod prompt;
pub mod cli;
pub mod mcp;
pub mod skills;
pub mod files;
pub mod tools;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

