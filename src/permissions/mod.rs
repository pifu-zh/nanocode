//! Permission rules — Rust port of `src/permissions/rules.ts` +
//! `engine.ts` decision flow (wired into the agent loop per PORTING_PLAN D1:
//! the TS engine was implemented but never called from the loop).

use std::path::Path;

use serde::Deserialize;
use serde_json::Value as Json;

use crate::core::types::{PermissionBehavior, PermissionRule, RuleSource};

// ---------------------------------------------------------------------------
// Settings loading (rules.ts)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SettingsFile {
    #[serde(default)]
    permissions: Option<SettingsPermissions>,
}

#[derive(Deserialize)]
struct SettingsPermissions {
    #[serde(default)]
    allow: Vec<SettingsRule>,
    #[serde(default)]
    deny: Vec<SettingsRule>,
}

#[derive(Deserialize)]
struct SettingsRule {
    tool: String,
    #[serde(default)]
    content: Option<String>,
}

fn extract_rules(settings: Option<SettingsFile>, source: RuleSource) -> Vec<PermissionRule> {
    let Some(settings) = settings else { return Vec::new() };
    let Some(perms) = settings.permissions else { return Vec::new() };
    let mut rules = Vec::new();
    for entry in perms.allow {
        rules.push(PermissionRule {
            tool: entry.tool,
            content: entry.content,
            behavior: PermissionBehavior::Allow,
            source,
        });
    }
    for entry in perms.deny {
        rules.push(PermissionRule {
            tool: entry.tool,
            content: entry.content,
            behavior: PermissionBehavior::Deny,
            source,
        });
    }
    rules
}

fn read_settings(path: &Path) -> Option<SettingsFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// .nanocode/ takes priority over .claude/ (both loaded, in that order).
const CONFIG_DIRS: &[&str] = &[".nanocode", ".claude"];

pub fn load_project_rules(cwd: &Path) -> Vec<PermissionRule> {
    let mut rules = Vec::new();
    for dir in CONFIG_DIRS {
        let path = cwd.join(dir).join("settings.json");
        rules.extend(extract_rules(read_settings(&path), RuleSource::Project));
    }
    rules
}

pub fn load_user_rules() -> Vec<PermissionRule> {
    let mut rules = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for dir in CONFIG_DIRS {
            let path = home.join(dir).join("settings.json");
            rules.extend(extract_rules(read_settings(&path), RuleSource::User));
        }
    }
    rules
}

// ---------------------------------------------------------------------------
// Rule matching (rules.ts globMatch / matchRule)
// ---------------------------------------------------------------------------

/// Simplified glob: `**` = anything, `*` = anything but `/`, `?` = one char.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                }
                regex.push_str(".*");
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '.' | '(' | ')' | '[' | ']' | '{' | '}' | '+' | '^' | '$' | '|' | '\\' => {
                regex.push('\\');
                regex.push(ch);
            }
            other => regex.push(other),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex).map(|re| re.is_match(value)).unwrap_or(false)
}

fn match_tool_name(tool_name: &str, rule_pattern: &str) -> bool {
    if tool_name == rule_pattern {
        return true;
    }
    // MCP prefix: "mcp__server__" matches "mcp__server__toolA"
    if rule_pattern.starts_with("mcp__") && rule_pattern.ends_with("__") {
        return tool_name.starts_with(rule_pattern);
    }
    if rule_pattern.contains('*') {
        return glob_match(rule_pattern, tool_name);
    }
    false
}

