//! CLI support modules: ANSI formatting, cost tracking, spinner, commands,
//! REPL. Terminal output is a behavior spec (BEHAVIOR.md §2/§3/§6).

pub mod commands;
pub mod format;
pub mod permission;
pub mod session;
pub mod spinner;

pub use session::{run_one_shot, run_repl};
