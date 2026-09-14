//! Slash commands — Rust port of `src/commands/index.ts` (single source of
//! truth). 15 commands + /quit alias.

use crate::cli::format::{blue, bold, dim, gold, green, yellow};
use crate::cli::format::CostTracker;
use crate::cli::session::{CommandOutcome, SessionState};
use crate::context::session::SessionStore;

pub struct CommandInfo {
    pub name: &'static str,
    pub description: &'static str,
}

pub fn command_list() -> Vec<CommandInfo> {
    vec![
        CommandInfo { name: "help", description: "Show available commands" },
        CommandInfo { name: "compact", description: "Compact conversation context" },
        CommandInfo { name: "clear", description: "Clear conversation history" },
        CommandInfo { name: "model", description: "Show or change model" },
        CommandInfo { name: "cost", description: "Show token usage and cost" },
        CommandInfo { name: "resume", description: "Resume a previous session" },
        CommandInfo { name: "plan", description: "Toggle plan mode (read-only)" },
        CommandInfo { name: "memory", description: "Show NANOCODE.md / CLAUDE.md content" },
        CommandInfo { name: "config", description: "Show current configuration" },
        CommandInfo { name: "status", description: "Show session status" },
        CommandInfo { name: "skills", description: "List available skills" },
        CommandInfo { name: "context", description: "Show context window usage" },
        CommandInfo { name: "exit", description: "Exit nanocode" },
        CommandInfo { name: "init", description: "Analyze codebase and generate NANOCODE.md" },
        CommandInfo { name: "mcp", description: "Show MCP server configuration" },
        CommandInfo { name: "quit", description: "Exit nanocode" }, // alias
    ]
}

/// /init prompt (commands/index.ts INIT_PROMPT).
const INIT_PROMPT: &str = "\
Analyze this codebase and create a NANOCODE.md file in the project root.

Do the following:
1. Read key project files to understand the codebase:
   - Package manifests: package.json, Cargo.toml, pyproject.toml, go.mod, pom.xml, etc.
   - Build/CI configs: Makefile, .github/workflows/, Dockerfile, etc.
   - Existing docs: README.md, CONTRIBUTING.md
   - Linter/formatter configs: .eslintrc*, prettier*, ruff.toml, .golangci.yml, etc.
   - Existing AI configs: CLAUDE.md, .cursorrules, .cursor/rules, AGENTS.md, .github/copilot-instructions.md
   - Use Glob and Read tools to explore. Check the directory structure first with Glob.

