//! Permission gates — CLI y/n/a interaction (cli.ts createPermissionHandler)
//! and the headless allow-all.

use async_trait::async_trait;
use serde_json::Value as Json;
use std::sync::Mutex;

use crate::core::types::{PermissionDecision, PermissionGate};

/// Headless: always allow (cli.ts passes rl=null; headless.ts default).
pub struct AllowAllGate;

#[async_trait]
impl PermissionGate for AllowAllGate {
    async fn decide(&self, _t: &str, _i: &Json, _m: &str) -> PermissionDecision {
        PermissionDecision::allow()
    }
}

/// Interactive y/n/a gate with per-session "always allow" memory.
/// Blocking readline happens on a blocking thread (tokio).
pub struct InteractiveGate {
    always_allowed: Mutex<std::collections::HashSet<String>>,
}

impl InteractiveGate {
    pub fn new() -> Self {
        InteractiveGate { always_allowed: Mutex::new(std::collections::HashSet::new()) }
    }

    pub fn always_allows(&self, tool: &str) -> bool {
        self.always_allowed.lock().unwrap().contains(tool)
    }
}

impl Default for InteractiveGate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PermissionGate for InteractiveGate {
    async fn decide(&self, tool: &str, _input: &Json, message: &str) -> PermissionDecision {
        if self.always_allows(tool) {
            return PermissionDecision::allow();
        }

        let prompt = format!(
            "\n  {} {} {}\n  {} > ",
            crate::cli::format::gold("⚡"),
            crate::cli::format::bold("Allow"),
            message,
            crate::cli::format::dim("[y] Yes  [n] No  [a] Always")
        );

        let tool = tool.to_string();
        let answer = tokio::task::spawn_blocking(move || -> String {
            use std::io::Write;
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return "y".to_string();
            }
            line.trim().to_lowercase()
        })
        .await
        .unwrap_or_default();

        match answer.as_str() {
            "n" | "no" => PermissionDecision::deny("User denied"),
            "a" | "always" => {
                self.always_allowed.lock().unwrap().insert(tool);
                PermissionDecision::allow()
            }
            _ => PermissionDecision::allow(), // default y
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allow_all_headless() {
        let g = AllowAllGate;
        let d = g.decide("Bash", &serde_json::json!({}), "x").await;
        assert_eq!(d.behavior, crate::core::types::PermissionBehavior::Allow);
    }

    #[tokio::test]
    async fn always_allowed_memory() {
        let g = InteractiveGate::new();
        assert!(!g.always_allows("Edit"));
        g.always_allowed.lock().unwrap().insert("Edit".into());
        assert!(g.always_allows("Edit"));
    }
}
