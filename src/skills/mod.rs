//! Skills system — Rust port of `src/skills/{types,loader,skill-tool}.ts`.
//! Skills are SKILL.md prompt templates from .nanocode/skills/ or
//! .claude/skills/, invoked inline or forked as sub-agents.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::core::types::{SubAgentRunner, ToolContext, ToolResult};
use crate::tools::ToolDef;

const SKILL_FILE: &str = "SKILL.md";
const CONFIG_DIRS: &[&str] = &[".nanocode", ".claude"];

// ---------------------------------------------------------------------------
// Types (skills/types.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub argument_names: Option<Vec<String>>,
    pub allowed_tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub user_invocable: bool,
    pub context: SkillContext,
    pub agent: Option<String>,
    pub paths: Option<Vec<String>>,
    pub skill_root: Option<PathBuf>,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillContext {
    #[default]
    Inline,
    Fork,
}

impl SkillDefinition {
    /// Expand template variables: $ARGUMENTS, $1..$9, named args,
    /// ${NANOCODE_SKILL_DIR}/${CLAUDE_SKILL_DIR} (loader.ts createPromptExpander).
    pub fn expand_prompt(&self, args: &str) -> String {
        let mut result = self.body.clone();

        if let Some(root) = &self.skill_root {
            let root_str = root.to_string_lossy().to_string();
            result = result.replace("${NANOCODE_SKILL_DIR}", &root_str);
            result = result.replace("$NANOCODE_SKILL_DIR", &root_str);
            result = result.replace("${CLAUDE_SKILL_DIR}", &root_str);
            result = result.replace("$CLAUDE_SKILL_DIR", &root_str);
        }

        result = result.replace("$ARGUMENTS", args);

        let positional = split_args(args);

        if let Some(names) = &self.argument_names {
            for (i, name) in names.iter().enumerate() {
                let value = positional.get(i).cloned().unwrap_or_default();
                result = replace_word(&result, &format!("${name}"), &value);
            }
        }

        for i in 0..9 {
            let value = positional.get(i).cloned().unwrap_or_default();
            result = replace_word(&result, &format!("${}", i + 1), &value);
        }

        result
    }
}