2. Based on your analysis, write a NANOCODE.md file using the Write tool. The file should be concise (under 100 lines) and include ONLY:
   - Common build, lint, test, and run commands (especially non-standard ones the AI wouldn't guess)
   - Key architectural patterns and conventions
   - Code style rules that differ from language defaults
   - Important gotchas or non-obvious workflows
   - File/directory structure overview if it's not obvious

3. Do NOT include:
   - Obvious things the AI can figure out from reading code (like \"this is a TypeScript project\")
   - Generic advice that applies to all projects
   - Long explanations — keep it terse, each rule on one line

If NANOCODE.md or CLAUDE.md already exists, read it first, then ask the user if you want to overwrite or merge.
";

pub async fn execute(name: &str, args: &str, state: &mut SessionState) -> CommandOutcome {
    match name {
        "help" => {
            let mut lines = vec!["Available commands:".to_string(), String::new()];
            for cmd in command_list() {
                if cmd.name == "quit" {
                    continue;
                }
                lines.push(format!(
                    "  {} {}",
                    blue(&format!("/{:<12}", cmd.name)),
                    cmd.description
                ));
            }
            lines.push(String::new());
            lines.push(dim("Type a message to start a conversation with the agent."));
            CommandOutcome::Message(lines.join("\n"))
        }

        "compact" => {
            if state.messages().is_empty() {
                return CommandOutcome::Message("Nothing to compact.".into());
            }
            match state.compact().await {
                Ok((old, new)) => CommandOutcome::Message(green(&format!("Compacted: {old} → {new} tokens"))),
                Err(e) => CommandOutcome::Error(e),
            }
        }

        "clear" => {
            state.clear_messages();
            CommandOutcome::Message("Conversation cleared.".into())
        }

        "model" => {
            if !args.trim().is_empty() {
                state.set_model(args.trim());
                CommandOutcome::Message(format!("Model changed to: {}", blue(args.trim())))
            } else {
                CommandOutcome::Message(format!("Current model: {}", blue(&state.model_config().model)))
            }
        }

        "cost" => {
            let tracker: &CostTracker = state.cost_tracker();
            let config = state.model_config();
            CommandOutcome::Message(
                [
                    format!("Model:    {}", blue(&config.model)),
                    format!("Turns:    {}", tracker.turns),
                    format!("Input:    {}", tracker.total_input_tokens),
                    format!("Output:   {}", tracker.total_output_tokens),
                    format!("Cache R:  {}", tracker.total_cache_read_tokens),
                    format!("Cache W:  {}", tracker.total_cache_creation_tokens),
                    format!("Cost:     {}", gold(&format!("${:.4}", tracker.total_cost_usd(&config)))),
                ]
                .join("\n"),
            )
        }

        "resume" => {
            if args.trim().is_empty() {
                let store = SessionStore::default();
                let sessions = store.list_sessions();
                if sessions.is_empty() {
                    return CommandOutcome::Message("No previous sessions found.".into());
                }
                let mut lines = vec!["Recent sessions:".to_string(), String::new()];
                for s in sessions.iter().take(10) {
                    lines.push(format!(
                        "  {}  {}  {}  {}",
                        blue(&s.id[..8.min(s.id.len())]),
                        dim(&format_time(s.updated_at)),
                        if s.cwd.is_empty() { "(unknown)".to_string() } else { s.cwd.clone() },
                        dim(&format!("({} msgs)", s.message_count))
                    ));
                }
                lines.push(String::new());
                lines.push(dim("Use /resume <session-id-prefix> to resume."));
                CommandOutcome::Message(lines.join("\n"))
            } else {
                let prefix = args.trim().to_lowercase();
                let store = SessionStore::default();
                let sessions = store.list_sessions();
                let Some(matched) = sessions.iter().find(|s| s.id.to_lowercase().starts_with(&prefix)) else {
                    return CommandOutcome::Message(format!("No session found matching \"{prefix}\"."));
                };
                let loaded = store.load_session(&matched.id);
                if loaded.is_empty() {
                    return CommandOutcome::Message(format!(
                        "Session {} is empty or not found.",
                        &matched.id[..8.min(matched.id.len())]
                    ));
                }
                let count = loaded.len();
                state.set_messages(loaded, &matched.id);
                CommandOutcome::Message(format!(
                    "Resumed session {} ({count} messages).",
                    blue(&matched.id[..8.min(matched.id.len())])
                ))
            }
        }

        "plan" => {
            if state.permission_mode() == crate::core::types::PermissionMode::Plan {
                state.set_permission_mode(crate::core::types::PermissionMode::Default);
                CommandOutcome::Message("Plan mode disabled. All tools available.".into())
            } else {
                state.set_permission_mode(crate::core::types::PermissionMode::Plan);
                CommandOutcome::Message(format!(
                    "{} Only read-only tools available.",
                    yellow("Plan mode enabled.")
                ))
            }
        }

        "memory" => {
            let content = crate::context::memory::load_claude_md(&state.cwd);
            if content.is_empty() {
                return CommandOutcome::Message("No NANOCODE.md or CLAUDE.md found.".into());
            }
            let truncated = if content.len() > 3000 {
                let head: String = content.chars().take(3000).collect();
                format!("{head}{}", dim(&format!("\n\n... ({} chars total)", content.len())))
            } else {
                content
            };
            CommandOutcome::Message(format!(
                "{}\n{truncated}",
                dim("─── NANOCODE.md / CLAUDE.md ───")
            ))
        }

        "config" => {
            let config = state.model_config();
            CommandOutcome::Message(
                [
                    format!("Model:       {}", blue(&config.model)),
                    format!("Context:     {}", config.context_window),
                    format!("Max output:  {}", config.max_output_tokens),
                    format!("Mode:        {:?}", state.permission_mode()),
                    format!("CWD:         {}", blue(&state.cwd.to_string_lossy())),
                    format!("Session:     {}", dim(&state.session_id[..8.min(state.session_id.len())])),
                    format!("Tools:       {} loaded", state.tool_count()),
                ]
                .join("\n"),
            )
        }

        "status" => {
            let tokens = crate::context::token_counting::estimate_message_tokens(&state.messages());
            let config = state.model_config();
            let threshold = config.context_window as i64 - config.max_output_tokens as i64 - 13000;
            let pct = ((tokens as f64 / threshold as f64) * 100.0) as i64;
            CommandOutcome::Message(
                [
                    format!("Session:    {}", dim(&state.session_id[..8.min(state.session_id.len())])),
                    format!("Model:      {}", blue(&config.model)),
                    format!("Messages:   {}", state.messages().len()),
                    format!("Tokens:     {tokens} / {threshold} {}", dim(&format!("({pct}%)"))),
                    format!("Mode:       {:?}", state.permission_mode()),
                    format!("CWD:        {}", blue(&state.cwd.to_string_lossy())),
                    format!("Files mod:  {}", state.modified_file_count()),
                    format!("Snapshots:  {}", state.snapshot_count()),
                ]
                .join("\n"),
            )
        }

        "skills" => {
            let skills = state.skills();
            if skills.is_empty() {
                return CommandOutcome::Message(
                    "No skills loaded. Place skills in .nanocode/skills/ or .claude/skills/ directories.".into(),
                );
            }
            let mut lines = vec!["Available skills:".to_string(), String::new()];
            for s in skills {
                lines.push(format!("  {}  {}", blue(&s.name), dim(&s.description)));
            }
            CommandOutcome::Message(lines.join("\n"))
        }

        "context" => {
            let config = state.model_config();
            let tokens = crate::context::token_counting::estimate_message_tokens(&state.messages());
            let usable = config.context_window as i64 - config.max_output_tokens as i64 - 13000;
            let pct = (((tokens as f64 / usable as f64) * 100.0) as i64).min(100);

            let bar_width = 40;
            let filled = ((pct as f64 / 100.0) * bar_width as f64) as usize;
            let bar = format!(
                "[{}{}]",
                blue(&"█".repeat(filled)),
                dim(&"░".repeat(bar_width - filled))
            );

            let system_est = (config.context_window as i64) / 100;
            let tools_est = (state.tool_count() * 150) as i64;
            let msg_est = (tokens - system_est - tools_est).max(0);
            let free_est = (usable - tokens).max(0);

            CommandOutcome::Message(
                [
                    format!("Context Usage  {}", blue(&config.model)),
                    format!("{bar} {pct}%"),
                    String::new(),
                    format!("  Messages:      ~{msg_est} tokens"),
                    format!("  System prompt: ~{system_est} tokens"),
                    format!("  Tool schemas:  ~{tools_est} tokens"),
                    format!("  Free:          ~{free_est} tokens"),
                    String::new(),
                    format!(
                        "  Total: {tokens} / {usable}  {}",
                        dim(&format!("(window: {})", config.context_window))
                    ),
                ]
                .join("\n"),
            )
        }

        "exit" | "quit" => CommandOutcome::Exit,

        "init" => CommandOutcome::Prompt(INIT_PROMPT.to_string()),

        "mcp" => {
            let config = crate::mcp::load_mcp_config(&state.cwd);
            let names: Vec<&String> = config.keys().collect();
            if names.is_empty() {
                CommandOutcome::Message(
                    [
                        "No MCP servers configured.".to_string(),
                        String::new(),
                        "Add servers to:".to_string(),
                        format!("  Project: {}", blue(&format!("{}/.nanocode/settings.json", state.cwd.display()))),
                        format!("  User:    {}", blue(&format!("{}/.nanocode/settings.json", dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default()))),
                        String::new(),
                        dim("Example settings.json:"),
                        dim("  { \"mcpServers\": { \"my-server\": { \"command\": \"npx\", \"args\": [\"-y\", \"...\"] } } }"),
                    ]
                    .join("\n"),
                )
            } else {
                let mut lines = vec![format!("MCP Servers ({}):", names.len()), String::new()];
                for name in &names {
                    let cfg = &config[*name];
                    lines.push(format!(
                        "  {}  {}",
                        blue(name),
                        dim(&[cfg.command.clone(), cfg.args.clone().unwrap_or_default().join(" ")].join(" "))
                    ));
                }
                lines.push(String::new());
                lines.push(format!(
                    "Config: {}",
                    blue(&format!("{}/.nanocode/settings.json", state.cwd.display()))
                ));
                CommandOutcome::Message(lines.join("\n"))
            }
        }

        other => CommandOutcome::Message(format!("Unknown command: /{other}. Try /help.")),
    }
}

fn format_time(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {:02}:{:02}", tod / 3600, (tod % 3600) / 60)
}

// silence unused import (bold used by session.rs)
#[allow(unused)]
fn _touch() {
    let _ = bold("x");
}