fn extract_command(input: &Json) -> String {
    ["command", "cmd"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default()
}

fn extract_file_path(input: &Json) -> String {
    ["file_path", "path", "filePath"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default()
}

fn match_content(tool_name: &str, input: &Json, pattern: &str) -> bool {
    let lower = tool_name.to_lowercase();
    if lower == "bash" || lower == "shell" {
        return glob_match(pattern, &extract_command(input));
    }
    let file_path = extract_file_path(input);
    if !file_path.is_empty() {
        return glob_match(pattern, &file_path);
    }
    // Generic: any string field
    if let Some(obj) = input.as_object() {
        for v in obj.values() {
            if let Some(s) = v.as_str() {
                if glob_match(pattern, s) {
                    return true;
                }
            }
        }
    }
    false
}

pub fn match_rule(tool_name: &str, input: &Json, rule: &PermissionRule) -> bool {
    if !match_tool_name(tool_name, &rule.tool) {
        return false;
    }
    match &rule.content {
        None => true,
        Some(content) => match_content(tool_name, input, content),
    }
}

// ---------------------------------------------------------------------------
// Engine decision flow (engine.ts checkPermission)
// ---------------------------------------------------------------------------

/// Rules loaded for one decision: session > project > user priority order.
#[derive(Clone, Default)]
pub struct RuleSet {
    pub session: Vec<PermissionRule>,
}

impl RuleSet {
    pub fn load(cwd: &Path, session: Vec<PermissionRule>) -> RuleSet {
        let _ = cwd;
        RuleSet { session }
    }

    fn all(&self, cwd: &Path) -> Vec<PermissionRule> {
        let mut all = self.session.clone();
        all.extend(load_project_rules(cwd));
        all.extend(load_user_rules());
        all
    }

    /// 8-step decision flow (engine.ts). Returns None when the decision
    /// falls through to the interactive gate.
    pub fn check(
        &self,
        tool_name: &str,
        input: &Json,
        tool_is_read_only: bool,
        mode: crate::core::types::PermissionMode,
        cwd: &Path,
    ) -> Option<PermissionBehavior> {
        use crate::core::types::PermissionMode;
        let rules = self.all(cwd);

        // 1. deny rules first
        for rule in &rules {
            if rule.behavior == PermissionBehavior::Deny && match_rule(tool_name, input, rule) {
                return Some(PermissionBehavior::Deny);
            }
        }

        // 2. read-only tool
        if tool_is_read_only {
            return Some(PermissionBehavior::Allow);
        }

        // 3. bypass mode
        if mode == PermissionMode::BypassPermissions {
            return Some(PermissionBehavior::Allow);
        }

        // 4. allow rules
        for rule in &rules {
            if rule.behavior == PermissionBehavior::Allow && match_rule(tool_name, input, rule) {
                return Some(PermissionBehavior::Allow);
            }
        }

        // 5. acceptEdits
        if mode == PermissionMode::AcceptEdits {
            return Some(PermissionBehavior::Allow);
        }

        // Plan-mode write denial and the interactive ask fall through to the
        // caller (agent.ts path A), preserving its exact user-visible copy.
        None
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/permissions/{engine,modes,path-validation,rules}
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::PermissionMode;

    fn rule(tool: &str, content: Option<&str>, behavior: PermissionBehavior) -> PermissionRule {
        PermissionRule { tool: tool.into(), content: content.map(String::from), behavior, source: RuleSource::Session }
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("*.ts", "foo.ts"));
        assert!(!glob_match("*.ts", "a/b.ts"));
        assert!(glob_match("**", "a/b/c.ts"));
        assert!(glob_match("src/**", "src/a/b.ts"));
        assert!(glob_match("?.txt", "a.txt"));
        assert!(!glob_match("?.txt", "ab.txt"));
        assert!(!glob_match("*.ts", "foo.rs"));
    }

    #[test]
    fn tool_name_matching() {
        assert!(match_tool_name("Bash", "Bash"));
        assert!(match_tool_name("mcp__srv__toolA", "mcp__srv__"));
        assert!(match_tool_name("mcp__srv__toolA", "mcp__srv__*"));
        assert!(!match_tool_name("Bash", "mcp__srv__"));
    }

    #[test]
    fn content_matching_by_tool_type() {
        let bash_input = serde_json::json!({"command": "npm install x"});
        let r = rule("Bash", Some("npm install *"), PermissionBehavior::Allow);
        assert!(match_rule("Bash", &bash_input, &r));

        let file_input = serde_json::json!({"file_path": "/proj/src/a.rs"});
        let r = rule("Edit", Some("/proj/**"), PermissionBehavior::Allow);
        assert!(match_rule("Edit", &file_input, &r));
    }

    #[test]
    fn engine_deny_wins_over_allow() {
        let rs = RuleSet {
            session: vec![
                rule("Bash", Some("git push*"), PermissionBehavior::Allow),
                rule("Bash", Some("git push*"), PermissionBehavior::Deny),
            ],
        };
        let input = serde_json::json!({"command": "git push origin main"});
        assert_eq!(
            rs.check("Bash", &input, false, PermissionMode::Default, Path::new("/")),
            Some(PermissionBehavior::Deny)
        );
    }

    #[test]
    fn engine_read_only_short_circuits() {
        let rs = RuleSet { session: vec![] };
        let input = serde_json::json!({});
        assert_eq!(
            rs.check("Read", &input, true, PermissionMode::Default, Path::new("/")),
            Some(PermissionBehavior::Allow)
        );
    }

    #[test]
    fn engine_bypass_allows() {
        let rs = RuleSet { session: vec![] };
        assert_eq!(
            rs.check("Bash", &serde_json::json!({}), false, PermissionMode::BypassPermissions, Path::new("/")),
            Some(PermissionBehavior::Allow)
        );
    }

    #[test]
    fn engine_allow_rule_before_mode() {
        let rs = RuleSet {
            session: vec![rule("Bash", Some("npm *"), PermissionBehavior::Allow)],
        };
        assert_eq!(
            rs.check("Bash", &serde_json::json!({"command": "npm install"}), false, PermissionMode::Default, Path::new("/")),
            Some(PermissionBehavior::Allow)
        );
    }

    #[test]
    fn engine_leaves_plan_mode_to_caller() {
        let rs = RuleSet { session: vec![] };
        // Plan denial is handled by the caller's path-A fallback (copy parity)
        assert_eq!(
            rs.check("Edit", &serde_json::json!({}), false, PermissionMode::Plan, Path::new("/")),
            None
        );
    }

    #[test]
    fn engine_falls_through_to_ask() {
        let rs = RuleSet { session: vec![] };
        assert_eq!(
            rs.check("Bash", &serde_json::json!({"command": "make"}), false, PermissionMode::Default, Path::new("/")),
            None
        );
    }

    #[test]
    fn accept_edits_allows_writes() {
        let rs = RuleSet { session: vec![] };
        assert_eq!(
            rs.check("Edit", &serde_json::json!({}), false, PermissionMode::AcceptEdits, Path::new("/")),
            Some(PermissionBehavior::Allow)
        );
    }
}