fn replace_word(text: &str, pattern: &str, value: &str) -> String {
    // Replace $name / $N only when not followed by a word char (TS \b).
    let mut out = String::new();
    let mut rest = text;
    while let Some(pos) = rest.find(pattern) {
        let after = &rest[pos + pattern.len()..];
        let boundary_ok = after
            .chars()
            .next()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        out.push_str(&rest[..pos]);
        if boundary_ok {
            out.push_str(value);
            rest = after;
        } else {
            out.push_str(pattern);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Split args respecting double quotes (loader.ts splitArgs).
fn split_args(args: &str) -> Vec<String> {
    if args.trim().is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in args.chars() {
        if ch == '"' {
            in_quote = !in_quote;
            continue;
        }
        if ch == ' ' && !in_quote {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

// ---------------------------------------------------------------------------
// Frontmatter parsing (simple YAML subset — loader.ts parseSimpleYaml)
// ---------------------------------------------------------------------------

pub fn parse_frontmatter(content: &str) -> (HashMap<String, FrontValue>, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (HashMap::new(), content.to_string());
    }
    let after_first = &trimmed[3..];
    let Some(closing) = after_first.find("\n---") else {
        return (HashMap::new(), content.to_string());
    };
    let yaml = after_first[..closing].trim();
    let body = after_first[closing + 4..].trim_start().to_string();

    (parse_simple_yaml(yaml), body)
}

#[derive(Debug, Clone)]
pub enum FrontValue {
    Str(String),
    Bool(bool),
    Num(f64),
    List(Vec<String>),
}

impl FrontValue {
    fn as_str(&self) -> Option<&str> {
        match self {
            FrontValue::Str(s) => Some(s),
            _ => None,
        }
    }
    fn as_bool(&self) -> Option<bool> {
        match self {
            FrontValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
    fn as_list(&self) -> Option<Vec<String>> {
        match self {
            FrontValue::List(l) => Some(l.clone()),
            FrontValue::Str(s) => Some(vec![s.clone()]),
            _ => None,
        }
    }
}

fn unquote(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn parse_simple_yaml(yaml: &str) -> HashMap<String, FrontValue> {
    let mut result = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_array: Option<Vec<String>> = None;

    for raw_line in yaml.split('\n') {
        let line = raw_line.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Array item continuation
        if let Some(item) = line.trim_start().strip_prefix("- ") {
            if let Some(_key) = &current_key {
                current_array.get_or_insert_with(Vec::new).push(unquote(item.trim()));
                continue;
            }
        }

        // Flush pending array handled above; parse key: value
        let Some((key, raw_value)) = line.split_once(':') else { continue };
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || key.is_empty()
        {
            continue;
        }
        let key = key.trim().to_string();
        let value = raw_value.trim();

        if value.is_empty() {
            current_key = Some(key);
            current_array = Some(Vec::new());
            continue;
        }

        if value.starts_with('[') && value.ends_with(']') {
            let inner = &value[1..value.len() - 1];
            let list: Vec<String> = inner
                .split(',')
                .map(|s| unquote(s.trim()))
                .filter(|s| !s.is_empty())
                .collect();
            result.insert(key, FrontValue::List(list));
            current_array = None;
            current_key = None;
            continue;
        }

        let lower = value.to_lowercase();
        if lower == "true" || lower == "yes" {
            result.insert(key, FrontValue::Bool(true));
        } else if lower == "false" || lower == "no" {
            result.insert(key, FrontValue::Bool(false));
        } else if value.parse::<f64>().is_ok() && !value.is_empty() {
            result.insert(key, FrontValue::Num(value.parse::<f64>().unwrap()));
        } else {
            result.insert(key, FrontValue::Str(unquote(value)));
        }
        current_array = None;
        current_key = None;
    }

    // Flush trailing array
    if let (Some(key), Some(arr)) = (current_key, current_array) {
        result.insert(key, FrontValue::List(arr));
    }

    result
}

/// Parse a SKILL.md into a SkillDefinition (loader.ts parseSkillFile).
pub fn parse_skill_file(path: &Path) -> Option<SkillDefinition> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    let (frontmatter, body) = parse_frontmatter(&content);
    let skill_dir = path.parent()?;
    let dir_name = skill_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let name = frontmatter
        .get("name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or(dir_name);
    let description = frontmatter
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("Skill: {name}"));

    let user_invocable = frontmatter
        .get("user-invocable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let context = match frontmatter.get("context").and_then(|v| v.as_str()) {
        Some("fork") => SkillContext::Fork,
        _ => SkillContext::Inline,
    };

    Some(SkillDefinition {
        name,
        description,
        when_to_use: frontmatter.get("when_to_use").and_then(|v| v.as_str()).map(String::from),
        argument_hint: frontmatter.get("argument-hint").and_then(|v| v.as_str()).map(String::from),
        argument_names: frontmatter.get("arguments").and_then(|v| v.as_list()),
        allowed_tools: frontmatter.get("allowed-tools").and_then(|v| v.as_list()),
        model: frontmatter.get("model").and_then(|v| v.as_str()).map(String::from),
        user_invocable,
        context,
        agent: frontmatter.get("agent").and_then(|v| v.as_str()).map(String::from),
        paths: frontmatter.get("paths").and_then(|v| v.as_list()),
        skill_root: Some(skill_dir.to_path_buf()),
        body,
    })
}

/// Discovery: walk cwd → home checking .nanocode/skills/ and .claude/skills/
/// at each level, then user-level. First skill with a given (lowercased) name
/// wins (project shadows user) — loader.ts loadAllSkills.
pub fn load_all_skills(cwd: &Path) -> Vec<SkillDefinition> {
    let mut seen: HashMap<String, SkillDefinition> = HashMap::new();

    let mut insert = |skill: SkillDefinition| {
        let key = skill.name.to_lowercase();
        seen.entry(key).or_insert(skill);
    };

    // Walk upward from cwd
    let mut current = cwd.to_path_buf();
    loop {
        for dir in CONFIG_DIRS {
            let skills_dir = current.join(dir).join("skills");
            insert_all_from(&skills_dir, &mut insert);
        }
        match current.parent() {
            Some(p) if p != current => current = p.to_path_buf(),
            _ => break,
        }
    }

    // User level
    if let Some(home) = dirs::home_dir() {
        for dir in CONFIG_DIRS {
            let skills_dir = home.join(dir).join("skills");
            insert_all_from(&skills_dir, &mut insert);
        }
    }

    seen.into_values().collect()
}

fn insert_all_from(skills_dir: &Path, insert: &mut impl FnMut(SkillDefinition)) {
    let Ok(entries) = std::fs::read_dir(skills_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(skill) = parse_skill_file(&path.join(SKILL_FILE)) {
                insert(skill);
            }
        }
    }
}

pub fn format_skill_listing(skills: &[SkillDefinition]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut lines = vec!["Available skills (invoke via the Skill tool):".to_string(), String::new()];
    for skill in skills {
        lines.push(format!("  - {}: {}", skill.name, skill.description));
        if let Some(w) = &skill.when_to_use {
            lines.push(format!("    When to use: {w}"));
        }
        if let Some(h) = &skill.argument_hint {
            lines.push(format!("    Arguments: {h}"));
        }
        if skill.context == SkillContext::Fork {
            lines.push("    Execution: forked sub-agent".to_string());
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Skill tool (skills/skill-tool.ts)
// ---------------------------------------------------------------------------

pub struct SkillTool {
    pub skills: Arc<RwLock<Vec<SkillDefinition>>>,
    pub sub_agent: Arc<RwLock<Option<Arc<dyn SubAgentRunner>>>>,
}

fn skill_input_schema() -> Json {
    serde_json::json!({
        "type": "object",
        "properties": {
            "skill": {"type": "string", "description": "The name of the skill to invoke"},
            "args": {"type": "string", "description": "Arguments to pass to the skill. Substituted into the skill template as $ARGUMENTS, $1, $2, etc."}
        },
        "required": ["skill"]
    })
}

/// Robust name/args extraction (skill-tool.ts tolerant input handling).
fn extract_skill_call(input: &Json) -> (String, String) {
    let obj = input.as_object();
    let name = obj
        .map(|o| {
            ["skill", "name"]
                .iter()
                .find_map(|k| o.get(*k).and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let args = obj
        .map(|o| {
            if let Some(v) = o.get("args").and_then(|v| v.as_str()) {
                v.to_string()
            } else if let Some(v) = o.get("arguments").and_then(|v| v.as_str()) {
                v.to_string()
            } else {
                // Extra fields joined as args
                o.iter()
                    .filter(|(k, _)| !["skill", "name", "args", "arguments"].contains(&k.as_str()))
                    .map(|(_, v)| match v {
                        Json::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        })
        .unwrap_or_default();

    (name, args)
}

fn find_skill<'a>(skills: &'a [SkillDefinition], name: &str) -> Option<&'a SkillDefinition> {
    let normalized = name.strip_prefix('/').unwrap_or(name).to_lowercase();
    skills
        .iter()
        .find(|s| s.name.to_lowercase() == normalized || s.name.to_lowercase() == name.to_lowercase())
}

fn skill_not_found(skills: &[SkillDefinition], requested: &str) -> String {
    if skills.is_empty() {
        return format!(
            "Skill \"{requested}\" not found. No skills are currently loaded.\nSkills are loaded from .nanocode/skills/ or .claude/skills/ directories."
        );
    }
    let available = skills
        .iter()
        .map(|s| format!("  - {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Skill \"{requested}\" not found. Available skills:\n{available}")
}

#[async_trait]
impl ToolDef for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self, _input: Option<&Json>) -> String {
        "Invoke a loaded skill by name. Skills are reusable prompt templates loaded from .nanocode/skills/ or .claude/skills/ directories. Use this tool when a task matches a loaded skill's purpose. Pass the skill name and any arguments.".into()
    }

    fn input_schema(&self) -> Json {
        skill_input_schema()
    }

    fn is_read_only(&self, _input: &Json) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Json) -> bool {
        false // forked skills may modify state
    }

    fn user_facing_name(&self, input: &Json) -> String {
        let (name, _) = extract_skill_call(input);
        format!("Skill({name})")
    }

    async fn call(&self, input: Json, _ctx: &ToolContext) -> ToolResult {
        let (name, args) = extract_skill_call(&input);
        if name.is_empty() {
            return ToolResult::err("Error: skill name is required.");
        }

        let skills = self.skills.read().unwrap().clone();
        let Some(skill) = find_skill(&skills, &name) else {
            return ToolResult::err(skill_not_found(&skills, &name));
        };

        let expanded = skill.expand_prompt(&args);

        if skill.context == SkillContext::Fork {
            let Some(runner) = self.sub_agent.read().unwrap().clone() else {
                return ToolResult::err(
                    "Error: Skill tool not properly initialized for forked execution.",
                );
            };
            let result = runner
                .run(expanded, skill.allowed_tools.clone(), None, 50)
                .await;
            ToolResult::ok(if result.is_empty() {
                "(Skill produced no output)".to_string()
            } else {
                result
            })
        } else {
            ToolResult::ok(expanded)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/skills/{loader,skill-tool}.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parsing() {
        let (fm, body) = parse_frontmatter("---\nname: my-skill\ndescription: Does things\nuser-invocable: false\narguments: [a, b]\n---\n\nBody here\n");
        assert_eq!(fm.get("name").and_then(|v| v.as_str()), Some("my-skill"));
        assert_eq!(fm.get("user-invocable").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            fm.get("arguments").and_then(|v| v.as_list()),
            Some(vec!["a".into(), "b".into()])
        );
        assert!(body.starts_with("Body here"));
    }

    #[test]
    fn frontmatter_missing_delimiters() {
        let (fm, body) = parse_frontmatter("no frontmatter here");
        assert!(fm.is_empty());
        assert_eq!(body, "no frontmatter here");
        let (fm2, _) = parse_frontmatter("---\nno closing");
        assert!(fm2.is_empty());
    }

    #[test]
    fn dash_array_syntax() {
        let (fm, _) = parse_frontmatter("---\nallowed-tools:\n  - Read\n  - Grep\n---\nbody");
        assert_eq!(
            fm.get("allowed-tools").and_then(|v| v.as_list()),
            Some(vec!["Read".into(), "Grep".into()])
        );
    }

    #[test]
    fn prompt_expansion() {
        let skill = SkillDefinition {
            name: "s".into(),
            skill_root: Some(PathBuf::from("/skills/s")),
            argument_names: Some(vec!["topic".into()]),
            body: "Study ${CLAUDE_SKILL_DIR} about $ARGUMENTS, first $topic then $1 and $2.".into(),
            ..Default::default()
        };
        let out = skill.expand_prompt("rust lifetimes extra");
        assert!(out.contains("/skills/s"));
        assert!(out.contains("about rust lifetimes extra,"));
        // TS semantics: $1..$9 map to the same positional slice ($1 = args[0])
        assert!(out.contains("first rust then rust and lifetimes."));
    }

    #[test]
    fn directory_name_fallback_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: desc here\n---\nbody",
        )
        .unwrap();
        let skill = parse_skill_file(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "desc here");

        assert!(parse_skill_file(&dir.path().join("nope/SKILL.md")).is_none());
    }

    #[test]
    fn load_all_dedupes_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join(".claude/skills");
        std::fs::create_dir_all(project.join("auth")).unwrap();
        std::fs::write(
            project.join("auth/SKILL.md"),
            "---\nname: Auth\ndescription: project version\n---\nbody",
        )
        .unwrap();
        // a second skill with same name lowercased elsewhere would lose
        let skills = load_all_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "project version");
    }

    fn tool_with(skills: Vec<SkillDefinition>) -> SkillTool {
        SkillTool {
            skills: Arc::new(RwLock::new(skills)),
            sub_agent: Arc::new(RwLock::new(None)),
        }
    }

    fn dummy_ctx() -> ToolContext {
        crate::tools::test_support::test_ctx(Path::new("/tmp"))
    }

    #[tokio::test]
    async fn inline_execution_and_robust_input() {
        let tool = tool_with(vec![SkillDefinition {
            name: "greet".into(),
            body: "Hello $ARGUMENTS!".into(),
            ..Default::default()
        }]);
        let ctx = dummy_ctx();

        // canonical input
        let r = tool
            .call(serde_json::json!({"skill": "greet", "args": "world"}), &ctx)
            .await;
        assert_eq!(r.result, "Hello world!");

        // tolerant { name } input
        let r = tool
            .call(serde_json::json!({"name": "greet", "arguments": "again"}), &ctx)
            .await;
        assert_eq!(r.result, "Hello again!");

        // leading slash lookup
        let r = tool.call(serde_json::json!({"skill": "/greet"}), &ctx).await;
        assert_eq!(r.result, "Hello !");

        // case-insensitive
        let r = tool.call(serde_json::json!({"skill": "GREET"}), &ctx).await;
        assert_eq!(r.result, "Hello !");
    }

    #[tokio::test]
    async fn not_found_lists_available() {
        let tool = tool_with(vec![SkillDefinition {
            name: "known".into(),
            description: "known skill".into(),
            ..Default::default()
        }]);
        let r = tool.call(serde_json::json!({"skill": "unknown"}), &dummy_ctx()).await;
        assert!(r.is_error());
        assert!(r.result.contains("Skill \"unknown\" not found. Available skills:"));
        assert!(r.result.contains("  - known: known skill"));
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let tool = tool_with(vec![]);
        let r = tool.call(serde_json::json!({}), &dummy_ctx()).await;
        assert!(r.result.contains("skill name is required"));
    }

    #[tokio::test]
    async fn fork_uses_sub_agent_runner() {
        struct FakeRunner;
        #[async_trait]
        impl SubAgentRunner for FakeRunner {
            async fn run(&self, prompt: String, _: Option<Vec<String>>, _: Option<Vec<String>>, _: u32) -> String {
                assert!(prompt.contains("fork body"));
                "fork result".to_string()
            }
        }
        let tool = SkillTool {
            skills: Arc::new(RwLock::new(vec![SkillDefinition {
                name: "f".into(),
                context: SkillContext::Fork,
                body: "fork body".into(),
                allowed_tools: Some(vec!["Read".into()]),
                ..Default::default()
            }])),
            sub_agent: Arc::new(RwLock::new(Some(Arc::new(FakeRunner) as Arc<dyn SubAgentRunner>))),
        };
        let r = tool.call(serde_json::json!({"skill": "f"}), &dummy_ctx()).await;
        assert_eq!(r.result, "fork result");
    }
}
